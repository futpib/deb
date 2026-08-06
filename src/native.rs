use crate::profile::{EngineDirectories, ProfileDirectories};
use shell_protocol::{
    MAX_PACKET_BYTES, ProtocolError, Transport, configure_child_command,
    wire::{self, Capability, Engine},
};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::mpsc::{self, RecvTimeoutError, TryRecvError},
    thread::{JoinHandle, sleep},
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{
        ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, EventMask, ImageFormat,
        StackMode, Window,
    },
    rust_connection::RustConnection,
};

pub(crate) type NativeResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

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

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum CefBackend {
    Chromium,
    Firefox,
}

impl CefBackend {
    pub(crate) fn label(self) -> &'static str {
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

    pub(crate) fn directories(self, profile: &ProfileDirectories) -> &EngineDirectories {
        match self {
            Self::Chromium => &profile.chromium,
            Self::Firefox => &profile.firefox,
        }
    }
}

pub(crate) struct CefInstance {
    child: Child,
    transport: Transport,
    incoming: mpsc::Receiver<Result<wire::Packet, ProtocolError>>,
    window: Window,
    browser_id: u64,
    next_request_id: u64,
    pending_requests: HashMap<u64, (u64, &'static str)>,
    last_event_sequence: u64,
    protocol_closed: bool,
}

impl CefInstance {
    fn send_browser_request(
        &mut self,
        browser_id: u64,
        operation: wire::request::Operation,
        description: &'static str,
    ) -> NativeResult<()> {
        let request_id = self.next_request_id;
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .ok_or("shell protocol request ID overflow")?;
        self.transport
            .send(&request_packet(request_id, browser_id, operation))?;
        self.pending_requests
            .insert(request_id, (browser_id, description));
        Ok(())
    }

    fn send_process_request(
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
            .send(&request_packet(request_id, 0, operation))?;
        self.pending_requests.insert(request_id, (0, description));
        Ok(())
    }

    pub(crate) fn create_browser(
        &mut self,
        browser_id: u64,
        parent: Window,
        bounds: NativeRect,
        url: &str,
        profile_id: &str,
        directories: &EngineDirectories,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
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
            "browser creation",
        )
    }

    pub(crate) fn navigate_browser(&mut self, browser_id: u64, url: &str) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::Navigate(wire::Navigate {
                url: url.to_owned(),
            }),
            "navigation",
        )
    }

    pub(crate) fn resize_browser(
        &mut self,
        browser_id: u64,
        bounds: NativeRect,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::Resize(wire::Resize {
                viewport: Some(bounds.viewport()),
            }),
            "resize",
        )
    }

    pub(crate) fn focus_browser(&mut self, browser_id: u64, focused: bool) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::SetFocus(wire::SetFocus { focused }),
            "focus",
        )
    }

    pub(crate) fn set_browser_visible(
        &mut self,
        browser_id: u64,
        visible: bool,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::SetVisibility(wire::SetVisibility { visible }),
            "visibility",
        )
    }

    pub(crate) fn close_browser(&mut self, browser_id: u64, force: bool) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::Close(wire::Close { force }),
            "close",
        )
    }

    pub(crate) fn read_browser_cookies(&mut self, browser_id: u64) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::ReadCookies(wire::ReadCookies {}),
            "cookie snapshot",
        )
    }

    pub(crate) fn set_browser_cookie(
        &mut self,
        browser_id: u64,
        cookie: wire::Cookie,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::SetCookie(wire::SetCookie {
                cookie: Some(cookie),
            }),
            "cookie set",
        )
    }

    pub(crate) fn delete_browser_cookie(
        &mut self,
        browser_id: u64,
        cookie: wire::Cookie,
    ) -> NativeResult<()> {
        self.send_browser_request(
            browser_id,
            wire::request::Operation::DeleteCookie(wire::DeleteCookie {
                cookie: Some(cookie),
            }),
            "cookie delete",
        )
    }

    pub(crate) fn exited(&mut self) -> NativeResult<Option<std::process::ExitStatus>> {
        Ok(self.child.try_wait()?)
    }

    pub(crate) fn drain_routed_notices(&mut self) -> Vec<RoutedNotice> {
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
                    notices.push(RoutedNotice {
                        browser_id: 0,
                        value: ProtocolNotice::ProtocolFailed(error.to_string()),
                    });
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.protocol_closed = true;
                    notices.push(RoutedNotice {
                        browser_id: 0,
                        value: ProtocolNotice::ProtocolFailed("protocol reader stopped".to_owned()),
                    });
                    break;
                }
            }
        }
        notices
    }

    fn handle_packet(&mut self, received: wire::Packet) -> Option<RoutedNotice> {
        let request_id = received.request_id;
        let (browser_id, value) = match received.body {
            Some(wire::packet::Body::Response(response)) => {
                let Some((browser_id, description)) = self.pending_requests.remove(&request_id)
                else {
                    return Some(RoutedNotice {
                        browser_id: 0,
                        value: ProtocolNotice::ProtocolFailed(format!(
                            "unsolicited response for request {request_id}"
                        )),
                    });
                };
                let value = match response.result {
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
                };
                (browser_id, value)
            }
            Some(wire::packet::Body::Event(event)) => {
                if event.sequence <= self.last_event_sequence {
                    return Some(RoutedNotice {
                        browser_id: event.browser_id,
                        value: ProtocolNotice::ProtocolFailed(format!(
                            "event sequence {} followed {}",
                            event.sequence, self.last_event_sequence
                        )),
                    });
                }
                self.last_event_sequence = event.sequence;
                let browser_id = event.browser_id;
                let value = match event.value {
                    Some(wire::event::Value::SurfaceReady(surface)) => match surface.x11 {
                        Some(surface) => Some(ProtocolNotice::SurfaceReady(surface.window)),
                        None => Some(ProtocolNotice::ProtocolFailed(
                            "surface readiness has no X11 window".to_owned(),
                        )),
                    },
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
                    Some(wire::event::Value::NavigationCommitted(navigation)) => {
                        Some(ProtocolNotice::NavigationCommitted(navigation.url))
                    }
                    Some(wire::event::Value::TitleChanged(title)) => {
                        Some(ProtocolNotice::TitleChanged(title.title))
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
                    None => None,
                };
                (browser_id, value)
            }
            Some(_) => (
                0,
                Some(ProtocolNotice::ProtocolFailed(
                    "unexpected runtime packet type".to_owned(),
                )),
            ),
            None => (
                0,
                Some(ProtocolNotice::ProtocolFailed(
                    "runtime packet has no body".to_owned(),
                )),
            ),
        };
        value.map(|value| RoutedNotice { browser_id, value })
    }

    pub(crate) fn shutdown(mut self) {
        let _ = self.send_process_request(
            wire::request::Operation::Shutdown(wire::Shutdown {}),
            "shutdown",
        );
        stop_child(&mut self.child);
    }

    pub(crate) fn initial_window(&self) -> Window {
        self.window
    }

    pub(crate) fn initial_browser_id(&self) -> u64 {
        self.browser_id
    }

    pub(crate) fn process_id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn protocol_closed(&self) -> bool {
        self.protocol_closed
    }
}

pub(crate) struct RoutedNotice {
    pub(crate) browser_id: u64,
    pub(crate) value: ProtocolNotice,
}

pub(crate) enum ProtocolNotice {
    CommandFailed(String),
    SurfaceReady(u64),
    LoadingChanged(bool),
    NavigationCommitted(String),
    TitleChanged(String),
    LoadFailed(String),
    Closed,
    Crashed(String),
    ProtocolFailed(String),
    CookieSnapshotEntry(wire::Cookie),
    CookieSnapshotComplete,
    CookieChanged(wire::Cookie, wire::CookieChangeCause),
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_cef_browser(
    connection: &RustConnection,
    parent: Window,
    bounds: NativeRect,
    url: &str,
    profile_id: &str,
    directories: &EngineDirectories,
    backend: CefBackend,
    browser_id: u64,
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
        browser_id,
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
        browser_id,
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
    browser_id: u64,
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
        browser_id,
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
                if event.browser_id != browser_id {
                    return Err(format!(
                        "startup event targets browser {}, expected {}",
                        event.browser_id, browser_id
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
        Capability::MultipleBrowsers,
        Capability::Visibility,
        Capability::RendererCrashEvents,
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

pub(crate) fn sampled_pixel_variants(
    connection: &RustConnection,
    window: Window,
) -> NativeResult<usize> {
    let geometry = connection.get_geometry(window)?.reply()?;
    let tree = connection.query_tree(window)?.reply()?;
    let position = connection
        .translate_coordinates(window, tree.root, 0, 0)?
        .reply()?;
    let x = position.dst_x.checked_add(4).ok_or("surface x overflow")?;
    let y = position.dst_y.checked_add(4).ok_or("surface y overflow")?;
    if x < 0 || y < 0 {
        return Err("browser surface is outside the root window".into());
    }
    let root_geometry = connection.get_geometry(tree.root)?.reply()?;
    let available_width = u16::try_from(i32::from(root_geometry.width) - i32::from(x))?;
    let available_height = u16::try_from(i32::from(root_geometry.height) - i32::from(y))?;
    let width = geometry
        .width
        .saturating_sub(8)
        .min(available_width)
        .min(512);
    let height = geometry
        .height
        .saturating_sub(8)
        .min(available_height)
        .min(512);
    if width < 2 || height < 2 {
        return Err("browser surface has no drawable area".into());
    }
    let image = connection
        .get_image(
            ImageFormat::Z_PIXMAP,
            tree.root,
            x,
            y,
            width,
            height,
            u32::MAX,
        )?
        .reply()?;
    Ok(image
        .data
        .chunks_exact(4)
        .step_by(97)
        .map(|pixel| [pixel[0], pixel[1], pixel[2], pixel[3]])
        .collect::<HashSet<_>>()
        .len())
}

pub(crate) fn configure_native_window(
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
    use super::{CefBackend, NativeRect, validate_hello_reply};
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
}
