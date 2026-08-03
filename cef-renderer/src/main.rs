use cef::{args::Args, *};
use std::{
    error::Error,
    io::{self, BufRead},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::sleep,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConfigureWindowAux, ConnectionExt},
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

enum ControlCommand {
    Navigate(String),
    Resize(Bounds),
    Quit,
}

fn parse_control_command(line: &str) -> Option<ControlCommand> {
    if let Some(url) = line.strip_prefix("navigate\t") {
        Some(ControlCommand::Navigate(url.to_owned()))
    } else if let Some(bounds) = line.strip_prefix("bounds\t") {
        parse_bounds(bounds).ok().map(ControlCommand::Resize)
    } else if line == "quit" {
        Some(ControlCommand::Quit)
    } else {
        None
    }
}

wrap_life_span_handler! {
    struct BrowserLifeSpanHandler {
        closed: Arc<AtomicBool>,
    }

    impl LifeSpanHandler {
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.closed.store(true, Ordering::Release);
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
            command_line.append_switch(Some(&"hide-crash-restore-bubble".into()));
            command_line.append_switch(Some(&"noerrdialogs".into()));
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(NativeBrowserProcessHandler::new(self.ready.clone()))
        }
    }
}

fn resize_browser(browser: &Browser, bounds: Bounds) -> Result<(), Box<dyn Error>> {
    let host = browser.host().ok_or("CEF browser has no host")?;
    let window = host.window_handle() as u32;
    if window == 0 {
        return Err("CEF browser has no native window yet".into());
    }

    let (connection, _) = x11rb::connect(None)?;
    connection.configure_window(
        window,
        &ConfigureWindowAux::new()
            .x(bounds.x)
            .y(bounds.y)
            .width(bounds.width)
            .height(bounds.height),
    )?;
    connection.flush()?;
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
    let cache_path = std::env::temp_dir().join("dual-engine-browser-cef");
    std::fs::create_dir_all(&cache_path)?;
    let settings = Settings {
        no_sandbox: 1,
        external_message_pump: 1,
        root_cache_path: CefString::from(cache_path.to_string_lossy().as_ref()),
        resources_dir_path: CefString::from(runtime_path.to_string_lossy().as_ref()),
        locales_dir_path: CefString::from(runtime_path.join("locales").to_string_lossy().as_ref()),
        log_file: CefString::from("/dev/stderr"),
        log_severity: LogSeverity::INFO,
        background_color: 0xffff_ffff,
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
        do_message_loop_work();
        sleep(Duration::from_millis(10));
    }
    if !context_ready.load(Ordering::Acquire) {
        shutdown();
        return Err("CEF context initialization timed out".into());
    }

    let closed = Arc::new(AtomicBool::new(false));
    let life_span_handler = BrowserLifeSpanHandler::new(closed.clone());
    let load_handler = BrowserLoadHandler::new();
    let mut client = BrowserClient::new(life_span_handler, load_handler);
    let cef_bounds = Rect {
        x: config.bounds.x,
        y: config.bounds.y,
        width: config.bounds.width as i32,
        height: config.bounds.height as i32,
    };
    let window_info = WindowInfo::default().set_as_child(config.parent as _, &cef_bounds);
    let browser_settings = BrowserSettings {
        background_color: 0xffff_ffff,
        ..Default::default()
    };
    let browser = browser_host_create_browser_sync(
        Some(&window_info),
        Some(&mut client),
        Some(&CefString::from(config.url.as_str())),
        Some(&browser_settings),
        None,
        None,
    )
    .ok_or("CEF did not create a browser")?;
    eprintln!("cef-renderer: native browser ready");

    let (command_sender, command_receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else {
                break;
            };
            if let Some(command) = parse_control_command(&line) {
                let quit = matches!(command, ControlCommand::Quit);
                if command_sender.send(command).is_err() || quit {
                    return;
                }
            }
        }
        let _ = command_sender.send(ControlCommand::Quit);
    });

    let mut closing = false;
    while !closed.load(Ordering::Acquire) {
        do_message_loop_work();
        while let Ok(command) = command_receiver.try_recv() {
            match command {
                ControlCommand::Navigate(url) => {
                    if let Some(frame) = browser.main_frame() {
                        frame.load_url(Some(&CefString::from(url.as_str())));
                    }
                }
                ControlCommand::Resize(bounds) => {
                    if let Err(error) = resize_browser(&browser, bounds) {
                        eprintln!("cef-renderer: resize failed: {error}");
                    }
                }
                ControlCommand::Quit if !closing => {
                    closing = true;
                    if let Some(host) = browser.host() {
                        host.close_browser(1);
                    }
                }
                ControlCommand::Quit => {}
            }
        }
        sleep(Duration::from_millis(10));
    }

    drop(browser);
    drop(client);
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
}
