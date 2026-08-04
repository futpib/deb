# Dual-engine browser prototype

This is a Linux/X11 Qt Quick shell that drives Chromium and Firefox through the same CEF-facing helper. The shell uses the official Qt Bridge for Rust from the start, and both engine panes are live on-screen surfaces.

| Pane | Shared helper dependency | Loaded implementation | Browser engine |
| --- | --- | --- | --- |
| Chromium | `DT_NEEDED: libcef.so` | Arch CEF's `libcef.so` | Chromium |
| Firefox | `DT_NEEDED: libcef.so` | `libfirefox_cef.so`, exposed as `libcef.so` in a private loader directory | Gecko in stock Firefox |

`cef-renderer` is one binary, not separate Chromium and Firefox integrations. The controller starts it normally for Chromium. For Firefox it creates a private `libcef.so` symlink pointing to `libfirefox_cef.so` and sets that helper's loader path. The Gecko adapter then implements the CEF 150 C ABI subset used by the helper, including initialization, browser/host/frame objects, reference counting, load and life-span callbacks, UI-thread task dispatch, navigation, resize, focus, and shutdown.

The adapter currently launches stock Firefox with an isolated profile behind that ABI. It removes the helper's loader overrides before starting Firefox, locates the resulting X11 window, and keeps its outer frame aligned and stacked over a Qt-owned host surface. The shell itself has no Firefox-specific process or navigation path.

## Requirements

- Rust 1.87 or newer
- A C++ compiler and `pkg-config`
- Qt 6.10 or newer with Qt Base and Qt Declarative development files
- CEF 150.0.14
- Firefox
- An X11 display, or XWayland for experimentation
- Xvfb and a small window manager such as Openbox for isolated UI tests

On Arch Linux:

```sh
paru -S --needed base-devel cef firefox openbox pkgconf qt6-base qt6-declarative xorg-server-xvfb
./scripts/setup-arch-cef.sh
```

The setup script stages symlinks to Arch's CEF runtime in the ignored `cef-runtime/` directory. If that directory is absent, `cef-rs` can download its matching CEF archive during the first build instead.

## Build and run

```sh
cargo build --workspace
cargo run -p dual-engine-browser
```

The application starts both helpers at `https://www.google.com/`. Enter another URL and select **Navigate both** to send the same CEF `load_url` operation to each implementation. The application selects Qt's `xcb` platform when `DISPLAY` is available because the native-window integration is X11-specific. Chromium uses its basic local password store for the helper's temporary profile so cookie initialization does not block on a desktop keyring.

By default the Firefox helper loads `target/debug/libfirefox_cef.so`. A different shim build can be selected with:

```sh
DUAL_ENGINE_FIREFOX_CEF=/path/to/libfirefox_cef.so cargo run -p dual-engine-browser
```

## Compatibility boundary

This is a real Gecko-backed CEF ABI shim for this controlled client, not a drop-in replacement for every CEF application. Exporting the same functions and object layouts is only the first compatibility layer; arbitrary CEF clients can call hundreds of methods and rely on Chromium-specific multiprocess, request-context, extension, accessibility, popup, off-screen rendering, and callback behavior that this prototype does not implement.

The current subset exports:

- CEF API hash/version, process entry, initialize, shutdown, and UTF string functions
- synchronous and asynchronous browser creation and CEF message-loop/task functions
- reference-counted browser, browser-host, and main-frame objects
- client life-span and loading callbacks
- main-frame navigation, reload, focus, resize, window handle, and close behavior

Extending the shim means implementing another coherent slice of CEF behavior in `firefox-cef`, not adding a second API to the Qt shell.

## Current limitations

- Gecko is not linked in-process. `libfirefox_cef.so` owns a stock Firefox subprocess and adapts it to the CEF subset.
- Firefox remains a coordinated top-level X11 window rather than a reparented child because stock Firefox's compositor stopped painting when reparented across processes. Clipping and unusual window-manager transitions are therefore limited.
- A navigation opens a new Firefox tab through the isolated profile. The temporary profile lasts for one helper lifetime; persistent accounts and CEF request-context/profile APIs are not implemented yet.
- Keyboard focus and IME forwarding into the Chromium child are incomplete. Native repaint, resize, mouse activation, and URL-bar navigation are wired.

## Verification

The shared-helper loader split can be inspected statically:

```sh
readelf -d target/debug/cef-renderer
nm -D --defined-only target/debug/libfirefox_cef.so
```

`readelf` should show one `NEEDED` entry for `libcef.so`. During a run, `/proc/<chromium-helper>/maps` resolves it to the real `libcef.so`, while `/proc/<firefox-helper>/maps` resolves the same dependency to `libfirefox_cef.so`.

Local checks:

```sh
cargo fmt --all --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```
