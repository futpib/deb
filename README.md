# deb

`deb` is a Linux/X11 desktop browser shell whose tabs can run in Chromium or Gecko across multiple Qt Quick windows. The Rust/Qt shell uses Qt Bridges, and each visible window's active tab is an accelerated Qt Quick scene item controlled through the same CEF-facing `cef-renderer` executable for either engine.

| Tab engine | `cef-renderer` resolves `libcef.so` to | Engine |
| --- | --- | --- |
| Chromium | the project-built, patched CEF runtime | Chromium from CEF's pinned Chromium checkout |
| Firefox | the private `firefox-cef` adapter | Gecko from the pinned Firefox source tree |

The Firefox side is not automation of a separately installed Firefox. Its private helper loads the project-built CEF adapter, the adapter loads `libxul.so`, and a small component compiled into that Firefox runtime provides the bridge between CEF operations and Gecko. Each loaded Gecko browser window reports its stable top-level XID through that component and remains parented to one app-owned tab container; activation reasserts that same parent to keep GTK/X11 presentation healthy after sibling crashes.

## Architecture

```text
Qt/Rust application process
  Qt GUI thread
    one host QWidget per Qt browser window (X11 parents)
      └── selected app-owned tab container XID (moves between hosts)
          └── engine surface XID (keeps the same direct parent)
                XComposite redirect + XDamage tracking
                  └── GLX texture-from-pixmap import
                        └── BrowserSurface Qt Quick scene item
                              └── Qt controls, tooltips, and popups

Per-profile Chromium cef-renderer process   Per-profile Firefox cef-renderer process
  helper main/control threads                 main thread: CEF loop -> XRE_main
  project-built patched libcef.so             private libcef.so (Rust adapter)
  one CEF browser per Chromium tab            -> libxul.so bridge
  native browser child XIDs ─────────────┐     FirefoxCEF XUL window XIDs ─────┐
  Chromium child processes              │     one remote browser per tab       │
    renderer / GPU / network            │     Gecko child processes           │
                                        └──── app-owned containers ────────────┘
                                                web content / socket / RDD
```

Both helpers have the same `DT_NEEDED: libcef.so` dependency and execute the same CEF calls. Loader isolation selects the implementation. A profile starts at most one helper per engine, and every tab of that engine is a separate browser inside the helper. Each tab owns a stable X11 container; moving it reparents that container to another Qt host while the engine surface remains attached to the same parent for its whole lifetime. The browser, page, and helper are not recreated. The Chromium helper enables CEF's multi-threaded loop; the Gecko helper runs the CEF loop and XRE on its process main thread. Standard Chromium renderer and Gecko content-process isolation remains in place behind those browser processes.

The native engine surface is manually redirected with XComposite instead of being left above the Qt window. `BrowserSurface` tracks XDamage, imports the redirected X pixmap into the OpenGL Qt Quick scene through `GLX_EXT_texture_from_pixmap`, and lets Qt compose its controls over the page. The application performs no CPU readback or texture upload in that presentation path; the GLX implementation is still allowed to copy internally when binding a pixmap. Qt adds its normal scene composition pass, so the project does not claim an absolute zero-latency guarantee. Pointer and keyboard events hit the Qt item and travel through the private protocol to CEF browser-host input methods; the Firefox CEF adapter maps those calls to trusted Gecko widget events. Qt therefore retains normal widget focus and stacking instead of transferring input ownership to the redirected child window.

The shell and each helper communicate over a private inherited `AF_UNIX/SOCK_SEQPACKET` socket. Messages use the internal protobuf schema in `shell-protocol/proto/shell.proto`; stdout and stderr are not protocol channels. Startup verifies the engine identity, CEF API version, packet limit, and capabilities before the shell requests an X11 browser child. Runtime operations use correlated request/response packets, while surface, loading, and lifecycle changes are ordered events. See [the shell protocol design](docs/shell-protocol.md).

Linux Firefox links its allocator glue into the launcher rather than shipping it as a reusable shared object. The staging script therefore links `libmozglue-cef.so` from the pinned build's exact launcher object list and preloads it before the adapter loads `libxul.so`. Gecko startup and child startup use Mozilla's `Bootstrap` interface, including the required null-terminated argument vectors.

## Requirements

- Linux desktop with an X11 display server, XComposite, XDamage, XFixes, and GLX texture-from-pixmap support
- Rust with Edition 2024 support
- Qt 6 Base and Qt 6 Declarative development files
- Enough disk and memory for a full Chromium/CEF build
- Firefox build prerequisites and enough space for a full Firefox object tree
- `pkg-config`, protobuf, a C/C++ toolchain, Python, Node.js, and the Firefox-provided build toolchains

On Arch Linux, the core host packages can be installed with:

```sh
paru -S --needed base-devel libx11 libxcomposite libxdamage libxfixes mesa pkgconf protobuf qt6-base qt6-declarative xorg-server-xvfb
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

The first CEF and Firefox builds are both large. The CEF script makes an official X11-only build with Chromium's matching PGO profile, applies `cef-patches/0001-partitioned-cookie-observer.patch`, verifies that Chromium and the Gecko adapter advertise the same generated API hash, and stages the result in `cef-runtime`. Unchanged CEF source, patch, build script, and GN settings skip that engine build; pass `--force` to rebuild it.

Firefox rebuilds reuse `target/firefox-source` and `target/firefox-obj`; when the pinned Firefox commit, mozconfig, patch, overlay, and packaged internal page are unchanged, the script skips `mach` entirely and only rebuilds/restages Rust. Its script refuses a Firefox worktree that is not at the submodule's pinned commit, applies `firefox-patches/0001-firefox-cef-runtime.patch`, overlays the maintained bridge sources, and stages the result under `target/debug/firefox-cef-runtime`. Both build scripts run the browser smoke test by default; pass `--no-smoke` while chaining the two builds and run `scripts/smoke-test.sh --no-build` once at the end.

Run the application:

```sh
cargo run -p deb
```

It opens `deb://new-tab/` in a Chromium tab. Create Chromium or Firefox tabs with the two add-tab buttons, use the engine picker to reload the current URL in the other engine, open another top-level window with **New window**, and move a live tab between windows from **Move tab**. Windows belonging to the same profile share its controller and helper pair. Use **Add profile** to create another isolated workspace in the same running application.

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
- navigation, reload, focus, resize, pointer and keyboard input, native-window lookup, and close
- multiple browsers per profile helper, app-container embedding, visibility changes, titles, committed URLs, and renderer/content crash events
- registration of the build-owned `deb://new-tab/` internal page
- global cookie snapshots, exact set/delete operations, live change observation, timestamps, and serializable partition keys

The Qt shell has no Firefox-specific navigation API. Extending Gecko support means implementing another coherent CEF behavior slice in `firefox-cef` and its Firefox bridge.

## Current limitations

- Each helper supports multiple browsers for one profile. Multiple profiles use isolated helper pairs; multiple CEF request contexts inside one helper are not implemented.
- Engine-created popup browsers, downloads, extensions, accessibility integration, full IME preedit/composition handling, CEF windowless rendering, devtools, arbitrary application-defined custom schemes, per-request-context cookie managers, and request interception are outside the current CEF slice. Normal key events and committed input-method text are supported.
- Presentation is X11/GLX only. Wayland-native and Xwayland presentation are not supported.

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

This performs an incremental workspace build, atomically restages the Rust helper and Gecko-backed `libcef.so`, and launches `deb` with isolated temporary XDG directories under `xvfb-run`. The application creates two tabs per engine in one profile, verifies cookie synchronization, navigates and samples both native engines to reject blank rendering, proves that a Qt scene marker remains visible above the composited browser texture, and requires one page marker per engine to remain at the texture's top edge so vertical inversion cannot pass as successful rendering. It deliberately crashes one Chromium renderer and one Gecko content process and requires recovery without affecting either same-engine sibling or replacing a helper. It then opens a second Qt window, moves live Chromium and Gecko tabs into it, verifies that each tab container moved while its engine surface parent and browser/helper identities stayed intact, samples both moved surfaces, switches the recovered Chromium tab to Gecko, and exits with a real pass/fail status. The wrapper also checks that both engine-native profile/cache trees and the canonical SQLite cookie store were created.

`scripts/build-firefox-cef.sh` runs the same smoke test automatically after either a full Gecko build or a cached Rust-only rebuild. `scripts/smoke-test.sh --no-build` is available when the binaries and staged runtime are already current.

For interactive debugging, `DEB_SMOKE_NAVIGATE_URL=<url> cargo run -p deb` still sends a one-time navigation after both helpers start without enabling automated exit behavior.
