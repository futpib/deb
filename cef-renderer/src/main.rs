use cef::{args::Args, *};
use std::{
    error::Error,
    io::{self, BufRead, Write},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};

const DEFAULT_URL: &str = "https://www.google.com/";

#[derive(Clone, Copy)]
struct Bounds {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

struct Config {
    url: String,
    parent: u64,
    bounds: Bounds,
}

impl Config {
    fn from_args() -> Result<Self, Box<dyn Error>> {
        let mut url = DEFAULT_URL.to_owned();
        let mut parent = None;
        let mut bounds = None;
        let mut args = std::env::args().skip(1);

        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--url" => url = args.next().ok_or("--url requires a value")?,
                "--parent" => {
                    let value = args.next().ok_or("--parent requires a value")?;
                    parent = Some(parse_window_id(&value)?);
                }
                "--bounds" => {
                    let value = args.next().ok_or("--bounds requires a value")?;
                    bounds = Some(parse_bounds(&value)?);
                }
                _ => {}
            }
        }

        Ok(Self {
            url,
            parent: parent.ok_or("--parent is required")?,
            bounds: bounds.ok_or("--bounds is required")?,
        })
    }
}

fn parse_window_id(value: &str) -> Result<u64, Box<dyn Error>> {
    if let Some(hex) = value.strip_prefix("0x") {
        Ok(u64::from_str_radix(hex, 16)?)
    } else {
        Ok(value.parse()?)
    }
}

fn parse_bounds(value: &str) -> Result<Bounds, Box<dyn Error>> {
    let values = value
        .split(',')
        .map(str::parse)
        .collect::<Result<Vec<i32>, _>>()?;
    if values.len() != 4 || values[2] <= 0 || values[3] <= 0 {
        return Err("bounds must be x,y,width,height with a positive size".into());
    }
    Ok(Bounds {
        x: values[0],
        y: values[1],
        width: values[2] as u32,
        height: values[3] as u32,
    })
}

#[derive(Clone)]
enum ControlCommand {
    Navigate(String),
    Resize(Bounds),
    Focus,
    Quit,
}

fn parse_control_command(line: &str) -> Option<ControlCommand> {
    if let Some(url) = line.strip_prefix("navigate\t") {
        Some(ControlCommand::Navigate(url.to_owned()))
    } else if let Some(bounds) = line.strip_prefix("bounds\t") {
        parse_bounds(bounds).ok().map(ControlCommand::Resize)
    } else if line == "focus" {
        Some(ControlCommand::Focus)
    } else if line == "quit" {
        Some(ControlCommand::Quit)
    } else {
        None
    }
}

wrap_life_span_handler! {
    struct BrowserLifeSpanHandler {
        browser: Arc<(Mutex<Option<Browser>>, Condvar)>,
        closed: Arc<(Mutex<bool>, Condvar)>,
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
            println!("ready\t{native_window}");
            let _ = io::stdout().flush();
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
        }
    }
}

wrap_load_handler! {
    struct BrowserLoadHandler;

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: i32,
            _can_go_back: i32,
            _can_go_forward: i32,
        ) {
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
            eprintln!(
                "cef-renderer: load error {error_code:?}: {} ({})",
                error_text.map(CefString::to_string).unwrap_or_default(),
                failed_url.map(CefString::to_string).unwrap_or_default(),
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
    }

    impl Task {
        fn execute(&self) {
            match &self.command {
                ControlCommand::Navigate(url) => {
                    if let Some(frame) = self.browser.main_frame() {
                        frame.load_url(Some(&CefString::from(url.as_str())));
                    }
                    if let Some(host) = self.browser.host() {
                        host.set_focus(1);
                    }
                }
                ControlCommand::Resize(bounds) => {
                    if let Err(error) = resize_browser(&self.browser) {
                        eprintln!(
                            "cef-renderer: resize to {}x{} failed: {error}",
                            bounds.width, bounds.height
                        );
                    }
                }
                ControlCommand::Focus => {
                    if let Some(host) = self.browser.host() {
                        host.set_focus(1);
                        host.notify_move_or_resize_started();
                    }
                }
                ControlCommand::Quit => {
                    if let Some(host) = self.browser.host() {
                        host.close_browser(1);
                    } else {
                        quit_message_loop();
                    }
                }
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
        multi_threaded_message_loop: 1,
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
        return Err("CEF initialization failed".into());
    }

    let initialization_deadline = Instant::now() + Duration::from_secs(10);
    while !context_ready.load(Ordering::Acquire) && Instant::now() < initialization_deadline {
        sleep(Duration::from_millis(10));
    }
    if !context_ready.load(Ordering::Acquire) {
        shutdown();
        return Err("CEF context initialization timed out".into());
    }

    let browser_slot = Arc::new((Mutex::new(None), Condvar::new()));
    let browser_closed = Arc::new((Mutex::new(false), Condvar::new()));
    let life_span_handler =
        BrowserLifeSpanHandler::new(browser_slot.clone(), browser_closed.clone());
    let load_handler = BrowserLoadHandler::new();
    let mut client = BrowserClient::new(life_span_handler, load_handler);
    let cef_bounds = Rect {
        x: config.bounds.x,
        y: config.bounds.y,
        width: config.bounds.width as i32,
        height: config.bounds.height as i32,
    };
    let window_info = WindowInfo {
        runtime_style: RuntimeStyle::ALLOY,
        ..WindowInfo::default().set_as_child(config.parent as _, &cef_bounds)
    };
    let browser_settings = BrowserSettings {
        background_color: 0xffff_ffff,
        ..Default::default()
    };
    let initial_url = CefString::from(config.url.as_str());
    if browser_host_create_browser(
        Some(&window_info),
        Some(&mut client),
        Some(&initial_url),
        Some(&browser_settings),
        None,
        None,
    ) != 1
    {
        shutdown();
        return Err("CEF did not accept asynchronous browser creation".into());
    }

    let command_browser = browser_slot.clone();
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
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if let Some(command) = parse_control_command(&line) {
                let quit = matches!(command, ControlCommand::Quit);
                let mut task = BrowserCommandTask::new(command_browser.clone(), command);
                if post_task(ThreadId::UI, Some(&mut task)) != 1 || quit {
                    return;
                }
            }
        }
        let mut task = BrowserCommandTask::new(command_browser, ControlCommand::Quit);
        let _ = post_task(ThreadId::UI, Some(&mut task));
    });

    {
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
    use super::{Bounds, ControlCommand, parse_bounds, parse_control_command};

    #[test]
    fn parses_native_bounds() {
        let bounds = parse_bounds("10,20,800,600").unwrap();
        assert_eq!(
            (bounds.x, bounds.y, bounds.width, bounds.height),
            (10, 20, 800, 600)
        );
    }

    #[test]
    fn rejects_empty_native_surface() {
        assert!(parse_bounds("0,0,0,600").is_err());
    }

    #[test]
    fn parses_navigation_control_message() {
        assert!(matches!(
            parse_control_command("navigate\thttps://example.com/a b"),
            Some(ControlCommand::Navigate(url)) if url == "https://example.com/a b"
        ));
    }

    #[test]
    fn parses_resize_control_message() {
        assert!(matches!(
            parse_control_command("bounds\t1,2,3,4"),
            Some(ControlCommand::Resize(Bounds {
                x: 1,
                y: 2,
                width: 3,
                height: 4
            }))
        ));
    }

    #[test]
    fn parses_focus_control_message() {
        assert!(matches!(
            parse_control_command("focus"),
            Some(ControlCommand::Focus)
        ));
    }
}
