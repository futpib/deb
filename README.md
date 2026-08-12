# deb

`deb` is a Linux/X11 desktop browser shell whose tabs can run in Chromium or Gecko across multiple Qt Quick windows. The Rust/Qt shell uses Qt Bridges, and each visible window's active tab is an accelerated Qt Quick scene item controlled through the same CEF-facing `cef-renderer` executable for either engine.

| Tab engine | `cef-renderer` resolves `libcef.so` to | Engine |
| --- | --- | --- |
| Chromium | the project-built, patched CEF runtime | Chromium from CEF's pinned Chromium checkout |
| Firefox | the private `firefox-cef` adapter | Gecko from the pinned Firefox source tree |

The Firefox side is not automation of a separately installed Firefox. Its private helper loads the project-built CEF adapter, the adapter loads `libxul.so`, and a small component compiled into that Firefox runtime provides the bridge between CEF operations and Gecko. Gecko uses headless widgets for these browsers, and a custom WebRender compositor exports their final frames through the same CEF accelerated-paint callback used by the shared helper.

## Architecture

```text
KDE/Qt/Rust application process
  Qt GUI thread
    KXmlGuiWindow, KActionCollection, native menus/configurable toolbar
    embedded QQuickView with QML tabs, tooltips, popups, and BrowserSurface items
  Qt Quick render thread
    import received DMA-BUF as EGLImage / GL texture
      └── compose browser texture with the rest of the Qt scene
  controller and protocol reader threads
    retain frame FDs and return FrameRelease after Qt's GL fence signals

Per-profile Chromium cef-renderer process   Per-profile Firefox cef-renderer process
  helper main/control threads                 main thread: CEF loop -> XRE_main
  project-built patched libcef.so             private libcef.so (Rust adapter)
  one windowless browser per tab               -> libxul.so bridge
  Alloy accelerated paint                      one headless widget per tab
  Chromium renderer / GPU children             WebRender DMA-BUF compositor
             │                                             │
             └──── CEF OnAcceleratedPaint with DMA-BUF FDs ┘
                                      │
                         AF_UNIX/SOCK_SEQPACKET
```

Both helpers have the same `DT_NEEDED: libcef.so` dependency and execute the same CEF calls. Loader isolation selects the implementation. A profile starts at most one helper per engine, and every tab of that engine is a separate browser inside the helper. Moving a tab changes which logical `BrowserSurface` receives its frames; the browser, page, and helper are not recreated. The Chromium helper enables CEF's multi-threaded loop; the Gecko helper runs the CEF loop and XRE on its process main thread. Chromium renderer isolation and Gecko content-process isolation remain in place behind those browser processes. Developer tools also remain engine-native: the Chromium host opens CEF's DevTools browser, while the Firefox adapter opens Mozilla's detached DevTools toolbox against the active Gecko browsing context.

There is no engine child window, XComposite redirect, pixmap capture, CPU readback, or texture upload in the presentation path. Chromium keeps its accelerated native-pixmap frame alive until the shell releases it. Gecko makes a `SharedSurface_DMABUF` the default WebRender framebuffer and applies the same lease rule. The helper passes plane descriptors and an optional acquire-fence FD over the control socket; `BrowserSurface` imports them with `EGL_EXT_image_dma_buf_import`, and Qt retires the old import only after a render-thread GL fence signals. Each tab retains only its newest view-frame lease, including while inactive, so selecting or moving it can rebind the existing texture immediately; an arriving replacement releases the older lease after GPU use completes. Qt still performs its normal scene composition pass, but the shell adds no full-frame copy or frame queue between the engine and that pass. Pointer and keyboard events hit the Qt item and travel through the private protocol to CEF browser-host input methods; the Firefox adapter maps those calls to trusted Gecko widget events. Cursor changes computed from hovered page content travel back through CEF and the protocol so the Qt surface displays the browser cursor for links, text, resizing, dragging, and other page interactions. A page right-click also stays engine-driven: Chromium supplies its CEF menu model, while the Firefox adapter first dispatches Gecko's trusted `contextmenu` event and then supplies its supported page commands through the same CEF handler. The helper serializes visible commands, separators, checked states, and nested submenus; the shell presents them as a native Qt `QMenu` and returns selection or dismissal to the pending CEF callback. HTML Fullscreen API state travels back through the same CEF display callback in both engines; the shell hides its native menu, toolbar, and status bar plus its QML profile/tab chrome, fullscreens the owning `KXmlGuiWindow`, and restores its prior window state on exit. A fullscreen-only KDE Escape shortcut forwards the key through the browser input protocol so the page exits fullscreen before the native window restores.

The shell and each helper communicate over a private inherited `AF_UNIX/SOCK_SEQPACKET` socket. Messages use the internal protobuf schema in `shell-protocol/proto/shell.proto`; DMA-BUF and fence descriptors travel as `SCM_RIGHTS` ancillary data, while stdout and stderr are not protocol channels. Startup verifies the engine identity, CEF API version, packet limit, and capabilities before the shell requests a logical browser surface. Runtime operations use correlated request/response packets, while frame, loading, and lifecycle changes are ordered events. Any failure promoted into shell state is also emitted to stderr as a `deb: failure:` record with the profile, window, tab, engine, and browser IDs available at that boundary. See [the shell protocol design](docs/shell-protocol.md).

Linux Firefox links its allocator glue into the launcher rather than shipping it as a reusable shared object. The staging script therefore links `libmozglue-cef.so` from the pinned build's exact launcher object list and preloads it before the adapter loads `libxul.so`. Gecko startup and child startup use Mozilla's `Bootstrap` interface, including the required null-terminated argument vectors.

## Requirements

- Linux desktop with an X11 display server, EGL, OpenGL, and DMA-BUF import support
- Rust with Edition 2024 support
- Qt 6.10 or newer Base, Widgets, and Declarative development files
- KDE Frameworks 6 KXmlGui, KConfig, KCoreAddons, and KI18n, plus KDE's desktop Qt Quick Controls style
- Enough disk and memory for a full Chromium/CEF build
- Firefox build prerequisites and enough space for a full Firefox object tree
- `pkg-config`, protobuf, a C/C++ toolchain, Python, PyGObject, Pillow, AT-SPI, `xdotool`, Node.js, and the Firefox-provided build toolchains

On Arch Linux, the core host packages can be installed with:

```sh
paru -S --needed at-spi2-core base-devel kconfig kcoreaddons ki18n kxmlgui libx11 libdrm mesa mesa-utils openbox pkgconf protobuf python-gobject python-pillow qqc2-desktop-style qt6-base qt6-declarative xdotool xorg-xdpyinfo xorg-xprop
```

If Firefox's build reports another missing prerequisite, run `./mach bootstrap` in the pinned `firefox` submodule and select the desktop Firefox build environment.

## Build and run

Initialize both pinned engine source trees:

```sh
git submodule update --init cef firefox
```

Build and stage the partition-aware Chromium CEF runtime, then build the patched Firefox runtime, Gecko-backed adapter, shared helper, and Qt shell:

```sh
scripts/build-cef.sh
scripts/build-firefox-cef.sh
```

The first CEF and Firefox builds are both large. The CEF script makes an official X11-only build with Chromium's matching PGO profile, applies the cookie-observer and windowless-extension-tab patches in `cef-patches/`, verifies that Chromium and the Gecko adapter advertise the same generated API hash, and stages the result in `cef-runtime`. Unchanged CEF source, patches, build script, and GN settings skip that engine build; pass `--force` to rebuild it.

Firefox rebuilds reuse `target/firefox-source` and `target/firefox-obj`; when the pinned Firefox commit, mozconfig, patch, overlay, and packaged internal page are unchanged, the script skips `mach` entirely and only rebuilds/restages Rust. Its script refuses a Firefox worktree that is not at the submodule's pinned commit, applies `firefox-patches/0001-firefox-cef-runtime.patch`, overlays the maintained bridge sources, and stages the result under `target/debug/firefox-cef-runtime`. Both build scripts run the browser smoke test by default; pass `--no-smoke` while chaining the two builds and run `scripts/smoke-test.sh --no-build` once at the end.

Run the application:

```sh
cargo run -p deb
```

It opens `deb://new-tab/` in a Chromium tab. The main window is a conventional `KXmlGuiWindow`: its menu actions, shortcuts, native engine picker and address field, and toolbar are backed by `KActionCollection`. Use **Settings → Configure Toolbars…** to add, remove, or reorder actions with KDE's standard editor; the layout and toolbar visibility persist through KDE's normal state/config files. **Configure Keyboard Shortcuts…** uses the standard KDE shortcut editor. **View → Developer Tools** or `Ctrl+Shift+I` opens and focuses the active tab's real engine debugger—Chromium DevTools for Chromium and Firefox Developer Tools for Gecko. Browser content stays in a directly embedded `QQuickView`, not `QQuickWidget`, so the existing render-thread DMA-BUF path remains intact.

The QML tab strip follows Konsole's desktop behavior: drag tabs to reorder them or move them between windows, drag outside every deb window or use the tab menu to detach one, middle-click or use the per-tab close button to close one, and use the new-tab menu to choose Chromium or Firefox. `Ctrl+Tab` / `Ctrl+Shift+Tab` and `Ctrl+PgDown` / `Ctrl+PgUp` cycle tabs, `Ctrl+Shift+T` opens another tab with the active engine, and `Ctrl+W` closes the active tab. The engine picker reloads the current URL in the other engine. Windows belonging to the same profile share its controller and helper pair. Use **Add profile** to create another isolated workspace in the same running application.

## Native extensions

`deb` can load unpacked native extensions at startup while Chromium remains an Alloy windowless browser. The extension code runs inside the selected engine's own implementation: Chromium uses its Manifest V3 service workers, content scripts, storage, declarative network rules, and extension APIs; Firefox uses Gecko's WebExtension manager, background documents, content scripts, storage, request rules, and APIs. The shell does not evaluate extension JavaScript or proxy extension network requests.

Use an engine-specific package when the Chromium and Firefox manifests differ:

```sh
cargo run -p deb -- \
  --load-chromium-extension /absolute/path/to/chromium-extension \
  --load-firefox-extension /absolute/path/to/firefox-extension
```

`--load-extension /absolute/path` loads the same unpacked directory into both engines. Every option is repeatable. Paths are canonicalized and must name a directory containing `manifest.json`; extension options are consumed by deb before Qt processes its own command line. Chromium registrations last for the process and use its persistent profile for extension-owned data. Firefox installs these packages through Gecko's temporary-addon API on each start while keeping their native profile storage.

The windowless integration makes deb's live pages extension-visible tabs. Chromium's CEF patch enumerates Alloy `WebContents` and applies index, active/highlighted, pinned, group, split-view, audible, muted, lifecycle, title, URL, and loading-status filters for `tabs.query`; Gecko receives a normal `browsers` message-manager group plus a thin `gBrowser` view backed by the adapter's browser map, so Firefox's own tabs implementation sees the same pages. Extension background execution, injection, runtime messaging, storage, declarative request blocking, and filtered tab enumeration are covered in both engines by the native Xorg E2E test.

## Profiles and XDG storage

`deb` owns stable profile IDs and uses the maintained Rust `xdg` crate to resolve the XDG Base Directory layout, including the specification's fallback and relative-path rules. The profile registry is stored at `$XDG_CONFIG_HOME/deb/profiles.json`, persistent engine data is below `$XDG_DATA_HOME/deb/profiles/<profile-id>/`, and disposable engine caches are below `$XDG_CACHE_HOME/deb/profiles/<profile-id>/`.

Each deb profile maps to two independent native stores:

```text
$XDG_DATA_HOME/deb/profiles/<profile-id>/
  cookies.sqlite3
  chromium/
  firefox/

$XDG_CACHE_HOME/deb/profiles/<profile-id>/
  chromium/
  firefox/
```

Chromium receives an explicit persistent CEF `cache_path`; its disk cache is redirected to the XDG cache directory. Gecko runs with the Firefox data directory as its native `--profile` and redirects `cache2` through `browser.cache.disk.parent_directory`. Each engine keeps its native cookie database, while `deb` reconciles both through the profile's WAL-protected `cookies.sqlite3` canonical store and mirrors live changes in both directions. Cookie identity includes domain, path, and the complete serializable partition key. Opaque/nonce partition keys remain engine-local because the other engine cannot recreate them. Local storage, IndexedDB, service workers, permissions, caches, and other state remain engine-native and are never shared as raw files.

Each profile workspace lazily owns one Chromium helper and one Firefox helper. All same-engine tabs and top-level windows in that profile share the corresponding helper, while each tab retains its own browser, native engine isolation, URL, title, loading state, crash state, and current window assignment. Opening another profile starts another isolated helper pair, and previously opened profiles remain alive while the application is running. This gives complete cookie/storage separation and avoids attempting to switch Gecko's process-global Firefox profile in place.

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
- navigation, reload, focus, resize, pointer and keyboard input, page cursor changes, page context menus, HTML fullscreen entry/exit, engine-native developer tools, and close
- windowless accelerated paint backed by leased DMA-BUF frames
- multiple browsers per profile helper, visibility changes, titles, committed URLs, and renderer/content crash events
- registration of the build-owned `deb://new-tab/` internal page
- global cookie snapshots, exact set/delete operations, live change observation, timestamps, and serializable partition keys
- native Gecko WebExtension startup and extension-visible windowless tab integration

The Qt shell has no Firefox-specific navigation API. Extending Gecko support means implementing another coherent CEF behavior slice in `firefox-cef` and its Firefox bridge.

## Current limitations

- Each helper supports multiple browsers for one profile. Multiple profiles use isolated helper pairs; multiple CEF request contexts inside one helper are not implemented.
- Engine-created popup browsers, downloads, packaged extension installation/update UI, browser-action toolbar surfaces and action popups, embedded web-content accessibility integration, full IME preedit/composition handling, arbitrary application-defined custom schemes, per-request-context cookie managers, and request interception are outside the current CEF slice. Native unpacked extension execution is supported as described above; APIs that require absent browser chrome still need a shell surface. The Qt shell itself exposes production accessibility IDs, roles, names, states, and geometry through AT-SPI. Normal key events and committed input-method text are supported. Chromium context menus preserve CEF's target-sensitive model; the Firefox adapter currently exposes page-level Back, Forward, Reload, and View Page Source rather than Gecko's full link, media, selection, and editable-control variants.
- Presentation is X11/EGL only. Wayland-native and Xwayland presentation are not supported.
- The Gecko helper keeps WebRender in the browser process so the CEF callback and frame leases remain process-local; Gecko content tabs still use normal content processes, but a separate Firefox GPU process is not currently used.

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
shellcheck scripts/*.sh
```

For the normal inner loop after changing Rust, QML, or the adapter, run:

```sh
scripts/smoke-test.sh
```

This requires native Xorg with hardware-accelerated OpenGL; it rejects XWayland and software renderers because those paths cannot prove the supported DMA-BUF presentation path. When the display has no EWMH window manager, the driver starts a temporary Openbox instance and stops it on exit. It performs an incremental workspace build, atomically restages the Rust helper and Gecko-backed `libcef.so`, and launches the unmodified `deb` application with isolated temporary XDG config, data, cache, and state directories plus real unpacked Chromium and Firefox extension fixtures through the public launch options. Before the browser flow, a hermetic native self-test verifies the KXMLGUI action order, developer-tools action, standard toolbar editor construction, and visibility persistence; the external driver then opens the real Configure Toolbars dialog and verifies the native toolbar widgets by their Qt accessibility IDs. It sends real mouse and keyboard input through XTEST, waits for both engines' initial loads, navigates both through the address field, and physically clicks fixed targets inside each browser surface. Each engine's native extension must start its background context, inject a content script, exchange runtime messages, round-trip native extension storage, block a request before it reaches the local server, and find the active windowless page through a filtered `tabs.query`. The page accepts only trusted DOM events and must change both its title and large screen markers before the test continues. For each engine, the driver also sends a real right-click, requires the page's trusted `contextmenu` listener to run, detects the new native popup at the pointer, executes Reload through that menu, waits for the popup to close, and proves that the page renders again. The separate tab context menu remains covered by executing its Detach action. A trusted click also calls `requestFullscreen()` on a player element; the driver requires the owning managed X11 window to acquire `_NET_WM_STATE_FULLSCREEN`, cover the display, hide both shell chrome layers, and show content that exists only under `:fullscreen`. It then sends Escape and requires the EWMH state, window geometry, shell chrome, page marker, and page title to restore. The driver opens each active tab's native developer-tools window through the KDE shortcut, requires its title to identify the inspected test page, triggers the shortcut again to prove that the existing debugger is focused instead of duplicated, and closes it before continuing. The localhost page also sets a cookie in Chromium, and the Firefox page must visibly confirm that it arrived through deb's synchronization path. The driver switches tabs through their real buttons and through both `Ctrl+Tab` directions, requires the active URL, engine status, and engine-specific final-screen marker after every switch, reorders tabs with a pointer drag, drags a live Firefox tab into a second production window, closes the other tab with a middle click, and detaches Firefox into a third placeholder-free window through its tab menu. It navigates and reloads that second window through the detached-window QML toolbar, then opens a fourth production window through the same QML toolbar so both the new KDE main-window path and the retained QML detached-window path stay covered. It also creates two live tabs in each engine, cycles their real tab buttons repeatedly, and sends a rapid `Ctrl+Tab` burst to reproduce helper/channel failures under the original interaction. Every wait watches the application log and fails immediately on a `deb: failure:` record. The test additionally proves that real Qt tooltips compose over each engine. Both engines' navigated, retained, moved, detached, and stress-switched DMA-BUF frames must contain the page marker and nontrivial pixels; both engine-native profile/cache trees and the canonical SQLite cookie store must also exist. Failures preserve a screenshot, accessibility-tree dump, driver log, and application log in the reported temporary directory.

If `/dev/uinput` is writable, the smoke test also creates a temporary direct-touch device and sends a real two-contact pinch through Qt into each engine. The page must receive trusted `touchstart`, `touchmove`, and `touchend` events with a changing contact span. Use `scripts/smoke-test.sh --require-touch` to make that coverage mandatory; the virtual device is destroyed when the driver exits.

`scripts/build-firefox-cef.sh` runs the same smoke test automatically after either a full Gecko build or a cached Rust-only rebuild. `scripts/smoke-test.sh --no-build` is available when the binaries and staged runtime are already current.

For interactive debugging, `DEB_URL=<url> cargo run -p deb` changes the initial URL without enabling automated exit behavior.
