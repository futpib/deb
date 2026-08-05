use crate::{
    cookie_store::{CanonicalCookie, CookieIdentity, CookieStore, cookie_contents_equal},
    profile::{EngineDirectories, ProfileDirectories},
};
use qtbridge::{QmlMethodInvoker, invoke_method};
use shell_protocol::{
    MAX_PACKET_BYTES, ProtocolError, Transport, configure_child_command,
    wire::{self, Capability, Engine},
};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::mpsc::{self, RecvTimeoutError, Sender, TryRecvError},
    thread::{JoinHandle, sleep},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, EventMask, ImageFormat,
            InputFocus, MapState, StackMode, Window,
        },
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

    fn viewport(self) -> wire::Viewport {
        wire::Viewport {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
            scale_factor: 1.0,
        }
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

pub struct Controller {
    sender: Sender<ControllerCommand>,
    thread: JoinHandle<()>,
}

struct ControllerConfig {
    profile_id: String,
    profile_directories: ProfileDirectories,
    url: String,
    chromium_parent: Window,
    firefox_parent: Window,
    layout: Layout,
}

impl Controller {
    pub fn send(
        &self,
        command: ControllerCommand,
    ) -> Result<(), mpsc::SendError<ControllerCommand>> {
        self.sender.send(command)
    }

    pub fn stop(self) {
        let _ = self.sender.send(ControllerCommand::Stop);
        let _ = self.thread.join();
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum CefBackend {
    Chromium,
    Firefox,
}

impl CefBackend {
    fn label(self) -> &'static str {
        match self {
            Self::Chromium => "Chromium",
            Self::Firefox => "Gecko",
        }
    }

    fn loader_directory(self, executable_directory: &Path) -> NativeResult<Option<PathBuf>> {
        if matches!(self, Self::Chromium) {
            return Ok(None);
        }
        let configured = std::env::var_os("DEB_FIREFOX_RUNTIME")
            .map(PathBuf::from)
            .unwrap_or_else(|| executable_directory.join("firefox-cef-runtime"));
        let directory = configured.canonicalize().map_err(|error| {
            format!(
                "cannot resolve {} at {}: {error}",
                self.name(),
                configured.display()
            )
        })?;
        if !directory.join("libcef.so").is_file() {
            return Err(format!("{} has no libcef.so", directory.display()).into());
        }
        Ok(Some(directory))
    }

    fn name(self) -> &'static str {
        match self {
            Self::Chromium => "Chromium libcef",
            Self::Firefox => "Firefox CEF adapter",
        }
    }

    fn engine(self) -> Engine {
        match self {
            Self::Chromium => Engine::Chromium,
            Self::Firefox => Engine::Gecko,
        }
    }

    fn directories(self, profile: &ProfileDirectories) -> &EngineDirectories {
        match self {
            Self::Chromium => &profile.chromium,
            Self::Firefox => &profile.firefox,
        }
    }
}

struct CefInstance {
    child: Child,
    transport: Transport,
    incoming: mpsc::Receiver<Result<wire::Packet, ProtocolError>>,
    window: Window,
    native_child: bool,
    browser_id: u64,
    next_request_id: u64,
    pending_requests: HashMap<u64, &'static str>,
    last_event_sequence: u64,
    protocol_closed: bool,
}

impl CefInstance {
    fn send_request(
        &mut self,
        operation: wire::request::Operation,
        description: &'static str,
    ) -> NativeResult<()> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or("shell protocol request ID overflow")?;
        self.transport
            .send(&request_packet(request_id, self.browser_id, operation))?;
        self.pending_requests.insert(request_id, description);
        Ok(())
    }

    fn navigate(&mut self, url: &str) -> NativeResult<()> {
        self.send_request(
            wire::request::Operation::Navigate(wire::Navigate {
                url: url.to_owned(),
            }),
            "navigation",
        )
    }

    fn read_cookies(&mut self) -> NativeResult<()> {
        self.send_request(
            wire::request::Operation::ReadCookies(wire::ReadCookies {}),
            "cookie snapshot",
        )
    }

    fn set_cookie(&mut self, cookie: wire::Cookie) -> NativeResult<()> {
        self.send_request(
            wire::request::Operation::SetCookie(wire::SetCookie {
                cookie: Some(cookie),
            }),
            "cookie set",
        )
    }

    fn delete_cookie(&mut self, cookie: wire::Cookie) -> NativeResult<()> {
        self.send_request(
            wire::request::Operation::DeleteCookie(wire::DeleteCookie {
                cookie: Some(cookie),
            }),
            "cookie delete",
        )
    }

    fn focus(&mut self, connection: &RustConnection, bounds: NativeRect) -> NativeResult<()> {
        if self.native_child {
            connection.set_input_focus(InputFocus::PARENT, self.window, x11rb::CURRENT_TIME)?;
            configure_native_window(connection, self.window, bounds)?;
        }
        self.send_request(
            wire::request::Operation::SetFocus(wire::SetFocus { focused: true }),
            "focus",
        )
    }

    fn resize(&mut self, connection: &RustConnection, bounds: NativeRect) -> NativeResult<()> {
        if self.native_child {
            configure_native_window(connection, self.window, bounds)?;
        }
        self.send_request(
            wire::request::Operation::Resize(wire::Resize {
                viewport: Some(bounds.viewport()),
            }),
            "resize",
        )
    }

    fn ensure_visible(&self, connection: &RustConnection) -> NativeResult<()> {
        if connection
            .get_window_attributes(self.window)?
            .reply()?
            .map_state
            == MapState::UNMAPPED
        {
            connection.map_window(self.window)?;
        }
        connection.configure_window(
            self.window,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        connection.flush()?;
        Ok(())
    }

    fn drain_notices(&mut self) -> Vec<ProtocolNotice> {
        let mut notices = Vec::new();
        if self.protocol_closed {
            return notices;
        }
        loop {
            match self.incoming.try_recv() {
                Ok(Ok(received)) => {
                    if let Some(notice) = self.handle_packet(received) {
                        notices.push(notice);
                    }
                }
                Ok(Err(error)) => {
                    self.protocol_closed = true;
                    notices.push(ProtocolNotice::ProtocolFailed(error.to_string()));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.protocol_closed = true;
                    notices.push(ProtocolNotice::ProtocolFailed(
                        "protocol reader stopped".to_owned(),
                    ));
                    break;
                }
            }
        }
        notices
    }

    fn handle_packet(&mut self, received: wire::Packet) -> Option<ProtocolNotice> {
        let request_id = received.request_id;
        match received.body {
            Some(wire::packet::Body::Response(response)) => {
                let Some(description) = self.pending_requests.remove(&request_id) else {
                    return Some(ProtocolNotice::ProtocolFailed(format!(
                        "unsolicited response for request {request_id}"
                    )));
                };
                match response.result {
                    Some(wire::response::Result::Success(_)) => None,
                    Some(wire::response::Result::Error(error)) => {
                        Some(ProtocolNotice::CommandFailed(format!(
                            "{description} rejected [{}]: {}",
                            error.code, error.message
                        )))
                    }
                    None => Some(ProtocolNotice::ProtocolFailed(format!(
                        "{description} response has no result"
                    ))),
                }
            }
            Some(wire::packet::Body::Event(event)) => {
                if event.browser_id != self.browser_id {
                    return Some(ProtocolNotice::ProtocolFailed(format!(
                        "event targets browser {}, expected {}",
                        event.browser_id, self.browser_id
                    )));
                }
                if event.sequence <= self.last_event_sequence {
                    return Some(ProtocolNotice::ProtocolFailed(format!(
                        "event sequence {} followed {}",
                        event.sequence, self.last_event_sequence
                    )));
                }
                self.last_event_sequence = event.sequence;
                match event.value {
                    Some(wire::event::Value::LoadingChanged(loading)) => {
                        Some(ProtocolNotice::LoadingChanged(loading.loading))
                    }
                    Some(wire::event::Value::LoadFailed(failure)) => {
                        Some(ProtocolNotice::LoadFailed(format!(
                            "{} ({})",
                            failure.error_text, failure.error_code
                        )))
                    }
                    Some(wire::event::Value::BrowserClosed(_)) => Some(ProtocolNotice::Closed),
                    Some(wire::event::Value::BrowserCrashed(crash)) => {
                        Some(ProtocolNotice::Crashed(crash.reason))
                    }
                    Some(wire::event::Value::CookieSnapshotEntry(entry)) => match entry.cookie {
                        Some(cookie) => Some(ProtocolNotice::CookieSnapshotEntry(cookie)),
                        None => Some(ProtocolNotice::ProtocolFailed(
                            "cookie snapshot entry has no cookie".to_owned(),
                        )),
                    },
                    Some(wire::event::Value::CookieSnapshotComplete(_)) => {
                        Some(ProtocolNotice::CookieSnapshotComplete)
                    }
                    Some(wire::event::Value::CookieChanged(change)) => match (
                        change.cookie,
                        wire::CookieChangeCause::try_from(change.cause),
                    ) {
                        (Some(cookie), Ok(cause)) => {
                            Some(ProtocolNotice::CookieChanged(cookie, cause))
                        }
                        (None, _) => Some(ProtocolNotice::ProtocolFailed(
                            "cookie change has no cookie".to_owned(),
                        )),
                        (_, Err(_)) => Some(ProtocolNotice::ProtocolFailed(format!(
                            "cookie change has invalid cause {}",
                            change.cause
                        ))),
                    },
                    Some(_) => None,
                    None => None,
                }
            }
            Some(_) => Some(ProtocolNotice::ProtocolFailed(
                "unexpected runtime packet type".to_owned(),
            )),
            None => Some(ProtocolNotice::ProtocolFailed(
                "runtime packet has no body".to_owned(),
            )),
        }
    }

    fn stop(mut self) {
        let _ = self.send_request(
            wire::request::Operation::Close(wire::Close { force: true }),
            "close",
        );
        stop_child(&mut self.child);
    }
}

enum ProtocolNotice {
    CommandFailed(String),
    LoadingChanged(bool),
    LoadFailed(String),
    Closed,
    Crashed(String),
    ProtocolFailed(String),
    CookieSnapshotEntry(wire::Cookie),
    CookieSnapshotComplete,
    CookieChanged(wire::Cookie, wire::CookieChangeCause),
}

#[derive(Default)]
struct EngineCookieSnapshot {
    expected: bool,
    complete: bool,
    cookies: Vec<wire::Cookie>,
}

struct CookieBroker {
    store: CookieStore,
    chromium: EngineCookieSnapshot,
    firefox: EngineCookieSnapshot,
    pending_changes: Vec<(CefBackend, wire::Cookie, wire::CookieChangeCause)>,
    reconciled: bool,
}

impl CookieBroker {
    fn new(
        profile_data: &Path,
        chromium_expected: bool,
        firefox_expected: bool,
    ) -> NativeResult<Self> {
        Ok(Self {
            store: CookieStore::open(profile_data)?,
            chromium: EngineCookieSnapshot {
                expected: chromium_expected,
                ..Default::default()
            },
            firefox: EngineCookieSnapshot {
                expected: firefox_expected,
                ..Default::default()
            },
            pending_changes: Vec::new(),
            reconciled: false,
        })
    }

    fn start(
        &mut self,
        chromium: &mut Option<CefInstance>,
        firefox: &mut Option<CefInstance>,
    ) -> NativeResult<()> {
        if let Some(instance) = chromium {
            instance.read_cookies()?;
        }
        if let Some(instance) = firefox {
            instance.read_cookies()?;
        }
        if !self.chromium.expected && !self.firefox.expected {
            self.reconciled = true;
        }
        Ok(())
    }

    fn is_reconciled(&self) -> bool {
        self.reconciled
    }

    fn observe(
        &mut self,
        backend: CefBackend,
        notices: &[ProtocolNotice],
        chromium: &mut Option<CefInstance>,
        firefox: &mut Option<CefInstance>,
    ) -> NativeResult<()> {
        for notice in notices {
            match notice {
                ProtocolNotice::CookieSnapshotEntry(cookie) if !self.reconciled => {
                    self.snapshot_mut(backend).cookies.push(cookie.clone());
                }
                ProtocolNotice::CookieSnapshotComplete if !self.reconciled => {
                    self.snapshot_mut(backend).complete = true;
                }
                ProtocolNotice::CookieChanged(cookie, cause) if self.reconciled => {
                    self.apply_change(backend, cookie, *cause, chromium, firefox)?;
                }
                ProtocolNotice::CookieChanged(cookie, cause) => {
                    self.pending_changes.push((backend, cookie.clone(), *cause));
                }
                ProtocolNotice::CommandFailed(error)
                    if !self.reconciled && error.starts_with("cookie snapshot rejected") =>
                {
                    return Err(
                        format!("{} cookie snapshot failed: {error}", backend.label()).into(),
                    );
                }
                _ => {}
            }
        }
        if !self.reconciled && self.snapshots_complete() {
            self.reconcile(chromium, firefox)?;
        }
        Ok(())
    }

    fn snapshot_mut(&mut self, backend: CefBackend) -> &mut EngineCookieSnapshot {
        match backend {
            CefBackend::Chromium => &mut self.chromium,
            CefBackend::Firefox => &mut self.firefox,
        }
    }

    fn snapshot(&self, backend: CefBackend) -> &EngineCookieSnapshot {
        match backend {
            CefBackend::Chromium => &self.chromium,
            CefBackend::Firefox => &self.firefox,
        }
    }

    fn snapshots_complete(&self) -> bool {
        (!self.chromium.expected || self.chromium.complete)
            && (!self.firefox.expected || self.firefox.complete)
    }

    fn reconcile(
        &mut self,
        chromium: &mut Option<CefInstance>,
        firefox: &mut Option<CefInstance>,
    ) -> NativeResult<()> {
        for backend in [CefBackend::Chromium, CefBackend::Firefox] {
            for cookie in &self.snapshot(backend).cookies {
                let identity = CookieIdentity::from_cookie(cookie)?;
                if identity.is_opaque() {
                    eprintln!(
                        "deb: {} has an opaque partitioned cookie that cannot cross engines",
                        backend.label()
                    );
                    continue;
                }
                self.store.merge_snapshot(cookie)?;
            }
        }

        let canonical = self.store.all()?;
        for backend in [CefBackend::Chromium, CefBackend::Firefox] {
            if !self.snapshot(backend).expected {
                continue;
            }
            let snapshot = snapshot_index(&self.snapshot(backend).cookies)?;
            let instance = match backend {
                CefBackend::Chromium => chromium.as_mut(),
                CefBackend::Firefox => firefox.as_mut(),
            }
            .ok_or("cookie snapshot expected an unavailable engine")?;
            for entry in &canonical {
                reconcile_cookie(instance, &snapshot, entry)?;
            }
        }

        self.reconciled = true;
        let pending = std::mem::take(&mut self.pending_changes);
        for (backend, cookie, cause) in pending {
            self.apply_change(backend, &cookie, cause, chromium, firefox)?;
        }
        eprintln!(
            "deb: canonical cookie store reconciled {} records across {} engine(s)",
            canonical.len(),
            usize::from(self.chromium.expected) + usize::from(self.firefox.expected)
        );
        Ok(())
    }

    fn apply_change(
        &self,
        source: CefBackend,
        cookie: &wire::Cookie,
        cause: wire::CookieChangeCause,
        chromium: &mut Option<CefInstance>,
        firefox: &mut Option<CefInstance>,
    ) -> NativeResult<()> {
        let identity = CookieIdentity::from_cookie(cookie)?;
        if identity.is_opaque() {
            return Ok(());
        }
        let changed = match cause {
            wire::CookieChangeCause::Inserted
            | wire::CookieChangeCause::InsertedNoChangeOverwrite
            | wire::CookieChangeCause::InsertedNoValueChangeOverwrite => {
                self.store.apply_live_change(cookie)?
            }
            wire::CookieChangeCause::Explicit
            | wire::CookieChangeCause::UnknownDeletion
            | wire::CookieChangeCause::Expired
            | wire::CookieChangeCause::Evicted => self.store.apply_deletion(cookie)?,
            wire::CookieChangeCause::Overwrite | wire::CookieChangeCause::ExpiredOverwrite => false,
        };
        if !changed {
            return Ok(());
        }
        let target = match source {
            CefBackend::Chromium => firefox.as_mut(),
            CefBackend::Firefox => chromium.as_mut(),
        };
        if let Some(target) = target {
            if matches!(
                cause,
                wire::CookieChangeCause::Explicit
                    | wire::CookieChangeCause::UnknownDeletion
                    | wire::CookieChangeCause::Expired
                    | wire::CookieChangeCause::Evicted
            ) {
                target.delete_cookie(cookie.clone())?;
            } else {
                target.set_cookie(cookie.clone())?;
            }
        }
        Ok(())
    }
}

fn snapshot_index(cookies: &[wire::Cookie]) -> NativeResult<HashMap<CookieIdentity, wire::Cookie>> {
    let mut index = HashMap::new();
    for cookie in cookies {
        let identity = CookieIdentity::from_cookie(cookie)?;
        if !identity.is_opaque() {
            index.insert(identity, cookie.clone());
        }
    }
    Ok(index)
}

fn reconcile_cookie(
    instance: &mut CefInstance,
    snapshot: &HashMap<CookieIdentity, wire::Cookie>,
    canonical: &CanonicalCookie,
) -> NativeResult<()> {
    let identity = CookieIdentity::from_cookie(&canonical.cookie)?;
    match (canonical.deleted, snapshot.get(&identity)) {
        (true, Some(_)) => instance.delete_cookie(canonical.cookie.clone()),
        (true, None) => Ok(()),
        (false, Some(cookie)) if cookie_contents_equal(cookie, &canonical.cookie) => Ok(()),
        (false, _) => instance.set_cookie(canonical.cookie.clone()),
    }
}

#[derive(Default)]
struct SmokeEngineState {
    initial_load_settled: bool,
    navigation_started: bool,
    navigation_settled: bool,
}

struct AutomatedSmokeTest {
    target_url: String,
    chromium: SmokeEngineState,
    firefox: SmokeEngineState,
    navigation_sent: bool,
    cookie_sync_started: bool,
    chromium_cookie_seen: bool,
    firefox_cookie_seen: bool,
    render_after: Option<Instant>,
    started_at: Instant,
}

impl AutomatedSmokeTest {
    fn new(target_url: String) -> Self {
        Self {
            target_url,
            chromium: SmokeEngineState::default(),
            firefox: SmokeEngineState::default(),
            navigation_sent: false,
            cookie_sync_started: false,
            chromium_cookie_seen: false,
            firefox_cookie_seen: false,
            render_after: None,
            started_at: Instant::now(),
        }
    }

    fn from_environment() -> NativeResult<Option<Self>> {
        if !automated_smoke_requested() {
            return Ok(None);
        }
        let target_url = std::env::var("DEB_SMOKE_NAVIGATE_URL")
            .map_err(|_| "DEB_AUTOMATED_SMOKE_TEST requires DEB_SMOKE_NAVIGATE_URL")?;
        if target_url.is_empty() {
            return Err("DEB_SMOKE_NAVIGATE_URL must not be empty".into());
        }
        Ok(Some(Self::new(target_url)))
    }

    fn observe(&mut self, backend: CefBackend, notices: &[ProtocolNotice]) -> Result<(), String> {
        let state = match backend {
            CefBackend::Chromium => &mut self.chromium,
            CefBackend::Firefox => &mut self.firefox,
        };
        for notice in notices {
            match notice {
                ProtocolNotice::LoadingChanged(false) if !self.navigation_sent => {
                    state.initial_load_settled = true;
                }
                ProtocolNotice::LoadingChanged(true) if self.navigation_sent => {
                    state.navigation_started = true;
                }
                ProtocolNotice::LoadingChanged(false)
                    if self.navigation_sent && state.navigation_started =>
                {
                    state.navigation_settled = true;
                }
                ProtocolNotice::CommandFailed(error)
                | ProtocolNotice::LoadFailed(error)
                | ProtocolNotice::Crashed(error)
                | ProtocolNotice::ProtocolFailed(error) => {
                    return Err(format!("{}: {error}", backend.label()));
                }
                ProtocolNotice::Closed => {
                    return Err(format!("{} closed during the smoke test", backend.label()));
                }
                ProtocolNotice::CookieChanged(cookie, cause)
                    if is_smoke_cookie(cookie)
                        && matches!(
                            cause,
                            wire::CookieChangeCause::Inserted
                                | wire::CookieChangeCause::InsertedNoChangeOverwrite
                                | wire::CookieChangeCause::InsertedNoValueChangeOverwrite
                        ) =>
                {
                    match backend {
                        CefBackend::Chromium => self.chromium_cookie_seen = true,
                        CefBackend::Firefox => self.firefox_cookie_seen = true,
                    }
                }
                ProtocolNotice::LoadingChanged(_)
                | ProtocolNotice::CookieSnapshotEntry(_)
                | ProtocolNotice::CookieSnapshotComplete
                | ProtocolNotice::CookieChanged(_, _) => {}
            }
        }
        Ok(())
    }

    fn initial_loads_settled(&self) -> bool {
        self.chromium.initial_load_settled && self.firefox.initial_load_settled
    }

    fn navigations_settled(&self) -> bool {
        self.chromium.navigation_settled && self.firefox.navigation_settled
    }

    fn cookies_synced(&self) -> bool {
        self.chromium_cookie_seen && self.firefox_cookie_seen
    }
}

fn smoke_cookie() -> wire::Cookie {
    wire::Cookie {
        key: Some(wire::CookieKey {
            name: "deb_cross_engine_smoke".to_owned(),
            domain: ".deb-smoke.invalid".to_owned(),
            path: "/".to_owned(),
            partition_key: None,
        }),
        value: "chromium-to-gecko".to_owned(),
        secure: false,
        http_only: true,
        creation: 0,
        last_access: 0,
        expires: None,
        same_site: wire::CookieSameSite::Lax as i32,
        priority: wire::CookiePriority::Medium as i32,
        last_update: 0,
    }
}

fn is_smoke_cookie(cookie: &wire::Cookie) -> bool {
    cookie
        .key
        .as_ref()
        .is_some_and(|key| key.name == "deb_cross_engine_smoke")
        && cookie.value == "chromium-to-gecko"
}

pub fn spawn_controller(
    profile_id: String,
    profile_directories: ProfileDirectories,
    url: String,
    chromium_parent: Window,
    firefox_parent: Window,
    layout: Layout,
    invoker: QmlMethodInvoker,
) -> Controller {
    let (sender, receiver) = mpsc::channel();
    let config = ControllerConfig {
        profile_id,
        profile_directories,
        url,
        chromium_parent,
        firefox_parent,
        layout,
    };
    let thread = std::thread::spawn(move || {
        if let Err(error) = run_controller(config, &invoker, receiver) {
            update_statuses(
                &invoker,
                format!("Native controller failed: {error}"),
                format!("Native controller failed: {error}"),
            );
            if automated_smoke_requested() {
                finish_smoke_test(&invoker, "FAIL", error.to_string());
            }
        }
    });
    Controller { sender, thread }
}

fn run_controller(
    config: ControllerConfig,
    invoker: &QmlMethodInvoker,
    receiver: mpsc::Receiver<ControllerCommand>,
) -> NativeResult<()> {
    let ControllerConfig {
        profile_id,
        profile_directories,
        url: initial_url,
        chromium_parent,
        firefox_parent,
        layout: initial_layout,
    } = config;
    let (connection, _) = x11rb::connect(None)?;
    let mut layout = initial_layout;
    let mut chromium_status = "Starting Chromium through the CEF ABI…".to_owned();
    let mut firefox_status = "Starting Firefox through the CEF ABI…".to_owned();
    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());

    let mut chromium = match spawn_cef(
        &connection,
        chromium_parent,
        layout.chromium,
        &initial_url,
        &profile_id,
        CefBackend::Chromium.directories(&profile_directories),
        CefBackend::Chromium,
    ) {
        Ok(instance) => {
            chromium_status = format!("Live · profile {profile_id} · libcef.so / Chromium");
            Some(instance)
        }
        Err(error) => {
            chromium_status = format!("Chromium CEF failed: {error}");
            None
        }
    };
    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());

    let mut firefox = match spawn_cef(
        &connection,
        firefox_parent,
        layout.firefox,
        &initial_url,
        &profile_id,
        CefBackend::Firefox.directories(&profile_directories),
        CefBackend::Firefox,
    ) {
        Ok(instance) => {
            firefox_status = format!("Live · profile {profile_id} · libcef.so / Gecko");
            Some(instance)
        }
        Err(error) => {
            firefox_status = format!("Firefox CEF adapter failed: {error}");
            None
        }
    };
    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());

    let mut cookie_broker = CookieBroker::new(
        &profile_directories.shared_data,
        chromium.is_some(),
        firefox.is_some(),
    )?;
    cookie_broker.start(&mut chromium, &mut firefox)?;

    let mut smoke_test = AutomatedSmokeTest::from_environment()?;
    let mut smoke_result = None;
    if smoke_test.is_some() && (chromium.is_none() || firefox.is_none()) {
        smoke_result = Some((
            "FAIL",
            "both Chromium and Gecko must start for the automated smoke test".to_owned(),
        ));
    }

    if smoke_test.is_none()
        && let Ok(smoke_url) = std::env::var("DEB_SMOKE_NAVIGATE_URL")
    {
        if let Some(instance) = &mut chromium {
            instance.navigate(&smoke_url).map_err(|error| {
                format!("Chromium CEF smoke navigation could not be sent: {error}")
            })?;
        }
        if let Some(instance) = &mut firefox {
            instance.navigate(&smoke_url).map_err(|error| {
                format!("Firefox CEF smoke navigation could not be sent: {error}")
            })?;
        }
    }

    while smoke_result.is_none() {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(ControllerCommand::Layout(next_layout)) => {
                layout = next_layout;
                if let Some(instance) = &mut chromium
                    && let Err(error) = instance.resize(&connection, layout.chromium)
                {
                    chromium_status = format!("Chromium CEF resize failed: {error}");
                }
                if let Some(instance) = &mut firefox
                    && let Err(error) = instance.resize(&connection, layout.firefox)
                {
                    firefox_status = format!("Firefox CEF resize failed: {error}");
                }
                update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
            }
            Ok(ControllerCommand::Navigate(url)) => {
                if let Some(instance) = &mut chromium {
                    match instance.navigate(&url) {
                        Ok(()) => {
                            chromium_status =
                                format!("Live · profile {profile_id} · libcef.so / Chromium")
                        }
                        Err(error) => {
                            chromium_status = format!("Chromium CEF navigation failed: {error}")
                        }
                    }
                }
                if let Some(instance) = &mut firefox {
                    match instance.navigate(&url) {
                        Ok(()) => {
                            firefox_status =
                                format!("Live · profile {profile_id} · libcef.so / Gecko")
                        }
                        Err(error) => {
                            firefox_status = format!("Firefox CEF navigation failed: {error}")
                        }
                    }
                }
                update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
            }
            Ok(ControllerCommand::Stop) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if let Some(instance) = &chromium
                    && let Err(error) = instance.ensure_visible(&connection)
                {
                    chromium_status = format!("Chromium CEF visibility failed: {error}");
                    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
                }
                if let Some(instance) = &firefox
                    && let Err(error) = instance.ensure_visible(&connection)
                {
                    firefox_status = format!("Firefox CEF visibility failed: {error}");
                    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
                }
            }
        }

        while let Some(event) = connection.poll_for_event()? {
            if let Event::ButtonPress(event) = event {
                if let Some(instance) = &mut chromium
                    && event.event == instance.window
                    && let Err(error) = instance.focus(&connection, layout.chromium)
                {
                    chromium_status = format!("Chromium CEF focus failed: {error}");
                }
                if let Some(instance) = &mut firefox
                    && event.event == instance.window
                    && let Err(error) = instance.focus(&connection, layout.firefox)
                {
                    firefox_status = format!("Firefox CEF focus failed: {error}");
                }
                update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
            }
        }

        let chromium_notices = chromium
            .as_mut()
            .map(CefInstance::drain_notices)
            .unwrap_or_default();
        let firefox_notices = firefox
            .as_mut()
            .map(CefInstance::drain_notices)
            .unwrap_or_default();

        cookie_broker.observe(
            CefBackend::Chromium,
            &chromium_notices,
            &mut chromium,
            &mut firefox,
        )?;
        cookie_broker.observe(
            CefBackend::Firefox,
            &firefox_notices,
            &mut chromium,
            &mut firefox,
        )?;

        if let Some(smoke) = &mut smoke_test {
            let observed = smoke
                .observe(CefBackend::Chromium, &chromium_notices)
                .and_then(|()| smoke.observe(CefBackend::Firefox, &firefox_notices));
            if let Err(error) = observed {
                smoke_result = Some(("FAIL", error));
            }
        }

        if let Some(smoke) = &mut smoke_test
            && cookie_broker.is_reconciled()
            && !smoke.cookie_sync_started
        {
            chromium
                .as_mut()
                .expect("automated smoke test requires Chromium")
                .set_cookie(smoke_cookie())?;
            smoke.cookie_sync_started = true;
            eprintln!("deb-smoke: exercising Chromium-to-Gecko cookie synchronization");
        }

        if let Some(status) = notice_status("Chromium CEF", chromium_notices) {
            chromium_status = status;
            update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
        }
        if let Some(status) = notice_status("Firefox CEF adapter", firefox_notices) {
            firefox_status = status;
            update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
        }

        if smoke_result.is_some() {
            break;
        }
        if let Some(smoke) = &mut smoke_test {
            if smoke.started_at.elapsed() > Duration::from_secs(20) {
                smoke_result = Some((
                    "FAIL",
                    "both engines did not complete the smoke test within 20 seconds".to_owned(),
                ));
                continue;
            }
            if !smoke.navigation_sent && smoke.initial_loads_settled() {
                let chromium_navigation = chromium
                    .as_mut()
                    .expect("automated smoke test requires Chromium")
                    .navigate(&smoke.target_url);
                let firefox_navigation = firefox
                    .as_mut()
                    .expect("automated smoke test requires Gecko")
                    .navigate(&smoke.target_url);
                match (chromium_navigation, firefox_navigation) {
                    (Ok(()), Ok(())) => {
                        smoke.navigation_sent = true;
                        eprintln!(
                            "deb-smoke: both initial pages settled; navigating to {}",
                            smoke.target_url
                        );
                    }
                    (chromium_result, firefox_result) => {
                        smoke_result = Some((
                            "FAIL",
                            format!(
                                "smoke navigation failed: Chromium={chromium_result:?}, Gecko={firefox_result:?}"
                            ),
                        ));
                    }
                }
            } else if smoke.navigations_settled()
                && smoke.cookies_synced()
                && smoke.render_after.is_none()
            {
                smoke.render_after = Some(Instant::now() + Duration::from_millis(500));
            } else if smoke
                .render_after
                .is_some_and(|deadline| Instant::now() >= deadline)
            {
                let chromium_variants = rendered_pixel_variants(
                    &connection,
                    chromium
                        .as_ref()
                        .expect("automated smoke test requires Chromium"),
                );
                let firefox_variants = rendered_pixel_variants(
                    &connection,
                    firefox
                        .as_ref()
                        .expect("automated smoke test requires Gecko"),
                );
                smoke_result = Some(match (chromium_variants, firefox_variants) {
                    (Ok(chromium_variants), Ok(firefox_variants)) => (
                        "PASS",
                        format!(
                            "both engines loaded, rendered, and synchronized a cookie (Chromium {chromium_variants} sampled colors, Gecko {firefox_variants})"
                        ),
                    ),
                    (chromium_result, firefox_result) => (
                        "FAIL",
                        format!(
                            "render verification failed: Chromium={chromium_result:?}, Gecko={firefox_result:?}"
                        ),
                    ),
                });
            }
        }
    }

    if let Some(instance) = chromium {
        instance.stop();
    }
    if let Some(instance) = firefox {
        instance.stop();
    }
    if let Some((outcome, details)) = smoke_result {
        finish_smoke_test(invoker, outcome, details);
    }
    Ok(())
}

fn spawn_cef(
    connection: &RustConnection,
    parent: Window,
    bounds: NativeRect,
    url: &str,
    profile_id: &str,
    directories: &EngineDirectories,
    backend: CefBackend,
) -> NativeResult<CefInstance> {
    let executable = std::env::current_exe()?;
    let executable_directory = executable
        .parent()
        .ok_or("application executable has no parent directory")?;
    let loader_directory = backend.loader_directory(executable_directory)?;
    let helper = match backend {
        CefBackend::Chromium => executable_directory.join("cef-renderer"),
        CefBackend::Firefox => loader_directory
            .as_ref()
            .ok_or("FirefoxCEF runtime directory is unavailable")?
            .join("cef-renderer"),
    };
    let mut command = Command::new(helper);
    if let Some(directory) = &loader_directory {
        command.env("LD_LIBRARY_PATH", directory);
        let mut preload = vec![directory.join("libmozglue-cef.so")];
        if let Some(existing) = std::env::var_os("LD_PRELOAD") {
            preload.extend(std::env::split_paths(&existing));
        }
        command.env("LD_PRELOAD", std::env::join_paths(preload)?);
        command.env("DEB_CEF_SINGLE_THREADED", "1");
        command.env(
            "FIREFOX_CEF_APP_INI",
            directory.join("browser/firefox-cef.ini"),
        );
        command.env("GDK_BACKEND", "x11");
        command.env("MOZ_ENABLE_WAYLAND", "0");
    }
    let (transport, child_transport) = Transport::pair()?;
    configure_child_command(&mut command, &child_transport);
    let mut child = command.spawn()?;
    drop(child_transport);
    let (incoming, _reader) = spawn_protocol_reader(transport.try_clone()?);
    let readiness_timeout = match backend {
        CefBackend::Chromium => Duration::from_secs(30),
        CefBackend::Firefox => Duration::from_secs(90),
    };
    let (window, last_event_sequence) = match initialize_helper(
        &transport,
        &incoming,
        &mut child,
        readiness_timeout,
        backend,
        parent,
        bounds,
        url,
        profile_id,
        directories,
    ) {
        Ok(ready) => ready,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!("{} did not become ready: {error}", backend.name()).into());
        }
    };
    let native_child = connection.query_tree(window)?.reply()?.parent == parent;
    if !native_child {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("{} returned a window outside the Qt host", backend.name()).into());
    }
    connection.change_window_attributes(
        window,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::BUTTON_PRESS),
    )?;
    configure_native_window(connection, window, bounds)?;
    Ok(CefInstance {
        child,
        transport,
        incoming,
        window,
        native_child,
        browser_id: 1,
        next_request_id: 3,
        pending_requests: HashMap::new(),
        last_event_sequence,
        protocol_closed: false,
    })
}

fn spawn_protocol_reader(
    transport: Transport,
) -> (
    mpsc::Receiver<Result<wire::Packet, ProtocolError>>,
    JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        loop {
            let received = transport.receive();
            let failed = received.is_err();
            if sender.send(received).is_err() || failed {
                break;
            }
        }
    });
    (receiver, thread)
}

#[allow(clippy::too_many_arguments)]
fn initialize_helper(
    transport: &Transport,
    incoming: &mpsc::Receiver<Result<wire::Packet, ProtocolError>>,
    child: &mut Child,
    timeout: Duration,
    backend: CefBackend,
    parent: Window,
    bounds: NativeRect,
    url: &str,
    profile_id: &str,
    directories: &EngineDirectories,
) -> NativeResult<(Window, u64)> {
    transport.send(&wire::Packet {
        request_id: 1,
        body: Some(wire::packet::Body::Hello(wire::Hello {
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            requested_capabilities: required_capabilities(),
        })),
    })?;
    let deadline = Instant::now() + timeout;
    let hello = receive_startup_packet(incoming, child, deadline)?;
    if hello.request_id != 1 {
        return Err(format!(
            "HelloReply used request ID {}, expected 1",
            hello.request_id
        )
        .into());
    }
    let reply = match hello.body {
        Some(wire::packet::Body::HelloReply(reply)) => reply,
        Some(wire::packet::Body::Response(response)) => {
            return Err(format_response_error("protocol negotiation", response).into());
        }
        _ => return Err("helper did not answer Hello with HelloReply".into()),
    };
    validate_hello_reply(&reply, backend)?;

    transport.send(&request_packet(
        2,
        1,
        wire::request::Operation::CreateBrowser(wire::CreateBrowser {
            initial_url: url.to_owned(),
            x11: Some(wire::X11Target {
                parent_window: u64::from(parent),
                viewport: Some(bounds.viewport()),
            }),
            profile_id: profile_id.to_owned(),
            profile_data_path: protocol_path(&directories.data)?,
            profile_cache_path: protocol_path(&directories.cache)?,
        }),
    ))?;

    let mut create_succeeded = false;
    let mut window = None;
    let mut last_event_sequence = 0;
    while !create_succeeded || window.is_none() {
        let received = receive_startup_packet(incoming, child, deadline)?;
        let request_id = received.request_id;
        match received.body {
            Some(wire::packet::Body::Response(response)) => {
                if request_id != 2 {
                    return Err(format!(
                        "startup response used request ID {}, expected 2",
                        request_id
                    )
                    .into());
                }
                match response.result {
                    Some(wire::response::Result::Success(_)) => create_succeeded = true,
                    _ => return Err(format_response_error("browser creation", response).into()),
                }
            }
            Some(wire::packet::Body::Event(event)) => {
                if event.browser_id != 1 {
                    return Err(format!(
                        "startup event targets browser {}, expected 1",
                        event.browser_id
                    )
                    .into());
                }
                if event.sequence <= last_event_sequence {
                    return Err(format!(
                        "startup event sequence {} followed {}",
                        event.sequence, last_event_sequence
                    )
                    .into());
                }
                last_event_sequence = event.sequence;
                match event.value {
                    Some(wire::event::Value::SurfaceReady(ready)) => {
                        let raw_window = ready
                            .x11
                            .ok_or("helper did not return an X11 surface")?
                            .window;
                        window = Some(u32::try_from(raw_window)?);
                    }
                    Some(wire::event::Value::BrowserCrashed(crash)) => {
                        return Err(
                            format!("browser crashed during startup: {}", crash.reason).into()
                        );
                    }
                    Some(wire::event::Value::BrowserClosed(_)) => {
                        return Err("browser closed during startup".into());
                    }
                    Some(_) => {}
                    None => {}
                }
            }
            _ => return Err("unexpected packet during browser startup".into()),
        }
    }
    Ok((
        window.expect("surface readiness was checked"),
        last_event_sequence,
    ))
}

fn protocol_path(path: &Path) -> NativeResult<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("profile path is not valid UTF-8: {}", path.display()).into())
}

fn receive_startup_packet(
    incoming: &mpsc::Receiver<Result<wire::Packet, ProtocolError>>,
    child: &mut Child,
    deadline: Instant,
) -> NativeResult<wire::Packet> {
    while Instant::now() < deadline {
        match incoming.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(received)) => return Ok(received),
            Ok(Err(error)) => return Err(error.into()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("CEF protocol reader stopped".into());
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("CEF helper exited before readiness: {status}").into());
        }
    }
    Err("CEF helper readiness timed out".into())
}

fn validate_hello_reply(reply: &wire::HelloReply, backend: CefBackend) -> NativeResult<()> {
    let engine = Engine::try_from(reply.engine).unwrap_or(Engine::Unspecified);
    if engine != backend.engine() {
        return Err(format!("{} identified itself as {engine:?}", backend.name()).into());
    }
    if reply.cef_api_version == 0 || reply.maximum_packet_bytes == 0 {
        return Err("helper returned invalid CEF or packet-size metadata".into());
    }
    let advertised = reply.capabilities.iter().copied().collect::<HashSet<_>>();
    let missing = required_capabilities()
        .into_iter()
        .filter(|capability| !advertised.contains(capability))
        .filter_map(|capability| Capability::try_from(capability).ok())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!("helper is missing required capabilities: {missing:?}").into());
    }
    Ok(())
}

fn required_capabilities() -> Vec<i32> {
    [
        Capability::BrowserLifecycle,
        Capability::Navigation,
        Capability::Resize,
        Capability::Focus,
        Capability::LoadingEvents,
        Capability::NativeX11Surface,
        Capability::CookieSync,
    ]
    .into_iter()
    .map(|capability| capability as i32)
    .collect()
}

fn request_packet(
    request_id: u64,
    browser_id: u64,
    operation: wire::request::Operation,
) -> wire::Packet {
    wire::Packet {
        request_id,
        body: Some(wire::packet::Body::Request(wire::Request {
            browser_id,
            operation: Some(operation),
        })),
    }
}

fn format_response_error(context: &str, response: wire::Response) -> String {
    match response.result {
        Some(wire::response::Result::Error(error)) => {
            format!("{context} failed [{}]: {}", error.code, error.message)
        }
        Some(wire::response::Result::Success(_)) => {
            format!("{context} returned an unexpected success")
        }
        None => format!("{context} response has no result"),
    }
}

fn notice_status(prefix: &str, notices: Vec<ProtocolNotice>) -> Option<String> {
    notices
        .into_iter()
        .filter_map(|notice| match notice {
            ProtocolNotice::CommandFailed(error) => {
                Some(format!("{prefix} command failed: {error}"))
            }
            ProtocolNotice::LoadFailed(error) => Some(format!("{prefix} load failed: {error}")),
            ProtocolNotice::Closed => Some(format!("{prefix} closed")),
            ProtocolNotice::Crashed(reason) => Some(format!("{prefix} crashed: {reason}")),
            ProtocolNotice::ProtocolFailed(error) => {
                Some(format!("{prefix} protocol failed: {error}"))
            }
            ProtocolNotice::LoadingChanged(_) => None,
            ProtocolNotice::CookieSnapshotEntry(_)
            | ProtocolNotice::CookieSnapshotComplete
            | ProtocolNotice::CookieChanged(_, _) => None,
        })
        .next_back()
}

fn rendered_pixel_variants(
    connection: &RustConnection,
    instance: &CefInstance,
) -> NativeResult<usize> {
    let mut last_error = "browser surface was not capturable".to_owned();
    for _ in 0..5 {
        instance.ensure_visible(connection)?;
        connection.flush()?;
        sleep(Duration::from_millis(100));
        match sampled_pixel_variants(connection, instance.window) {
            Ok(variants) if variants >= 8 => return Ok(variants),
            Ok(variants) => {
                last_error = format!("browser surface has only {variants} sampled pixel variants")
            }
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(last_error.into())
}

fn sampled_pixel_variants(connection: &RustConnection, window: Window) -> NativeResult<usize> {
    let geometry = connection.get_geometry(window)?.reply()?;
    let width = geometry.width.saturating_sub(8).min(512);
    let height = geometry.height.saturating_sub(8).min(512);
    if width < 2 || height < 2 {
        return Err("browser surface has no drawable area".into());
    }
    let image = connection
        .get_image(ImageFormat::Z_PIXMAP, window, 4, 4, width, height, u32::MAX)?
        .reply()?;
    Ok(image
        .data
        .chunks_exact(4)
        .step_by(97)
        .map(|pixel| [pixel[0], pixel[1], pixel[2], pixel[3]])
        .collect::<HashSet<_>>()
        .len())
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

fn update_statuses(invoker: &QmlMethodInvoker, chromium: String, firefox: String) {
    invoke_method!(invoker, "update_statuses", chromium, firefox);
}

fn automated_smoke_requested() -> bool {
    std::env::var("DEB_AUTOMATED_SMOKE_TEST").as_deref() == Ok("1")
}

fn finish_smoke_test(invoker: &QmlMethodInvoker, outcome: &str, details: String) {
    invoke_method!(invoker, "finish_smoke_test", outcome.to_owned(), details);
}

fn stop_child(child: &mut Child) {
    let deadline = Instant::now() + Duration::from_secs(15);
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
    use super::{AutomatedSmokeTest, CefBackend, NativeRect, ProtocolNotice, validate_hello_reply};
    use shell_protocol::{MAX_PACKET_BYTES, wire};

    #[test]
    fn rejects_zero_sized_surfaces() {
        assert!(NativeRect::new(0, 0, 0, 100).is_none());
        assert!(NativeRect::new(0, 0, 100, 1).is_none());
    }

    #[test]
    fn converts_bounds_to_a_viewport() {
        let bounds = NativeRect::new(10, 20, 800, 600).unwrap();
        assert_eq!(
            bounds.viewport(),
            wire::Viewport {
                x: 10,
                y: 20,
                width: 800,
                height: 600,
                scale_factor: 1.0,
            }
        );
    }

    #[test]
    fn rejects_a_helper_with_the_wrong_engine() {
        let reply = wire::HelloReply {
            engine: wire::Engine::Gecko as i32,
            engine_version: "test".to_owned(),
            cef_api_version: 1,
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            capabilities: super::required_capabilities(),
        };
        let error = validate_hello_reply(&reply, CefBackend::Chromium)
            .unwrap_err()
            .to_string();
        assert!(error.contains("identified itself as Gecko"));
    }

    #[test]
    fn rejects_a_helper_missing_a_required_capability() {
        let mut capabilities = super::required_capabilities();
        capabilities.retain(|capability| *capability != wire::Capability::NativeX11Surface as i32);
        let reply = wire::HelloReply {
            engine: wire::Engine::Chromium as i32,
            engine_version: "test".to_owned(),
            cef_api_version: 1,
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            capabilities,
        };
        let error = validate_hello_reply(&reply, CefBackend::Chromium)
            .unwrap_err()
            .to_string();
        assert!(error.contains("NativeX11Surface"));
    }

    #[test]
    fn smoke_test_requires_new_load_cycles_from_both_engines() {
        let mut smoke = AutomatedSmokeTest::new("deb://new-tab/".to_owned());
        smoke
            .observe(
                CefBackend::Chromium,
                &[ProtocolNotice::LoadingChanged(false)],
            )
            .unwrap();
        assert!(!smoke.initial_loads_settled());
        smoke
            .observe(
                CefBackend::Firefox,
                &[ProtocolNotice::LoadingChanged(false)],
            )
            .unwrap();
        assert!(smoke.initial_loads_settled());

        smoke.navigation_sent = true;
        smoke
            .observe(
                CefBackend::Chromium,
                &[ProtocolNotice::LoadingChanged(false)],
            )
            .unwrap();
        assert!(!smoke.navigations_settled());
        for backend in [CefBackend::Chromium, CefBackend::Firefox] {
            smoke
                .observe(
                    backend,
                    &[
                        ProtocolNotice::LoadingChanged(true),
                        ProtocolNotice::LoadingChanged(false),
                    ],
                )
                .unwrap();
        }
        assert!(smoke.navigations_settled());
    }

    #[test]
    fn smoke_test_fails_on_backend_errors() {
        let mut smoke = AutomatedSmokeTest::new("deb://new-tab/".to_owned());
        let error = smoke
            .observe(
                CefBackend::Firefox,
                &[ProtocolNotice::LoadFailed("broken page".to_owned())],
            )
            .unwrap_err();
        assert_eq!(error, "Gecko: broken page");
    }
}
