# Shell/helper protocol

The Qt shell talks to Chromium and Gecko helpers through the same engine-neutral protocol. This is the application boundary; engine-internal Chromium Mojo/IPC and Gecko IPDL remain behind each helper.

## Transport and process boundary

```text
Qt/Rust process                       cef-renderer process

controller thread                    control reader thread
  encode protobuf request              decode request
  send SOCK_SEQPACKET ───────────────> post task to CEF UI thread

protocol reader thread               CEF UI/main threads
  recv SOCK_SEQPACKET <─────────────── response or ordered event
  update controller state              serialized by ProtocolEmitter

fd 3: inherited AF_UNIX/SOCK_SEQPACKET socketpair endpoint
optional SCM_RIGHTS: dma-buf, fence, or shared-memory descriptors
```

The socket is unnamed and inherited at process creation. `SOCK_SEQPACKET` preserves message boundaries, so there is no separate length-prefix parser and a partial stream message cannot be mistaken for another command. The transport rejects truncated packets, packets larger than 256 KiB, malformed protobuf, surplus descriptors, and descriptor metadata that does not match the `SCM_RIGHTS` payload. The inherited endpoint is marked close-on-exec as soon as the helper owns it so browser child processes do not retain the control channel.

## Session lifecycle

1. The shell sends `Hello` with its supported major-version range, packet limit, and requested capabilities.
2. The helper returns `HelloReply` with the selected version, actual engine, engine version, CEF API version, limit, and capabilities. An engine mismatch or missing required capability aborts startup.
3. The shell sends `CreateBrowser` with a logical browser ID, initial URL, presentation target, and viewport.
4. Creation is complete only after both a successful response and a `SurfaceReady` event. Their order is deliberately unspecified.
5. Navigation, resize, focus, reload, and close are requests with nonzero IDs. Every accepted request receives exactly one response with the same ID.
6. Asynchronous state uses browser-scoped, monotonically increasing event sequence numbers. The shell rejects duplicate or reordered events.

The current helper supports one logical browser per process. Browser IDs are already part of every request and event so multiplexing can be added without changing message shapes.

## Evolution rules

The checked-in schema is `dual_engine.shell.v1` and protocol version 1.0.

- A major version changes only for an incompatible semantic or wire change. Peers must reject a session when their supported major ranges do not overlap.
- A minor version is additive. Add new protobuf fields, oneof variants, enum values, requests, events, and capabilities; do not change the meaning or type of an existing field.
- Never reuse a removed field number or enum value. Reserve it in the schema when deleting a field.
- Unknown protobuf fields and unrequested optional events must be ignored. A feature that affects behavior must also have a capability so the shell can gate its use before sending the corresponding request.
- Request IDs are unique among in-flight requests and never zero. Events use request ID zero. Errors are structured as stable machine-readable codes plus human-readable and backend-specific details.
- Presentation is negotiated explicitly. X11 is implemented now. Wayland tokens, dma-buf streams, off-screen surfaces, and attached-file metadata are schema reservations, not claims that those paths work yet.

CEF API compatibility and shell-protocol compatibility are independent. `cef_api_version` is diagnostic metadata; protocol capabilities determine what the shell may use.

## Source layout

- `shell-protocol/proto/shell.proto`: canonical wire schema
- `shell-protocol/src/lib.rs`: framing, descriptor passing, validation, and child-FD setup
- `src/native.rs`: shell client, negotiation, request correlation, and event validation
- `cef-renderer/src/main.rs`: engine-neutral protocol server and CEF task dispatch

Changes to this boundary should include transport/schema tests, a full workspace test and lint run, and an on-screen startup/navigation check against both backends.
