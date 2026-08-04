use qtbridge::{QmlMethodInvoker, invoke_method};
use shell_protocol::{
    MAX_PACKET_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, ProtocolError, ReceivedPacket, Transport,
    configure_child_command,
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
            ChangeWindowAttributesAux, ConfigureWindowAux, ConnectionExt, EventMask, InputFocus,
            MapState, StackMode, Window,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CefBackend {
    Chromium,
    Firefox,
}

impl CefBackend {
    fn loader_directory(self, executable_directory: &Path) -> NativeResult<Option<PathBuf>> {
        if matches!(self, Self::Chromium) {
            return Ok(None);
        }
        let configured = std::env::var_os("DUAL_ENGINE_FIREFOX_RUNTIME")
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
}

struct CefInstance {
    child: Child,
    transport: Transport,
    incoming: mpsc::Receiver<Result<ReceivedPacket, ProtocolError>>,
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

    fn handle_packet(&mut self, received: ReceivedPacket) -> Option<ProtocolNotice> {
        if !received.files.is_empty() {
            return Some(ProtocolNotice::ProtocolFailed(
                "unexpected attached files".to_owned(),
            ));
        }
        match received.packet.body {
            Some(wire::packet::Body::Response(response)) => {
                let Some(description) = self.pending_requests.remove(&received.packet.request_id)
                else {
                    return Some(ProtocolNotice::ProtocolFailed(format!(
                        "unsolicited response for request {}",
                        received.packet.request_id
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
    LoadFailed(String),
    Closed,
    Crashed(String),
    ProtocolFailed(String),
}

pub fn spawn_controller(
    url: String,
    chromium_parent: Window,
    firefox_parent: Window,
    layout: Layout,
    invoker: QmlMethodInvoker,
) -> Controller {
    let (sender, receiver) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        if let Err(error) = run_controller(
            url,
            chromium_parent,
            firefox_parent,
            layout,
            &invoker,
            receiver,
        ) {
            update_statuses(
                &invoker,
                format!("Native controller failed: {error}"),
                format!("Native controller failed: {error}"),
            );
        }
    });
    Controller { sender, thread }
}

fn run_controller(
    initial_url: String,
    chromium_parent: Window,
    firefox_parent: Window,
    initial_layout: Layout,
    invoker: &QmlMethodInvoker,
    receiver: mpsc::Receiver<ControllerCommand>,
) -> NativeResult<()> {
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
        CefBackend::Chromium,
    ) {
        Ok(instance) => {
            chromium_status = "Live · libcef.so / Chromium · shared CEF helper".to_owned();
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
        CefBackend::Firefox,
    ) {
        Ok(instance) => {
            firefox_status = "Live · libcef.so / Gecko · native X11 child".to_owned();
            Some(instance)
        }
        Err(error) => {
            firefox_status = format!("Firefox CEF adapter failed: {error}");
            None
        }
    };
    update_statuses(invoker, chromium_status.clone(), firefox_status.clone());

    if let Ok(smoke_url) = std::env::var("DUAL_ENGINE_SMOKE_NAVIGATE_URL") {
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

    loop {
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
                                "Live · libcef.so / Chromium · shared CEF helper".to_owned()
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
                                "Live · libcef.so / Gecko · native X11 child".to_owned()
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

        if let Some(instance) = &mut chromium
            && let Some(status) = notice_status("Chromium CEF", instance.drain_notices())
        {
            chromium_status = status;
            update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
        }
        if let Some(instance) = &mut firefox
            && let Some(status) = notice_status("Firefox CEF adapter", instance.drain_notices())
        {
            firefox_status = status;
            update_statuses(invoker, chromium_status.clone(), firefox_status.clone());
        }
    }

    if let Some(instance) = chromium {
        instance.stop();
    }
    if let Some(instance) = firefox {
        instance.stop();
    }
    Ok(())
}

fn spawn_cef(
    connection: &RustConnection,
    parent: Window,
    bounds: NativeRect,
    url: &str,
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
        command.env("DUAL_ENGINE_CEF_SINGLE_THREADED", "1");
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
    mpsc::Receiver<Result<ReceivedPacket, ProtocolError>>,
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
    incoming: &mpsc::Receiver<Result<ReceivedPacket, ProtocolError>>,
    child: &mut Child,
    timeout: Duration,
    backend: CefBackend,
    parent: Window,
    bounds: NativeRect,
    url: &str,
) -> NativeResult<(Window, u64)> {
    transport.send(&wire::Packet {
        request_id: 1,
        attached_files: Vec::new(),
        body: Some(wire::packet::Body::Hello(wire::Hello {
            minimum_major: PROTOCOL_MAJOR,
            maximum_major: PROTOCOL_MAJOR,
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            requested_capabilities: required_capabilities(),
        })),
    })?;
    let deadline = Instant::now() + timeout;
    let hello = receive_startup_packet(incoming, child, deadline)?;
    if !hello.files.is_empty() {
        return Err("HelloReply unexpectedly carried files".into());
    }
    if hello.packet.request_id != 1 {
        return Err(format!(
            "HelloReply used request ID {}, expected 1",
            hello.packet.request_id
        )
        .into());
    }
    let reply = match hello.packet.body {
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
            surface: Some(wire::SurfaceTarget {
                target: Some(wire::surface_target::Target::X11(wire::X11Target {
                    parent_window: u64::from(parent),
                    viewport: Some(bounds.viewport()),
                })),
            }),
        }),
    ))?;

    let mut create_succeeded = false;
    let mut window = None;
    let mut last_event_sequence = 0;
    while !create_succeeded || window.is_none() {
        let received = receive_startup_packet(incoming, child, deadline)?;
        if !received.files.is_empty() {
            return Err("browser startup packet unexpectedly carried files".into());
        }
        match received.packet.body {
            Some(wire::packet::Body::Response(response)) => {
                if received.packet.request_id != 2 {
                    return Err(format!(
                        "startup response used request ID {}, expected 2",
                        received.packet.request_id
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
                        let raw_window = match ready.presentation {
                            Some(wire::surface_ready::Presentation::X11(surface)) => surface.window,
                            _ => return Err("helper did not return an X11 surface".into()),
                        };
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

fn receive_startup_packet(
    incoming: &mpsc::Receiver<Result<ReceivedPacket, ProtocolError>>,
    child: &mut Child,
    deadline: Instant,
) -> NativeResult<ReceivedPacket> {
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
    if reply.major != PROTOCOL_MAJOR {
        return Err(format!(
            "helper selected protocol {}.{}, shell requires {PROTOCOL_MAJOR}.{PROTOCOL_MINOR}",
            reply.major, reply.minor
        )
        .into());
    }
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
        Capability::FileDescriptorPassing,
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
        attached_files: Vec::new(),
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
        .map(|notice| match notice {
            ProtocolNotice::CommandFailed(error) => format!("{prefix} command failed: {error}"),
            ProtocolNotice::LoadFailed(error) => format!("{prefix} load failed: {error}"),
            ProtocolNotice::Closed => format!("{prefix} closed"),
            ProtocolNotice::Crashed(reason) => format!("{prefix} crashed: {reason}"),
            ProtocolNotice::ProtocolFailed(error) => format!("{prefix} protocol failed: {error}"),
        })
        .next_back()
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
    use shell_protocol::{MAX_PACKET_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, wire};

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
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
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
        capabilities
            .retain(|capability| *capability != wire::Capability::FileDescriptorPassing as i32);
        let reply = wire::HelloReply {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            engine: wire::Engine::Chromium as i32,
            engine_version: "test".to_owned(),
            cef_api_version: 1,
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            capabilities,
        };
        let error = validate_hello_reply(&reply, CefBackend::Chromium)
            .unwrap_err()
            .to_string();
        assert!(error.contains("FileDescriptorPassing"));
    }
}
