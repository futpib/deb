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
```

The socket is unnamed and inherited at process creation. `SOCK_SEQPACKET` preserves message boundaries, so there is no separate length-prefix parser and a partial stream message cannot be mistaken for another command. The transport rejects truncated packets, packets larger than 256 KiB, and malformed protobuf. The inherited endpoint is marked close-on-exec as soon as the helper owns it so browser child processes do not retain the control channel.

## Session lifecycle

1. The shell sends `Hello` with its packet limit and required capabilities.
2. The helper returns `HelloReply` with the actual engine, engine version, CEF API version, limit, and capabilities. An engine mismatch or missing required capability aborts startup.
3. The shell sends `CreateBrowser` with a logical browser ID, initial URL, X11 parent window, and viewport.
4. Creation is complete only after both a successful response and a `SurfaceReady` event. Their order is deliberately unspecified.
5. Navigation, resize, focus, reload, and close are requests with nonzero IDs. Every accepted request receives exactly one response with the same ID.
6. Asynchronous state uses browser-scoped, monotonically increasing event sequence numbers. The shell rejects duplicate or reordered events.

The current helper supports one logical browser per process. Browser IDs are already part of every request and event so multiplexing can be added without changing message shapes.

## Build-locked contract

The schema is internal to this application. There is no wire-version negotiation or compatibility promise between separately built binaries. The Qt shell, Chromium helper, and staged Firefox helper must be rebuilt together from the same checkout.

- Schema changes may be incompatible as long as every binary is rebuilt and staged in the same build.
- Capabilities describe backend behavior within that build; they are not a cross-version negotiation mechanism.
- Request IDs are unique among in-flight requests and never zero. Events use request ID zero. Errors are structured as stable machine-readable codes plus human-readable and backend-specific details.
- Presentation is always an X11 native child window.

`cef_api_version` is diagnostic metadata. Capabilities verify that the selected helper implements everything this build of the shell will use.

## Source layout

- `shell-protocol/proto/shell.proto`: canonical wire schema
- `shell-protocol/src/lib.rs`: framing, validation, and child socket setup
- `src/native.rs`: shell client, negotiation, request correlation, and event validation
- `cef-renderer/src/main.rs`: engine-neutral protocol server and CEF task dispatch

Changes to this boundary should include transport/schema tests, a full workspace test and lint run, and an on-screen startup/navigation check against both backends.
