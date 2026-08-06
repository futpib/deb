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
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, EventMask, InputFocus,
            MapState, StackMode, Window,
        },
    },
    rust_connection::RustConnection,
};

type TabResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

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
    Layout(NativeRect),
    Navigate(String),
    Reload,
    NewTab(TabEngine),
    Select(u64),
    Close(u64),
    SwitchEngine(u64, TabEngine),
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
    initial_url: String,
    parent: Window,
    bounds: NativeRect,
    invoker: QmlMethodInvoker,
) -> TabController {
    let (sender, receiver) = mpsc::channel();
    let invoker = Arc::new(Mutex::new(invoker));
    let thread = std::thread::spawn(move || {
        let failure_invoker = Arc::clone(&invoker);
        let result = Runtime::new(profile_id, directories, parent, bounds, invoker, receiver)
            .and_then(|mut runtime| runtime.run(initial_url));
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

struct Tab {
    id: u64,
    engine: TabEngine,
    browser_id: Option<u64>,
    url: String,
    title: String,
    status: String,
    loading: bool,
    crashed: bool,
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
    parent: Window,
    bounds: NativeRect,
    invoker: Arc<Mutex<QmlMethodInvoker>>,
    receiver: Receiver<TabCommand>,
    connection: RustConnection,
    tabs: Vec<Tab>,
    active_tab: u64,
    next_tab_id: u64,
    next_browser_id: u64,
    chromium: Option<EngineRuntime>,
    firefox: Option<EngineRuntime>,
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
        })
    }
}

impl Runtime {
    fn new(
        profile_id: String,
        directories: ProfileDirectories,
        parent: Window,
        bounds: NativeRect,
        invoker: Arc<Mutex<QmlMethodInvoker>>,
        receiver: Receiver<TabCommand>,
    ) -> TabResult<Self> {
        let (connection, _) = x11rb::connect(None)?;
        let cookie_sync = CookieSync::new(&directories.shared_data)?;
        Ok(Self {
            profile_id,
            directories,
            parent,
            bounds,
            invoker,
            receiver,
            connection,
            tabs: Vec::new(),
            active_tab: 0,
            next_tab_id: 1,
            next_browser_id: 1,
            chromium: None,
            firefox: None,
            cookie_sync,
            automation: None,
            dirty: true,
        })
    }

    fn run(&mut self, initial_url: String) -> TabResult<()> {
        self.automation = Automation::from_environment(initial_url.clone());
        let tab_id = self.add_tab(TabEngine::Chromium, initial_url);
        self.active_tab = tab_id;
        self.attach_tab(tab_id)?;
        self.refresh_visibility()?;
        self.publish();

        loop {
            match self.receiver.recv_timeout(Duration::from_millis(25)) {
                Ok(TabCommand::Layout(bounds)) => self.layout(bounds)?,
                Ok(TabCommand::Navigate(url)) => self.navigate(&url)?,
                Ok(TabCommand::Reload) => self.reload()?,
                Ok(TabCommand::NewTab(engine)) => self.new_tab(engine)?,
                Ok(TabCommand::Select(tab_id)) => self.select(tab_id)?,
                Ok(TabCommand::Close(tab_id)) => self.close_tab(tab_id)?,
                Ok(TabCommand::SwitchEngine(tab_id, engine)) => {
                    self.switch_engine(tab_id, engine)?
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

    fn add_tab(&mut self, engine: TabEngine, url: String) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        self.tabs.push(Tab {
            id,
            engine,
            browser_id: None,
            url,
            title: "New tab".to_owned(),
            status: "Starting engine…".to_owned(),
            loading: true,
            crashed: false,
        });
        self.dirty = true;
        id
    }

    fn new_tab(&mut self, engine: TabEngine) -> TabResult<()> {
        let id = self.add_tab(engine, "deb://new-tab/".to_owned());
        self.active_tab = id;
        self.attach_tab(id)?;
        self.refresh_visibility()
    }

    fn attach_tab(&mut self, tab_id: u64) -> TabResult<()> {
        let browser_id = self.next_browser_id;
        self.next_browser_id += 1;
        let (backend, url) = {
            let tab = self.tab_mut(tab_id)?;
            tab.browser_id = Some(browser_id);
            tab.status = format!("Starting {}…", tab.engine.backend().label());
            tab.loading = true;
            tab.crashed = false;
            (tab.engine.backend(), tab.url.clone())
        };
        let profile_id = self.profile_id.clone();
        let directories = backend.directories(&self.directories).clone();
        if self.engine(backend).is_none() {
            match spawn_cef_browser(
                &self.connection,
                self.parent,
                self.bounds,
                &url,
                &profile_id,
                &directories,
                backend,
                browser_id,
            ) {
                Ok(process) => {
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
                    let tab = self.tab_mut(tab_id)?;
                    tab.browser_id = None;
                    tab.loading = false;
                    tab.status = format!("{} failed: {error}", backend.label());
                }
            }
        } else {
            let parent = self.parent;
            let bounds = self.bounds;
            self.engine_mut(backend)
                .as_mut()
                .expect("engine presence was checked")
                .process
                .create_browser(browser_id, parent, bounds, &url, &profile_id, &directories)?;
        }
        self.dirty = true;
        Ok(())
    }

    fn navigate(&mut self, url: &str) -> TabResult<()> {
        let tab_id = self.active_tab;
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

    fn reload(&mut self) -> TabResult<()> {
        let url = self.tab(self.active_tab)?.url.clone();
        self.navigate(&url)
    }

    fn select(&mut self, tab_id: u64) -> TabResult<()> {
        self.tab(tab_id)?;
        self.active_tab = tab_id;
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
        if let Some(browser_id) = tab.browser_id
            && let Some(engine) = self.engine_mut(tab.engine.backend())
        {
            engine.surfaces.remove(&browser_id);
            engine.process.close_browser(browser_id, true)?;
        }
        if self.tabs.is_empty() {
            let id = self.add_tab(TabEngine::Chromium, "deb://new-tab/".to_owned());
            self.active_tab = id;
            self.attach_tab(id)?;
        } else if self.active_tab == tab_id {
            let next_index = index.min(self.tabs.len() - 1);
            self.active_tab = self.tabs[next_index].id;
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

    fn layout(&mut self, bounds: NativeRect) -> TabResult<()> {
        self.bounds = bounds;
        for backend in [CefBackend::Chromium, CefBackend::Firefox] {
            let browser_ids = self
                .tabs
                .iter()
                .filter(|tab| tab.engine.backend() == backend)
                .filter_map(|tab| tab.browser_id)
                .collect::<Vec<_>>();
            if let Some(engine) = self.engine_mut(backend) {
                for browser_id in browser_ids {
                    engine.process.resize_browser(browser_id, bounds)?;
                }
                for window in engine.surfaces.values().copied().collect::<HashSet<_>>() {
                    configure_native_window(&self.connection, window, bounds)?;
                }
            }
        }
        Ok(())
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
                if self.connection.query_tree(window)?.reply()?.parent != self.parent {
                    return Err(format!(
                        "{} returned a window outside the Qt host",
                        backend.label()
                    )
                    .into());
                }
                self.connection.change_window_attributes(
                    window,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::BUTTON_PRESS),
                )?;
                configure_native_window(&self.connection, window, self.bounds)?;
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
        if automation.started.elapsed() > Duration::from_secs(25) {
            return Ok(Some((
                "FAIL".to_owned(),
                "tab-aware smoke test did not complete within 25 seconds".to_owned(),
            )));
        }
        let phase = automation.phase.clone();
        match phase {
            AutomationPhase::WaitChromium => {
                let chromium_tab = automation.chromium_tab;
                if self.tab_ready(chromium_tab) {
                    let initial_url = automation.initial_url.clone();
                    let firefox_tab = self.add_tab(TabEngine::Firefox, initial_url);
                    self.active_tab = firefox_tab;
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
                    let chromium_sibling = self.add_tab(TabEngine::Chromium, initial_url.clone());
                    self.attach_tab(chromium_sibling)?;
                    let firefox_sibling = self.add_tab(TabEngine::Firefox, initial_url);
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
                if automation.navigation_settled.contains(&chromium_tab)
                    && automation.navigation_settled.contains(&firefox_tab)
                {
                    self.select(chromium_tab)?;
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
                match self.capture_active() {
                    Ok(variants) if variants >= 8 => {
                        let firefox_tab = automation.firefox_tab.expect("Firefox tab was created");
                        self.select(firefox_tab)?;
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
                match self.capture_active() {
                    Ok(variants) if variants >= 8 => {
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
                    let chromium_tab = automation.chromium_tab;
                    self.switch_engine(chromium_tab, TabEngine::Firefox)?;
                    self.automation
                        .as_mut()
                        .expect("automation is active")
                        .phase = AutomationPhase::WaitEngineSwitch;
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
                    if firefox_browser_ids.len() != 3
                        || !firefox_browser_ids
                            .iter()
                            .all(|browser_id| firefox.surfaces.contains_key(browser_id))
                    {
                        return Ok(Some((
                            "FAIL".to_owned(),
                            "engine switch did not create three Gecko browsers in one helper"
                                .to_owned(),
                        )));
                    }
                    self.select(switched_tab)?;
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
                match self.capture_active() {
                    Ok(switched_variants) if switched_variants >= 8 => {
                        return Ok(Some((
                            "PASS".to_owned(),
                            format!(
                                "four concurrent tabs rendered in two profile helpers and synchronized a cookie; Chromium renderer and Gecko content crashes stayed isolated from same-engine siblings and recovered without replacing either helper; Chromium->Firefox switching then created a third Gecko browser in the same helper (Chromium {}, Gecko {}, switched Gecko {switched_variants} sampled colors)",
                                automation.chromium_variants.unwrap_or_default(),
                                automation.firefox_variants.unwrap_or_default(),
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

    fn capture_active(&mut self) -> TabResult<usize> {
        self.refresh_visibility()?;
        let active = self.tab(self.active_tab)?;
        let browser_id = active.browser_id.ok_or("active tab has no browser")?;
        let window = self
            .engine(active.engine.backend())
            .and_then(|engine| engine.surfaces.get(&browser_id))
            .copied()
            .ok_or("active tab has no native surface")?;
        sampled_pixel_variants(&self.connection, window)
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
        let active = self.tab(self.active_tab)?;
        let active_backend = active.engine.backend();
        let active_browser = active.browser_id;
        let active_window = active_browser.and_then(|browser_id| {
            self.engine(active_backend)
                .and_then(|engine| engine.surfaces.get(&browser_id).copied())
        });
        let browser_states = self
            .tabs
            .iter()
            .filter_map(|tab| {
                tab.browser_id
                    .map(|browser_id| (tab.engine.backend(), browser_id, tab.id == self.active_tab))
            })
            .collect::<Vec<_>>();
        for (backend, browser_id, visible) in browser_states {
            if let Some(engine) = self.engine_mut(backend) {
                engine.process.set_browser_visible(browser_id, visible)?;
                engine.process.focus_browser(browser_id, visible)?;
            }
        }
        let windows = [self.chromium.as_ref(), self.firefox.as_ref()]
            .into_iter()
            .flatten()
            .flat_map(|engine| engine.surfaces.values().copied())
            .collect::<HashSet<_>>();
        for window in windows {
            if Some(window) == active_window {
                configure_native_window(&self.connection, window, self.bounds)?;
                if self
                    .connection
                    .get_window_attributes(window)?
                    .reply()?
                    .map_state
                    == MapState::UNMAPPED
                {
                    self.connection.map_window(window)?;
                }
                self.connection.configure_window(
                    window,
                    &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                )?;
                self.connection
                    .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)?;
            } else {
                let _ = self.connection.unmap_window(window);
            }
        }
        self.connection.flush()?;
        Ok(())
    }

    fn handle_x11_events(&mut self) -> TabResult<()> {
        while let Some(event) = self.connection.poll_for_event()? {
            if let Event::ButtonPress(event) = event {
                let active = self.tab(self.active_tab)?;
                if let Some(browser_id) = active.browser_id
                    && self
                        .engine(active.engine.backend())
                        .and_then(|engine| engine.surfaces.get(&browser_id))
                        .is_some_and(|window| *window == event.event)
                    && let Some(engine) = self.engine_mut(active.engine.backend())
                {
                    engine.process.focus_browser(browser_id, true)?;
                }
            }
        }
        Ok(())
    }

    fn publish(&mut self) {
        let active = match self.tab(self.active_tab) {
            Ok(tab) => tab,
            Err(error) => {
                eprintln!("deb: cannot publish active tab: {error}");
                return;
            }
        };
        let json = serde_json::to_string(&self.tabs.iter().map(Tab::snapshot).collect::<Vec<_>>())
            .unwrap_or_else(|_| "[]".to_owned());
        let invoker = self
            .invoker
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        invoke_method!(
            &*invoker,
            "update_tab_state",
            json,
            active.id.to_string(),
            active.url.clone(),
            active.status.clone()
        );
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
