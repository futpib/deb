mod cookies;

use cef::{args::Args, *};
use cookies::CookieBridge;
use shell_protocol::{
    MAX_PACKET_BYTES, Transport, is_valid_profile_id,
    wire::{self, Capability, Engine},
};
use std::{
    error::Error,
    ffi::CStr,
    os::fd::RawFd,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};

const DEB_SCHEME: &str = "deb";
const DEB_NEW_TAB_HOST: &str = "new-tab";
const DEB_NEW_TAB_PAGE: &[u8] = include_bytes!("../../internal-pages/new-tab.html");

fn validate_cef_api_hash(hash: &CStr) -> Result<(), String> {
    if hash.to_bytes_with_nul() == cef_cookie::CEF_API_HASH_EXPERIMENTAL_LINUX {
        return Ok(());
    }
    Err(format!(
        "libcef.so has experimental API hash {}, expected {}",
        hash.to_string_lossy(),
        CStr::from_bytes_with_nul(cef_cookie::CEF_API_HASH_EXPERIMENTAL_LINUX)
            .expect("the compiled CEF API hash must be NUL-terminated")
            .to_string_lossy()
    ))
}

fn configure_cef_api() -> Result<(), Box<dyn Error>> {
    let hash = api_hash(cef_cookie::CEF_API_VERSION_EXPERIMENTAL, 0);
    if hash.is_null() {
        return Err("libcef.so does not support the experimental CEF API".into());
    }
    validate_cef_api_hash(unsafe { CStr::from_ptr(hash) })?;
    Ok(())
}

fn is_deb_internal_url(url: &str) -> bool {
    let Some(remainder) = url.strip_prefix("deb://new-tab") else {
        return false;
    };
    let path = remainder.split(['?', '#']).next().unwrap_or_default();
    path.is_empty() || path == "/"
}

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
    ReadCookies,
    SetCookie(wire::Cookie),
    DeleteCookie(wire::Cookie),
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
        wire::request::Operation::ReadCookies(_) => Ok(ControlCommand::ReadCookies),
        wire::request::Operation::SetCookie(command) => Ok(ControlCommand::SetCookie(
            command.cookie.ok_or("SetCookie cookie is required")?,
        )),
        wire::request::Operation::DeleteCookie(command) => Ok(ControlCommand::DeleteCookie(
            command.cookie.ok_or("DeleteCookie cookie is required")?,
        )),
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
    profile_id: String,
    profile_data_path: PathBuf,
    profile_cache_path: PathBuf,
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
        cookie_bridge: CookieBridge,
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
            if let Err(error) = self.cookie_bridge.ensure_observer() {
                eprintln!("cef-renderer: cookie observer setup failed: {error}");
            }
            *self
                .browser
                .0
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(browser.clone());
            self.browser.1.notify_all();
            self.emitter.event(wire::event::Value::SurfaceReady(
                wire::SurfaceReady {
                    x11: Some(wire::X11Surface {
                        window: native_window,
                    }),
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

wrap_resource_handler! {
    struct DebResourceHandler {
        body: &'static [u8],
        cursor: Arc<Mutex<usize>>,
    }

    impl ResourceHandler {
        fn open(
            &self,
            _request: Option<&mut Request>,
            handle_request: Option<&mut i32>,
            _callback: Option<&mut Callback>,
        ) -> i32 {
            if let Some(handle_request) = handle_request {
                *handle_request = 1;
            }
            1
        }

        fn response_headers(
            &self,
            response: Option<&mut Response>,
            response_length: Option<&mut i64>,
            _redirect_url: Option<&mut CefString>,
        ) {
            if let Some(response) = response {
                response.set_status(200);
                response.set_status_text(Some(&"OK".into()));
                response.set_mime_type(Some(&"text/html".into()));
                response.set_charset(Some(&"utf-8".into()));
                response.set_header_by_name(
                    Some(&"Cache-Control".into()),
                    Some(&"no-store".into()),
                    1,
                );
            }
            if let Some(response_length) = response_length {
                *response_length = self.body.len() as i64;
            }
        }

        fn read(
            &self,
            data_out: *mut u8,
            bytes_to_read: i32,
            bytes_read: Option<&mut i32>,
            _callback: Option<&mut ResourceReadCallback>,
        ) -> i32 {
            let Some(bytes_read) = bytes_read else {
                return 0;
            };
            *bytes_read = 0;
            if data_out.is_null() || bytes_to_read <= 0 {
                return 0;
            }

            let mut cursor = self
                .cursor
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let remaining = self.body.len().saturating_sub(*cursor);
            let length = remaining.min(bytes_to_read as usize);
            if length == 0 {
                return 0;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(self.body.as_ptr().add(*cursor), data_out, length);
            }
            *cursor += length;
            *bytes_read = length as i32;
            1
        }
    }
}

wrap_scheme_handler_factory! {
    struct DebSchemeHandlerFactory;

    impl SchemeHandlerFactory {
        fn create(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            scheme_name: Option<&CefString>,
            request: Option<&mut Request>,
        ) -> Option<ResourceHandler> {
            if scheme_name.map(CefString::to_string).as_deref() != Some(DEB_SCHEME) {
                return None;
            }
            let request_url = CefString::from(&request?.url()).to_string();
            is_deb_internal_url(&request_url).then(|| {
                DebResourceHandler::new(DEB_NEW_TAB_PAGE, Arc::new(Mutex::new(0)))
            })
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
            if let Some(cache_path) = std::env::var_os("DEB_PROFILE_CACHE_PATH") {
                command_line.append_switch_with_value(
                    Some(&"disk-cache-dir".into()),
                    Some(&CefString::from(cache_path.to_string_lossy().as_ref())),
                );
            }
        }

        fn on_register_custom_schemes(&self, registrar: Option<&mut SchemeRegistrar>) {
            let Some(registrar) = registrar else {
                return;
            };
            let options = SchemeOptions::STANDARD.get_raw()
                | SchemeOptions::LOCAL.get_raw()
                | SchemeOptions::SECURE.get_raw()
                | SchemeOptions::DISPLAY_ISOLATED.get_raw();
            if registrar.add_custom_scheme(Some(&DEB_SCHEME.into()), options as i32) != 1 {
                eprintln!("cef-renderer: could not register the {DEB_SCHEME} scheme");
            }
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
        cookie_bridge: CookieBridge,
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
                ControlCommand::ReadCookies => match self.cookie_bridge.read_all(self.request_id) {
                    Ok(()) => return,
                    Err(error) => Err(error),
                },
                ControlCommand::SetCookie(cookie) => {
                    match self.cookie_bridge.set(self.request_id, cookie.clone()) {
                        Ok(()) => return,
                        Err(error) => Err(error),
                    }
                }
                ControlCommand::DeleteCookie(cookie) => {
                    match self.cookie_bridge.delete(self.request_id, cookie.clone()) {
                        Ok(()) => return,
                        Err(error) => Err(error),
                    }
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
        Capability::CookieSync,
    ]
    .into_iter()
    .map(|capability| capability as i32)
    .collect()
}

fn negotiate_protocol(transport: &Transport, engine: Engine) -> Result<(), Box<dyn Error>> {
    let received = transport.receive()?;
    let request_id = received.request_id;
    let hello = match received.body {
        Some(wire::packet::Body::Hello(hello)) => hello,
        _ => return Err("first shell protocol packet must be Hello".into()),
    };
    if request_id == 0 {
        return Err("hello request ID must be nonzero".into());
    }
    if hello.maximum_packet_bytes == 0 {
        return Err("shell packet limit must be nonzero".into());
    }
    transport.send(&wire::Packet {
        request_id,
        body: Some(wire::packet::Body::HelloReply(wire::HelloReply {
            engine: engine as i32,
            engine_version: match engine {
                Engine::Chromium => "Chromium through CEF".to_owned(),
                Engine::Gecko => "Gecko through FirefoxCEF".to_owned(),
                Engine::Unspecified => String::new(),
            },
            cef_api_version: cef_cookie::CEF_API_VERSION_EXPERIMENTAL as u32,
            maximum_packet_bytes: MAX_PACKET_BYTES as u32,
            capabilities: advertised_capabilities(),
        })),
    })?;
    Ok(())
}

fn receive_browser_config(transport: &Transport) -> Result<BrowserConfig, Box<dyn Error>> {
    let received = transport.receive()?;
    let request_id = received.request_id;
    let parsed = (|| -> Result<BrowserConfig, Box<dyn Error>> {
        if request_id == 0 {
            return Err("CreateBrowser request ID must be nonzero".into());
        }
        let request = match received.body {
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
        let x11 = create.x11.ok_or("CreateBrowser X11 target is required")?;
        if x11.parent_window == 0 {
            return Err("X11 parent window must be nonzero".into());
        }
        if !is_valid_profile_id(&create.profile_id) {
            return Err(format!("invalid profile ID {:?}", create.profile_id).into());
        }
        let profile_data_path =
            absolute_profile_path(&create.profile_data_path, "CreateBrowser profile data path")?;
        let profile_cache_path = absolute_profile_path(
            &create.profile_cache_path,
            "CreateBrowser profile cache path",
        )?;
        if profile_data_path == profile_cache_path {
            return Err("profile data and cache paths must be different".into());
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
            profile_id: create.profile_id,
            profile_data_path,
            profile_cache_path,
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

fn absolute_profile_path(value: &str, description: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = PathBuf::from(value);
    if value.is_empty() || !path.is_absolute() {
        return Err(format!("{description} must be an absolute path").into());
    }
    Ok(path)
}

fn run() -> Result<i32, Box<dyn Error>> {
    configure_cef_api()?;
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
    let single_threaded = std::env::var_os("DEB_CEF_SINGLE_THREADED").is_some();
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
    std::fs::create_dir_all(&browser_config.profile_data_path)?;
    std::fs::create_dir_all(&browser_config.profile_cache_path)?;
    eprintln!(
        "cef-renderer: profile {} uses data {} and cache {}",
        browser_config.profile_id,
        browser_config.profile_data_path.display(),
        browser_config.profile_cache_path.display()
    );
    unsafe {
        std::env::set_var("DEB_PROFILE_CACHE_PATH", &browser_config.profile_cache_path);
    }
    let remote_debugging_port = std::env::var("DEB_CEF_DEBUG_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_default();
    let settings = Settings {
        no_sandbox: 1,
        multi_threaded_message_loop: i32::from(!single_threaded),
        cache_path: CefString::from(browser_config.profile_data_path.to_string_lossy().as_ref()),
        root_cache_path: CefString::from(
            browser_config.profile_data_path.to_string_lossy().as_ref(),
        ),
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

    let mut scheme_factory = DebSchemeHandlerFactory::new();
    if register_scheme_handler_factory(
        Some(&DEB_SCHEME.into()),
        Some(&DEB_NEW_TAB_HOST.into()),
        Some(&mut scheme_factory),
    ) != 1
    {
        emitter.error(
            browser_config.request_id,
            "SCHEME_REGISTRATION_FAILED",
            "CEF rejected the deb:// scheme handler",
        );
        shutdown();
        return Err("CEF rejected the deb:// scheme handler".into());
    }

    let browser_slot = Arc::new((Mutex::new(None), Condvar::new()));
    let browser_closed = Arc::new((Mutex::new(false), Condvar::new()));
    let cookie_bridge = CookieBridge::new(emitter.clone());
    let life_span_handler = BrowserLifeSpanHandler::new(
        browser_slot.clone(),
        browser_closed.clone(),
        emitter.clone(),
        cookie_bridge.clone(),
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
    let command_cookie_bridge = cookie_bridge.clone();
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
            let request_id = received.request_id;
            let request = match received.body {
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
                command_cookie_bridge.clone(),
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
            command_cookie_bridge,
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
    cookie_bridge.shutdown();
    shutdown();
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
    use super::{
        ControlCommand, absolute_profile_path, bounds_from_viewport, control_command,
        is_deb_internal_url, validate_cef_api_hash,
    };
    use shell_protocol::wire;
    use std::ffi::CStr;

    #[test]
    fn rejects_an_unpatched_cef_api_hash() {
        let stock =
            CStr::from_bytes_with_nul(b"a5d187477e0cbe23eb1043c2f1868582b7018260\0").unwrap();
        let error = validate_cef_api_hash(stock).unwrap_err();
        assert!(error.contains("expected 9c4f3ddc9baede09fb12229355d593dd60565bee"));
    }

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
    fn recognizes_only_the_new_tab_internal_page() {
        assert!(is_deb_internal_url("deb://new-tab/"));
        assert!(is_deb_internal_url("deb://new-tab?source=startup"));
        assert!(!is_deb_internal_url("deb://settings/"));
        assert!(!is_deb_internal_url("https://new-tab/"));
        assert!(!is_deb_internal_url("deb://new-tab/not-found"));
    }

    #[test]
    fn requires_absolute_profile_paths() {
        assert!(absolute_profile_path("/data/deb/default", "data path").is_ok());
        assert!(absolute_profile_path("relative/default", "data path").is_err());
        assert!(absolute_profile_path("", "data path").is_err());
    }
}
