use cef::{args::Args, *};
use image::ColorType;
use std::{
    error::Error,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::sleep,
    time::{Duration, Instant},
};

const DEFAULT_URL: &str = "https://www.google.com/";
const WIDTH: i32 = 1200;
const HEIGHT: i32 = 800;

struct Config {
    url: String,
    output: PathBuf,
}

impl Config {
    fn from_args() -> Result<Self, Box<dyn Error>> {
        let mut url = DEFAULT_URL.to_owned();
        let mut output = PathBuf::from("cef-google.png");
        let mut args = std::env::args().skip(1);

        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--url" => url = args.next().ok_or("--url requires a value")?,
                "--output" => {
                    output = PathBuf::from(args.next().ok_or("--output requires a value")?)
                }
                _ => {}
            }
        }

        Ok(Self { url, output })
    }
}

#[derive(Clone)]
struct Frame {
    bgra: Vec<u8>,
    width: u32,
    height: u32,
    painted_at: Instant,
}

#[derive(Clone)]
struct ScreenshotRenderHandler {
    frame: Arc<Mutex<Option<Frame>>>,
}

wrap_render_handler! {
    struct ScreenshotRenderHandlerBuilder {
        handler: ScreenshotRenderHandler,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            if let Some(rect) = rect {
                rect.width = WIDTH;
                rect.height = HEIGHT;
            }
        }

        fn on_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty_rects: Option<&[Rect]>,
            buffer: *const u8,
            width: i32,
            height: i32,
        ) {
            if type_ != PaintElementType::VIEW || buffer.is_null() || width <= 0 || height <= 0 {
                return;
            }

            let byte_count = width as usize * height as usize * 4;
            let bgra = unsafe { std::slice::from_raw_parts(buffer, byte_count) }.to_vec();
            *self.handler.frame.lock().expect("frame mutex poisoned") = Some(Frame {
                bgra,
                width: width as u32,
                height: height as u32,
                painted_at: Instant::now(),
            });
        }
    }
}

wrap_life_span_handler! {
    struct ScreenshotLifeSpanHandler {
        closed: Arc<AtomicBool>,
    }

    impl LifeSpanHandler {
        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            self.closed.store(true, Ordering::Release);
        }
    }
}

wrap_load_handler! {
    struct ScreenshotLoadHandler {
        loaded: Arc<AtomicBool>,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: i32,
            _can_go_back: i32,
            _can_go_forward: i32,
        ) {
            if is_loading == 0 {
                self.loaded.store(true, Ordering::Release);
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
    struct ScreenshotClient {
        render_handler: RenderHandler,
        life_span_handler: LifeSpanHandler,
        load_handler: LoadHandler,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(self.render_handler.clone())
        }

        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(self.life_span_handler.clone())
        }

        fn load_handler(&self) -> Option<LoadHandler> {
            Some(self.load_handler.clone())
        }
    }
}

wrap_browser_process_handler! {
    struct ScreenshotBrowserProcessHandler {
        ready: Arc<AtomicBool>,
    }

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            self.ready.store(true, Ordering::Release);
        }
    }
}

wrap_app! {
    struct ScreenshotApp {
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
            command_line.append_switch_with_value(
                Some(&"disable-features".into()),
                Some(&"PaintHolding".into()),
            );
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(ScreenshotBrowserProcessHandler::new(self.ready.clone()))
        }
    }
}

fn save_frame(frame: Frame, output: &PathBuf) -> Result<(), Box<dyn Error>> {
    let mut rgba = frame.bgra;
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        let transparent = 255_u16 - pixel[3] as u16;
        pixel[0] = (pixel[0] as u16 + transparent).min(255) as u8;
        pixel[1] = (pixel[1] as u16 + transparent).min(255) as u8;
        pixel[2] = (pixel[2] as u16 + transparent).min(255) as u8;
        pixel[3] = 255;
    }

    image::save_buffer(output, &rgba, frame.width, frame.height, ColorType::Rgba8)?;
    Ok(())
}

fn frame_has_content(frame: &Frame) -> bool {
    frame
        .bgra
        .chunks_exact(4)
        .any(|pixel| pixel[3] > 16 && (pixel[0] < 245 || pixel[1] < 245 || pixel[2] < 245))
}

fn run() -> Result<i32, Box<dyn Error>> {
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    let cef_args = Args::new();
    let context_ready = Arc::new(AtomicBool::new(false));
    let mut app = ScreenshotApp::new(context_ready.clone());
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
        windowless_rendering_enabled: 1,
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

    let frame = Arc::new(Mutex::new(None));
    let closed = Arc::new(AtomicBool::new(false));
    let loaded = Arc::new(AtomicBool::new(false));
    let render_handler = ScreenshotRenderHandlerBuilder::new(ScreenshotRenderHandler {
        frame: frame.clone(),
    });
    let life_span_handler = ScreenshotLifeSpanHandler::new(closed.clone());
    let load_handler = ScreenshotLoadHandler::new(loaded.clone());
    let mut client = ScreenshotClient::new(render_handler, life_span_handler, load_handler);
    let window_info = WindowInfo {
        windowless_rendering_enabled: 1,
        ..Default::default()
    };
    let browser_settings = BrowserSettings {
        windowless_frame_rate: 30,
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

    let started_at = Instant::now();
    let deadline = started_at + Duration::from_secs(30);
    let frame = loop {
        do_message_loop_work();

        if (loaded.load(Ordering::Acquire) && started_at.elapsed() >= Duration::from_secs(3))
            || started_at.elapsed() >= Duration::from_secs(8)
        {
            let candidate = frame.lock().expect("frame mutex poisoned").clone();
            if let Some(candidate) = candidate
                && candidate.painted_at.elapsed() >= Duration::from_millis(500)
                && frame_has_content(&candidate)
            {
                break candidate;
            }
        }

        if Instant::now() >= deadline {
            return Err("CEF did not paint a settled frame within 30 seconds".into());
        }
        sleep(Duration::from_millis(10));
    };

    save_frame(frame, &config.output)?;

    if let Some(host) = browser.host() {
        host.close_browser(1);
    }
    let close_deadline = Instant::now() + Duration::from_secs(3);
    while !closed.load(Ordering::Acquire) && Instant::now() < close_deadline {
        do_message_loop_work();
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
