# deb

`deb` is a Linux/X11 desktop browser shell that puts Chromium and Gecko in one Qt Quick window. The Rust/Qt shell uses Qt Bridges, and both panes are native child windows controlled through the same CEF-facing `cef-renderer` executable.

| Pane | `cef-renderer` resolves `libcef.so` to | Engine |
| --- | --- | --- |
| Chromium | Arch CEF | Chromium |
| Firefox | the private `firefox-cef` adapter | Gecko from the pinned Firefox source tree |

The Firefox side is not automation of a separately installed Firefox. Its private helper loads the project-built CEF adapter, the adapter loads `libxul.so`, and a small component compiled into that Firefox runtime provides the bridge between CEF operations and Gecko. The patched GTK widget creates the `FirefoxCEF` surface as a direct X11 child of the Qt host.

## Architecture

```text
Qt/Rust application process
  Qt GUI thread
    Chromium host QWidget ── X11 parent ──┐
    Firefox host QWidget  ── X11 parent ──┼──────────────────────────┐
                                          │                          │
Chromium cef-renderer process             │  Firefox cef-renderer process
  helper main/control threads             │    main thread: CEF loop -> XRE_main
  Arch libcef.so                          │    private libcef.so (Rust adapter)
  Chromium CEF UI thread                  │      -> libxul.so bridge
  Chromium child processes                │    FirefoxCEF GTK child ─────────┘
    renderer / GPU / network ─────────────┘    Gecko child processes
                                                web content / socket / RDD
```

Both helpers have the same `DT_NEEDED: libcef.so` dependency and execute the same CEF calls. Loader isolation selects the implementation. The Chromium helper enables CEF's multi-threaded loop; the Gecko helper runs the CEF loop and XRE on its process main thread. Standard Chromium and Gecko content isolation remains in place behind those browser processes.

The shell and each helper communicate over a private inherited `AF_UNIX/SOCK_SEQPACKET` socket. Messages use the internal protobuf schema in `shell-protocol/proto/shell.proto`; stdout and stderr are not protocol channels. Startup verifies the engine identity, CEF API version, packet limit, and capabilities before the shell requests an X11 browser child. Runtime operations use correlated request/response packets, while surface, loading, and lifecycle changes are ordered events. See [the shell protocol design](docs/shell-protocol.md).

Linux Firefox links its allocator glue into the launcher rather than shipping it as a reusable shared object. The staging script therefore links `libmozglue-cef.so` from the pinned build's exact launcher object list and preloads it before the adapter loads `libxul.so`. Gecko startup and child startup use Mozilla's `Bootstrap` interface, including the required null-terminated argument vectors.

## Requirements

- Linux desktop with an X11 display server
- Rust with Edition 2024 support
- Qt 6 Base and Qt 6 Declarative development files
- CEF matching `cef = 150.2.1`
- Firefox build prerequisites and enough space for a full Firefox object tree
- `pkg-config`, protobuf, a C/C++ toolchain, Python, Node.js, and the Firefox-provided build toolchains

On Arch Linux, the core host packages can be installed with:

```sh
paru -S --needed base-devel cef pkgconf protobuf qt6-base qt6-declarative
```

If Firefox's build reports another missing prerequisite, run `./mach bootstrap` in the pinned `firefox` submodule and select the desktop Firefox build environment.

## Build and run

Initialize the pinned Firefox source and stage Arch's CEF runtime:

```sh
git submodule update --init firefox
scripts/setup-arch-cef.sh
```

Build the patched Firefox runtime, the Gecko-backed adapter, the shared helper, and the Qt shell:

```sh
scripts/build-firefox-cef.sh
```

The first Firefox build is large. Later runs reuse `target/firefox-source` and `target/firefox-obj` and are incremental. The script refuses a Firefox worktree that is not at the submodule's pinned commit, applies `firefox-patches/0001-firefox-cef-runtime.patch`, overlays the maintained bridge sources, and stages the result under `target/debug/firefox-cef-runtime`.

Run the application:

```sh
cargo run -p deb
```

It opens `deb://new-tab/` in both panes. Enter another URL and select **Navigate both** to send the same CEF `load_url` operation to both implementations.

## Internal pages

`deb://` is the build-private internal-page origin shared by both engines. Chromium registers it as a standard, local, secure, display-isolated CEF scheme and serves requests through a `CefSchemeHandlerFactory`. Gecko registers an `nsIProtocolHandler` with equivalent local and trustworthy flags. Both implementations currently expose `deb://new-tab/` and serve the exact same source file, `internal-pages/new-tab.html`; unknown `deb://` routes fail closed.

The page is packaged into the staged Firefox chrome archive during `scripts/build-firefox-cef.sh`, but its channel retains the `deb://new-tab/` original URI and a `deb://` content principal. It does not receive Firefox chrome privileges. The CEF-facing Firefox adapter accepts the corresponding scheme-factory registration used by the shared helper, while Gecko performs delivery through its native protocol handler.

## CEF compatibility boundary

`firefox-cef` is a real exported CEF C ABI implementation for this controlled client. It is not yet a drop-in replacement for arbitrary CEF applications. Matching symbol names and object layouts is only part of compatibility; general CEF clients can depend on hundreds of methods and Chromium-specific behavior.

The implemented slice covers:

- API version/hash queries, process entry, initialization, shutdown, and UTF string functions
- synchronous and asynchronous browser creation
- CEF message-loop and UI-task dispatch
- reference-counted browser, browser-host, main-frame, and task objects
- life-span, loading-state, and load-error callbacks
- navigation, reload, focus, resize, native-window lookup, and close
- registration of the build-owned `deb://new-tab/` internal page

The Qt shell has no Firefox-specific navigation API. Extending Gecko support means implementing another coherent CEF behavior slice in `firefox-cef` and its Firefox bridge.

## Current limitations

- The Gecko adapter supports one browser surface per helper process. Multiple profiles/accounts and CEF request-context isolation are not implemented.
- Popups, downloads, extensions, accessibility integration, off-screen rendering, devtools, arbitrary application-defined custom schemes, CEF cookie/request-context APIs, and request interception are outside the current CEF slice.
- X11 native child windows are the sole presentation backend.
- Each launch uses temporary Chromium and Firefox profile directories.

## Verification

Static loader checks:

```sh
readelf -d target/debug/cef-renderer
nm -D --defined-only target/debug/firefox-cef-runtime/libcef.so
nm -D --defined-only target/debug/firefox-cef-runtime/libxul.so
```

`readelf` should report `libcef.so` for the shared helper. The staged adapter should export the implemented `cef_*` entry points, and `libxul.so` should export the `firefox_cef_gecko_*` bridge.

Local checks:

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
shellcheck scripts/build-firefox-cef.sh scripts/setup-arch-cef.sh
```

To exercise a real runtime `Navigate` request after both engines report `SurfaceReady`, launch the staged application with an explicit smoke URL:

```sh
DEB_SMOKE_NAVIGATE_URL=https://example.com cargo run -p deb
```

This hook is opt-in and does not change normal startup. A successful smoke run renders the requested page in both native children through the same shell-protocol request path used by **Navigate both**.
