# Dual-engine browser prototype

This is a Linux/X11 vertical slice of a Qt Quick browser shell with two live, on-screen engine surfaces. The shell and backend boundary use the official Qt Bridge for Rust from the start.

- Chromium runs in a persistent CEF 150 helper. CEF creates a native child window inside a Qt-owned `QWindow`, which QML presents with `WindowContainer`.
- Gecko runs in a stock Firefox 153 process. Its native X11 window is positioned and stacked over the matching Qt pane.
- The shared URL bar navigates both engines. CEF navigates in place; this prototype restarts the isolated Firefox process with the new URL.

The Firefox side is deliberately not described as a Gecko-backed CEF implementation. Stock Firefox's compositor stops painting page content when its window is reparented into another process, so this milestone keeps it as a managed top-level window. A real `firefox-cef` would need a maintained Gecko embedding runtime and a separate CEF ABI adapter.

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

The application starts both engines at `https://www.google.com/`. Enter another URL and select **Navigate both** to compare them. The application selects Qt's `xcb` platform when `DISPLAY` is available because the native-window integration is X11-specific.

## Current limitations

- The Firefox pane is a coordinated top-level window, not a true child. It can briefly outlive the shell's stacking during window-manager transitions, and clipping is limited to ordinary rectangular pane geometry.
- Keyboard focus and IME forwarding into the cross-process CEF child are not complete. Navigation through the Qt URL bar works; native CEF repaint, resize, and mouse activation are wired.
- Firefox navigation currently creates a fresh temporary profile and process, so browser history and login state do not persist between navigations.
- This is not yet a `libcef.so`-compatible Gecko adapter. The stock Firefox process is an integration stand-in while that much larger runtime and ABI project remains separate.

## Verification note

The complete on-screen path was exercised under Xvfb/Openbox: the Qt-owned CEF child rendered and resized live `data:` content, while the managed Firefox window rendered Google over HTTPS. On this development host, CEF 150's network service and the matching system Chromium 150 both stall on external URLs, so Google could not be truthfully marked as verified in the CEF pane here. The default remains Google so the same build can exercise it on a normal desktop/network environment.

Useful local checks are:

```sh
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Toward a Firefox CEF backend

The next Gecko-specific milestone is independent of this window-management prototype:

1. Maintain an embeddable Gecko runtime rather than driving stock Firefox UI.
2. Expose browser, frame, host, client, load-handler, request-context, and off-screen-rendering primitives through a stable C/C++ boundary.
3. Implement the matching subset of the CEF exported ABI and preserve CEF's process/callback semantics.
4. Add shared profile/request-context handling, input, IME, accessibility, popup, and lifecycle behavior before expanding compatibility.

That produces an honest Gecko-backed CEF subset for this shell without claiming arbitrary third-party CEF application compatibility.
