# Dual-engine browser prototype

This is the first vertical slice of a Linux browser shell that can host Chromium and Gecko tabs. The shell and backend boundary use the official Qt Bridge for Rust from the start.

The current milestone renders the same URL into two side-by-side Qt Quick panes:

- Chromium is a real CEF 150 off-screen browser using `cef-rs` and `CefRenderHandler::OnPaint`.
- Gecko is a real Firefox 153 headless render launched behind the provisional engine-process boundary.

Both panes are snapshots. They prove that Qt Bridge, QML, Rust orchestration, CEF subprocesses, and both rendering engines work together, but they are not interactive web views yet. In particular, the Firefox helper is not yet a drop-in `libcef.so`; that requires a maintained Gecko embedding runtime plus the CEF ABI adapter discussed below.

## Requirements

- Rust 1.87 or newer
- Qt 6.10 or newer with Qt Base and Qt Declarative
- CEF 150.0.14
- Firefox
- Xvfb for headless CEF tests

On Arch Linux:

```sh
paru -S cef firefox qt6-base qt6-declarative xorg-server-xvfb
./scripts/setup-arch-cef.sh
```

The setup script stages symlinks to Arch's CEF runtime in the ignored `cef-runtime/` directory. If that directory is absent, `cef-rs` can download its matching CEF archive during the first build instead.

## Build and run

```sh
cargo build --workspace
cargo run -p dual-engine-browser
```

The application initially renders `https://www.google.com/`. Enter another URL and select **Render both** to compare the engines.

### Verification note

On the development host, the complete two-pane path was verified with a `data:` URL and Firefox independently rendered Google over HTTPS. CEF 150 did not produce a settled Google frame within its 30-second limit. The matching system Chromium 150 binary also stalls on external HTTPS there, so the prototype reports the CEF timeout instead of treating a blank frame as success. This host-specific Chromium networking issue remains to be resolved before Google can be claimed as verified in both panes.

For a display-independent smoke test:

```sh
DUAL_ENGINE_URL='data:text/html,Hello%20Engines' QT_QPA_PLATFORM=offscreen timeout 12s target/debug/dual-engine-browser
```

Rendered snapshots are written to `/tmp/dual-engine-browser/`.

## Architecture after this milestone

The snapshot API is deliberately an engine-process boundary rather than UI code tied to either engine. The next rendering milestone is:

1. Replace PNG completion messages with shared-memory BGRA frame streams.
2. Add pointer, keyboard, focus, resize, and IME messages in the opposite direction.
3. Present those streams through a custom Qt Quick scene-graph item.
4. Replace the stock Firefox screenshot command with an in-tree patched Gecko runtime.
5. Put a CEF C/C++ ABI facade in front of that Gecko runtime, initially implementing the browser, frame, host, client, render-handler, load-handler, and request-context paths used by this shell.

That turns the current `Firefox / Gecko adapter bootstrap` pane into the first controlled consumer of the Gecko-backed CEF implementation without claiming arbitrary CEF application compatibility prematurely.
