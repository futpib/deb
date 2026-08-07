use crate::{
    cookie_store::{CanonicalCookie, CookieIdentity, CookieStore, cookie_contents_equal},
    native::{
        CefBackend, CefInstance, NativeRect, ProtocolNotice, RoutedNotice, clear_qt_surface,
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
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
use x11rb::{protocol::xproto::Window, rust_connection::RustConnection};

type TabResult<T> = Result<T, Box<dyn Error + Send + Sync>>;
const MIN_RENDER_VARIANTS: usize = 8;
static NEXT_BROWSER_ID: AtomicU64 = AtomicU64::new(1);

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
    let thread = std::thread::spawn({
        let invoker = Arc::clone(&invoker);
        move || {
            let result = Runtime::new(profile_id, directories, Arc::clone(&invoker), receiver)
                .and_then(Runtime::run);
            if let Err(error) = result {
                eprintln!("deb: tab controller failed: {error}");
                if std::env::var("DEB_AUTOMATED_SMOKE_TEST").as_deref() == Ok("1") {
                    let invoker = invoker.lock().unwrap_or_else(|error| error.into_inner());
                    invoke_method!(
                        &*invoker,
                        "finish_smoke_test",
                        "FAIL".to_owned(),
                        error.to_string()
                    );
                }
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
struct ProfileSnapshot<'a> {
    windows: Vec<WindowSnapshot<'a>>,
}

struct Tab {
    id: u64,
    window_id: u64,
    engine: TabEngine,
    browser_id: Option<u64>,
    url: String,
    title: String,
    status: String,
    loading: bool,
    crashed: bool,
    frame_ready: bool,
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

struct BrowserWindow {
    id: u64,
    parent: Window,
    bounds: NativeRect,
    label: String,
    active_tab: u64,
    displayed_tab: Option<u64>,
    visible: bool,
    focused: bool,
}

struct EngineRuntime {
    process: CefInstance,
    browsers: HashSet<u64>,
}

impl EngineRuntime {
    fn initial(process: CefInstance) -> Self {
        Self {
            browsers: HashSet::from([process.initial_browser_id()]),
            process,
        }
    }

    fn cookie_browser(&self) -> Option<u64> {
        self.browsers.iter().copied().next()
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

enum AutomationPhase {
    WaitChromium,
    WaitFirefox,
    WaitChromiumFrame,
    CaptureChromium(Instant),
    WaitFirefoxFrame,
    CaptureFirefox(Instant),
}

struct Automation {
    started: Instant,
    chromium_tab: u64,
    firefox_tab: Option<u64>,
    phase: AutomationPhase,
    chromium_variants: usize,
}

impl Automation {
    fn from_environment() -> Option<Self> {
        (std::env::var("DEB_AUTOMATED_SMOKE_TEST").as_deref() == Ok("1")).then(|| Self {
            started: Instant::now(),
            chromium_tab: 1,
            firefox_tab: None,
            phase: AutomationPhase::WaitChromium,
            chromium_variants: 0,
        })
    }
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
    chromium: Option<EngineRuntime>,
    firefox: Option<EngineRuntime>,
    cookie_sync: CookieSync,
    automation: Option<Automation>,
    dirty: bool,
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
            chromium: None,
            firefox: None,
            cookie_sync,
            automation: Automation::from_environment(),
            dirty: false,
        })
    }

    fn run(mut self) -> TabResult<()> {
        loop {
            match self.receiver.recv_timeout(Duration::from_millis(16)) {
                Ok(command) => {
                    if matches!(command, TabCommand::Stop) {
                        break;
                    }
                    self.command(command)?;
                }
                Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            self.poll_engine(CefBackend::Chromium)?;
            self.poll_engine(CefBackend::Firefox)?;
            if let Some((outcome, details)) = self.advance_automation()? {
                self.shutdown_engines();
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
        self.shutdown_engines();
        Ok(())
    }

    fn command(&mut self, command: TabCommand) -> TabResult<()> {
        match command {
            TabCommand::AddWindow {
                id,
                parent,
                bounds,
                label,
                initial_url,
            } => self.add_window(id, parent, bounds, label, initial_url),
            TabCommand::RemoveWindow(id) => self.remove_window(id),
            TabCommand::Layout(id, bounds) => self.layout(id, bounds),
            TabCommand::SetWindowState {
                id,
                visible,
                focused,
            } => self.set_window_state(id, visible, focused),
            TabCommand::Navigate(id, url) => self.navigate(id, &url),
            TabCommand::Reload(id) => self.reload(id),
            TabCommand::NewTab(id, engine) => self.new_tab(id, engine),
            TabCommand::Select(id, tab) => self.select(id, tab),
            TabCommand::Close(tab) => self.close_tab(tab),
            TabCommand::SwitchEngine(tab, engine) => self.switch_engine(tab, engine),
            TabCommand::Move(tab, window) => self.move_tab(tab, window),
            TabCommand::MouseMove {
                window_id,
                x,
                y,
                modifiers,
                leaving,
            } => self.with_active_process(window_id, |process, browser| {
                process.send_mouse_move(browser, x, y, modifiers, leaving)
            }),
            TabCommand::MouseClick {
                window_id,
                x,
                y,
                modifiers,
                button,
                mouse_up,
                click_count,
            } => self.with_active_process(window_id, |process, browser| {
                process.send_mouse_click(browser, x, y, modifiers, button, mouse_up, click_count)
            }),
            TabCommand::MouseWheel {
                window_id,
                x,
                y,
                modifiers,
                delta_x,
                delta_y,
            } => self.with_active_process(window_id, |process, browser| {
                process.send_mouse_wheel(browser, x, y, modifiers, delta_x, delta_y)
            }),
            TabCommand::KeyEvent { window_id, event } => self
                .with_active_process(window_id, |process, browser| {
                    process.send_key_event(browser, event)
                }),
            TabCommand::Stop => Ok(()),
        }
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
        self.windows.insert(
            id,
            BrowserWindow {
                id,
                parent,
                bounds,
                label,
                active_tab: 0,
                displayed_tab: None,
                visible: true,
                focused: false,
            },
        );
        let tab_id = self.add_tab(id, TabEngine::Chromium, initial_url);
        self.window_mut(id)?.active_tab = tab_id;
        self.attach_tab(tab_id)?;
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn remove_window(&mut self, id: u64) -> TabResult<()> {
        if self.windows.remove(&id).is_none() {
            return Ok(());
        }
        let tabs = self
            .tabs
            .iter()
            .filter(|tab| tab.window_id == id)
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        for tab in tabs {
            self.close_tab_internal(tab, false)?;
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
            url,
            title: "New tab".to_owned(),
            status: "Starting engine…".to_owned(),
            loading: true,
            crashed: false,
            frame_ready: false,
        });
        id
    }

    fn new_tab(&mut self, window_id: u64, engine: TabEngine) -> TabResult<()> {
        self.window(window_id)?;
        let tab = self.add_tab(window_id, engine, "deb://new-tab/".to_owned());
        self.window_mut(window_id)?.active_tab = tab;
        self.attach_tab(tab)?;
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn attach_tab(&mut self, tab_id: u64) -> TabResult<()> {
        let browser_id = NEXT_BROWSER_ID.fetch_add(1, Ordering::Relaxed);
        if browser_id == 0 {
            return Err("application browser ID space is exhausted".into());
        }
        let (backend, url, window_id) = {
            let tab = self.tab_mut(tab_id)?;
            tab.browser_id = Some(browser_id);
            tab.status = format!("Starting {}…", tab.engine.backend().label());
            tab.loading = true;
            tab.crashed = false;
            (tab.engine.backend(), tab.url.clone(), tab.window_id)
        };
        let (parent, bounds) = {
            let window = self.window(window_id)?;
            (window.parent, window.bounds)
        };
        let profile_id = self.profile_id.clone();
        let directories = backend.directories(&self.directories).clone();
        let surface_id = window_id.to_string();
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
                surface_id,
            ) {
                Ok(process) => {
                    let mut runtime = EngineRuntime::initial(process);
                    self.cookie_sync.begin(backend);
                    runtime.process.read_browser_cookies(browser_id)?;
                    *self.engine_mut(backend) = Some(runtime);
                }
                Err(error) => {
                    let tab = self.tab_mut(tab_id)?;
                    tab.browser_id = None;
                    tab.loading = false;
                    tab.status = format!("{} failed: {error}", backend.label());
                }
            }
        } else {
            let runtime = self.engine_mut(backend).as_mut().expect("engine exists");
            runtime.process.create_browser(
                browser_id,
                parent,
                bounds,
                &url,
                &profile_id,
                &directories,
                surface_id,
            )?;
            runtime.browsers.insert(browser_id);
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
            self.engine_mut(backend)
                .as_mut()
                .ok_or("engine unavailable")?
                .process
                .navigate_browser(browser_id, url)?;
        } else {
            self.attach_tab(tab_id)?;
        }
        self.dirty = true;
        Ok(())
    }

    fn reload(&mut self, window_id: u64) -> TabResult<()> {
        let tab = self.window(window_id)?.active_tab;
        let url = self.tab(tab)?.url.clone();
        self.navigate(window_id, &url)
    }

    fn select(&mut self, window_id: u64, tab_id: u64) -> TabResult<()> {
        if self.tab(tab_id)?.window_id != window_id {
            return Err(format!("tab {tab_id} does not belong to window {window_id}").into());
        }
        self.window_mut(window_id)?.active_tab = tab_id;
        self.tab_mut(tab_id)?.frame_ready = false;
        if self.tab(tab_id)?.browser_id.is_none() {
            self.attach_tab(tab_id)?;
        }
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn close_tab(&mut self, tab_id: u64) -> TabResult<()> {
        self.close_tab_internal(tab_id, true)
    }

    fn close_tab_internal(&mut self, tab_id: u64, replace_last: bool) -> TabResult<()> {
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.id == tab_id)
            .ok_or_else(|| format!("tab {tab_id} does not exist"))?;
        let tab = self.tabs.remove(index);
        if let Some(browser_id) = tab.browser_id
            && let Some(runtime) = self.engine_mut(tab.engine.backend())
        {
            runtime.process.bind_browser_surface(browser_id, None);
            runtime.browsers.remove(&browser_id);
            runtime.process.close_browser(browser_id, true)?;
        }
        let remaining = self
            .tabs
            .iter()
            .filter(|item| item.window_id == tab.window_id)
            .map(|item| item.id)
            .collect::<Vec<_>>();
        if replace_last && self.windows.contains_key(&tab.window_id) && remaining.is_empty() {
            let replacement = self.add_tab(
                tab.window_id,
                TabEngine::Chromium,
                "deb://new-tab/".to_owned(),
            );
            self.window_mut(tab.window_id)?.active_tab = replacement;
            self.attach_tab(replacement)?;
        } else if self
            .windows
            .get(&tab.window_id)
            .is_some_and(|window| window.active_tab == tab_id)
            && let Some(next) = remaining.first()
        {
            self.window_mut(tab.window_id)?.active_tab = *next;
        }
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn switch_engine(&mut self, tab_id: u64, engine: TabEngine) -> TabResult<()> {
        let (old_backend, old_browser) = {
            let tab = self.tab(tab_id)?;
            if tab.engine == engine {
                return Ok(());
            }
            (tab.engine.backend(), tab.browser_id)
        };
        if let Some(browser) = old_browser
            && let Some(runtime) = self.engine_mut(old_backend)
        {
            runtime.process.bind_browser_surface(browser, None);
            runtime.browsers.remove(&browser);
            runtime.process.close_browser(browser, true)?;
        }
        let tab = self.tab_mut(tab_id)?;
        let window_id = tab.window_id;
        tab.engine = engine;
        tab.browser_id = None;
        tab.title = "Loading in new engine…".to_owned();
        tab.status = "Switching engine…".to_owned();
        tab.loading = true;
        tab.crashed = false;
        self.window_mut(window_id)?.displayed_tab = None;
        self.attach_tab(tab_id)?;
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn move_tab(&mut self, tab_id: u64, target_window: u64) -> TabResult<()> {
        self.window(target_window)?;
        let source = self.tab(tab_id)?.window_id;
        if source == target_window {
            return Ok(());
        }
        self.tab_mut(tab_id)?.window_id = target_window;
        self.window_mut(target_window)?.active_tab = tab_id;
        let source_tabs = self
            .tabs
            .iter()
            .filter(|tab| tab.window_id == source)
            .map(|tab| tab.id)
            .collect::<Vec<_>>();
        if source_tabs.is_empty() {
            let replacement =
                self.add_tab(source, TabEngine::Chromium, "deb://new-tab/".to_owned());
            self.window_mut(source)?.active_tab = replacement;
            self.attach_tab(replacement)?;
        } else if self.window(source)?.active_tab == tab_id {
            self.window_mut(source)?.active_tab = source_tabs[0];
        }
        self.refresh_visibility()?;
        self.dirty = true;
        Ok(())
    }

    fn layout(&mut self, id: u64, bounds: NativeRect) -> TabResult<()> {
        self.window_mut(id)?.bounds = bounds;
        self.refresh_visibility()
    }

    fn set_window_state(&mut self, id: u64, visible: bool, focused: bool) -> TabResult<()> {
        let window = self.window_mut(id)?;
        window.visible = visible;
        window.focused = focused;
        self.refresh_visibility()
    }

    fn with_active_process<F>(&mut self, window_id: u64, operation: F) -> TabResult<()>
    where
        F: FnOnce(&mut CefInstance, u64) -> TabResult<()>,
    {
        let tab_id = self.window(window_id)?.active_tab;
        let tab = self.tab(tab_id)?;
        let backend = tab.engine.backend();
        let browser = tab.browser_id.ok_or("active tab has no browser")?;
        operation(
            &mut self
                .engine_mut(backend)
                .as_mut()
                .ok_or("engine unavailable")?
                .process,
            browser,
        )
    }

    fn refresh_visibility(&mut self) -> TabResult<()> {
        for window in self.windows.values_mut() {
            if window.displayed_tab != Some(window.active_tab) {
                let surface = window.id.to_string();
                clear_qt_surface(&surface, wire::SurfaceLayer::View);
                clear_qt_surface(&surface, wire::SurfaceLayer::Popup);
                window.displayed_tab = Some(window.active_tab);
            }
        }
        let states = self
            .tabs
            .iter()
            .filter_map(|tab| {
                let window = self.windows.get(&tab.window_id)?;
                let browser = tab.browser_id?;
                let visible = window.visible && window.active_tab == tab.id;
                Some((
                    tab.engine.backend(),
                    browser,
                    visible,
                    visible && window.focused,
                    window.bounds,
                    visible.then(|| tab.window_id.to_string()),
                ))
            })
            .collect::<Vec<_>>();
        for (backend, browser, visible, focused, bounds, surface) in states {
            if let Some(runtime) = self.engine_mut(backend) {
                runtime.process.bind_browser_surface(browser, surface);
                runtime.process.resize_browser(browser, bounds)?;
                runtime.process.set_browser_visible(browser, visible)?;
                runtime.process.focus_browser(browser, focused)?;
            }
        }
        Ok(())
    }

    fn poll_engine(&mut self, backend: CefBackend) -> TabResult<()> {
        let Some(runtime) = self.engine_mut(backend) else {
            return Ok(());
        };
        let notices = runtime.process.drain_routed_notices();
        let exited = runtime.process.exited()?;
        let protocol_closed = runtime.process.protocol_closed();
        let protocol_error = notices.iter().find_map(|notice| match &notice.value {
            ProtocolNotice::ProtocolFailed(error) if notice.browser_id == 0 => Some(error.clone()),
            _ => None,
        });
        let actions = self.cookie_sync.observe(backend, &notices)?;
        for notice in notices {
            self.handle_notice(backend, notice)?;
        }
        self.apply_cookie_actions(actions)?;
        if let Some(status) = exited {
            self.fail_engine(backend, format!("helper exited: {status}"));
        } else if protocol_closed {
            self.fail_engine(
                backend,
                protocol_error.unwrap_or_else(|| "helper protocol closed".to_owned()),
            );
        }
        Ok(())
    }

    fn handle_notice(&mut self, backend: CefBackend, notice: RoutedNotice) -> TabResult<()> {
        let Some(tab) = self.tab_for_browser_mut(backend, notice.browser_id) else {
            return Ok(());
        };
        match notice.value {
            ProtocolNotice::CommandFailed(error) | ProtocolNotice::ProtocolFailed(error) => {
                tab.status = error;
            }
            ProtocolNotice::SurfaceReady => {
                tab.status = format!("Live · {}", backend.label());
            }
            ProtocolNotice::FrameReady => tab.frame_ready = true,
            ProtocolNotice::LoadingChanged(loading) => {
                tab.loading = loading;
                if !loading {
                    tab.status = format!("Live · {}", backend.label());
                }
            }
            ProtocolNotice::NavigationCommitted(url) => tab.url = url,
            ProtocolNotice::TitleChanged(title) => tab.title = title,
            ProtocolNotice::LoadFailed(error) => {
                tab.loading = false;
                tab.status = format!("Load failed: {error}");
            }
            ProtocolNotice::Closed => {
                tab.browser_id = None;
                tab.loading = false;
                tab.status = "Browser closed".to_owned();
            }
            ProtocolNotice::Crashed(reason) => {
                tab.crashed = true;
                tab.loading = false;
                tab.status = format!("Renderer crashed: {reason}");
            }
            ProtocolNotice::CookieSnapshotEntry(_)
            | ProtocolNotice::CookieSnapshotComplete
            | ProtocolNotice::CookieChanged(_, _) => {}
        }
        self.dirty = true;
        Ok(())
    }

    fn fail_engine(&mut self, backend: CefBackend, reason: String) {
        let runtime = self.engine_mut(backend).take();
        if let Some(runtime) = runtime {
            runtime.process.shutdown();
        }
        for tab in self
            .tabs
            .iter_mut()
            .filter(|tab| tab.engine.backend() == backend)
        {
            tab.browser_id = None;
            tab.loading = false;
            tab.crashed = true;
            tab.status = reason.clone();
        }
        self.dirty = true;
    }

    fn apply_cookie_actions(&mut self, actions: Vec<CookieAction>) -> TabResult<()> {
        for action in actions {
            let Some(runtime) = self.engine_mut(action.target) else {
                continue;
            };
            let Some(browser) = runtime.cookie_browser() else {
                continue;
            };
            match action.mutation {
                CookieMutation::Set(cookie) => {
                    runtime.process.set_browser_cookie(browser, cookie)?
                }
                CookieMutation::Delete(cookie) => {
                    runtime.process.delete_browser_cookie(browser, cookie)?
                }
            }
        }
        Ok(())
    }

    fn advance_automation(&mut self) -> TabResult<Option<(String, String)>> {
        let Some(automation) = self.automation.as_ref() else {
            return Ok(None);
        };
        if automation.started.elapsed() > Duration::from_secs(180) {
            return Ok(Some((
                "FAIL".to_owned(),
                "DMA-BUF smoke test timed out".to_owned(),
            )));
        }
        match automation.phase {
            AutomationPhase::WaitChromium if self.tab_ready(automation.chromium_tab) => {
                let window = self.tab(automation.chromium_tab)?.window_id;
                let firefox = self.add_tab(
                    window,
                    TabEngine::Firefox,
                    "deb://new-tab/#deb-smoke".to_owned(),
                );
                self.window_mut(window)?.active_tab = firefox;
                self.attach_tab(firefox)?;
                self.refresh_visibility()?;
                let automation = self.automation.as_mut().expect("automation exists");
                automation.firefox_tab = Some(firefox);
                automation.phase = AutomationPhase::WaitFirefox;
            }
            AutomationPhase::WaitFirefox => {
                let firefox = automation.firefox_tab.expect("Firefox tab exists");
                if self.tab_ready(firefox) {
                    let window = self.tab(automation.chromium_tab)?.window_id;
                    self.select(window, automation.chromium_tab)?;
                    self.automation.as_mut().expect("automation exists").phase =
                        AutomationPhase::WaitChromiumFrame;
                }
            }
            AutomationPhase::WaitChromiumFrame
                if self.tab(automation.chromium_tab)?.frame_ready =>
            {
                self.automation.as_mut().expect("automation exists").phase =
                    AutomationPhase::CaptureChromium(Instant::now() + Duration::from_secs(1));
            }
            AutomationPhase::CaptureChromium(deadline) if Instant::now() >= deadline => {
                let variants = self.capture_window_for_tab(automation.chromium_tab)?;
                if variants < MIN_RENDER_VARIANTS {
                    return Ok(Some((
                        "FAIL".to_owned(),
                        format!("Chromium rendered only {variants} colors"),
                    )));
                }
                let firefox = automation.firefox_tab.expect("Firefox tab exists");
                let window = self.tab(firefox)?.window_id;
                self.select(window, firefox)?;
                let automation = self.automation.as_mut().expect("automation exists");
                automation.chromium_variants = variants;
                automation.phase = AutomationPhase::WaitFirefoxFrame;
            }
            AutomationPhase::WaitFirefoxFrame
                if self
                    .tab(automation.firefox_tab.expect("Firefox tab exists"))?
                    .frame_ready =>
            {
                self.automation.as_mut().expect("automation exists").phase =
                    AutomationPhase::CaptureFirefox(Instant::now() + Duration::from_secs(1));
            }
            AutomationPhase::CaptureFirefox(deadline) if Instant::now() >= deadline => {
                let firefox = automation.firefox_tab.expect("Firefox tab exists");
                let variants = self.capture_window_for_tab(firefox)?;
                if variants < MIN_RENDER_VARIANTS {
                    return Ok(Some((
                        "FAIL".to_owned(),
                        format!("Firefox rendered only {variants} colors"),
                    )));
                }
                return Ok(Some((
                    "PASS".to_owned(),
                    format!(
                        "Qt scene composited direct DMA-BUF frames above both engines (Chromium {} colors, Firefox {variants} colors)",
                        automation.chromium_variants,
                    ),
                )));
            }
            _ => {}
        }
        Ok(None)
    }

    fn capture_window_for_tab(&self, tab_id: u64) -> TabResult<usize> {
        let tab = self.tab(tab_id)?;
        let engine = match tab.engine {
            TabEngine::Chromium => "Chromium",
            TabEngine::Firefox => "Firefox",
        };
        let window = self.window(tab.window_id)?;
        let (variants, has_qt_overlay, has_engine_marker) =
            sampled_pixel_variants(&self.connection, window.parent, window.bounds)?;
        if !has_qt_overlay {
            return Err(format!("Qt overlay was not composited above the {engine} texture").into());
        }
        if !has_engine_marker {
            return Err(format!("{engine} DMA-BUF content marker was not rendered").into());
        }
        Ok(variants)
    }

    fn tab_ready(&self, tab_id: u64) -> bool {
        self.tab(tab_id)
            .is_ok_and(|tab| tab.browser_id.is_some() && !tab.loading && !tab.crashed)
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

    fn shutdown_engines(&mut self) {
        if let Some(runtime) = self.chromium.take() {
            runtime.process.shutdown();
        }
        if let Some(runtime) = self.firefox.take() {
            runtime.process.shutdown();
        }
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

    fn tab(&self, id: u64) -> TabResult<&Tab> {
        self.tabs
            .iter()
            .find(|tab| tab.id == id)
            .ok_or_else(|| format!("tab {id} does not exist").into())
    }

    fn tab_mut(&mut self, id: u64) -> TabResult<&mut Tab> {
        self.tabs
            .iter_mut()
            .find(|tab| tab.id == id)
            .ok_or_else(|| format!("tab {id} does not exist").into())
    }

    fn tab_for_browser_mut(&mut self, backend: CefBackend, browser: u64) -> Option<&mut Tab> {
        self.tabs
            .iter_mut()
            .find(|tab| tab.engine.backend() == backend && tab.browser_id == Some(browser))
    }
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
    fn reconciles_a_missing_cookie_from_the_canonical_store() {
        let canonical = CanonicalCookie {
            cookie: cookie("canonical"),
            deleted: false,
            modified_at: 1,
        };
        assert!(matches!(
            reconcile_mutation(&HashMap::new(), canonical).unwrap(),
            Some(CookieMutation::Set(_))
        ));
    }

    #[test]
    fn does_not_rewrite_an_equal_cookie() {
        let value = cookie("same");
        let identity = CookieIdentity::from_cookie(&value).unwrap();
        let snapshot = HashMap::from([(identity.clone(), value.clone())]);
        let canonical = CanonicalCookie {
            cookie: value,
            deleted: false,
            modified_at: 1,
        };
        assert!(reconcile_mutation(&snapshot, canonical).unwrap().is_none());
    }
}
