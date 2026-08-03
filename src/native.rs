use qtbridge::{QmlMethodInvoker, invoke_method};
use std::{
    collections::BTreeSet,
    error::Error,
    io::Write,
    path::PathBuf,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread::sleep,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        Atom, AtomEnum, ConfigureWindowAux, ConnectionExt, MapState, StackMode, Window,
    },
    rust_connection::RustConnection,
};

type NativeResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl NativeRect {
    pub fn new(x: i32, y: i32, width: i32, height: i32) -> Option<Self> {
        (width > 1 && height > 1).then_some(Self {
            x,
            y,
            width: width as u32,
            height: height as u32,
        })
    }

    fn argument(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.width, self.height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub chromium: NativeRect,
    pub firefox: NativeRect,
}

pub enum ControllerCommand {
    Layout(Layout),
    Navigate(String),
    Stop,
}

struct Atoms {
    net_client_list: Atom,
    net_wm_pid: Atom,
    wm_class: Atom,
}

impl Atoms {
    fn new(connection: &RustConnection) -> NativeResult<Self> {
        Ok(Self {
            net_client_list: intern(connection, b"_NET_CLIENT_LIST")?,
            net_wm_pid: intern(connection, b"_NET_WM_PID")?,
            wm_class: intern(connection, b"WM_CLASS")?,
        })
    }
}

struct CefInstance {
    child: Child,
    input: ChildStdin,
    window: Window,
}

impl CefInstance {
    fn send(&mut self, command: &str) -> NativeResult<()> {
        self.input.write_all(command.as_bytes())?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        Ok(())
    }

    fn navigate(&mut self, url: &str) -> NativeResult<()> {
        self.send(&format!("navigate\t{url}"))
    }

    fn resize(&mut self, connection: &RustConnection, bounds: NativeRect) -> NativeResult<()> {
        configure_native_window(connection, self.window, bounds)?;
        self.send(&format!("bounds\t{}", bounds.argument()))
    }

    fn stop(mut self) {
        let _ = self.send("quit");
        stop_child(&mut self.child);
    }
}

struct FirefoxInstance {
    child: Child,
    window: Window,
    profile: PathBuf,
}

impl FirefoxInstance {
    fn resize(&self, connection: &RustConnection, bounds: NativeRect) -> NativeResult<()> {
        configure_native_window(connection, self.window, bounds)
    }

    fn ensure_mapped(&self, connection: &RustConnection) -> NativeResult<()> {
        if connection
            .get_window_attributes(self.window)?
            .reply()?
            .map_state
            == MapState::UNMAPPED
        {
            connection.map_window(self.window)?;
            connection.flush()?;
        }
        Ok(())
    }

    fn stop(mut self) {
        stop_child(&mut self.child);
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

pub fn spawn_controller(
    url: String,
    layout: Layout,
    invoker: QmlMethodInvoker,
) -> Sender<ControllerCommand> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        if let Err(error) = run_controller(url, layout, &invoker, receiver) {
            update_statuses(
                &invoker,
                format!("Native controller failed: {error}"),
                format!("Native controller failed: {error}"),
            );
        }
    });
    sender
}

fn run_controller(
    initial_url: String,
    initial_layout: Layout,
    invoker: &QmlMethodInvoker,
    receiver: mpsc::Receiver<ControllerCommand>,
) -> NativeResult<()> {
    let (connection, screen_number) = x11rb::connect(None)?;
    let root = connection.setup().roots[screen_number].root;
    let atoms = Atoms::new(&connection)?;
    let host = wait_for_pid_window(
        &connection,
        &atoms,
        root,
        std::process::id(),
        Duration::from_secs(10),
    )?;
    let mut layout = layout_relative_to_host(&connection, root, host, initial_layout)?;

    let mut chromium_status = "Starting native CEF child…".to_owned();
    let mut firefox_status = "Starting Firefox X11 child…".to_owned();
    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());

    let mut cef = match spawn_cef(&connection, host, layout.chromium, &initial_url) {
        Ok(instance) => {
            chromium_status = "Live · CEF 150 / Chromium 150".to_owned();
            Some(instance)
        }
        Err(error) => {
            chromium_status = format!("CEF child failed: {error}");
            None
        }
    };
    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());

    let mut firefox_generation = 0;
    let mut firefox = match spawn_firefox(
        &connection,
        &atoms,
        root,
        host,
        layout.firefox,
        &initial_url,
        firefox_generation,
    ) {
        Ok(instance) => {
            firefox_status = "Live · Firefox 153 / Gecko · X11 child".to_owned();
            Some(instance)
        }
        Err(error) => {
            firefox_status = format!("Firefox child failed: {error}");
            None
        }
    };
    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());

    loop {
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(ControllerCommand::Layout(next_layout)) => {
                layout = layout_relative_to_host(&connection, root, host, next_layout)?;
                if let Some(cef) = &mut cef
                    && let Err(error) = cef.resize(&connection, layout.chromium)
                {
                    chromium_status = format!("CEF resize failed: {error}");
                }
                if let Some(firefox) = &firefox
                    && let Err(error) = firefox.resize(&connection, layout.firefox)
                {
                    firefox_status = format!("Firefox resize failed: {error}");
                }
                update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
            }
            Ok(ControllerCommand::Navigate(url)) => {
                if let Some(cef) = &mut cef {
                    match cef.navigate(&url) {
                        Ok(()) => chromium_status = "Live · CEF 150 / Chromium 150".to_owned(),
                        Err(error) => chromium_status = format!("CEF navigation failed: {error}"),
                    }
                }

                if let Some(instance) = firefox.take() {
                    instance.stop();
                }
                firefox_generation += 1;
                firefox_status = "Restarting Firefox / Gecko…".to_owned();
                update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
                firefox = match spawn_firefox(
                    &connection,
                    &atoms,
                    root,
                    host,
                    layout.firefox,
                    &url,
                    firefox_generation,
                ) {
                    Ok(instance) => {
                        firefox_status = "Live · Firefox 153 / Gecko · X11 child".to_owned();
                        Some(instance)
                    }
                    Err(error) => {
                        firefox_status = format!("Firefox child failed: {error}");
                        None
                    }
                };
                update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
            }
            Ok(ControllerCommand::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(firefox) = &firefox
                    && let Err(error) = firefox.ensure_mapped(&connection)
                {
                    firefox_status = format!("Firefox remap failed: {error}");
                    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
                }
            }
        }
    }

    if let Some(instance) = cef {
        instance.stop();
    }
    if let Some(instance) = firefox {
        instance.stop();
    }
    Ok(())
}

fn spawn_cef(
    connection: &RustConnection,
    host: Window,
    bounds: NativeRect,
    url: &str,
) -> NativeResult<CefInstance> {
    let previous_children = direct_children(connection, host)?;
    let executable = std::env::current_exe()?;
    let helper = executable
        .parent()
        .ok_or("application executable has no parent directory")?
        .join("cef-renderer");
    let mut child = Command::new(helper)
        .arg("--parent")
        .arg(host.to_string())
        .arg("--bounds")
        .arg(bounds.argument())
        .arg("--url")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()?;
    let input = child
        .stdin
        .take()
        .ok_or("CEF control pipe is unavailable")?;
    let window = wait_for_new_child(
        connection,
        host,
        &previous_children,
        &mut child,
        Duration::from_secs(10),
    )?;
    configure_native_window(connection, window, bounds)?;
    Ok(CefInstance {
        child,
        input,
        window,
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_firefox(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
    host: Window,
    bounds: NativeRect,
    url: &str,
    generation: u64,
) -> NativeResult<FirefoxInstance> {
    let previous_windows = candidate_windows(connection, atoms, root)?;
    let profile = std::env::temp_dir().join(format!(
        "dual-engine-browser-firefox-{}-{generation}",
        std::process::id()
    ));
    std::fs::create_dir_all(&profile)?;
    std::fs::write(
        profile.join("user.js"),
        concat!(
            "user_pref(\"browser.shell.checkDefaultBrowser\", false);\n",
            "user_pref(\"browser.aboutwelcome.enabled\", false);\n",
            "user_pref(\"browser.startup.firstrunSkipsHomepage\", true);\n",
            "user_pref(\"datareporting.policy.dataSubmissionEnabled\", false);\n",
        ),
    )?;

    let mut child = Command::new("firefox")
        .env("MOZ_ENABLE_WAYLAND", "0")
        .arg("--new-instance")
        .arg("--profile")
        .arg(&profile)
        .arg("--new-window")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let window = wait_for_firefox_window(
        connection,
        atoms,
        root,
        &previous_windows,
        &mut child,
        Duration::from_secs(20),
    )?;

    sleep(Duration::from_secs(1));
    connection.unmap_window(window)?;
    connection.reparent_window(window, host, bounds.x as i16, bounds.y as i16)?;
    configure_native_window(connection, window, bounds)?;
    connection.map_window(window)?;
    connection.flush()?;
    Ok(FirefoxInstance {
        child,
        window,
        profile,
    })
}

fn configure_native_window(
    connection: &RustConnection,
    window: Window,
    bounds: NativeRect,
) -> NativeResult<()> {
    connection.configure_window(
        window,
        &ConfigureWindowAux::new()
            .x(bounds.x)
            .y(bounds.y)
            .width(bounds.width)
            .height(bounds.height)
            .border_width(0)
            .stack_mode(StackMode::ABOVE),
    )?;
    connection.flush()?;
    Ok(())
}

fn layout_relative_to_host(
    connection: &RustConnection,
    root: Window,
    host: Window,
    layout: Layout,
) -> NativeResult<Layout> {
    Ok(Layout {
        chromium: rect_relative_to_host(connection, root, host, layout.chromium)?,
        firefox: rect_relative_to_host(connection, root, host, layout.firefox)?,
    })
}

fn rect_relative_to_host(
    connection: &RustConnection,
    root: Window,
    host: Window,
    rect: NativeRect,
) -> NativeResult<NativeRect> {
    let translated = connection
        .translate_coordinates(root, host, rect.x as i16, rect.y as i16)?
        .reply()?;
    Ok(NativeRect {
        x: i32::from(translated.dst_x),
        y: i32::from(translated.dst_y),
        ..rect
    })
}

fn wait_for_pid_window(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
    pid: u32,
    timeout: Duration,
) -> NativeResult<Window> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let mut matches = candidate_windows(connection, atoms, root)?
            .into_iter()
            .filter(|window| window_pid(connection, atoms, *window).ok() == Some(pid))
            .filter_map(|window| {
                window_area(connection, window)
                    .ok()
                    .map(|area| (area, window))
            })
            .collect::<Vec<_>>();
        matches.sort_unstable();
        if let Some((_, window)) = matches.pop() {
            return Ok(window);
        }
        sleep(Duration::from_millis(50));
    }
    Err(format!("could not locate the Qt X11 window for PID {pid}").into())
}

fn wait_for_new_child(
    connection: &RustConnection,
    parent: Window,
    previous: &BTreeSet<Window>,
    child: &mut Child,
    timeout: Duration,
) -> NativeResult<Window> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!("CEF exited before creating its child window: {status}").into());
        }
        let mut candidates = direct_children(connection, parent)?
            .difference(previous)
            .copied()
            .filter_map(|window| {
                window_area(connection, window)
                    .ok()
                    .map(|area| (area, window))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        if let Some((area, window)) = candidates.pop()
            && area > 100
        {
            return Ok(window);
        }
        sleep(Duration::from_millis(50));
    }
    Err("CEF did not create a native child window within 10 seconds".into())
}

fn wait_for_firefox_window(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
    previous: &BTreeSet<Window>,
    child: &mut Child,
    timeout: Duration,
) -> NativeResult<Window> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!("Firefox exited before creating its window: {status}").into());
        }
        let mut candidates = candidate_windows(connection, atoms, root)?
            .difference(previous)
            .copied()
            .filter(|window| {
                window_pid(connection, atoms, *window).ok() == Some(child.id())
                    && window_class(connection, atoms, *window)
                        .map(|class| class.to_ascii_lowercase().contains("firefox"))
                        .unwrap_or(false)
            })
            .filter_map(|window| {
                window_area(connection, window)
                    .ok()
                    .map(|area| (area, window))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable();
        if let Some((area, window)) = candidates.pop()
            && area > 10_000
        {
            return Ok(window);
        }
        sleep(Duration::from_millis(50));
    }
    Err("Firefox did not create an X11 browser window within 20 seconds".into())
}

fn candidate_windows(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
) -> NativeResult<BTreeSet<Window>> {
    let mut windows = direct_children(connection, root)?;
    if let Ok(reply) = connection
        .get_property(
            false,
            root,
            atoms.net_client_list,
            AtomEnum::WINDOW,
            0,
            u32::MAX,
        )?
        .reply()
        && let Some(values) = reply.value32()
    {
        windows.extend(values);
    }
    Ok(windows)
}

fn direct_children(connection: &RustConnection, parent: Window) -> NativeResult<BTreeSet<Window>> {
    Ok(connection
        .query_tree(parent)?
        .reply()?
        .children
        .into_iter()
        .collect())
}

fn window_pid(connection: &RustConnection, atoms: &Atoms, window: Window) -> NativeResult<u32> {
    connection
        .get_property(false, window, atoms.net_wm_pid, AtomEnum::CARDINAL, 0, 1)?
        .reply()?
        .value32()
        .and_then(|mut values| values.next())
        .ok_or_else(|| "window has no _NET_WM_PID".into())
}

fn window_class(
    connection: &RustConnection,
    atoms: &Atoms,
    window: Window,
) -> NativeResult<String> {
    let reply = connection
        .get_property(false, window, atoms.wm_class, AtomEnum::STRING, 0, 1024)?
        .reply()?;
    Ok(String::from_utf8_lossy(&reply.value).into_owned())
}

fn window_area(connection: &RustConnection, window: Window) -> NativeResult<u64> {
    let geometry = connection.get_geometry(window)?.reply()?;
    Ok(u64::from(geometry.width) * u64::from(geometry.height))
}

fn intern(connection: &RustConnection, name: &[u8]) -> NativeResult<Atom> {
    Ok(connection.intern_atom(false, name)?.reply()?.atom)
}

fn update_statuses(invoker: &QmlMethodInvoker, chromium: String, firefox: String) {
    invoke_method!(invoker, "update_statuses", chromium, firefox);
}

fn stop_child(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => sleep(Duration::from_millis(50)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::NativeRect;

    #[test]
    fn rejects_zero_sized_surfaces() {
        assert!(NativeRect::new(0, 0, 0, 100).is_none());
        assert!(NativeRect::new(0, 0, 100, 1).is_none());
    }

    #[test]
    fn formats_cef_bounds() {
        let bounds = NativeRect::new(10, 20, 800, 600).unwrap();
        assert_eq!(bounds.argument(), "10,20,800,600");
    }
}
