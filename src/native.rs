use qtbridge::{QmlMethodInvoker, invoke_method};
use std::{
    error::Error,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc::{self, RecvTimeoutError, Sender},
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

    fn argument(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.width, self.height)
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

#[derive(Clone, Copy)]
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
}

struct CefInstance {
    child: Child,
    input: ChildStdin,
    window: Window,
    native_child: bool,
}

impl CefInstance {
    fn send(&mut self, command: &str) -> NativeResult<()> {
        self.input.write_all(command.as_bytes())?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        Ok(())
    }

    fn navigate(&mut self, url: &str) -> NativeResult<()> {
        self.send(&format!("navigate\t{url}"))
    }

    fn focus(&mut self, connection: &RustConnection, bounds: NativeRect) -> NativeResult<()> {
        if self.native_child {
            connection.set_input_focus(InputFocus::PARENT, self.window, x11rb::CURRENT_TIME)?;
            configure_native_window(connection, self.window, bounds)?;
        }
        self.send("focus")
    }

    fn resize(&mut self, connection: &RustConnection, bounds: NativeRect) -> NativeResult<()> {
        if self.native_child {
            configure_native_window(connection, self.window, bounds)?;
        }
        self.send(&format!("bounds\t{}", bounds.argument()))
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

    fn stop(mut self) {
        let _ = self.send("quit");
        stop_child(&mut self.child);
    }
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
        let mut preload = vec![directory.join("libmozglue.so"), directory.join("libxul.so")];
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
    let mut child = command
        .arg("--parent")
        .arg(parent.to_string())
        .arg("--bounds")
        .arg(bounds.argument())
        .arg("--url")
        .arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let input = child
        .stdin
        .take()
        .ok_or("CEF control pipe is unavailable")?;
    let output = child
        .stdout
        .take()
        .ok_or("CEF readiness pipe is unavailable")?;
    let readiness_timeout = match backend {
        CefBackend::Chromium => Duration::from_secs(30),
        CefBackend::Firefox => Duration::from_secs(90),
    };
    let window = match wait_for_cef_ready(output, &mut child, readiness_timeout) {
        Ok(window) => window,
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
        input,
        window,
        native_child,
    })
}

fn wait_for_cef_ready(
    output: ChildStdout,
    child: &mut Child,
    timeout: Duration,
) -> NativeResult<Window> {
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(output).read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(line)) => {
                let value = line
                    .trim()
                    .strip_prefix("ready\t")
                    .ok_or_else(|| format!("invalid helper readiness message: {line:?}"))?;
                let raw = value.parse::<u64>()?;
                return Ok(u32::try_from(raw)?);
            }
            Ok(Err(error)) => return Err(error.into()),
            Err(RecvTimeoutError::Disconnected) => {
                return Err("CEF readiness pipe closed".into());
            }
            Err(RecvTimeoutError::Timeout) => {}
        }
        if let Some(status) = child.try_wait()? {
            return Err(format!("CEF helper exited before readiness: {status}").into());
        }
    }
    Err("CEF helper readiness timed out".into())
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
    use super::NativeRect;

    #[test]
    fn rejects_zero_sized_surfaces() {
        assert!(NativeRect::new(0, 0, 0, 100).is_none());
        assert!(NativeRect::new(0, 0, 100, 1).is_none());
    }

    #[test]
    fn formats_cef_bounds() {
        let bounds = NativeRect::new(10, 20, 800, 600).unwrap();
        assert_eq!(bounds.argument(), "10,20,800,600");
    }
}
