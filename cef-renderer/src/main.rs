use cef::{args::Args, *};
use shell_protocol::{
    MAX_PACKET_BYTES, PROTOCOL_MAJOR, PROTOCOL_MINOR, Transport,
    wire::{self, Capability, Engine},
};
use std::{
    error::Error,
    os::fd::RawFd,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Bounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

struct Config {
    control_fd: RawFd,
}

impl Config {
    fn from_args() -> Result<Self, Box<dyn Error>> {
        let mut control_fd = None;
        let mut args = std::env::args().skip(1);

        while let Some(argument) = args.next() {
            if argument == "--control-fd" {
                control_fd = Some(
                    args.next()
                        .ok_or("--control-fd requires a value")?
                        .parse()?,
                );
            }
        }

        Ok(Self {
            control_fd: control_fd.ok_or("--control-fd is required")?,
        })
    }
}

fn bounds_from_viewport(viewport: Option<wire::Viewport>) -> Result<Bounds, Box<dyn Error>> {
    let viewport = viewport.ok_or("surface viewport is required")?;
    if viewport.width < 2 || viewport.height < 2 {
        return Err("surface viewport must be at least 2x2".into());
    }
    Ok(Bounds {
        x: viewport.x,
        y: viewport.y,
        width: viewport.width,
        height: viewport.height,
    })
}

#[derive(Clone)]
enum ControlCommand {
    Navigate(String),
    Resize(Bounds),
    Focus(bool),
    Reload,
    Quit(bool),
}

fn control_command(request: wire::Request) -> Result<ControlCommand, Box<dyn Error>> {
    match request.operation.ok_or("request operation is required")? {
        wire::request::Operation::Navigate(command) => Ok(ControlCommand::Navigate(command.url)),
        wire::request::Operation::Resize(command) => Ok(ControlCommand::Resize(
            bounds_from_viewport(command.viewport)?,
        )),
        wire::request::Operation::SetFocus(command) => Ok(ControlCommand::Focus(command.focused)),
        wire::request::Operation::Reload(_) => Ok(ControlCommand::Reload),
        wire::request::Operation::Close(command) => Ok(ControlCommand::Quit(command.force)),
        wire::request::Operation::CreateBrowser(_) => {
            Err("a browser already exists in this helper".into())
        }
    }
}

struct BrowserConfig {
    request_id: u64,
    browser_id: u64,
    url: String,
    parent: u64,
    bounds: Bounds,
}

#[derive(Clone)]
struct ProtocolEmitter {
    transport: Arc<Mutex<Transport>>,
    browser_id: u64,
    next_sequence: Arc<AtomicU64>,
}

impl ProtocolEmitter {
    fn new(transport: Transport, browser_id: u64) -> Self {
        Self {
            transport: Arc::new(Mutex::new(transport)),
            browser_id,
            next_sequence: Arc::new(AtomicU64::new(1)),
        }
    }

    fn send(&self, packet: wire::Packet) -> Result<(), Box<dyn Error + Send + Sync>> {
        self.transport
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .send(&packet)?;
        Ok(())
    }

    fn success(&self, request_id: u64) {
        if let Err(error) = self.send(response_packet(
            request_id,
            wire::response::Result::Success(wire::Success {}),
        )) {
            eprintln!("cef-renderer: protocol response failed: {error}");
        }
    }

    fn error(&self, request_id: u64, code: &str, message: impl Into<String>) {
        if let Err(error) = self.send(response_packet(
            request_id,
            wire::response::Result::Error(wire::Error {
                code: code.to_owned(),
                message: message.into(),
                retryable: false,
                backend_code: String::new(),
            }),
        )) {
            eprintln!("cef-renderer: protocol error response failed: {error}");
        }
    }

    fn event(&self, value: wire::event::Value) {
        let packet = wire::Packet {
            request_id: 0,
            attached_files: Vec::new(),
            body: Some(wire::packet::Body::Event(wire::Event {
                sequence: self.next_sequence.fetch_add(1, Ordering::Relaxed),
                browser_id: self.browser_id,
                value: Some(value),
            })),
        };
        if let Err(error) = self.send(packet) {
            eprintln!("cef-renderer: protocol event failed: {error}");
        }
    }
}

fn response_packet(request_id: u64, result: wire::response::Result) -> wire::Packet {
    wire::Packet {
        request_id,
        attached_files: Vec::new(),
        body: Some(wire::packet::Body::Response(wire::Response {
            result: Some(result),
        })),
    }
}

fn error_packet(request_id: u64, code: &str, message: impl Into<String>) -> wire::Packet {
    response_packet(
        request_id,
        wire::response::Result::Error(wire::Error {
            code: code.to_owned(),
            message: message.into(),
            retryable: false,
            backend_code: String::new(),
        }),
    )
}

wrap_life_span_handler! {
    struct BrowserLifeSpanHandler {
        browser: Arc<(Mutex<Option<Browser>>, Condvar)>,
        closed: Arc<(Mutex<bool>, Condvar)>,
        emitter: ProtocolEmitter,
    }

    impl LifeSpanHandler {
        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else {
                return;
            };
            let Some(host) = browser.host() else {
                return;
            };
            let native_window = host.window_handle();
            if native_window == 0 {
                return;
            }
            host.set_focus(1);
            *self
                .browser
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(browser.clone());
            self.browser.1.notify_all();
            self.emitter.event(wire::event::Value::SurfaceReady(
                wire::SurfaceReady {
                    presentation: Some(wire::surface_ready::Presentation::X11(
                        wire::X11Surface {
                            window: native_window,
                        },
                    )),
                },
            ));
            eprintln!("cef-renderer: native browser ready");
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.browser
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            *self
                .closed
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = true;
            self.closed.1.notify_all();
            self.emitter
                .event(wire::event::Value::BrowserClosed(wire::BrowserClosed {}));
        }
    }
}

wrap_load_handler! {
    struct BrowserLoadHandler {
        emitter: ProtocolEmitter,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: i32,
            _can_go_back: i32,
            _can_go_forward: i32,
        ) {
            self.emitter
                .event(wire::event::Value::LoadingChanged(wire::LoadingChanged {
                    loading: is_loading != 0,
                }));
            if is_loading == 0 {
                eprintln!("cef-renderer: page load settled");
            }
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut cef::Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            if error_code == Errorcode::ABORTED {
                return;
            }
            let error_text = error_text.map(CefString::to_string).unwrap_or_default();
            let failed_url = failed_url.map(CefString::to_string).unwrap_or_default();
            let raw_error: sys::cef_errorcode_t = error_code.into();
            self.emitter
                .event(wire::event::Value::LoadFailed(wire::LoadFailed {
                    error_code: raw_error as i32,
                    error_text: error_text.clone(),
                    failed_url: failed_url.clone(),
                }));
            eprintln!(
                "cef-renderer: load error {error_code:?}: {} ({})",
                error_text, failed_url,
            );
        }
    }
}

wrap_client! {
    struct BrowserClient {
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }
    }
}

wrap_browser_process_handler! {
    struct NativeBrowserProcessHandler {
        ready: Arc<AtomicBool>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            self.ready.store(true, Ordering::Release);
        }
    }
}

wrap_app! {
    struct BrowserApp {
        ready: Arc<AtomicBool>,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else {
                return;
            };

            command_line.append_switch(Some(&"disable-session-crashed-bubble".into()));
            command_line.append_switch(Some(&"disable-gpu".into()));
            command_line.append_switch(Some(&"disable-gpu-compositing".into()));
            command_line.append_switch(Some(&"hide-crash-restore-bubble".into()));
            command_line.append_switch(Some(&"noerrdialogs".into()));
            command_line.append_switch_with_value(
                Some(&"password-store".into()),
                Some(&"basic".into()),
            );
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(NativeBrowserProcessHandler::new(self.ready.clone()))
        }
    }
}

wrap_task! {
    struct BrowserCommandTask {
        browser: Browser,
        command: ControlCommand,
        request_id: u64,
        emitter: ProtocolEmitter,
    }

    impl Task {
        fn execute(&self) {
            let result = match &self.command {
                ControlCommand::Navigate(url) => {
                    if let Some(frame) = self.browser.main_frame() {
                        frame.load_url(Some(&CefString::from(url.as_str())));
                    } else {
                        self.emitter.error(
                            self.request_id,
                            "NO_MAIN_FRAME",
                            "CEF browser has no main frame",
                        );
                        return;
                    }
                    if let Some(host) = self.browser.host() {
                        host.set_focus(1);
                    }
                    Ok(())
                }
                ControlCommand::Resize(_bounds) => resize_browser(&self.browser),
                ControlCommand::Focus(focused) => {
                    if let Some(host) = self.browser.host() {
                        host.set_focus(i32::from(*focused));
                        if *focused {
                            host.notify_move_or_resize_started();
                        }
                        Ok(())
                    } else {
                        Err("CEF browser has no host".into())
                    }
                }
                ControlCommand::Reload => {
                    self.browser.reload();
                    Ok(())
                }
                ControlCommand::Quit(force) => {
                    if self.request_id != 0 {
                        self.emitter.success(self.request_id);
                    }
                    if let Some(host) = self.browser.host() {
                        host.close_browser(i32::from(*force));
                    } else {
                        quit_message_loop();
                    }
                    return;
                }
            };
            match result {
                Ok(()) => self.emitter.success(self.request_id),
                Err(error) => self
                    .emitter
                    .error(self.request_id, "BACKEND_REJECTED", error.to_string()),
            }
        }
    }
}

fn resize_browser(browser: &Browser) -> Result<(), Box<dyn Error>> {
    let host = browser.host().ok_or("CEF browser has no host")?;
    if host.window_handle() == 0 {
        return Err("CEF browser has no native window yet".into());
    }
    host.notify_move_or_resize_started();
    Ok(())
}

fn advertised_capabilities() -> Vec<i32> {
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

fn negotiate_protocol(transport: &Transport, engine: Engine) -> Result<(), Box<dyn Error>> {
    let received = transport.receive()?;
    if !received.files.is_empty() {
        return Err("hello packet must not carry files".into());
    }
    let request_id = received.packet.request_id;
    let hello = match received.packet.body {
        Some(wire::packet::Body::Hello(hello)) => hello,
        _ => return Err("first shell protocol packet must be Hello".into()),
    };
    if request_id == 0 {
        return Err("hello request ID must be nonzero".into());
    }
    if hello.minimum_major > PROTOCOL_MAJOR || hello.maximum_major < PROTOCOL_MAJOR {
        transport.send(&error_packet(
            request_id,
            "UNSUPPORTED_PROTOCOL",
            format!(
                "helper supports protocol {PROTOCOL_MAJOR}; shell offered {} through {}",
                hello.minimum_major, hello.maximum_major
            ),
        ))?;
        return Err("shell and helper have no compatible protocol version".into());
    }
    transport.send(&wire::Packet {
        request_id,
        attached_files: Vec::new(),
        body: Some(wire::packet::Body::HelloReply(wire::HelloReply {
            major: PROTOCOL_MAJOR,
            minor: PROTOCOL_MINOR,
            engine: engine as i32,
            engine_version: match engine {
                Engine::Chromium => "Chromium through CEF".to_owned(),
                Engine::Gecko => "Gecko through FirefoxCEF".to_owned(),
                Engine::Unspecified => String::new(),
            },
            cef_api_version: sys::CEF_API_VERSION_LAST as u32,
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            capabilities: advertised_capabilities(),
        })),
    })?;
    Ok(())
}

fn receive_browser_config(transport: &Transport) -> Result<BrowserConfig, Box<dyn Error>> {
    let received = transport.receive()?;
    if !received.files.is_empty() {
        return Err("CreateBrowser packet must not carry files".into());
    }
    let request_id = received.packet.request_id;
    let parsed = (|| -> Result<BrowserConfig, Box<dyn Error>> {
        if request_id == 0 {
            return Err("CreateBrowser request ID must be nonzero".into());
        }
        let request = match received.packet.body {
            Some(wire::packet::Body::Request(request)) => request,
            _ => return Err("second shell protocol packet must be a request".into()),
        };
        if request.browser_id == 0 {
            return Err("browser ID must be nonzero".into());
        }
        let create = match request.operation {
            Some(wire::request::Operation::CreateBrowser(create)) => create,
            _ => return Err("first shell request must be CreateBrowser".into()),
        };
        let surface = create.surface.ok_or("CreateBrowser surface is required")?;
        let x11 = match surface.target {
            Some(wire::surface_target::Target::X11(x11)) => x11,
            _ => return Err("helper currently requires an X11 surface target".into()),
        };
        if x11.parent_window == 0 {
            return Err("X11 parent window must be nonzero".into());
        }
        Ok(BrowserConfig {
            request_id,
            browser_id: request.browser_id,
            url: if create.initial_url.is_empty() {
                "about:blank".to_owned()
            } else {
                create.initial_url
            },
            parent: x11.parent_window,
            bounds: bounds_from_viewport(x11.viewport)?,
        })
    })();
    if let Err(error) = &parsed
        && request_id != 0
    {
        transport.send(&error_packet(
            request_id,
            "INVALID_CREATE_BROWSER",
            error.to_string(),
        ))?;
    }
    parsed
}

fn run() -> Result<i32, Box<dyn Error>> {
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let cef_args = Args::new();
    let context_ready = Arc::new(AtomicBool::new(false));
    let mut app = BrowserApp::new(context_ready.clone());
    let process_code = execute_process(
        Some(cef_args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if process_code >= 0 {
        return Ok(process_code);
    }

    let config = Config::from_args()?;
    let transport = unsafe { Transport::from_raw_fd(config.control_fd)? };
    let single_threaded = std::env::var_os("DUAL_ENGINE_CEF_SINGLE_THREADED").is_some();
    let engine = if single_threaded {
        Engine::Gecko
    } else {
        Engine::Chromium
    };
    negotiate_protocol(&transport, engine)?;
    let browser_config = receive_browser_config(&transport)?;
    let command_transport = transport.try_clone()?;
    let emitter = ProtocolEmitter::new(transport, browser_config.browser_id);
    let runtime_path = std::env::current_exe()?
        .parent()
        .ok_or("CEF executable has no parent directory")?
        .to_path_buf();
    let cache_path =
        std::env::temp_dir().join(format!("dual-engine-browser-cef-{}", std::process::id()));
    std::fs::create_dir_all(&cache_path)?;
    let remote_debugging_port = std::env::var("DUAL_ENGINE_CEF_DEBUG_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    let settings = Settings {
        no_sandbox: 1,
        multi_threaded_message_loop: i32::from(!single_threaded),
        root_cache_path: CefString::from(cache_path.to_string_lossy().as_ref()),
        resources_dir_path: CefString::from(runtime_path.to_string_lossy().as_ref()),
        locales_dir_path: CefString::from(runtime_path.join("locales").to_string_lossy().as_ref()),
        log_file: CefString::from("/dev/stderr"),
        log_severity: LogSeverity::INFO,
        background_color: 0xffff_ffff,
        remote_debugging_port,
        ..Default::default()
    };
    if initialize(
        Some(cef_args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) != 1
    {
        emitter.error(
            browser_config.request_id,
            "INITIALIZATION_FAILED",
            "CEF initialization failed",
        );
        return Err("CEF initialization failed".into());
    }

    let initialization_deadline = Instant::now() + Duration::from_secs(10);
    while !context_ready.load(Ordering::Acquire) && Instant::now() < initialization_deadline {
        sleep(Duration::from_millis(10));
    }
    if !context_ready.load(Ordering::Acquire) {
        emitter.error(
            browser_config.request_id,
            "INITIALIZATION_TIMEOUT",
            "CEF context initialization timed out",
        );
        shutdown();
        return Err("CEF context initialization timed out".into());
    }

    let browser_slot = Arc::new((Mutex::new(None), Condvar::new()));
    let browser_closed = Arc::new((Mutex::new(false), Condvar::new()));
    let life_span_handler = BrowserLifeSpanHandler::new(
        browser_slot.clone(),
        browser_closed.clone(),
        emitter.clone(),
    );
    let load_handler = BrowserLoadHandler::new(emitter.clone());
    let mut client = BrowserClient::new(life_span_handler, load_handler);
    let cef_bounds = Rect {
        x: browser_config.bounds.x,
        y: browser_config.bounds.y,
        width: browser_config.bounds.width as i32,
        height: browser_config.bounds.height as i32,
    };
    let window_info = WindowInfo {
        runtime_style: RuntimeStyle::ALLOY,
        ..WindowInfo::default().set_as_child(browser_config.parent as _, &cef_bounds)
    };
    let browser_settings = BrowserSettings {
        background_color: 0xffff_ffff,
        ..Default::default()
    };
    let initial_url = CefString::from(browser_config.url.as_str());
    if browser_host_create_browser(
        Some(&window_info),
        Some(&mut client),
        Some(&initial_url),
        Some(&browser_settings),
        None,
        None,
    ) != 1
    {
        emitter.error(
            browser_config.request_id,
            "CREATE_REJECTED",
            "CEF did not accept asynchronous browser creation",
        );
        shutdown();
        return Err("CEF did not accept asynchronous browser creation".into());
    }
    emitter.success(browser_config.request_id);

    let command_browser = browser_slot.clone();
    let command_emitter = emitter.clone();
    let browser_id = browser_config.browser_id;
    std::thread::spawn(move || {
        let command_browser = {
            let (browser, ready) = &*command_browser;
            let mut browser = browser.lock().unwrap_or_else(|error| error.into_inner());
            while browser.is_none() {
                browser = ready
                    .wait(browser)
                    .unwrap_or_else(|error| error.into_inner());
            }
            browser.as_ref().unwrap().clone()
        };
        loop {
            let received = match command_transport.receive() {
                Ok(received) => received,
                Err(error) => {
                    eprintln!("cef-renderer: control channel closed: {error}");
                    break;
                }
            };
            let request_id = received.packet.request_id;
            let request = match received.packet.body {
                Some(wire::packet::Body::Request(request)) => request,
                _ => {
                    command_emitter.error(
                        request_id,
                        "UNEXPECTED_PACKET",
                        "control channel accepts Request packets after startup",
                    );
                    continue;
                }
            };
            if request_id == 0 {
                command_emitter.error(0, "INVALID_REQUEST_ID", "request ID must be nonzero");
                continue;
            }
            if request.browser_id != browser_id {
                command_emitter.error(
                    request_id,
                    "UNKNOWN_BROWSER",
                    format!("browser {} does not exist", request.browser_id),
                );
                continue;
            }
            let command = match control_command(request) {
                Ok(command) => command,
                Err(error) => {
                    command_emitter.error(request_id, "INVALID_REQUEST", error.to_string());
                    continue;
                }
            };
            let quit = matches!(command, ControlCommand::Quit(_));
            let mut task = BrowserCommandTask::new(
                command_browser.clone(),
                command,
                request_id,
                command_emitter.clone(),
            );
            if post_task(ThreadId::UI, Some(&mut task)) != 1 {
                command_emitter.error(
                    request_id,
                    "DISPATCH_FAILED",
                    "CEF rejected the UI-thread task",
                );
                break;
            }
            if quit {
                return;
            }
        }
        let mut task = BrowserCommandTask::new(
            command_browser,
            ControlCommand::Quit(true),
            0,
            command_emitter,
        );
        let _ = post_task(ThreadId::UI, Some(&mut task));
    });

    if single_threaded {
        run_message_loop();
    } else {
        let (closed, wakeup) = &*browser_closed;
        let mut closed = closed.lock().unwrap_or_else(|error| error.into_inner());
        while !*closed {
            closed = wakeup
                .wait(closed)
                .unwrap_or_else(|error| error.into_inner());
        }
    }

    browser_slot
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    drop(client);
    shutdown();
    let _ = std::fs::remove_dir_all(cache_path);
    Ok(0)
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("cef-renderer: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlCommand, bounds_from_viewport, control_command, negotiate_protocol};
    use shell_protocol::{MAX_PACKET_BYTES, PROTOCOL_MAJOR, Transport, wire};

    #[test]
    fn parses_native_viewport() {
        let bounds = bounds_from_viewport(Some(wire::Viewport {
            x: 10,
            y: 20,
            width: 800,
            height: 600,
            scale_factor: 1.0,
        }))
        .unwrap();
        assert_eq!(
            (bounds.x, bounds.y, bounds.width, bounds.height),
            (10, 20, 800, 600)
        );
    }

    #[test]
    fn rejects_empty_native_surface() {
        assert!(
            bounds_from_viewport(Some(wire::Viewport {
                x: 0,
                y: 0,
                width: 0,
                height: 600,
                scale_factor: 1.0,
            }))
            .is_err()
        );
    }

    #[test]
    fn parses_navigation_request() {
        assert!(matches!(
            control_command(wire::Request {
                browser_id: 1,
                operation: Some(wire::request::Operation::Navigate(wire::Navigate {
                    url: "https://example.com/a b".to_owned(),
                })),
            }),
            Ok(ControlCommand::Navigate(url)) if url == "https://example.com/a b"
        ));
    }

    #[test]
    fn parses_resize_request() {
        assert!(matches!(
            control_command(wire::Request {
                browser_id: 1,
                operation: Some(wire::request::Operation::Resize(wire::Resize {
                    viewport: Some(wire::Viewport {
                        x: 1,
                        y: 2,
                        width: 3,
                        height: 4,
                        scale_factor: 1.0,
                    }),
                })),
            }),
            Ok(ControlCommand::Resize(_))
        ));
    }

    #[test]
    fn parses_focus_request() {
        assert!(matches!(
            control_command(wire::Request {
                browser_id: 1,
                operation: Some(wire::request::Operation::SetFocus(wire::SetFocus {
                    focused: true,
                })),
            }),
            Ok(ControlCommand::Focus(true))
        ));
    }

    #[test]
    fn rejects_an_incompatible_protocol_major() {
        let (shell, helper) = Transport::pair().unwrap();
        let helper_thread = std::thread::spawn(move || {
            assert!(negotiate_protocol(&helper, wire::Engine::Chromium).is_err());
        });
        shell
            .send(&wire::Packet {
                request_id: 41,
                attached_files: Vec::new(),
                body: Some(wire::packet::Body::Hello(wire::Hello {
                    minimum_major: PROTOCOL_MAJOR + 1,
                    maximum_major: PROTOCOL_MAJOR + 1,
                    maximum_packet_bytes: MAX_PACKET_BYTES as u32,
                    requested_capabilities: Vec::new(),
                })),
            })
            .unwrap();
        let response = shell.receive().unwrap().packet;
        assert_eq!(response.request_id, 41);
        assert!(matches!(
            response.body,
            Some(wire::packet::Body::Response(wire::Response {
                result: Some(wire::response::Result::Error(wire::Error { code, .. }))
            })) if code == "UNSUPPORTED_PROTOCOL"
        ));
        helper_thread.join().unwrap();
    }
}
