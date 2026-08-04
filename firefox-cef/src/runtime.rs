use cef_dll_sys::{_cef_browser_host_t, _cef_browser_t, _cef_client_t, _cef_frame_t};
use std::{
    collections::BTreeSet,
    error::Error,
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConfigureWindowAux, ConnectionExt,
        EventMask, InputFocus, PropMode, StackMode, Window,
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use crate::refcount::{add_ref_raw, release_raw};

type RuntimeResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

static NEXT_BROWSER_ID: AtomicU64 = AtomicU64::new(1);
static STATES: OnceLock<Mutex<Vec<Arc<BrowserState>>>> = OnceLock::new();

struct Atoms {
    net_client_list: Atom,
    net_frame_extents: Atom,
    net_wm_pid: Atom,
    net_wm_state: Atom,
    net_wm_state_above: Atom,
    wm_class: Atom,
}

impl Atoms {
    fn new(connection: &RustConnection) -> RuntimeResult<Self> {
        Ok(Self {
            net_client_list: intern(connection, b"_NET_CLIENT_LIST")?,
            net_frame_extents: intern(connection, b"_NET_FRAME_EXTENTS")?,
            net_wm_pid: intern(connection, b"_NET_WM_PID")?,
            net_wm_state: intern(connection, b"_NET_WM_STATE")?,
            net_wm_state_above: intern(connection, b"_NET_WM_STATE_ABOVE")?,
            wm_class: intern(connection, b"WM_CLASS")?,
        })
    }
}

pub struct BrowserState {
    pub id: i32,
    pub parent: Window,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub browser: AtomicPtr<_cef_browser_t>,
    pub host: AtomicPtr<_cef_browser_host_t>,
    pub frame: AtomicPtr<_cef_frame_t>,
    pub client: AtomicPtr<_cef_client_t>,
    window: AtomicU32,
    process: Mutex<Option<Child>>,
    profile: PathBuf,
    firefox_binary: PathBuf,
    current_url: Mutex<String>,
    closed: AtomicBool,
}

impl BrowserState {
    pub fn launch(parent: Window, width: u32, height: u32, url: &str) -> RuntimeResult<Arc<Self>> {
        let id = NEXT_BROWSER_ID.fetch_add(1, Ordering::Relaxed);
        let profile = std::env::temp_dir().join(format!("firefox-cef-{}-{id}", std::process::id()));
        fs::create_dir_all(&profile)?;
        fs::write(
            profile.join("user.js"),
            concat!(
                "user_pref(\"browser.shell.checkDefaultBrowser\", false);\n",
                "user_pref(\"browser.aboutwelcome.enabled\", false);\n",
                "user_pref(\"browser.startup.firstrunSkipsHomepage\", true);\n",
                "user_pref(\"datareporting.policy.dataSubmissionEnabled\", false);\n",
            ),
        )?;
        let firefox_binary = std::env::var_os("FIREFOX_CEF_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("firefox"));
        let (connection, screen_number) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen_number].root;
        let atoms = Atoms::new(&connection)?;
        let previous = candidate_windows(&connection, &atoms, root)?;
        let mut process = Command::new(&firefox_binary)
            .arg("--new-instance")
            .arg("--profile")
            .arg(&profile)
            .arg("--new-window")
            .arg(url)
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let window = wait_for_window(
            &connection,
            &atoms,
            root,
            &previous,
            &mut process,
            Duration::from_secs(20),
        )?;
        let state = Arc::new(Self {
            id: id as i32,
            parent,
            width: AtomicU32::new(width),
            height: AtomicU32::new(height),
            browser: AtomicPtr::new(ptr::null_mut()),
            host: AtomicPtr::new(ptr::null_mut()),
            frame: AtomicPtr::new(ptr::null_mut()),
            client: AtomicPtr::new(ptr::null_mut()),
            window: AtomicU32::new(window),
            process: Mutex::new(Some(process)),
            profile,
            firefox_binary,
            current_url: Mutex::new(url.to_owned()),
            closed: AtomicBool::new(false),
        });
        state.sync_window(false)?;
        states()
            .lock()
            .expect("Firefox CEF state lock poisoned")
            .push(state.clone());
        Ok(state)
    }

    pub fn window(&self) -> Window {
        self.window.load(Ordering::Acquire)
    }

    pub fn is_valid(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    pub fn set_client(&self, client: *mut _cef_client_t) {
        if !client.is_null() {
            unsafe { add_ref_raw(client) };
        }
        self.client.store(client, Ordering::Release);
    }

    pub fn navigate(&self, url: &str) -> RuntimeResult<()> {
        if !self.is_valid() {
            return Err("browser is closed".into());
        }
        self.notify_loading(true);
        let status = Command::new(&self.firefox_binary)
            .arg("--profile")
            .arg(&self.profile)
            .arg("--new-tab")
            .arg(url)
            .env_remove("LD_LIBRARY_PATH")
            .env_remove("LD_PRELOAD")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(format!("Firefox navigation command exited with {status}").into());
        }
        *self.current_url.lock().expect("Firefox URL lock poisoned") = url.to_owned();
        self.notify_loading(false);
        Ok(())
    }

    pub fn reload(&self) -> RuntimeResult<()> {
        let url = self
            .current_url
            .lock()
            .expect("Firefox URL lock poisoned")
            .clone();
        self.navigate(&url)
    }

    pub fn focus(&self) -> RuntimeResult<()> {
        self.sync_from_parent(true)
    }

    pub fn sync_from_parent(&self, focus: bool) -> RuntimeResult<()> {
        let (connection, _) = x11rb::connect(None)?;
        let geometry = connection.get_geometry(self.parent)?.reply()?;
        self.width
            .store(u32::from(geometry.width).max(2), Ordering::Release);
        self.height
            .store(u32::from(geometry.height).max(2), Ordering::Release);
        self.sync_window(focus)
    }

    pub fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let browser = self.browser.load(Ordering::Acquire);
        self.notify_before_close(browser);
        if let Some(mut process) = self
            .process
            .lock()
            .expect("Firefox process lock poisoned")
            .take()
        {
            let _ = process.kill();
            let _ = process.wait();
        }
        let client = self.client.swap(ptr::null_mut(), Ordering::AcqRel);
        unsafe { release_raw(client) };
        let _ = fs::remove_dir_all(&self.profile);
    }

    fn release_objects(&self) {
        let host = self.host.swap(ptr::null_mut(), Ordering::AcqRel);
        let frame = self.frame.swap(ptr::null_mut(), Ordering::AcqRel);
        unsafe {
            release_raw(host);
            release_raw(frame);
        }
    }

    pub fn notify_after_created(&self) {
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() {
            return;
        }
        unsafe {
            let Some(get_handler) = (*client).get_life_span_handler else {
                return;
            };
            let handler = get_handler(client);
            if handler.is_null() {
                return;
            }
            if let Some(callback) = (*handler).on_after_created {
                add_ref_raw(browser);
                callback(handler, browser);
            }
            release_raw(handler);
        }
    }

    pub fn notify_loading(&self, loading: bool) {
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() {
            return;
        }
        unsafe {
            let Some(get_handler) = (*client).get_load_handler else {
                return;
            };
            let handler = get_handler(client);
            if handler.is_null() {
                return;
            }
            if let Some(callback) = (*handler).on_loading_state_change {
                add_ref_raw(browser);
                callback(handler, browser, i32::from(loading), 0, 0);
            }
            release_raw(handler);
        }
    }

    fn notify_before_close(&self, browser: *mut _cef_browser_t) {
        let client = self.client.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() {
            return;
        }
        unsafe {
            let Some(get_handler) = (*client).get_life_span_handler else {
                return;
            };
            let handler = get_handler(client);
            if handler.is_null() {
                return;
            }
            if let Some(callback) = (*handler).on_before_close {
                add_ref_raw(browser);
                callback(handler, browser);
            }
            release_raw(handler);
        }
    }

    fn sync_window(&self, focus: bool) -> RuntimeResult<()> {
        let window = self.window();
        if window == 0 {
            return Ok(());
        }
        let (connection, screen_number) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen_number].root;
        let atoms = Atoms::new(&connection)?;
        let shell = connection.query_tree(self.parent)?.reply()?.parent;
        if shell != root {
            connection.change_property32(
                PropMode::REPLACE,
                window,
                AtomEnum::WM_TRANSIENT_FOR,
                AtomEnum::WINDOW,
                &[shell],
            )?;
        }
        let translated = connection
            .translate_coordinates(self.parent, root, 0, 0)?
            .reply()?;
        let extents = frame_extents(&connection, &atoms, window);
        let x = i32::from(translated.dst_x);
        let y = i32::from(translated.dst_y);
        let width = self
            .width
            .load(Ordering::Acquire)
            .saturating_sub((extents.0 + extents.1).max(0) as u32)
            .max(2);
        let height = self
            .height
            .load(Ordering::Acquire)
            .saturating_sub((extents.2 + extents.3).max(0) as u32)
            .max(2);
        connection.configure_window(
            window,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(width)
                .height(height)
                .stack_mode(StackMode::ABOVE),
        )?;
        request_above(&connection, &atoms, root, window)?;
        if focus {
            connection.set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)?;
        }
        connection.flush()?;
        Ok(())
    }
}

impl Drop for BrowserState {
    fn drop(&mut self) {
        self.close();
    }
}

pub fn shutdown_all() {
    let mut states = states().lock().expect("Firefox CEF state lock poisoned");
    for state in states.drain(..) {
        state.close();
        state.release_objects();
    }
}

fn states() -> &'static Mutex<Vec<Arc<BrowserState>>> {
    STATES.get_or_init(|| Mutex::new(Vec::new()))
}

fn intern(connection: &RustConnection, name: &[u8]) -> RuntimeResult<Atom> {
    Ok(connection.intern_atom(false, name)?.reply()?.atom)
}

fn candidate_windows(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
) -> RuntimeResult<BTreeSet<Window>> {
    let listed = connection
        .get_property(
            false,
            root,
            atoms.net_client_list,
            AtomEnum::WINDOW,
            0,
            u32::MAX,
        )?
        .reply()?
        .value32()
        .map(|values| values.collect::<Vec<_>>());
    let windows = match listed {
        Some(windows) => windows,
        None => connection.query_tree(root)?.reply()?.children,
    };
    Ok(windows
        .into_iter()
        .filter(|window| is_firefox_window(connection, atoms, *window))
        .collect())
}

fn is_firefox_window(connection: &RustConnection, atoms: &Atoms, window: Window) -> bool {
    connection
        .get_property(false, window, atoms.wm_class, AtomEnum::STRING, 0, 128)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .is_some_and(|property| {
            String::from_utf8_lossy(&property.value)
                .to_ascii_lowercase()
                .contains("firefox")
        })
}

fn wait_for_window(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
    previous: &BTreeSet<Window>,
    child: &mut Child,
    timeout: Duration,
) -> RuntimeResult<Window> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return Err(format!("Firefox exited before creating a window: {status}").into());
        }
        for window in candidate_windows(connection, atoms, root)? {
            if !previous.contains(&window) && window_pid(connection, atoms, window) == child.id() {
                return Ok(window);
            }
        }
        sleep(Duration::from_millis(50));
    }
    Err("Firefox did not create a window within 20 seconds".into())
}

fn window_pid(connection: &RustConnection, atoms: &Atoms, window: Window) -> u32 {
    connection
        .get_property(false, window, atoms.net_wm_pid, AtomEnum::CARDINAL, 0, 1)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| reply.value32()?.next())
        .unwrap_or(0)
}

fn frame_extents(
    connection: &RustConnection,
    atoms: &Atoms,
    window: Window,
) -> (i32, i32, i32, i32) {
    let values = connection
        .get_property(
            false,
            window,
            atoms.net_frame_extents,
            AtomEnum::CARDINAL,
            0,
            4,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|reply| reply.value32().map(|values| values.collect::<Vec<_>>()));
    values
        .filter(|values| values.len() == 4)
        .map(|values| {
            (
                values[0] as i32,
                values[1] as i32,
                values[2] as i32,
                values[3] as i32,
            )
        })
        .unwrap_or_default()
}

fn request_above(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
    window: Window,
) -> RuntimeResult<()> {
    connection.change_property32(
        PropMode::REPLACE,
        window,
        atoms.net_wm_state,
        AtomEnum::ATOM,
        &[atoms.net_wm_state_above],
    )?;
    connection.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY,
        ClientMessageEvent::new(
            32,
            window,
            atoms.net_wm_state,
            ClientMessageData::from([1, atoms.net_wm_state_above, 0, 1, 0]),
        ),
    )?;
    Ok(())
}
