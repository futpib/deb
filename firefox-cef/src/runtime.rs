use cef_dll_sys::{
    _cef_browser_host_t, _cef_browser_t, _cef_client_t, _cef_frame_t, _cef_task_t, cef_errorcode_t,
    cef_main_args_t, cef_string_t, cef_termination_status_t, cef_transition_type_t,
};
use libc::{c_char, c_int, c_void};
use std::{
    error::Error,
    ffi::{CStr, CString},
    fs, mem,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering},
    },
};
use x11rb::{
    connection::Connection,
    protocol::xproto::{ConfigureWindowAux, ConnectionExt},
};

use crate::refcount::{add_ref_raw, release_raw};

type RuntimeResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

#[repr(C)]
struct FirefoxCefCallbacks {
    size: usize,
    context: *mut c_void,
    on_after_created: Option<unsafe extern "C" fn(*mut c_void, i32, u64)>,
    on_address_change: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
    on_title_change: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
    on_loading_state_change: Option<unsafe extern "C" fn(*mut c_void, i32, u8)>,
    on_load_error:
        Option<unsafe extern "C" fn(*mut c_void, i32, i32, *const c_char, *const c_char)>,
    on_browser_crashed: Option<unsafe extern "C" fn(*mut c_void, i32, *const c_char)>,
    on_before_close: Option<unsafe extern "C" fn(*mut c_void, i32)>,
    on_cookie_changed: Option<unsafe extern "C" fn(*mut c_void, *const FirefoxCefCookie, u8)>,
}

#[repr(C)]
pub struct FirefoxCefCookie {
    pub name: *const c_char,
    pub value: *const c_char,
    pub domain: *const c_char,
    pub path: *const c_char,
    pub partition_key_top_level_site: *const c_char,
    pub secure: u8,
    pub http_only: u8,
    pub session: u8,
    pub partitioned: u8,
    pub partition_key_has_cross_site_ancestor: u8,
    pub expires_milliseconds: i64,
    pub creation_microseconds: i64,
    pub last_access_microseconds: i64,
    pub update_microseconds: i64,
    pub same_site: i32,
}

type SetCallbacks = unsafe extern "C" fn(*const FirefoxCefCallbacks);
type Configure = unsafe extern "C" fn(u32, u64, u32, u32, *const c_char) -> c_int;
type Command = unsafe extern "C" fn() -> c_int;
type BrowserCommand = unsafe extern "C" fn(u32) -> c_int;
type BrowserBoolCommand = unsafe extern "C" fn(u32, u8) -> c_int;
type BrowserStringCommand = unsafe extern "C" fn(u32, *const c_char) -> c_int;
type PostTask =
    unsafe extern "C" fn(Option<unsafe extern "C" fn(*mut c_void)>, *mut c_void) -> c_int;
pub type CookieVisitor = unsafe extern "C" fn(*mut c_void, *const FirefoxCefCookie);
pub type CookieCompletion = unsafe extern "C" fn(*mut c_void, u8);
type VisitCookies =
    unsafe extern "C" fn(Option<CookieVisitor>, Option<CookieCompletion>, *mut c_void) -> c_int;
type MutateCookie =
    unsafe extern "C" fn(*const FirefoxCefCookie, Option<CookieCompletion>, *mut c_void) -> c_int;
type Run = unsafe extern "C" fn(c_int, *mut *mut c_char, *const c_char) -> c_int;

struct GeckoApi {
    _libxul: usize,
    set_callbacks: SetCallbacks,
    configure: Configure,
    navigate: BrowserStringCommand,
    reload: BrowserCommand,
    focus: BrowserCommand,
    set_visibility: BrowserBoolCommand,
    close: BrowserBoolCommand,
    shutdown: Command,
    post_task: PostTask,
    visit_cookies: VisitCookies,
    set_cookie: MutateCookie,
    delete_cookie: MutateCookie,
    run: Run,
}

unsafe impl Send for GeckoApi {}
unsafe impl Sync for GeckoApi {}

impl GeckoApi {
    fn load() -> RuntimeResult<Self> {
        let directory = std::env::current_exe()?
            .parent()
            .ok_or("FirefoxCEF helper executable has no parent directory")?
            .to_path_buf();
        let libxul = load_library(&directory.join("libxul.so"))?;
        Ok(Self {
            _libxul: libxul as usize,
            set_callbacks: unsafe { symbol(libxul, b"firefox_cef_gecko_set_callbacks\0")? },
            configure: unsafe { symbol(libxul, b"firefox_cef_gecko_configure\0")? },
            navigate: unsafe { symbol(libxul, b"firefox_cef_gecko_navigate\0")? },
            reload: unsafe { symbol(libxul, b"firefox_cef_gecko_reload\0")? },
            focus: unsafe { symbol(libxul, b"firefox_cef_gecko_focus\0")? },
            set_visibility: unsafe { symbol(libxul, b"firefox_cef_gecko_set_visibility\0")? },
            close: unsafe { symbol(libxul, b"firefox_cef_gecko_close\0")? },
            shutdown: unsafe { symbol(libxul, b"firefox_cef_gecko_shutdown\0")? },
            post_task: unsafe { symbol(libxul, b"firefox_cef_gecko_post_task\0")? },
            visit_cookies: unsafe { symbol(libxul, b"firefox_cef_gecko_visit_cookies\0")? },
            set_cookie: unsafe { symbol(libxul, b"firefox_cef_gecko_set_cookie\0")? },
            delete_cookie: unsafe { symbol(libxul, b"firefox_cef_gecko_delete_cookie\0")? },
            run: unsafe { symbol(libxul, b"firefox_cef_gecko_run\0")? },
        })
    }

    fn command(&self, command: Command, name: &str) -> RuntimeResult<()> {
        if unsafe { command() } == 0 {
            return Err(format!("Gecko rejected {name}").into());
        }
        Ok(())
    }

    fn browser_command(
        &self,
        command: BrowserCommand,
        browser_id: i32,
        name: &str,
    ) -> RuntimeResult<()> {
        if unsafe { command(browser_id as u32) } == 0 {
            return Err(format!("Gecko rejected {name} for browser {browser_id}").into());
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RuntimeConfig {
    profile: PathBuf,
    app_ini: PathBuf,
}

pub struct BrowserState {
    pub id: i32,
    pub parent: u32,
    pub width: AtomicU32,
    pub height: AtomicU32,
    pub browser: AtomicPtr<_cef_browser_t>,
    pub host: AtomicPtr<_cef_browser_host_t>,
    pub frame: AtomicPtr<_cef_frame_t>,
    pub client: AtomicPtr<_cef_client_t>,
    window: AtomicU32,
    current_url: Mutex<String>,
    loading: AtomicBool,
    after_created: AtomicBool,
    closed: AtomicBool,
}

static GECKO: OnceLock<Result<GeckoApi, String>> = OnceLock::new();
static CONFIG: OnceLock<Mutex<Option<RuntimeConfig>>> = OnceLock::new();
static NEXT_BROWSER_ID: AtomicU64 = AtomicU64::new(1);
static STATES: OnceLock<Mutex<Vec<Arc<BrowserState>>>> = OnceLock::new();

fn gecko() -> RuntimeResult<&'static GeckoApi> {
    GECKO
        .get_or_init(|| GeckoApi::load().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| error.clone().into())
}

fn config() -> &'static Mutex<Option<RuntimeConfig>> {
    CONFIG.get_or_init(|| Mutex::new(None))
}

fn states() -> &'static Mutex<Vec<Arc<BrowserState>>> {
    STATES.get_or_init(|| Mutex::new(Vec::new()))
}

fn find_state(id: i32) -> Option<Arc<BrowserState>> {
    states()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .find(|state| state.id == id)
        .cloned()
}

fn remove_state(id: i32) -> Option<Arc<BrowserState>> {
    let mut states = states().lock().unwrap_or_else(|error| error.into_inner());
    let position = states.iter().position(|state| state.id == id)?;
    Some(states.remove(position))
}

fn app_ini_path() -> RuntimeResult<PathBuf> {
    if let Some(path) = std::env::var_os("FIREFOX_CEF_APP_INI") {
        return Ok(path.into());
    }
    Ok(std::env::current_exe()?
        .parent()
        .ok_or("FirefoxCEF helper executable has no parent directory")?
        .join("browser/firefox-cef.ini"))
}

fn path_cstring(path: &Path) -> RuntimeResult<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}

fn load_library(path: &Path) -> RuntimeResult<*mut c_void> {
    let path = path_cstring(path)?;
    unsafe { libc::dlerror() };
    let handle = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
    if handle.is_null() {
        return Err(dynamic_error("dlopen failed").into());
    }
    Ok(handle)
}

unsafe fn symbol<T: Copy>(handle: *mut c_void, name: &'static [u8]) -> RuntimeResult<T> {
    unsafe { libc::dlerror() };
    let pointer = unsafe { libc::dlsym(handle, name.as_ptr().cast()) };
    if pointer.is_null() {
        return Err(dynamic_error("dlsym failed").into());
    }
    debug_assert_eq!(mem::size_of::<T>(), mem::size_of::<*mut c_void>());
    Ok(unsafe { mem::transmute_copy(&pointer) })
}

fn dynamic_error(fallback: &str) -> String {
    let error = unsafe { libc::dlerror() };
    if error.is_null() {
        fallback.to_owned()
    } else {
        unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned()
    }
}

pub fn is_content_process(args: *const cef_main_args_t) -> bool {
    let Some(args) = (unsafe { args.as_ref() }) else {
        return false;
    };
    if args.argc < 2 || args.argv.is_null() {
        return false;
    }
    let argument = unsafe { *args.argv.add(1) };
    if argument.is_null() {
        return false;
    }
    let argument = unsafe { CStr::from_ptr(argument) }.to_bytes();
    argument == b"-contentproc" || argument == b"--contentproc"
}

fn terminated_argument_vector(args: &cef_main_args_t) -> RuntimeResult<Vec<*mut c_char>> {
    let argument_count = usize::try_from(args.argc)?;
    if argument_count != 0 && args.argv.is_null() {
        return Err("missing CEF process argument vector".into());
    }
    let mut arguments = if argument_count == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(args.argv, argument_count) }.to_vec()
    };
    arguments.push(ptr::null_mut());
    Ok(arguments)
}

pub fn execute_child(args: *const cef_main_args_t) -> RuntimeResult<c_int> {
    let args = unsafe { args.as_ref() }.ok_or("missing CEF process arguments")?;
    let mut arguments = terminated_argument_vector(args)?;
    let app_ini = path_cstring(&app_ini_path()?)?;
    Ok(unsafe { (gecko()?.run)(args.argc, arguments.as_mut_ptr(), app_ini.as_ptr()) })
}

pub fn initialize(root_cache_path: &str) -> RuntimeResult<()> {
    let app_ini = app_ini_path()?;
    let profile = PathBuf::from(root_cache_path);
    let cache = std::env::var_os("DEB_PROFILE_CACHE_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| profile.join("cache"));
    fs::create_dir_all(&profile)?;
    fs::create_dir_all(&cache)?;
    fs::write(
        profile.join("user.js"),
        format!(
            concat!(
                "user_pref(\"browser.startup.blankWindow\", false);\n",
                "user_pref(\"extensions.enabledScopes\", 0);\n",
                "user_pref(\"browser.cache.disk.parent_directory\", \"{}\");\n",
            ),
            javascript_string(cache.to_string_lossy().as_ref()),
        ),
    )?;
    unsafe { std::env::set_var("FIREFOX_CEF_APP_INI", &app_ini) };
    *config().lock().unwrap_or_else(|error| error.into_inner()) =
        Some(RuntimeConfig { profile, app_ini });
    let callbacks = FirefoxCefCallbacks {
        size: mem::size_of::<FirefoxCefCallbacks>(),
        context: ptr::null_mut(),
        on_after_created: Some(on_after_created),
        on_address_change: Some(on_address_change),
        on_title_change: Some(on_title_change),
        on_loading_state_change: Some(on_loading_state_change),
        on_load_error: Some(on_load_error),
        on_browser_crashed: Some(on_browser_crashed),
        on_before_close: Some(on_before_close),
        on_cookie_changed: Some(on_cookie_changed),
    };
    unsafe { (gecko()?.set_callbacks)(&callbacks) };
    Ok(())
}

fn javascript_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\u{2028}' => escaped.push_str("\\u2028"),
            '\u{2029}' => escaped.push_str("\\u2029"),
            _ => escaped.push(character),
        }
    }
    escaped
}

pub fn run_message_loop() -> RuntimeResult<c_int> {
    let runtime = config()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .ok_or("FirefoxCEF was not initialized")?;
    let executable = std::env::current_exe()?;
    let arguments = [
        path_cstring(&executable)?,
        CString::new("--profile")?,
        path_cstring(&runtime.profile)?,
        CString::new("--no-remote")?,
    ];
    let mut pointers = arguments
        .iter()
        .map(|argument| argument.as_ptr().cast_mut())
        .collect::<Vec<_>>();
    let argument_count = c_int::try_from(pointers.len())?;
    pointers.push(ptr::null_mut());
    let app_ini = path_cstring(&runtime.app_ini)?;
    Ok(unsafe { (gecko()?.run)(argument_count, pointers.as_mut_ptr(), app_ini.as_ptr()) })
}

pub fn post_task(
    callback: Option<unsafe extern "C" fn(*mut c_void)>,
    context: *mut c_void,
) -> RuntimeResult<()> {
    if unsafe { (gecko()?.post_task)(callback, context) } == 0 {
        return Err("Gecko rejected a main-thread task".into());
    }
    Ok(())
}

pub unsafe fn visit_cookies(
    visitor: Option<CookieVisitor>,
    completion: Option<CookieCompletion>,
    context: *mut c_void,
) -> RuntimeResult<()> {
    if unsafe { (gecko()?.visit_cookies)(visitor, completion, context) } == 0 {
        return Err("Gecko rejected a cookie snapshot".into());
    }
    Ok(())
}

pub unsafe fn set_cookie(
    cookie: *const FirefoxCefCookie,
    completion: Option<CookieCompletion>,
    context: *mut c_void,
) -> RuntimeResult<()> {
    if unsafe { (gecko()?.set_cookie)(cookie, completion, context) } == 0 {
        return Err("Gecko rejected a cookie mutation".into());
    }
    Ok(())
}

pub unsafe fn delete_cookie(
    cookie: *const FirefoxCefCookie,
    completion: Option<CookieCompletion>,
    context: *mut c_void,
) -> RuntimeResult<()> {
    if unsafe { (gecko()?.delete_cookie)(cookie, completion, context) } == 0 {
        return Err("Gecko rejected a cookie deletion".into());
    }
    Ok(())
}

pub fn quit_message_loop() {
    if let Ok(api) = gecko()
        && let Err(error) = api.command(api.shutdown, "message-loop quit")
    {
        eprintln!("firefox-cef: message-loop quit failed: {error}");
    }
}

impl BrowserState {
    pub fn create(parent: u32, width: u32, height: u32, url: &str) -> RuntimeResult<Arc<Self>> {
        let id = NEXT_BROWSER_ID.fetch_add(1, Ordering::Relaxed);
        let id = i32::try_from(id)?;
        let state = Arc::new(Self {
            id,
            parent,
            width: AtomicU32::new(width.max(2)),
            height: AtomicU32::new(height.max(2)),
            browser: AtomicPtr::new(ptr::null_mut()),
            host: AtomicPtr::new(ptr::null_mut()),
            frame: AtomicPtr::new(ptr::null_mut()),
            client: AtomicPtr::new(ptr::null_mut()),
            window: AtomicU32::new(0),
            current_url: Mutex::new(url.to_owned()),
            loading: AtomicBool::new(false),
            after_created: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        let url = CString::new(url)?;
        let api = gecko()?;
        if unsafe {
            (api.configure)(
                id as u32,
                u64::from(parent),
                width.max(2),
                height.max(2),
                url.as_ptr(),
            )
        } == 0
        {
            return Err("Gecko rejected browser configuration".into());
        }
        states()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(state.clone());
        Ok(state)
    }

    pub fn window(&self) -> u32 {
        self.window.load(Ordering::Acquire)
    }

    pub fn is_valid(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    pub fn is_loading(&self) -> bool {
        self.loading.load(Ordering::Acquire)
    }

    pub fn take_client(&self, client: *mut _cef_client_t) {
        self.client.store(client, Ordering::Release);
    }

    pub fn navigate(&self, url: &str) -> RuntimeResult<()> {
        if !self.is_valid() {
            return Err("browser is closed".into());
        }
        let url = CString::new(url)?;
        if unsafe { (gecko()?.navigate)(self.id as u32, url.as_ptr()) } == 0 {
            return Err("Gecko rejected navigation".into());
        }
        *self
            .current_url
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = url.to_string_lossy().into_owned();
        Ok(())
    }

    pub fn reload(&self) -> RuntimeResult<()> {
        let api = gecko()?;
        api.browser_command(api.reload, self.id, "reload")
    }

    pub fn focus(&self) -> RuntimeResult<()> {
        let api = gecko()?;
        api.browser_command(api.focus, self.id, "focus")
    }

    pub fn set_visible(&self, visible: bool) -> RuntimeResult<()> {
        if unsafe { (gecko()?.set_visibility)(self.id as u32, u8::from(visible)) } == 0 {
            return Err(format!("Gecko rejected visibility for browser {}", self.id).into());
        }
        Ok(())
    }

    pub fn sync_from_parent(&self, focus: bool) -> RuntimeResult<()> {
        let (connection, _) = x11rb::connect(None)?;
        let geometry = connection.get_geometry(self.parent)?.reply()?;
        let width = u32::from(geometry.width).max(2);
        let height = u32::from(geometry.height).max(2);
        self.width.store(width, Ordering::Release);
        self.height.store(height, Ordering::Release);
        let window = self.window();
        if window != 0 {
            connection.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .x(0)
                    .y(0)
                    .width(width)
                    .height(height)
                    .border_width(0),
            )?;
            connection.flush()?;
        }
        if focus {
            self.focus()?;
        }
        Ok(())
    }

    pub fn close(&self, force: bool) {
        if self.closed.load(Ordering::Acquire) {
            return;
        }
        if let Ok(api) = gecko() {
            let accepted = unsafe { (api.close)(self.id as u32, u8::from(force)) } != 0;
            if !accepted {
                eprintln!("firefox-cef: close failed for browser {}", self.id);
            }
        }
    }

    pub fn notify_after_created(&self) {
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() {
            return;
        }
        if self
            .after_created
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        unsafe {
            let Some(get_handler) = (*client).get_life_span_handler else {
                return;
            };
            let handler = get_handler(client);
            if handler.is_null() {
                return;
            }
            if let Some(callback) = (*handler).on_after_created {
                add_ref_raw(browser);
                callback(handler, browser);
            }
            release_raw(handler);
        }
    }

    pub fn notify_address(&self, url: &str) {
        *self
            .current_url
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = url.to_owned();
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        let frame = self.frame.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() || frame.is_null() {
            return;
        }
        let mut url = url.encode_utf16().collect::<Vec<_>>();
        let url = cef_string_t {
            str_: url.as_mut_ptr(),
            length: url.len(),
            dtor: None,
        };
        unsafe {
            if let Some(get_handler) = (*client).get_load_handler {
                let handler = get_handler(client);
                if !handler.is_null() {
                    if let Some(callback) = (*handler).on_load_start {
                        add_ref_raw(browser);
                        add_ref_raw(frame);
                        callback(handler, browser, frame, cef_transition_type_t::TT_EXPLICIT);
                    }
                    release_raw(handler);
                }
            }
            if let Some(get_handler) = (*client).get_display_handler {
                let handler = get_handler(client);
                if !handler.is_null() {
                    if let Some(callback) = (*handler).on_address_change {
                        add_ref_raw(browser);
                        add_ref_raw(frame);
                        callback(handler, browser, frame, &url);
                    }
                    release_raw(handler);
                }
            }
        }
    }

    pub fn notify_title(&self, title: &str) {
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() {
            return;
        }
        let mut title = title.encode_utf16().collect::<Vec<_>>();
        let title = cef_string_t {
            str_: title.as_mut_ptr(),
            length: title.len(),
            dtor: None,
        };
        unsafe {
            let Some(get_handler) = (*client).get_display_handler else {
                return;
            };
            let handler = get_handler(client);
            if handler.is_null() {
                return;
            }
            if let Some(callback) = (*handler).on_title_change {
                add_ref_raw(browser);
                callback(handler, browser, &title);
            }
            release_raw(handler);
        }
    }

    pub fn notify_crashed(&self, reason: &str) {
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() {
            return;
        }
        let mut reason = reason.encode_utf16().collect::<Vec<_>>();
        let reason = cef_string_t {
            str_: reason.as_mut_ptr(),
            length: reason.len(),
            dtor: None,
        };
        unsafe {
            let Some(get_handler) = (*client).get_request_handler else {
                return;
            };
            let handler = get_handler(client);
            if handler.is_null() {
                return;
            }
            if let Some(callback) = (*handler).on_render_process_terminated {
                add_ref_raw(browser);
                callback(
                    handler,
                    browser,
                    cef_termination_status_t::TS_PROCESS_CRASHED,
                    0,
                    &reason,
                );
            }
            release_raw(handler);
        }
    }

    pub fn notify_loading(&self, loading: bool) {
        self.loading.store(loading, Ordering::Release);
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() {
            return;
        }
        unsafe {
            let Some(get_handler) = (*client).get_load_handler else {
                return;
            };
            let handler = get_handler(client);
            if handler.is_null() {
                return;
            }
            if let Some(callback) = (*handler).on_loading_state_change {
                add_ref_raw(browser);
                callback(handler, browser, i32::from(loading), 0, 0);
            }
            release_raw(handler);
        }
    }

    pub fn notify_load_error(&self, error_text: &str, failed_url: &str) {
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        let frame = self.frame.load(Ordering::Acquire);
        if client.is_null() || browser.is_null() || frame.is_null() {
            return;
        }
        let mut error_text = error_text.encode_utf16().collect::<Vec<_>>();
        let mut failed_url = failed_url.encode_utf16().collect::<Vec<_>>();
        let error_text = cef_string_t {
            str_: error_text.as_mut_ptr(),
            length: error_text.len(),
            dtor: None,
        };
        let failed_url = cef_string_t {
            str_: failed_url.as_mut_ptr(),
            length: failed_url.len(),
            dtor: None,
        };
        unsafe {
            let Some(get_handler) = (*client).get_load_handler else {
                return;
            };
            let handler = get_handler(client);
            if handler.is_null() {
                return;
            }
            if let Some(callback) = (*handler).on_load_error {
                add_ref_raw(browser);
                add_ref_raw(frame);
                callback(
                    handler,
                    browser,
                    frame,
                    cef_errorcode_t::ERR_FAILED,
                    &error_text,
                    &failed_url,
                );
            }
            release_raw(handler);
        }
    }

    fn notify_before_close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let client = self.client.load(Ordering::Acquire);
        let browser = self.browser.load(Ordering::Acquire);
        if !client.is_null() && !browser.is_null() {
            unsafe {
                if let Some(get_handler) = (*client).get_life_span_handler {
                    let handler = get_handler(client);
                    if !handler.is_null() {
                        if let Some(callback) = (*handler).on_before_close {
                            add_ref_raw(browser);
                            callback(handler, browser);
                        }
                        release_raw(handler);
                    }
                }
            }
        }
        let client = self.client.swap(ptr::null_mut(), Ordering::AcqRel);
        unsafe { release_raw(client) };
    }

    fn release_objects(&self) {
        let host = self.host.swap(ptr::null_mut(), Ordering::AcqRel);
        let frame = self.frame.swap(ptr::null_mut(), Ordering::AcqRel);
        let browser = self.browser.swap(ptr::null_mut(), Ordering::AcqRel);
        unsafe {
            release_raw(host);
            release_raw(frame);
            release_raw(browser);
        }
    }
}

unsafe extern "C" fn on_after_created(_context: *mut c_void, id: i32, window: u64) {
    let Some(state) = find_state(id) else {
        return;
    };
    let Ok(window) = u32::try_from(window) else {
        return;
    };
    state.window.store(window, Ordering::Release);
    state.notify_after_created();
}

unsafe extern "C" fn on_loading_state_change(_context: *mut c_void, id: i32, loading: u8) {
    if let Some(state) = find_state(id) {
        state.notify_loading(loading != 0);
    }
}

unsafe extern "C" fn on_address_change(_context: *mut c_void, id: i32, url: *const c_char) {
    if let Some(state) = find_state(id) {
        state.notify_address(&c_string(url));
    }
}

unsafe extern "C" fn on_title_change(_context: *mut c_void, id: i32, title: *const c_char) {
    if let Some(state) = find_state(id) {
        state.notify_title(&c_string(title));
    }
}

unsafe extern "C" fn on_load_error(
    _context: *mut c_void,
    id: i32,
    _error_code: i32,
    error_text: *const c_char,
    failed_url: *const c_char,
) {
    let Some(state) = find_state(id) else {
        return;
    };
    let error_text = c_string(error_text);
    let failed_url = c_string(failed_url);
    state.notify_load_error(&error_text, &failed_url);
}

unsafe extern "C" fn on_browser_crashed(_context: *mut c_void, id: i32, reason: *const c_char) {
    if let Some(state) = find_state(id) {
        state.notify_crashed(&c_string(reason));
    }
}

fn c_string(value: *const c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

unsafe extern "C" fn on_before_close(_context: *mut c_void, id: i32) {
    if let Some(state) = remove_state(id) {
        state.notify_before_close();
        state.release_objects();
    }
}

unsafe extern "C" fn on_cookie_changed(
    _context: *mut c_void,
    cookie: *const FirefoxCefCookie,
    action: u8,
) {
    unsafe { crate::notify_cookie_changed(cookie, action) };
}

pub fn shutdown_all() {
    let states = states()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .drain(..)
        .collect::<Vec<_>>();
    for state in states {
        state.notify_before_close();
        state.release_objects();
    }
    if let Ok(api) = gecko() {
        unsafe { (api.set_callbacks)(ptr::null()) };
    }
}

pub unsafe extern "C" fn execute_cef_task(context: *mut c_void) {
    let task = context.cast::<_cef_task_t>();
    if let Some(task) = unsafe { task.as_mut() }
        && let Some(execute) = task.execute
    {
        unsafe { execute(task) };
    }
    unsafe { release_raw(task) };
}

#[cfg(test)]
mod tests {
    use super::{is_content_process, javascript_string, terminated_argument_vector};
    use cef_dll_sys::cef_main_args_t;
    use std::ffi::CString;

    #[test]
    fn detects_gecko_content_process_arguments() {
        let mut values = [
            CString::new("cef-renderer").unwrap(),
            CString::new("-contentproc").unwrap(),
        ];
        let mut pointers = values
            .iter_mut()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        let arguments = cef_main_args_t {
            argc: pointers.len() as i32,
            argv: pointers.as_mut_ptr(),
        };
        assert!(is_content_process(&arguments));
    }

    #[test]
    fn does_not_treat_browser_arguments_as_content_process() {
        let mut values = [
            CString::new("cef-renderer").unwrap(),
            CString::new("--parent").unwrap(),
        ];
        let mut pointers = values
            .iter_mut()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        let arguments = cef_main_args_t {
            argc: pointers.len() as i32,
            argv: pointers.as_mut_ptr(),
        };
        assert!(!is_content_process(&arguments));
    }

    #[test]
    fn terminates_gecko_child_argument_vectors() {
        let mut values = [
            CString::new("cef-renderer").unwrap(),
            CString::new("-contentproc").unwrap(),
        ];
        let mut pointers = values
            .iter_mut()
            .map(|value| value.as_ptr().cast_mut())
            .collect::<Vec<_>>();
        let arguments = cef_main_args_t {
            argc: pointers.len() as i32,
            argv: pointers.as_mut_ptr(),
        };

        let terminated = terminated_argument_vector(&arguments).unwrap();

        assert_eq!(&terminated[..pointers.len()], pointers.as_slice());
        assert!(terminated.last().unwrap().is_null());
    }

    #[test]
    fn rejects_missing_gecko_child_argument_vector() {
        let arguments = cef_main_args_t {
            argc: 1,
            argv: std::ptr::null_mut(),
        };
        assert!(terminated_argument_vector(&arguments).is_err());
    }

    #[test]
    fn escapes_profile_cache_paths_for_user_js() {
        assert_eq!(javascript_string("/tmp/a\\b\"c\n"), "/tmp/a\\\\b\\\"c\\n");
    }
}
