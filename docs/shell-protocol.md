# Shell/helper protocol

The Qt shell talks to Chromium and Gecko helpers through the same engine-neutral protocol. This is the application boundary; engine-internal Chromium Mojo/IPC and Gecko IPDL remain behind each helper.

## Transport and process boundary

```text
Qt/Rust process                       cef-renderer process

controller thread                    control reader thread
  encode protobuf request              decode request
  send SOCK_SEQPACKET ───────────────> post task to CEF UI thread

protocol reader thread               CEF UI/main threads
  recv SOCK_SEQPACKET <─────────────── response, ordered event, DMA-BUF FDs
  update controller state              serialized by ProtocolEmitter

Qt Quick render thread               engine compositor / GPU producer
  EGLImage import                      leased native-pixmap frame
  sample texture in Qt scene
  retire behind GL fence
  FrameRelease ──────────────────────> recycle native buffer

fd 3: inherited AF_UNIX/SOCK_SEQPACKET socketpair endpoint
```

The socket is unnamed and inherited at process creation. `SOCK_SEQPACKET` preserves message boundaries, so there is no separate length-prefix parser and a partial stream message cannot be mistaken for another command. DMA-BUF plane descriptors and optional acquire fences are attached to the matching `AcceleratedFrame` packet with `SCM_RIGHTS`. The transport rejects truncated packets, packets larger than 256 KiB, malformed protobuf, and excessive ancillary descriptors. The inherited endpoint is marked close-on-exec as soon as the helper owns it so browser child processes do not retain the control channel.

## Session lifecycle

1. The shell sends `Hello` with its packet limit and required capabilities.
2. The helper returns `HelloReply` with the actual engine, engine version, CEF API version, limit, and capabilities. An engine mismatch or missing required capability aborts startup.
3. The shell binds the tab to a logical Qt surface, then sends `CreateBrowser` with a logical browser ID, profile ID, resolved XDG data/cache directories, initial URL, top-level X11 handle, and viewport. The helper validates and creates the profile directories before initializing its CEF implementation. Chromium uses the handle only for its windowless CEF host context; neither backend creates an embedded child window.
4. Creation is complete only after both a successful response and a `SurfaceReady` event. Their order is deliberately unspecified.
5. Additional browser IDs can be created in the same helper when their profile ID and paths match the process profile. Navigation, resize, focus, visibility, pointer and keyboard input, context-menu completion, reload, developer-tools, cookie, and close operations route to the addressed browser.
6. Every accepted request has a nonzero ID and receives exactly one response with the same ID. Process-scoped shutdown uses browser ID zero.
7. Asynchronous state uses a process-wide, monotonically increasing event sequence. Every event also carries its browser ID, and the shell rejects duplicate or reordered events before routing it to a tab.

Each helper supports multiple logical browsers in one engine-native profile. A running shell owns at most one Chromium helper and one Gecko helper per open deb profile, regardless of how many Qt windows that profile spans. Chromium maps browser IDs to windowless Alloy CEF browser instances. Gecko maps browser IDs to remote `<browser>` elements backed by headless widgets. Both implementations invoke the CEF accelerated-paint callback with DMA-BUF metadata; the shared helper emits `AcceleratedFrame`, and the shell holds the frame lease until Qt has finished sampling it. The shell caches the newest view-frame lease by browser ID even when that browser has no active Qt surface. Binding the tab to a window reuses that lease immediately and updates its presentation generation; a newer frame replaces it, while popup frames remain surface-local. Replacing a pending or cached frame, destroying its scene node, or failing an import eventually emits exactly one `FrameRelease`. Moving a tab changes the browser-to-surface routing and resizes the browser; browser IDs and helper ownership do not change. Pointer, touch, and keyboard events take the reverse path as ordinary correlated commands and are delivered through CEF browser-host input methods. The engine's resulting `OnCursorChange` callback becomes a browser-scoped `CursorChanged` event; the shell caches it by browser ID and applies it to whichever Qt surface currently presents that tab. A handled CEF context-menu request becomes `ContextMenuRequested`, which carries the pointer location and a bounded nested model of visible commands, separators, enabled and checked state. The shell shows that model as a native menu and sends exactly one `ContextMenuCommand` to select a command or dismiss the pending callback; opening another menu or closing its browser cancels the older callback. HTML Fullscreen API transitions similarly become browser-scoped `FullscreenChanged` events so the shell can fullscreen the tab's owning Qt window and hide its chrome. Escape or hiding the browser calls CEF's fullscreen-exit method before normal input/visibility processing. `OpenDevTools` is also browser-scoped: Chromium uses `CefBrowserHost::ShowDevTools`, and the Firefox adapter resolves the target browsing context through Mozilla's `CommandsFactory` and opens `gDevTools` in its window host. Renderer/content crash events remain browser-scoped; a helper-process failure causes the shell to recreate every affected tab in a replacement helper.

## Build-locked contract

The schema is internal to this application. There is no wire-version negotiation or compatibility promise between separately built binaries. The Qt shell, Chromium helper, and staged Firefox helper must be rebuilt together from the same checkout.

- Schema changes may be incompatible as long as every binary is rebuilt and staged in the same build.
- Capabilities describe backend behavior within that build; they are not a cross-version negotiation mechanism.
- Request IDs are unique among in-flight requests and never zero. Events use request ID zero. Errors are structured as stable machine-readable codes plus human-readable and backend-specific details.
- Presentation is build-locked to X11/EGL: leased DMA-BUF frames imported into Qt Quick without an engine child window or CPU copy.

`cef_api_version` is diagnostic metadata. Capabilities verify that the selected helper implements everything this build of the shell will use.

## Source layout

- `shell-protocol/proto/shell.proto`: canonical wire schema
- `shell-protocol/src/lib.rs`: framing, validation, and child socket setup
- `src/native.rs`: shell client, negotiation, request correlation, and event validation
- `src/tab_controller.rs`: logical tabs, helper ownership, browser routing, visibility, recovery, and cookie synchronization
- `cef-renderer/src/main.rs`: engine-neutral protocol server and CEF task dispatch

Changes to this boundary should include transport/schema tests, a full workspace test and lint run, and an on-screen startup/navigation check against both backends.
