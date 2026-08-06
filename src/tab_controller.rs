use crate::{
    cookie_store::{CanonicalCookie, CookieIdentity, CookieStore, cookie_contents_equal},
    native::{
        CefBackend, CefInstance, NativeRect, ProtocolNotice, RoutedNotice, configure_native_window,
        sampled_pixel_variants, spawn_cef_browser,
    },
    profile::ProfileDirectories,
};
use qtbridge::{QmlMethodInvoker, invoke_method};
use serde::Serialize;
use shell_protocol::wire;
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT,
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, CreateWindowAux,
            EventMask, InputFocus, MapState, StackMode, Window, WindowClass,
        },
    },
    rust_connection::RustConnection,
};

type TabResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
const MIN_RENDER_VARIANTS: usize = 8;

fn embedded_bounds(bounds: NativeRect) -> NativeRect {
    NativeRect {
        x: 0,
        y: 0,
        width: bounds.width,
        height: bounds.height,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TabEngine {
    Chromium,
    Firefox,
}

impl TabEngine {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "chromium" => Some(Self::Chromium),
            "firefox" => Some(Self::Firefox),
            _ => None,
        }
    }

    fn backend(self) -> CefBackend {
        match self {
            Self::Chromium => CefBackend::Chromium,
            Self::Firefox => CefBackend::Firefox,
        }
    }
}

impl From<CefBackend> for TabEngine {
    fn from(value: CefBackend) -> Self {
        match value {
            CefBackend::Chromium => Self::Chromium,
            CefBackend::Firefox => Self::Firefox,
        }
    }
}

pub enum TabCommand {
    AddWindow {
        id: u64,
        parent: Window,
        bounds: NativeRect,
        label: String,
        initial_url: String,
    },
    RemoveWindow(u64),
    Layout(u64, NativeRect),
    SetWindowState {
        id: u64,
        visible: bool,
        focused: bool,
    },
    Navigate(u64, String),
    Reload(u64),
    NewTab(u64, TabEngine),
    Select(u64, u64),
    Close(u64),
    SwitchEngine(u64, TabEngine),
    Move(u64, u64),
    MouseMove {
        window_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        leaving: bool,
    },
    MouseClick {
        window_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        button: wire::MouseButton,
        mouse_up: bool,
        click_count: u32,
    },
    MouseWheel {
        window_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        delta_x: i32,
        delta_y: i32,
    },
    KeyEvent {
        window_id: u64,
        event: wire::KeyEvent,
    },
    Stop,
}

pub struct TabController {
    sender: Sender<TabCommand>,
    thread: JoinHandle<()>,
}

impl TabController {
    pub fn send(&self, command: TabCommand) -> Result<(), mpsc::SendError<TabCommand>> {
        self.sender.send(command)
    }

    pub fn stop(self) {
        let _ = self.sender.send(TabCommand::Stop);
        let _ = self.thread.join();
    }
}

pub fn spawn(
    profile_id: String,
    directories: ProfileDirectories,
    invoker: QmlMethodInvoker,
) -> TabController {
    let (sender, receiver) = mpsc::channel();
    let invoker = Arc::new(Mutex::new(invoker));
    let thread = std::thread::spawn(move || {
        let failure_invoker = Arc::clone(&invoker);
        let result =
            Runtime::new(profile_id, directories, invoker, receiver).and_then(Runtime::run);
        if let Err(error) = result {
            eprintln!("deb: tab controller failed: {error}");
            if std::env::var("DEB_AUTOMATED_SMOKE_TEST").as_deref() == Ok("1") {
                let invoker = failure_invoker
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                invoke_method!(
                    &*invoker,
                    "finish_smoke_test",
                    "FAIL".to_owned(),
                    error.to_string()
                );
            }
        }
    });
    TabController { sender, thread }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TabSnapshot<'a> {
    id: String,
    engine: TabEngine,
    url: &'a str,
    title: &'a str,
    status: &'a str,
    loading: bool,
    crashed: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowSnapshot<'a> {
    id: String,
    label: &'a str,
    active_tab_id: String,
    tabs: Vec<TabSnapshot<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileSnapshot<'a> {
    windows: Vec<WindowSnapshot<'a>>,
}

struct Tab {
    id: u64,
    window_id: u64,
    engine: TabEngine,
    browser_id: Option<u64>,
    container: Option<Window>,
    url: String,
    title: String,
    status: String,
    loading: bool,
    crashed: bool,
}

struct BrowserWindow {
    id: u64,
    parent: Window,
    bounds: NativeRect,
    label: String,
    active_tab: u64,
    visible: bool,
    focused: bool,
}

impl Tab {
    fn snapshot(&self) -> TabSnapshot<'_> {
        TabSnapshot {
            id: self.id.to_string(),
            engine: self.engine,
            url: &self.url,
            title: &self.title,
            status: &self.status,
            loading: self.loading,
            crashed: self.crashed,
        }
    }
}

struct EngineRuntime {
    process: CefInstance,
    surfaces: HashMap<u64, Window>,
}

impl EngineRuntime {
    fn initial(process: CefInstance) -> Self {
        let mut surfaces = HashMap::new();
        surfaces.insert(process.initial_browser_id(), process.initial_window());
        Self { process, surfaces }
    }

    fn cookie_browser(&self) -> Option<u64> {
        self.surfaces.keys().copied().next()
    }
}

#[derive(Default)]
struct CookieSnapshot {
    active: bool,
    cookies: Vec<wire::Cookie>,
}

enum CookieMutation {
    Set(wire::Cookie),
    Delete(wire::Cookie),
}

struct CookieAction {
    target: CefBackend,
    mutation: CookieMutation,
}

struct CookieSync {
    store: CookieStore,
    chromium: CookieSnapshot,
    firefox: CookieSnapshot,
}

impl CookieSync {
    fn new(profile_data: &std::path::Path) -> TabResult<Self> {
        Ok(Self {
            store: CookieStore::open(profile_data)?,
            chromium: CookieSnapshot::default(),
            firefox: CookieSnapshot::default(),
        })
    }

    fn snapshot_mut(&mut self, backend: CefBackend) -> &mut CookieSnapshot {
        match backend {
            CefBackend::Chromium => &mut self.chromium,
            CefBackend::Firefox => &mut self.firefox,
        }
    }

    fn begin(&mut self, backend: CefBackend) {
        let snapshot = self.snapshot_mut(backend);
        snapshot.active = true;
        snapshot.cookies.clear();
    }

    fn observe(
        &mut self,
        source: CefBackend,
        notices: &[RoutedNotice],
    ) -> TabResult<Vec<CookieAction>> {
        let mut actions = Vec::new();
        for notice in notices {
            match &notice.value {
                ProtocolNotice::CookieSnapshotEntry(cookie) => {
                    if self.snapshot_mut(source).active {
                        self.snapshot_mut(source).cookies.push(cookie.clone());
                    }
                }
                ProtocolNotice::CookieSnapshotComplete => {
                    actions.extend(self.finish_snapshot(source)?);
                }
                ProtocolNotice::CookieChanged(cookie, cause) => {
                    if let Some(action) = self.apply_change(source, cookie, *cause)? {
                        actions.push(action);
                    }
                }
                _ => {}
            }
        }
        Ok(actions)
    }

    fn finish_snapshot(&mut self, source: CefBackend) -> TabResult<Vec<CookieAction>> {
        let cookies = {
            let snapshot = self.snapshot_mut(source);
            if !snapshot.active {
                return Ok(Vec::new());
            }
            snapshot.active = false;
            std::mem::take(&mut snapshot.cookies)
        };
        for cookie in &cookies {
            self.store.merge_snapshot(cookie)?;
        }
        let mut index = HashMap::new();
        for cookie in cookies {
            let identity = CookieIdentity::from_cookie(&cookie)?;
            if !identity.is_opaque() {
                index.insert(identity, cookie);
            }
        }
        let mut actions = Vec::new();
        for canonical in self.store.all()? {
            if let Some(mutation) = reconcile_mutation(&index, canonical)? {
                actions.push(CookieAction {
                    target: source,
                    mutation,
                });
            }
        }
        Ok(actions)
    }

    fn apply_change(
        &self,
        source: CefBackend,
        cookie: &wire::Cookie,
        cause: wire::CookieChangeCause,
    ) -> TabResult<Option<CookieAction>> {
        let identity = CookieIdentity::from_cookie(cookie)?;
        if identity.is_opaque() {
            return Ok(None);
        }
        let deletion = matches!(
            cause,
            wire::CookieChangeCause::Explicit
                | wire::CookieChangeCause::UnknownDeletion
                | wire::CookieChangeCause::Expired
                | wire::CookieChangeCause::Evicted
        );
        let changed = if deletion {
            self.store.apply_deletion(cookie)?
        } else if matches!(
            cause,
            wire::CookieChangeCause::Inserted
                | wire::CookieChangeCause::InsertedNoChangeOverwrite
                | wire::CookieChangeCause::InsertedNoValueChangeOverwrite
        ) {
            self.store.apply_live_change(cookie)?
        } else {
            false
        };
        if !changed {
            return Ok(None);
        }
        Ok(Some(CookieAction {
            target: match source {
                CefBackend::Chromium => CefBackend::Firefox,
                CefBackend::Firefox => CefBackend::Chromium,
            },
            mutation: if deletion {
                CookieMutation::Delete(cookie.clone())
            } else {
                CookieMutation::Set(cookie.clone())
            },
        }))
    }
}

fn reconcile_mutation(
    snapshot: &HashMap<CookieIdentity, wire::Cookie>,
    canonical: CanonicalCookie,
) -> TabResult<Option<CookieMutation>> {
    let identity = CookieIdentity::from_cookie(&canonical.cookie)?;
    Ok(match (canonical.deleted, snapshot.get(&identity)) {
        (true, Some(_)) => Some(CookieMutation::Delete(canonical.cookie)),
        (true, None) => None,
        (false, Some(cookie)) if cookie_contents_equal(cookie, &canonical.cookie) => None,
        (false, _) => Some(CookieMutation::Set(canonical.cookie)),
    })
}

struct Runtime {
    profile_id: String,
    directories: ProfileDirectories,
    invoker: Arc<Mutex<QmlMethodInvoker>>,
    receiver: Receiver<TabCommand>,
    connection: RustConnection,
    windows: HashMap<u64, BrowserWindow>,
    tabs: Vec<Tab>,
    next_tab_id: u64,
    next_browser_id: u64,
    chromium: Option<EngineRuntime>,
    firefox: Option<EngineRuntime>,
    retired_containers: HashMap<u64, (CefBackend, Window)>,
    cookie_sync: CookieSync,
    automation: Option<Automation>,
    dirty: bool,
}

#[derive(Clone)]
enum AutomationPhase {
    WaitChromium,
    WaitFirefox,
    WaitCookies,
    WaitSiblings,
    WaitNavigations,
    CaptureChromium { deadline: Instant, attempts: u8 },
    CaptureFirefox { deadline: Instant, attempts: u8 },
    WaitChromiumCrash,
    WaitChromiumRecovery,
    WaitFirefoxCrash,
    WaitFirefoxRecovery,
    PrepareChromiumMove { deadline: Instant, attempts: u8 },
    PrepareFirefoxMove { deadline: Instant, attempts: u8 },
    WaitWindowMoves,
    CaptureMovedChromium { deadline: Instant, attempts: u8 },
    CaptureMovedFirefox { deadline: Instant, attempts: u8 },
    WaitEngineSwitch,
    CaptureSwitched { deadline: Instant, attempts: u8 },
}

#[derive(Clone)]
struct Automation {
    started: Instant,
    initial_url: String,
    target_url: String,
    phase: AutomationPhase,
    chromium_tab: u64,
    firefox_tab: Option<u64>,
    chromium_sibling: Option<u64>,
    firefox_sibling: Option<u64>,
    navigation_started: HashSet<u64>,
    navigation_settled: HashSet<u64>,
    chromium_cookie_seen: bool,
    firefox_cookie_seen: bool,
    chromium_variants: Option<usize>,
    firefox_variants: Option<usize>,
    crashed_tabs: HashSet<u64>,
    chromium_process: Option<u32>,
    firefox_process: Option<u32>,
    second_window: Option<u64>,
    moved_chromium_variants: Option<usize>,
    moved_firefox_variants: Option<usize>,
}

impl Automation {
    fn from_environment(initial_url: String) -> Option<Self> {
        if std::env::var("DEB_AUTOMATED_SMOKE_TEST").as_deref() != Ok("1") {
            return None;
        }
        Some(Self {
            started: Instant::now(),
            initial_url,
            target_url: std::env::var("DEB_SMOKE_NAVIGATE_URL")
                .unwrap_or_else(|_| "deb://new-tab/".to_owned()),
            phase: AutomationPhase::WaitChromium,
            chromium_tab: 1,
            firefox_tab: None,
            chromium_sibling: None,
            firefox_sibling: None,
            navigation_started: HashSet::new(),
            navigation_settled: HashSet::new(),
            chromium_cookie_seen: false,
            firefox_cookie_seen: false,
            chromium_variants: None,
            firefox_variants: None,
            crashed_tabs: HashSet::new(),
            chromium_process: None,
            firefox_process: None,
            second_window: None,
            moved_chromium_variants: None,
            moved_firefox_variants: None,
        })
    }
}

impl Runtime {
    fn new(
        profile_id: String,
        directories: ProfileDirectories,
        invoker: Arc<Mutex<QmlMethodInvoker>>,
        receiver: Receiver<TabCommand>,
    ) -> TabResult<Self> {
        let (connection, _) = x11rb::connect(None)?;
        let cookie_sync = CookieSync::new(&directories.shared_data)?;
        Ok(Self {
            profile_id,
            directories,
            invoker,
            receiver,
            connection,
            windows: HashMap::new(),
            tabs: Vec::new(),
            next_tab_id: 1,
            next_browser_id: 1,
            chromium: None,
            firefox: None,
            retired_containers: HashMap::new(),
            cookie_sync,
            automation: None,
            dirty: false,
        })
    }

    fn run(mut self) -> TabResult<()> {
        loop {
            match self.receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(TabCommand::AddWindow {
                    id,
                    parent,
                    bounds,
                    label,
                    initial_url,
                }) => self.add_window(id, parent, bounds, label, initial_url)?,
                Ok(TabCommand::RemoveWindow(id)) => self.remove_window(id)?,
                Ok(TabCommand::Layout(id, bounds)) => self.layout(id, bounds)?,
                Ok(TabCommand::SetWindowState {
                    id,
                    visible,
                    focused,
                }) => self.set_window_state(id, visible, focused)?,
                Ok(TabCommand::Navigate(window_id, url)) => self.navigate(window_id, &url)?,
                Ok(TabCommand::Reload(window_id)) => self.reload(window_id)?,
                Ok(TabCommand::NewTab(window_id, engine)) => self.new_tab(window_id, engine)?,
                Ok(TabCommand::Select(window_id, tab_id)) => self.select(window_id, tab_id)?,
                Ok(TabCommand::Close(tab_id)) => self.close_tab(tab_id)?,
                Ok(TabCommand::SwitchEngine(tab_id, engine)) => {
                    self.switch_engine(tab_id, engine)?
                }
                Ok(TabCommand::Move(tab_id, target_window)) => {
                    self.move_tab(tab_id, target_window)?
                }
                Ok(TabCommand::MouseMove {
                    window_id,
                    x,
                    y,
                    modifiers,
                    leaving,
                }) => self.mouse_move(window_id, x, y, modifiers, leaving)?,
                Ok(TabCommand::MouseClick {
                    window_id,
                    x,
                    y,
                    modifiers,
                    button,
                    mouse_up,
                    click_count,
                }) => {
                    self.mouse_click(window_id, x, y, modifiers, button, mouse_up, click_count)?
                }
                Ok(TabCommand::MouseWheel {
                    window_id,
                    x,
                    y,
                    modifiers,
                    delta_x,
                    delta_y,
                }) => self.mouse_wheel(window_id, x, y, modifiers, delta_x, delta_y)?,
                Ok(TabCommand::KeyEvent { window_id, event }) => {
                    self.key_event(window_id, event)?
                }
                Ok(TabCommand::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.handle_x11_events()?;
            self.poll_engine(CefBackend::Chromium)?;
            self.poll_engine(CefBackend::Firefox)?;
            if let Some((outcome, details)) = self.advance_automation()? {
                if let Some(engine) = self.chromium.take() {
                    engine.process.shutdown();
                }
                if let Some(engine) = self.firefox.take() {
                    engine.process.shutdown();
                }
                let invoker = self
                    .invoker
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                invoke_method!(&*invoker, "finish_smoke_test", outcome, details);
                return Ok(());
            }
            if self.dirty {
                self.publish();
            }
        }

        if let Some(engine) = self.chromium.take() {
            engine.process.shutdown();
        }
        if let Some(engine) = self.firefox.take() {
            engine.process.shutdown();
        }
        Ok(())
    }

    fn add_window(
        &mut self,
        id: u64,
        parent: Window,
        bounds: NativeRect,
        label: String,
        initial_url: String,
    ) -> TabResult<()> {
        if self.windows.contains_key(&id) {
            return Err(format!("window {id} is already registered").into());
        }
        if self.automation.is_none() {
            self.automation = Automation::from_environment(initial_url.clone());
        }
        self.windows.insert(
            id,
            BrowserWindow {
                id,
                parent,
                bounds,
                label,
                active_tab: 0,
                visible: true,
                focused: false,
            },
        );
        let tab_id = self.add_tab(id, TabEngine::Chromium, initial_url);
        self.windows
            .get_mut(&id)
            .expect("window was inserted")
            .active_tab = tab_id;
        self.attach_tab(tab_id)?;
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn remove_window(&mut self, id: u64) -> TabResult<()> {
        if self.windows.remove(&id).is_none() {
            return Ok(());
        }
        let tab_ids = self
            .tabs
            .iter()
            .filter(|tab| tab.window_id == id)
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        for tab_id in tab_ids {
            self.close_tab(tab_id)?;
        }
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn add_tab(&mut self, window_id: u64, engine: TabEngine, url: String) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(Tab {
            id,
            window_id,
            engine,
            browser_id: None,
            container: None,
            url,
            title: "New tab".to_owned(),
            status: "Starting engine…".to_owned(),
            loading: true,
            crashed: false,
        });
        self.dirty = true;
        id
    }

    fn new_tab(&mut self, window_id: u64, engine: TabEngine) -> TabResult<()> {
        self.window(window_id)?;
        let id = self.add_tab(window_id, engine, "deb://new-tab/".to_owned());
        self.window_mut(window_id)?.active_tab = id;
        self.attach_tab(id)?;
        self.refresh_visibility()
    }

    fn create_tab_container(&self, parent: Window, bounds: NativeRect) -> TabResult<Window> {
        let container = self.connection.generate_id()?;
        self.connection.create_window(
            COPY_DEPTH_FROM_PARENT,
            container,
            parent,
            i16::try_from(bounds.x)?,
            i16::try_from(bounds.y)?,
            u16::try_from(bounds.width)?,
            u16::try_from(bounds.height)?,
            0,
            WindowClass::INPUT_OUTPUT,
            COPY_FROM_PARENT,
            &CreateWindowAux::new(),
        )?;
        self.connection.map_window(container)?;
        self.connection.flush()?;
        Ok(container)
    }

    fn attach_tab(&mut self, tab_id: u64) -> TabResult<()> {
        let browser_id = self.next_browser_id;
        self.next_browser_id += 1;
        let (backend, url, window_id) = {
            let tab = self.tab_mut(tab_id)?;
            tab.browser_id = Some(browser_id);
            tab.status = format!("Starting {}…", tab.engine.backend().label());
            tab.loading = true;
            tab.crashed = false;
            (tab.engine.backend(), tab.url.clone(), tab.window_id)
        };
        let (host, bounds) = {
            let window = self.window(window_id)?;
            (window.parent, window.bounds)
        };
        let parent = if let Some(container) = self.tab(tab_id)?.container {
            container
        } else {
            let container = self.create_tab_container(host, bounds)?;
            self.tab_mut(tab_id)?.container = Some(container);
            container
        };
        let profile_id = self.profile_id.clone();
        let directories = backend.directories(&self.directories).clone();
        if self.engine(backend).is_none() {
            match spawn_cef_browser(
                &self.connection,
                parent,
                bounds,
                &url,
                &profile_id,
                &directories,
                backend,
                browser_id,
            ) {
                Ok(process) => {
                    let surface = process.initial_window();
                    let actual_parent = self.connection.query_tree(surface)?.reply()?.parent;
                    if actual_parent != parent {
                        process.shutdown();
                        return Err(format!(
                            "{} returned a window outside its tab container",
                            backend.label()
                        )
                        .into());
                    }
                    self.connection.change_window_attributes(
                        surface,
                        &ChangeWindowAttributesAux::new().event_mask(EventMask::BUTTON_PRESS),
                    )?;
                    configure_native_window(&self.connection, surface, embedded_bounds(bounds))?;
                    let mut runtime = EngineRuntime::initial(process);
                    self.cookie_sync.begin(backend);
                    runtime
                        .process
                        .read_browser_cookies(browser_id)
                        .map_err(|error| {
                            format!("{} cookie snapshot failed: {error}", backend.label())
                        })?;
                    *self.engine_mut(backend) = Some(runtime);
                    self.tab_mut(tab_id)?.status = format!("Live · {}", backend.label());
                }
                Err(error) => {
                    self.connection.destroy_window(parent)?;
                    let tab = self.tab_mut(tab_id)?;
                    tab.browser_id = None;
                    tab.container = None;
                    tab.loading = false;
                    tab.status = format!("{} failed: {error}", backend.label());
                }
            }
        } else {
            self.engine_mut(backend)
                .as_mut()
                .expect("engine presence was checked")
                .process
                .create_browser(browser_id, parent, bounds, &url, &profile_id, &directories)?;
        }
        self.dirty = true;
        Ok(())
    }

    fn navigate(&mut self, window_id: u64, url: &str) -> TabResult<()> {
        let tab_id = self.window(window_id)?.active_tab;
        let (backend, browser_id) = {
            let tab = self.tab_mut(tab_id)?;
            tab.url = url.to_owned();
            tab.loading = true;
            tab.crashed = false;
            tab.status = "Navigating…".to_owned();
            (tab.engine.backend(), tab.browser_id)
        };
        if let Some(browser_id) = browser_id {
            if let Some(engine) = self.engine_mut(backend) {
                engine.process.navigate_browser(browser_id, url)?;
            }
        } else {
            self.attach_tab(tab_id)?;
        }
        self.dirty = true;
        Ok(())
    }

    fn reload(&mut self, window_id: u64) -> TabResult<()> {
        let tab_id = self.window(window_id)?.active_tab;
        let url = self.tab(tab_id)?.url.clone();
        self.navigate(window_id, &url)
    }

    fn select(&mut self, window_id: u64, tab_id: u64) -> TabResult<()> {
        if self.tab(tab_id)?.window_id != window_id {
            return Err(format!("tab {tab_id} does not belong to window {window_id}").into());
        }
        self.window_mut(window_id)?.active_tab = tab_id;
        if self.tab(tab_id)?.browser_id.is_none() {
            self.attach_tab(tab_id)?;
        }
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn close_tab(&mut self, tab_id: u64) -> TabResult<()> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .ok_or_else(|| format!("tab {tab_id} does not exist"))?;
        let tab = self.tabs.remove(index);
        if let Some(container) = tab.container {
            let _ = self.connection.unmap_window(container);
            if let Some(browser_id) = tab.browser_id {
                self.retired_containers
                    .insert(browser_id, (tab.engine.backend(), container));
            } else {
                self.connection.destroy_window(container)?;
            }
        }
        if let Some(browser_id) = tab.browser_id {
            if let Some(engine) = self.engine_mut(tab.engine.backend()) {
                engine.surfaces.remove(&browser_id);
                engine.process.close_browser(browser_id, true)?;
            } else if let Some((_, container)) = self.retired_containers.remove(&browser_id) {
                self.connection.destroy_window(container)?;
            }
        }
        let remaining = self
            .tabs
            .iter()
            .filter(|candidate| candidate.window_id == tab.window_id)
            .map(|candidate| candidate.id)
            .collect::<Vec<_>>();
        if self.windows.contains_key(&tab.window_id) && remaining.is_empty() {
            let id = self.add_tab(
                tab.window_id,
                TabEngine::Chromium,
                "deb://new-tab/".to_owned(),
            );
            self.window_mut(tab.window_id)?.active_tab = id;
            self.attach_tab(id)?;
        } else if self
            .windows
            .get(&tab.window_id)
            .is_some_and(|window| window.active_tab == tab_id)
            && let Some(next) = remaining.get(index.min(remaining.len().saturating_sub(1)))
        {
            self.window_mut(tab.window_id)?.active_tab = *next;
        }
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn switch_engine(&mut self, tab_id: u64, engine: TabEngine) -> TabResult<()> {
        let (old_backend, old_browser_id) = {
            let tab = self.tab(tab_id)?;
            if tab.engine == engine {
                return Ok(());
            }
            (tab.engine.backend(), tab.browser_id)
        };
        if let Some(browser_id) = old_browser_id
            && let Some(runtime) = self.engine_mut(old_backend)
        {
            runtime.surfaces.remove(&browser_id);
            runtime.process.set_browser_visible(browser_id, false)?;
            runtime.process.close_browser(browser_id, true)?;
        }
        {
            let tab = self.tab_mut(tab_id)?;
            tab.engine = engine;
            tab.browser_id = None;
            tab.title = "Loading in new engine…".to_owned();
            tab.status = "Switching engine…".to_owned();
            tab.loading = true;
            tab.crashed = false;
        }
        self.attach_tab(tab_id)?;
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn move_tab(&mut self, tab_id: u64, target_window: u64) -> TabResult<()> {
        let (source_window, backend, browser_id, container) = {
            let tab = self.tab(tab_id)?;
            (
                tab.window_id,
                tab.engine.backend(),
                tab.browser_id,
                tab.container,
            )
        };
        if source_window == target_window {
            return Ok(());
        }
        let (target_parent, target_bounds) = {
            let window = self.window(target_window)?;
            (window.parent, window.bounds)
        };
        if let Some(container) = container {
            self.connection
                .reparent_window(container, target_parent, 0, 0)?;
            configure_native_window(&self.connection, container, target_bounds)?;
        }
        if let Some(browser_id) = browser_id {
            let surface = self
                .engine(backend)
                .and_then(|engine| engine.surfaces.get(&browser_id))
                .copied()
                .ok_or("moving tab has no native surface")?;
            if self.connection.query_tree(surface)?.reply()?.parent
                != container.ok_or("moving tab has no native container")?
            {
                return Err("moving tab escaped its native container".into());
            }
            configure_native_window(&self.connection, surface, embedded_bounds(target_bounds))?;
            if let Some(engine) = self.engine_mut(backend) {
                engine.process.resize_browser(browser_id, target_bounds)?;
            }
        }
        self.tab_mut(tab_id)?.window_id = target_window;
        self.window_mut(target_window)?.active_tab = tab_id;

        let source_tabs = self
            .tabs
            .iter()
            .filter(|tab| tab.window_id == source_window)
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        if source_tabs.is_empty() {
            let replacement = self.add_tab(
                source_window,
                TabEngine::Chromium,
                "deb://new-tab/".to_owned(),
            );
            self.window_mut(source_window)?.active_tab = replacement;
            self.attach_tab(replacement)?;
        } else if self.window(source_window)?.active_tab == tab_id {
            self.window_mut(source_window)?.active_tab = source_tabs[0];
        }
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn active_browser(&self, window_id: u64) -> TabResult<(CefBackend, Option<u64>)> {
        let tab = self.tab(self.window(window_id)?.active_tab)?;
        Ok((tab.engine.backend(), tab.browser_id))
    }

    fn mouse_move(
        &mut self,
        window_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        leaving: bool,
    ) -> TabResult<()> {
        let (backend, browser_id) = self.active_browser(window_id)?;
        if let Some(browser_id) = browser_id
            && let Some(engine) = self.engine_mut(backend)
        {
            engine
                .process
                .send_mouse_move(browser_id, x, y, modifiers, leaving)?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn mouse_click(
        &mut self,
        window_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        button: wire::MouseButton,
        mouse_up: bool,
        click_count: u32,
    ) -> TabResult<()> {
        let (backend, browser_id) = self.active_browser(window_id)?;
        if let Some(browser_id) = browser_id
            && let Some(engine) = self.engine_mut(backend)
        {
            engine.process.send_mouse_click(
                browser_id,
                x,
                y,
                modifiers,
                button,
                mouse_up,
                click_count,
            )?;
        }
        Ok(())
    }

    fn mouse_wheel(
        &mut self,
        window_id: u64,
        x: i32,
        y: i32,
        modifiers: u32,
        delta_x: i32,
        delta_y: i32,
    ) -> TabResult<()> {
        let (backend, browser_id) = self.active_browser(window_id)?;
        if let Some(browser_id) = browser_id
            && let Some(engine) = self.engine_mut(backend)
        {
            engine
                .process
                .send_mouse_wheel(browser_id, x, y, modifiers, delta_x, delta_y)?;
        }
        Ok(())
    }

    fn key_event(&mut self, window_id: u64, event: wire::KeyEvent) -> TabResult<()> {
        let (backend, browser_id) = self.active_browser(window_id)?;
        if let Some(browser_id) = browser_id
            && let Some(engine) = self.engine_mut(backend)
        {
            engine.process.send_key_event(browser_id, event)?;
        }
        Ok(())
    }

    fn layout(&mut self, window_id: u64, bounds: NativeRect) -> TabResult<()> {
        self.window_mut(window_id)?.bounds = bounds;
        let tab_states = self
            .tabs
            .iter()
            .filter(|tab| tab.window_id == window_id)
            .map(|tab| {
                (
                    tab.engine.backend(),
                    tab.browser_id,
                    tab.container,
                    tab.browser_id.and_then(|browser_id| {
                        self.engine(tab.engine.backend())
                            .and_then(|engine| engine.surfaces.get(&browser_id))
                            .copied()
                    }),
                )
            })
            .collect::<Vec<_>>();
        for (backend, browser_id, container, surface) in tab_states {
            if let Some(container) = container {
                configure_native_window(&self.connection, container, bounds)?;
            }
            if let Some(surface) = surface {
                configure_native_window(&self.connection, surface, embedded_bounds(bounds))?;
            }
            if let Some(browser_id) = browser_id
                && let Some(engine) = self.engine_mut(backend)
            {
                engine.process.resize_browser(browser_id, bounds)?;
            }
        }
        Ok(())
    }

    fn set_window_state(&mut self, id: u64, visible: bool, focused: bool) -> TabResult<()> {
        let window = self.window_mut(id)?;
        window.visible = visible;
        window.focused = focused;
        self.refresh_visibility()
    }

    fn poll_engine(&mut self, backend: CefBackend) -> TabResult<()> {
        let Some(engine) = self.engine_mut(backend) else {
            return Ok(());
        };
        let notices = engine.process.drain_routed_notices();
        let exited = engine.process.exited()?;
        let protocol_closed = engine.process.protocol_closed();
        let actions = self.cookie_sync.observe(backend, &notices)?;
        self.apply_cookie_actions(actions)?;
        for notice in notices {
            self.handle_notice(backend, notice)?;
        }
        if exited.is_some() || protocol_closed {
            self.recover_engine(backend, exited.map(|status| status.to_string()))?;
        }
        Ok(())
    }

    fn handle_notice(&mut self, backend: CefBackend, notice: RoutedNotice) -> TabResult<()> {
        let browser_id = notice.browser_id;
        self.observe_automation(backend, browser_id, &notice.value);
        match notice.value {
            ProtocolNotice::SurfaceReady(raw_window) => {
                let window = u32::try_from(raw_window)?;
                let (expected_parent, bounds) = self
                    .tabs
                    .iter()
                    .find(|tab| {
                        tab.engine.backend() == backend && tab.browser_id == Some(browser_id)
                    })
                    .map(|tab| {
                        let bounds = self.windows.get(&tab.window_id).map(|entry| entry.bounds);
                        (tab.container, bounds)
                    })
                    .ok_or("surface event targets an unknown tab")?;
                let expected_parent = expected_parent.ok_or("tab has no native container")?;
                let bounds = bounds.ok_or("surface event targets an unknown window")?;
                let actual_parent = self.connection.query_tree(window)?.reply()?.parent;
                if actual_parent != expected_parent {
                    return Err(format!(
                        "{} returned a window outside its tab container",
                        backend.label()
                    )
                    .into());
                }
                self.connection.change_window_attributes(
                    window,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::BUTTON_PRESS),
                )?;
                configure_native_window(&self.connection, window, embedded_bounds(bounds))?;
                self.engine_mut(backend)
                    .as_mut()
                    .expect("surface event requires an engine")
                    .surfaces
                    .insert(browser_id, window);
                if let Some(tab) = self.tab_for_browser_mut(backend, browser_id) {
                    tab.status = format!("Live · {}", backend.label());
                }
                self.refresh_visibility()?;
            }
            ProtocolNotice::LoadingChanged(loading) => {
                if let Some(tab) = self.tab_for_browser_mut(backend, browser_id) {
                    tab.loading = loading;
                    tab.status = if loading {
                        "Loading…".to_owned()
                    } else {
                        format!("Live · {}", backend.label())
                    };
                }
            }
            ProtocolNotice::NavigationCommitted(url) => {
                if let Some(tab) = self.tab_for_browser_mut(backend, browser_id) {
                    tab.url = url;
                }
            }
            ProtocolNotice::TitleChanged(title) => {
                if let Some(tab) = self.tab_for_browser_mut(backend, browser_id) {
                    tab.title = if title.is_empty() {
                        tab.url.clone()
                    } else {
                        title
                    };
                }
            }
            ProtocolNotice::LoadFailed(error) | ProtocolNotice::CommandFailed(error) => {
                if let Some(tab) = self.tab_for_browser_mut(backend, browser_id) {
                    tab.loading = false;
                    tab.status = error;
                }
            }
            ProtocolNotice::Crashed(reason) => {
                if let Some(tab) = self.tab_for_browser_mut(backend, browser_id) {
                    tab.loading = false;
                    tab.crashed = true;
                    tab.status = format!("Renderer crashed · {reason} · reload to recover");
                }
            }
            ProtocolNotice::Closed => {
                if let Some((_, container)) = self.retired_containers.remove(&browser_id) {
                    self.connection.destroy_window(container)?;
                }
                if let Some(tab) = self.tab_for_browser_mut(backend, browser_id) {
                    tab.browser_id = None;
                    tab.loading = false;
                    tab.status = "Browser closed · reload to restore".to_owned();
                }
                if let Some(engine) = self.engine_mut(backend) {
                    engine.surfaces.remove(&browser_id);
                }
            }
            ProtocolNotice::ProtocolFailed(error) => {
                if browser_id != 0
                    && let Some(tab) = self.tab_for_browser_mut(backend, browser_id)
                {
                    tab.status = format!("Protocol failed: {error}");
                }
            }
            ProtocolNotice::CookieSnapshotEntry(_)
            | ProtocolNotice::CookieSnapshotComplete
            | ProtocolNotice::CookieChanged(_, _) => {}
        }
        self.dirty = true;
        Ok(())
    }

    fn recover_engine(&mut self, backend: CefBackend, exit: Option<String>) -> TabResult<()> {
        let reason = exit.unwrap_or_else(|| "protocol connection closed".to_owned());
        if let Some(engine) = self.engine_mut(backend).take() {
            engine.process.shutdown();
        }
        let retired = self
            .retired_containers
            .iter()
            .filter_map(|(browser_id, (retired_backend, container))| {
                (*retired_backend == backend).then_some((*browser_id, *container))
            })
            .collect::<Vec<_>>();
        for (browser_id, container) in retired {
            self.retired_containers.remove(&browser_id);
            self.connection.destroy_window(container)?;
        }
        let affected = self
            .tabs
            .iter()
            .filter(|tab| tab.engine.backend() == backend)
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        for tab_id in &affected {
            let tab = self.tab_mut(*tab_id)?;
            tab.browser_id = None;
            tab.loading = true;
            tab.status = format!("{} process exited ({reason}); restarting…", backend.label());
        }
        for tab_id in affected {
            if let Err(error) = self.attach_tab(tab_id) {
                let tab = self.tab_mut(tab_id)?;
                tab.loading = false;
                tab.status = format!("{} restart failed: {error}", backend.label());
                break;
            }
        }
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn observe_automation(
        &mut self,
        backend: CefBackend,
        browser_id: u64,
        notice: &ProtocolNotice,
    ) {
        let tab_id = self
            .tabs
            .iter()
            .find(|tab| tab.engine.backend() == backend && tab.browser_id == Some(browser_id))
            .map(|tab| tab.id);
        let Some(automation) = &mut self.automation else {
            return;
        };
        match notice {
            ProtocolNotice::LoadingChanged(true)
                if matches!(automation.phase, AutomationPhase::WaitNavigations) =>
            {
                if let Some(tab_id) = tab_id {
                    automation.navigation_started.insert(tab_id);
                }
            }
            ProtocolNotice::LoadingChanged(false)
                if matches!(automation.phase, AutomationPhase::WaitNavigations) =>
            {
                if let Some(tab_id) = tab_id
                    && automation.navigation_started.contains(&tab_id)
                {
                    automation.navigation_settled.insert(tab_id);
                }
            }
            ProtocolNotice::CookieChanged(cookie, cause)
                if is_automation_cookie(cookie)
                    && matches!(
                        cause,
                        wire::CookieChangeCause::Inserted
                            | wire::CookieChangeCause::InsertedNoChangeOverwrite
                            | wire::CookieChangeCause::InsertedNoValueChangeOverwrite
                    ) =>
            {
                match backend {
                    CefBackend::Chromium => automation.chromium_cookie_seen = true,
                    CefBackend::Firefox => automation.firefox_cookie_seen = true,
                }
            }
            ProtocolNotice::Crashed(_) => {
                if let Some(tab_id) = tab_id {
                    automation.crashed_tabs.insert(tab_id);
                }
            }
            _ => {}
        }
    }

    fn advance_automation(&mut self) -> TabResult<Option<(String, String)>> {
        let Some(automation) = self.automation.clone() else {
            return Ok(None);
        };
        if automation.started.elapsed() > Duration::from_secs(35) {
            return Ok(Some((
                "FAIL".to_owned(),
                "multi-window tab smoke test did not complete within 35 seconds".to_owned(),
            )));
        }
        let phase = automation.phase.clone();
        match phase {
            AutomationPhase::WaitChromium => {
                let chromium_tab = automation.chromium_tab;
                if self.tab_ready(chromium_tab) {
                    let initial_url = automation.initial_url.clone();
                    let window_id = self.tab(chromium_tab)?.window_id;
                    let firefox_tab = self.add_tab(window_id, TabEngine::Firefox, initial_url);
                    self.window_mut(window_id)?.active_tab = firefox_tab;
                    self.attach_tab(firefox_tab)?;
                    self.refresh_visibility()?;
                    let automation = self.automation.as_mut().expect("automation is active");
                    automation.firefox_tab = Some(firefox_tab);
                    automation.phase = AutomationPhase::WaitFirefox;
                }
            }
            AutomationPhase::WaitFirefox => {
                let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                if self.tab_ready(firefox_tab) {
                    let chromium = self
                        .chromium
                        .as_mut()
                        .ok_or("automation lost the Chromium process")?;
                    let browser_id = chromium
                        .cookie_browser()
                        .ok_or("automation has no Chromium browser")?;
                    chromium
                        .process
                        .set_browser_cookie(browser_id, automation_cookie())?;
                    self.automation
                        .as_mut()
                        .expect("automation is active")
                        .phase = AutomationPhase::WaitCookies;
                    eprintln!("deb-smoke: exercising Chromium-to-Gecko cookie synchronization");
                }
            }
            AutomationPhase::WaitCookies => {
                if automation.chromium_cookie_seen && automation.firefox_cookie_seen {
                    let initial_url = automation.initial_url.clone();
                    let window_id = self.tab(automation.chromium_tab)?.window_id;
                    let chromium_sibling =
                        self.add_tab(window_id, TabEngine::Chromium, initial_url.clone());
                    self.window_mut(window_id)?.active_tab = chromium_sibling;
                    self.attach_tab(chromium_sibling)?;
                    self.refresh_visibility()?;
                    let firefox_sibling = self.add_tab(window_id, TabEngine::Firefox, initial_url);
                    self.window_mut(window_id)?.active_tab = firefox_sibling;
                    self.attach_tab(firefox_sibling)?;
                    self.refresh_visibility()?;
                    let automation = self.automation.as_mut().expect("automation is active");
                    automation.chromium_sibling = Some(chromium_sibling);
                    automation.firefox_sibling = Some(firefox_sibling);
                    automation.phase = AutomationPhase::WaitSiblings;
                    eprintln!(
                        "deb-smoke: both engines synchronized a cookie; creating same-engine sibling tabs"
                    );
                }
            }
            AutomationPhase::WaitSiblings => {
                let chromium_sibling = automation
                    .chromium_sibling
                    .expect("Chromium sibling was created");
                let firefox_sibling = automation
                    .firefox_sibling
                    .expect("Firefox sibling was created");
                if self.tab_ready(chromium_sibling) && self.tab_ready(firefox_sibling) {
                    let chromium_tab = automation.chromium_tab;
                    let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                    let target_url = automation.target_url.clone();
                    self.navigate_tab_to(chromium_tab, &target_url)?;
                    self.navigate_tab_to(firefox_tab, &target_url)?;
                    self.navigate_tab_to(chromium_sibling, &target_url)?;
                    self.navigate_tab_to(firefox_sibling, &target_url)?;
                    let source_window = self.tab(chromium_tab)?.window_id;
                    if let Some(second_window_chromium) = self
                        .tabs
                        .iter()
                        .find(|tab| {
                            tab.window_id != source_window && tab.engine == TabEngine::Chromium
                        })
                        .map(|tab| tab.id)
                    {
                        self.navigate_tab_to(second_window_chromium, &target_url)?;
                    }
                    let automation = self.automation.as_mut().expect("automation is active");
                    automation.navigation_started.clear();
                    automation.navigation_settled.clear();
                    automation.phase = AutomationPhase::WaitNavigations;
                    eprintln!(
                        "deb-smoke: four tabs share two profile helpers; navigating the crash-test tabs to {target_url}"
                    );
                }
            }
            AutomationPhase::WaitNavigations => {
                let chromium_tab = automation.chromium_tab;
                let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                let chromium_sibling = automation
                    .chromium_sibling
                    .expect("Chromium sibling was created");
                let firefox_sibling = automation
                    .firefox_sibling
                    .expect("Firefox sibling was created");
                if automation.navigation_settled.contains(&chromium_tab)
                    && automation.navigation_settled.contains(&firefox_tab)
                    && automation.navigation_settled.contains(&chromium_sibling)
                    && automation.navigation_settled.contains(&firefox_sibling)
                {
                    let window_id = self.tab(chromium_tab)?.window_id;
                    self.select(window_id, chromium_tab)?;
                    self.automation
                        .as_mut()
                        .expect("automation is active")
                        .phase = AutomationPhase::CaptureChromium {
                        deadline: Instant::now() + Duration::from_millis(400),
                        attempts: 0,
                    };
                }
            }
            AutomationPhase::CaptureChromium { deadline, attempts }
                if Instant::now() >= deadline =>
            {
                match self.capture_tab(automation.chromium_tab, true) {
                    Ok(variants) if variants >= MIN_RENDER_VARIANTS => {
                        let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                        let window_id = self.tab(firefox_tab)?.window_id;
                        self.select(window_id, firefox_tab)?;
                        let automation = self.automation.as_mut().expect("automation is active");
                        automation.chromium_variants = Some(variants);
                        automation.phase = AutomationPhase::CaptureFirefox {
                            deadline: Instant::now() + Duration::from_millis(400),
                            attempts: 0,
                        };
                    }
                    result if attempts < 4 => {
                        eprintln!("deb-smoke: Chromium capture retry: {result:?}");
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::CaptureChromium {
                            deadline: Instant::now() + Duration::from_millis(200),
                            attempts: attempts + 1,
                        };
                    }
                    result => {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            format!("Chromium tab did not render: {result:?}"),
                        )));
                    }
                }
            }
            AutomationPhase::CaptureFirefox { deadline, attempts }
                if Instant::now() >= deadline =>
            {
                match self.capture_tab(
                    automation.firefox_tab.expect("Firefox tab was created"),
                    true,
                ) {
                    Ok(variants) if variants >= MIN_RENDER_VARIANTS => {
                        let chromium_process = self
                            .chromium
                            .as_ref()
                            .ok_or("automation lost the Chromium process")?
                            .process
                            .process_id();
                        let firefox_process = self
                            .firefox
                            .as_ref()
                            .ok_or("automation lost the Firefox process")?
                            .process
                            .process_id();
                        let chromium_tab = automation.chromium_tab;
                        self.navigate_tab_to(chromium_tab, "chrome://crash")?;
                        eprintln!("deb-smoke: requested a Chromium renderer crash");
                        let automation = self.automation.as_mut().expect("automation is active");
                        automation.firefox_variants = Some(variants);
                        automation.chromium_process = Some(chromium_process);
                        automation.firefox_process = Some(firefox_process);
                        automation.phase = AutomationPhase::WaitChromiumCrash;
                    }
                    result if attempts < 4 => {
                        eprintln!("deb-smoke: Firefox capture retry: {result:?}");
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::CaptureFirefox {
                            deadline: Instant::now() + Duration::from_millis(200),
                            attempts: attempts + 1,
                        };
                    }
                    result => {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            format!("Firefox tab did not render: {result:?}"),
                        )));
                    }
                }
            }
            AutomationPhase::WaitChromiumCrash => {
                let chromium_tab = automation.chromium_tab;
                let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                if automation.crashed_tabs.contains(&chromium_tab) {
                    let chromium_sibling = automation
                        .chromium_sibling
                        .expect("Chromium sibling was created");
                    let firefox_sibling = automation
                        .firefox_sibling
                        .expect("Firefox sibling was created");
                    if self.tab(chromium_sibling)?.crashed
                        || self.tab(firefox_tab)?.crashed
                        || self.tab(firefox_sibling)?.crashed
                    {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "a Chromium renderer crash propagated to a sibling tab".to_owned(),
                        )));
                    }
                    if self
                        .chromium
                        .as_ref()
                        .map(|engine| engine.process.process_id())
                        != automation.chromium_process
                    {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "a renderer crash replaced the Chromium helper process".to_owned(),
                        )));
                    }
                    let target_url = automation.target_url.clone();
                    self.navigate_tab_to(chromium_tab, &target_url)?;
                    self.automation
                        .as_mut()
                        .expect("automation is active")
                        .phase = AutomationPhase::WaitChromiumRecovery;
                }
            }
            AutomationPhase::WaitChromiumRecovery => {
                let chromium_tab = automation.chromium_tab;
                if self.tab_ready(chromium_tab) {
                    let chromium_sibling = automation
                        .chromium_sibling
                        .expect("Chromium sibling was created");
                    if !self.tab_ready(chromium_sibling) {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "the sibling Chromium tab did not survive renderer recovery".to_owned(),
                        )));
                    }
                    automation
                        .firefox_process
                        .ok_or("automation did not record the Firefox process")?;
                    let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                    self.navigate_tab_to(firefox_tab, "about:crashcontent")?;
                    eprintln!("deb-smoke: requested a Gecko content-process crash");
                    self.automation
                        .as_mut()
                        .expect("automation is active")
                        .phase = AutomationPhase::WaitFirefoxCrash;
                }
            }
            AutomationPhase::WaitFirefoxCrash => {
                let chromium_tab = automation.chromium_tab;
                let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                if automation.crashed_tabs.contains(&firefox_tab) {
                    let chromium_sibling = automation
                        .chromium_sibling
                        .expect("Chromium sibling was created");
                    let firefox_sibling = automation
                        .firefox_sibling
                        .expect("Firefox sibling was created");
                    if self.tab(chromium_tab)?.crashed
                        || self.tab(chromium_sibling)?.crashed
                        || self.tab(firefox_sibling)?.crashed
                    {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "a Gecko content crash propagated to a sibling tab".to_owned(),
                        )));
                    }
                    if self
                        .firefox
                        .as_ref()
                        .map(|engine| engine.process.process_id())
                        != automation.firefox_process
                    {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "a content crash replaced the Firefox helper process".to_owned(),
                        )));
                    }
                    let target_url = automation.target_url.clone();
                    self.navigate_tab_to(firefox_tab, &target_url)?;
                    self.automation
                        .as_mut()
                        .expect("automation is active")
                        .phase = AutomationPhase::WaitFirefoxRecovery;
                }
            }
            AutomationPhase::WaitFirefoxRecovery => {
                let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                if self.tab_ready(firefox_tab) {
                    let firefox_sibling = automation
                        .firefox_sibling
                        .expect("Firefox sibling was created");
                    if !self.tab_ready(firefox_sibling) {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "the sibling Firefox tab did not survive content-process recovery"
                                .to_owned(),
                        )));
                    }
                    let source_window = self.tab(automation.chromium_tab)?.window_id;
                    if let Some(target_window) = self
                        .windows
                        .keys()
                        .copied()
                        .find(|window_id| *window_id != source_window)
                    {
                        let chromium_sibling = automation
                            .chromium_sibling
                            .expect("Chromium sibling was created");
                        self.select(source_window, chromium_sibling)?;
                        let automation = self.automation.as_mut().expect("automation is active");
                        automation.second_window = Some(target_window);
                        automation.phase = AutomationPhase::PrepareChromiumMove {
                            deadline: Instant::now() + Duration::from_millis(400),
                            attempts: 0,
                        };
                    }
                }
            }
            AutomationPhase::PrepareChromiumMove { deadline, attempts }
                if Instant::now() >= deadline =>
            {
                let chromium_sibling = automation
                    .chromium_sibling
                    .expect("Chromium sibling was created");
                match self.capture_tab(chromium_sibling, false) {
                    Ok(variants) if variants >= MIN_RENDER_VARIANTS => {
                        let source_window = self.tab(chromium_sibling)?.window_id;
                        let target_window = automation
                            .second_window
                            .ok_or("automation lost the second window")?;
                        let firefox_sibling = automation
                            .firefox_sibling
                            .expect("Firefox sibling was created");
                        self.move_tab(chromium_sibling, target_window)?;
                        self.select(source_window, firefox_sibling)?;
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::PrepareFirefoxMove {
                            deadline: Instant::now() + Duration::from_millis(400),
                            attempts: 0,
                        };
                    }
                    result if attempts < 8 => {
                        eprintln!("deb-smoke: source Chromium capture retry: {result:?}");
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::PrepareChromiumMove {
                            deadline: Instant::now() + Duration::from_millis(250),
                            attempts: attempts + 1,
                        };
                    }
                    result => {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            format!("source Chromium tab did not render before moving: {result:?}"),
                        )));
                    }
                }
            }
            AutomationPhase::PrepareFirefoxMove { deadline, attempts }
                if Instant::now() >= deadline =>
            {
                let firefox_sibling = automation
                    .firefox_sibling
                    .expect("Firefox sibling was created");
                match self.capture_tab(firefox_sibling, false) {
                    Ok(variants) if variants >= MIN_RENDER_VARIANTS => {
                        let target_window = automation
                            .second_window
                            .ok_or("automation lost the second window")?;
                        self.move_tab(firefox_sibling, target_window)?;
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::WaitWindowMoves;
                        eprintln!(
                            "deb-smoke: moved live Chromium and Gecko tabs into the second Qt window"
                        );
                    }
                    result if attempts < 8 => {
                        eprintln!("deb-smoke: source Gecko capture retry: {result:?}");
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::PrepareFirefoxMove {
                            deadline: Instant::now() + Duration::from_millis(250),
                            attempts: attempts + 1,
                        };
                    }
                    result => {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            format!("source Gecko tab did not render before moving: {result:?}"),
                        )));
                    }
                }
            }
            AutomationPhase::WaitWindowMoves => {
                if self
                    .chromium
                    .as_ref()
                    .map(|engine| engine.process.process_id())
                    != automation.chromium_process
                    || self
                        .firefox
                        .as_ref()
                        .map(|engine| engine.process.process_id())
                        != automation.firefox_process
                {
                    return Ok(Some((
                        "FAIL".to_owned(),
                        "moving a tab replaced a shared profile helper".to_owned(),
                    )));
                }
                let target_window = automation
                    .second_window
                    .ok_or("automation did not record the second window")?;
                let target_parent = self.window(target_window)?.parent;
                let chromium_sibling = automation
                    .chromium_sibling
                    .expect("Chromium sibling was created");
                let firefox_sibling = automation
                    .firefox_sibling
                    .expect("Firefox sibling was created");
                let moved_tabs = [chromium_sibling, firefox_sibling];
                let mut correctly_reparented = true;
                for tab_id in moved_tabs {
                    let tab = self.tab(tab_id)?;
                    let browser_id = tab.browser_id.ok_or("moved tab has no browser")?;
                    let container = tab.container.ok_or("moved tab has no native container")?;
                    let surface = self
                        .engine(tab.engine.backend())
                        .and_then(|engine| engine.surfaces.get(&browser_id))
                        .copied()
                        .ok_or("moved tab has no native surface")?;
                    correctly_reparented &= tab.window_id == target_window
                        && self.connection.query_tree(container)?.reply()?.parent == target_parent
                        && self.connection.query_tree(surface)?.reply()?.parent == container;
                }
                if correctly_reparented
                    && self.tab_ready(chromium_sibling)
                    && self.tab_ready(firefox_sibling)
                {
                    self.select(target_window, chromium_sibling)?;
                    self.automation
                        .as_mut()
                        .expect("automation is active")
                        .phase = AutomationPhase::CaptureMovedChromium {
                        deadline: Instant::now() + Duration::from_millis(400),
                        attempts: 0,
                    };
                }
            }
            AutomationPhase::CaptureMovedChromium { deadline, attempts }
                if Instant::now() >= deadline =>
            {
                let chromium_sibling = automation
                    .chromium_sibling
                    .expect("Chromium sibling was created");
                match self.capture_tab(chromium_sibling, false) {
                    Ok(variants) if variants >= MIN_RENDER_VARIANTS => {
                        let target_window = automation
                            .second_window
                            .ok_or("automation lost the second window")?;
                        let firefox_sibling = automation
                            .firefox_sibling
                            .expect("Firefox sibling was created");
                        self.select(target_window, firefox_sibling)?;
                        let automation = self.automation.as_mut().expect("automation is active");
                        automation.moved_chromium_variants = Some(variants);
                        automation.phase = AutomationPhase::CaptureMovedFirefox {
                            deadline: Instant::now() + Duration::from_millis(400),
                            attempts: 0,
                        };
                    }
                    result if attempts < 4 => {
                        eprintln!("deb-smoke: moved Chromium capture retry: {result:?}");
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::CaptureMovedChromium {
                            deadline: Instant::now() + Duration::from_millis(200),
                            attempts: attempts + 1,
                        };
                    }
                    result => {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            format!("moved Chromium tab did not render: {result:?}"),
                        )));
                    }
                }
            }
            AutomationPhase::CaptureMovedFirefox { deadline, attempts }
                if Instant::now() >= deadline =>
            {
                let firefox_sibling = automation
                    .firefox_sibling
                    .expect("Firefox sibling was created");
                match self.capture_tab(firefox_sibling, false) {
                    Ok(variants) if variants >= MIN_RENDER_VARIANTS => {
                        let chromium_tab = automation.chromium_tab;
                        self.switch_engine(chromium_tab, TabEngine::Firefox)?;
                        let automation = self.automation.as_mut().expect("automation is active");
                        automation.moved_firefox_variants = Some(variants);
                        automation.phase = AutomationPhase::WaitEngineSwitch;
                    }
                    result if attempts < 4 => {
                        eprintln!("deb-smoke: moved Gecko capture retry: {result:?}");
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::CaptureMovedFirefox {
                            deadline: Instant::now() + Duration::from_millis(200),
                            attempts: attempts + 1,
                        };
                    }
                    result => {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            format!("moved Gecko tab did not render: {result:?}"),
                        )));
                    }
                }
            }
            AutomationPhase::WaitEngineSwitch => {
                let switched_tab = automation.chromium_tab;
                if self.tab_ready(switched_tab) {
                    let firefox = self
                        .firefox
                        .as_ref()
                        .ok_or("automation lost the Firefox process during engine switch")?;
                    if Some(firefox.process.process_id()) != automation.firefox_process {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "engine switch replaced the shared Firefox profile process".to_owned(),
                        )));
                    }
                    let firefox_browser_ids = self
                        .tabs
                        .iter()
                        .filter(|tab| tab.engine == TabEngine::Firefox)
                        .filter_map(|tab| tab.browser_id)
                        .collect::<HashSet<_>>();
                    if firefox_browser_ids.len() != 3 {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "engine switch did not create three Gecko browsers in one helper"
                                .to_owned(),
                        )));
                    }
                    if !firefox_browser_ids
                        .iter()
                        .all(|browser_id| firefox.surfaces.contains_key(browser_id))
                    {
                        return Ok(None);
                    }
                    let window_id = self.tab(switched_tab)?.window_id;
                    self.select(window_id, switched_tab)?;
                    self.automation
                        .as_mut()
                        .expect("automation is active")
                        .phase = AutomationPhase::CaptureSwitched {
                        deadline: Instant::now() + Duration::from_millis(400),
                        attempts: 0,
                    };
                }
            }
            AutomationPhase::CaptureSwitched { deadline, attempts }
                if Instant::now() >= deadline =>
            {
                match self.capture_tab(automation.chromium_tab, false) {
                    Ok(switched_variants) if switched_variants >= MIN_RENDER_VARIANTS => {
                        return Ok(Some((
                            "PASS".to_owned(),
                            format!(
                                "two Qt windows shared one helper per profile-engine; live Chromium and Gecko tabs retained their browser instances while moving between X11 hosts; renderer/content crashes stayed isolated and recovered; Chromium->Firefox switching created a third Gecko browser (Chromium {}, Gecko {}, moved Chromium {}, moved Gecko {}, switched Gecko {switched_variants} sampled colors)",
                                automation.chromium_variants.unwrap_or_default(),
                                automation.firefox_variants.unwrap_or_default(),
                                automation.moved_chromium_variants.unwrap_or_default(),
                                automation.moved_firefox_variants.unwrap_or_default(),
                            ),
                        )));
                    }
                    result if attempts < 4 => {
                        eprintln!("deb-smoke: switched Gecko capture retry: {result:?}");
                        self.automation
                            .as_mut()
                            .expect("automation is active")
                            .phase = AutomationPhase::CaptureSwitched {
                            deadline: Instant::now() + Duration::from_millis(200),
                            attempts: attempts + 1,
                        };
                    }
                    result => {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            format!("engine-switched Gecko tab did not render: {result:?}"),
                        )));
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    fn tab_ready(&self, tab_id: u64) -> bool {
        self.tab(tab_id)
            .is_ok_and(|tab| tab.browser_id.is_some() && !tab.loading && !tab.crashed)
    }

    fn navigate_tab_to(&mut self, tab_id: u64, url: &str) -> TabResult<()> {
        let (backend, browser_id) = {
            let tab = self.tab_mut(tab_id)?;
            tab.url = url.to_owned();
            tab.loading = true;
            tab.crashed = false;
            tab.status = "Navigating…".to_owned();
            (
                tab.engine.backend(),
                tab.browser_id.ok_or("automation tab has no browser")?,
            )
        };
        self.engine_mut(backend)
            .as_mut()
            .ok_or("automation engine is unavailable")?
            .process
            .navigate_browser(browser_id, url)?;
        Ok(())
    }

    fn capture_tab(&mut self, tab_id: u64, require_orientation: bool) -> TabResult<usize> {
        let window_id = self.tab(tab_id)?.window_id;
        if self.window(window_id)?.active_tab != tab_id {
            self.select(window_id, tab_id)?;
        } else {
            self.refresh_visibility()?;
        }
        let active = self.tab(tab_id)?;
        let browser_id = active.browser_id.ok_or("active tab has no browser")?;
        let window = self
            .engine(active.engine.backend())
            .and_then(|engine| engine.surfaces.get(&browser_id))
            .copied()
            .ok_or("active tab has no native surface")?;
        let (variants, has_qt_overlay, has_orientation_marker) =
            sampled_pixel_variants(&self.connection, window)?;
        if !has_qt_overlay {
            return Err("Qt scene overlay was occluded by the browser surface".into());
        }
        if require_orientation && !has_orientation_marker {
            return Err("browser texture orientation marker was missing from its top edge".into());
        }
        eprintln!(
            "deb-smoke: sampled tab {tab_id} browser {browser_id} ({:?}, {:?}): {variants} colors",
            active.url, active.title
        );
        Ok(variants)
    }

    fn apply_cookie_actions(&mut self, actions: Vec<CookieAction>) -> TabResult<()> {
        for action in actions {
            let Some(engine) = self.engine_mut(action.target) else {
                continue;
            };
            let Some(browser_id) = engine.cookie_browser() else {
                continue;
            };
            match action.mutation {
                CookieMutation::Set(cookie) => {
                    engine.process.set_browser_cookie(browser_id, cookie)?
                }
                CookieMutation::Delete(cookie) => {
                    engine.process.delete_browser_cookie(browser_id, cookie)?
                }
            }
        }
        Ok(())
    }

    fn refresh_visibility(&mut self) -> TabResult<()> {
        let browser_states = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let window = self.windows.get(&tab.window_id)?;
                tab.browser_id.map(|browser_id| {
                    let visible = window.visible && window.active_tab == tab.id;
                    (
                        tab.engine.backend(),
                        browser_id,
                        visible,
                        visible && window.focused,
                        window.parent,
                        window.bounds,
                        tab.container,
                    )
                })
            })
            .collect::<Vec<_>>();
        for (backend, browser_id, visible, focused, parent, bounds, container) in browser_states {
            let surface = self
                .engine(backend)
                .and_then(|engine| engine.surfaces.get(&browser_id))
                .copied();
            let mut revealed = false;
            if let Some(container) = container {
                if self.connection.query_tree(container)?.reply()?.parent != parent {
                    self.connection.reparent_window(container, parent, 0, 0)?;
                }
                if visible {
                    configure_native_window(&self.connection, container, bounds)?;
                    revealed = self
                        .connection
                        .get_window_attributes(container)?
                        .reply()?
                        .map_state
                        == MapState::UNMAPPED;
                    if revealed {
                        self.connection.map_window(container)?;
                    }
                    self.connection.configure_window(
                        container,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    )?;
                } else {
                    let _ = self.connection.unmap_window(container);
                }
            }
            if let Some(surface) = surface {
                if self.connection.query_tree(surface)?.reply()?.parent
                    != container.ok_or("browser surface has no tab container")?
                {
                    return Err("browser surface escaped its tab container".into());
                }
                if visible {
                    configure_native_window(&self.connection, surface, embedded_bounds(bounds))?;
                    if self
                        .connection
                        .get_window_attributes(surface)?
                        .reply()?
                        .map_state
                        == MapState::UNMAPPED
                    {
                        self.connection.map_window(surface)?;
                    }
                    self.connection.configure_window(
                        surface,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    )?;
                    if focused {
                        self.connection.set_input_focus(
                            InputFocus::PARENT,
                            surface,
                            x11rb::CURRENT_TIME,
                        )?;
                    }
                    if revealed && backend == CefBackend::Chromium {
                        self.connection.clear_area(true, surface, 0, 0, 0, 0)?;
                    }
                }
                self.connection.flush()?;
            }
            if let Some(engine) = self.engine_mut(backend) {
                engine.process.resize_browser(browser_id, bounds)?;
                engine.process.set_browser_visible(browser_id, visible)?;
                engine.process.focus_browser(browser_id, focused)?;
            }
        }
        self.connection.flush()?;
        Ok(())
    }

    fn handle_x11_events(&mut self) -> TabResult<()> {
        while let Some(event) = self.connection.poll_for_event()? {
            if let Event::ButtonPress(event) = event {
                let target = self.tabs.iter().find_map(|tab| {
                    let browser_id = tab.browser_id?;
                    self.engine(tab.engine.backend())
                        .and_then(|engine| engine.surfaces.get(&browser_id))
                        .is_some_and(|window| *window == event.event)
                        .then_some((tab.engine.backend(), browser_id))
                });
                if let Some((backend, browser_id)) = target
                    && let Some(engine) = self.engine_mut(backend)
                {
                    engine.process.focus_browser(browser_id, true)?;
                }
            }
        }
        Ok(())
    }

    fn publish(&mut self) {
        let mut windows = self.windows.values().collect::<Vec<_>>();
        windows.sort_by_key(|window| window.id);
        let snapshot = ProfileSnapshot {
            windows: windows
                .into_iter()
                .map(|window| WindowSnapshot {
                    id: window.id.to_string(),
                    label: &window.label,
                    active_tab_id: window.active_tab.to_string(),
                    tabs: self
                        .tabs
                        .iter()
                        .filter(|tab| tab.window_id == window.id)
                        .map(Tab::snapshot)
                        .collect(),
                })
                .collect(),
        };
        let json =
            serde_json::to_string(&snapshot).unwrap_or_else(|_| "{\"windows\":[]}".to_owned());
        let invoker = self
            .invoker
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        invoke_method!(&*invoker, "update_window_state", json);
        self.dirty = false;
    }

    fn engine(&self, backend: CefBackend) -> Option<&EngineRuntime> {
        match backend {
            CefBackend::Chromium => self.chromium.as_ref(),
            CefBackend::Firefox => self.firefox.as_ref(),
        }
    }

    fn engine_mut(&mut self, backend: CefBackend) -> &mut Option<EngineRuntime> {
        match backend {
            CefBackend::Chromium => &mut self.chromium,
            CefBackend::Firefox => &mut self.firefox,
        }
    }

    fn window(&self, id: u64) -> TabResult<&BrowserWindow> {
        self.windows
            .get(&id)
            .ok_or_else(|| format!("window {id} does not exist").into())
    }

    fn window_mut(&mut self, id: u64) -> TabResult<&mut BrowserWindow> {
        self.windows
            .get_mut(&id)
            .ok_or_else(|| format!("window {id} does not exist").into())
    }

    fn tab(&self, tab_id: u64) -> TabResult<&Tab> {
        self.tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| format!("tab {tab_id} does not exist").into())
    }

    fn tab_mut(&mut self, tab_id: u64) -> TabResult<&mut Tab> {
        self.tabs
            .iter_mut()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| format!("tab {tab_id} does not exist").into())
    }

    fn tab_for_browser_mut(&mut self, backend: CefBackend, browser_id: u64) -> Option<&mut Tab> {
        self.tabs
            .iter_mut()
            .find(|tab| tab.engine.backend() == backend && tab.browser_id == Some(browser_id))
    }
}

fn automation_cookie() -> wire::Cookie {
    wire::Cookie {
        key: Some(wire::CookieKey {
            name: "deb-smoke-sync".to_owned(),
            domain: ".deb.invalid".to_owned(),
            path: "/".to_owned(),
            partition_key: None,
        }),
        value: format!("pid-{}", std::process::id()),
        secure: false,
        http_only: false,
        creation: 0,
        last_access: 0,
        expires: None,
        same_site: wire::CookieSameSite::Lax as i32,
        priority: wire::CookiePriority::Medium as i32,
        last_update: 0,
    }
}

fn is_automation_cookie(cookie: &wire::Cookie) -> bool {
    cookie
        .key
        .as_ref()
        .is_some_and(|key| key.name == "deb-smoke-sync" && key.domain == ".deb.invalid")
}

#[cfg(test)]
mod tests {
    use super::{CookieMutation, TabEngine, reconcile_mutation};
    use crate::cookie_store::{CanonicalCookie, CookieIdentity};
    use shell_protocol::wire;
    use std::collections::HashMap;

    fn cookie(value: &str) -> wire::Cookie {
        wire::Cookie {
            key: Some(wire::CookieKey {
                name: "session".to_owned(),
                domain: ".example.test".to_owned(),
                path: "/".to_owned(),
                partition_key: None,
            }),
            value: value.to_owned(),
            secure: true,
            http_only: true,
            creation: 1,
            last_access: 2,
            expires: None,
            same_site: wire::CookieSameSite::Lax as i32,
            priority: wire::CookiePriority::Medium as i32,
            last_update: 3,
        }
    }

    #[test]
    fn parses_only_supported_tab_engines() {
        assert_eq!(TabEngine::parse("chromium"), Some(TabEngine::Chromium));
        assert_eq!(TabEngine::parse("firefox"), Some(TabEngine::Firefox));
        assert_eq!(TabEngine::parse("webkit"), None);
    }

    #[test]
    fn canonical_cookie_reconciliation_sets_and_deletes_engine_state() {
        let original = cookie("old");
        let identity = CookieIdentity::from_cookie(&original).unwrap();
        let snapshot = HashMap::from([(identity, original.clone())]);

        let set = reconcile_mutation(
            &snapshot,
            CanonicalCookie {
                cookie: cookie("new"),
                deleted: false,
                modified_at: 4,
            },
        )
        .unwrap();
        assert!(matches!(set, Some(CookieMutation::Set(_))));

        let delete = reconcile_mutation(
            &snapshot,
            CanonicalCookie {
                cookie: original,
                deleted: true,
                modified_at: 5,
            },
        )
        .unwrap();
        assert!(matches!(delete, Some(CookieMutation::Delete(_))));
    }
}
