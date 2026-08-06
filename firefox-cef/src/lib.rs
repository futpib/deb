mod cookies;
mod refcount;
mod runtime;
mod strings;

use cef_dll_sys::{
    _cef_app_t, _cef_browser_host_t, _cef_browser_settings_t, _cef_browser_t, _cef_client_t,
    _cef_dictionary_value_t, _cef_frame_t, _cef_request_context_t, _cef_scheme_handler_factory_t,
    _cef_settings_t, _cef_task_t, cef_key_event_t, cef_main_args_t, cef_mouse_button_type_t,
    cef_mouse_event_t, cef_string_t, cef_thread_id_t, cef_window_handle_t, cef_window_info_t,
};
use libc::{c_char, c_int, c_void};
use refcount::{CefRefCounted, RefObject, add_ref_raw, release_raw};
use runtime::{BrowserState, shutdown_all};
use std::{
    ptr,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use strings::cef_string_to_string;

const API_HASH_15000_LINUX: &[u8] = b"210767725a6feb2e4becd3956b648cab6a006712\0";
const CEF_COMMIT_HASH: &[u8] = b"7c1aa68455db1f1fad159c2b83070ad318212b3d\0";
const CEF_SANDBOX_COMPAT_HASH: &[u8] = b"\0";
static CONFIGURED_API_VERSION: AtomicI32 = AtomicI32::new(0);

fn state_from<T: CefRefCounted>(raw: *mut T) -> Arc<BrowserState> {
    unsafe { RefObject::<T, Arc<BrowserState>>::get(raw).state.clone() }
}

unsafe extern "C" fn browser_is_valid(browser: *mut _cef_browser_t) -> c_int {
    i32::from(state_from(browser).is_valid())
}

unsafe extern "C" fn browser_get_host(browser: *mut _cef_browser_t) -> *mut _cef_browser_host_t {
    let host = state_from(browser).host.load(Ordering::Acquire);
    unsafe { add_ref_raw(host) };
    host
}

unsafe extern "C" fn browser_is_loading(browser: *mut _cef_browser_t) -> c_int {
    i32::from(state_from(browser).is_loading())
}

unsafe extern "C" fn browser_reload(browser: *mut _cef_browser_t) {
    if let Err(error) = state_from(browser).reload() {
        eprintln!("firefox-cef: reload failed: {error}");
    }
}

unsafe extern "C" fn browser_get_identifier(browser: *mut _cef_browser_t) -> c_int {
    state_from(browser).id
}

unsafe extern "C" fn browser_is_same(
    browser: *mut _cef_browser_t,
    other: *mut _cef_browser_t,
) -> c_int {
    i32::from(browser == other)
}

unsafe extern "C" fn browser_false(_browser: *mut _cef_browser_t) -> c_int {
    0
}

unsafe extern "C" fn browser_has_document(browser: *mut _cef_browser_t) -> c_int {
    i32::from(state_from(browser).is_valid())
}

unsafe extern "C" fn browser_get_main_frame(browser: *mut _cef_browser_t) -> *mut _cef_frame_t {
    let frame = state_from(browser).frame.load(Ordering::Acquire);
    unsafe { add_ref_raw(frame) };
    frame
}

unsafe extern "C" fn browser_get_frame_count(_browser: *mut _cef_browser_t) -> usize {
    1
}

unsafe extern "C" fn host_get_browser(host: *mut _cef_browser_host_t) -> *mut _cef_browser_t {
    let browser = state_from(host).browser.load(Ordering::Acquire);
    unsafe { add_ref_raw(browser) };
    browser
}

unsafe extern "C" fn host_close_browser(host: *mut _cef_browser_host_t, force: c_int) {
    state_from(host).close(force != 0);
}

unsafe extern "C" fn host_try_close_browser(host: *mut _cef_browser_host_t) -> c_int {
    state_from(host).close(false);
    1
}

unsafe extern "C" fn host_is_ready_to_close(host: *mut _cef_browser_host_t) -> c_int {
    i32::from(!state_from(host).is_valid())
}

unsafe extern "C" fn host_set_focus(host: *mut _cef_browser_host_t, focus: c_int) {
    if focus != 0
        && let Err(error) = state_from(host).focus()
    {
        eprintln!("firefox-cef: focus failed: {error}");
    }
}

unsafe extern "C" fn host_send_key_event(
    host: *mut _cef_browser_host_t,
    event: *const cef_key_event_t,
) {
    let Some(event) = (unsafe { event.as_ref() }) else {
        return;
    };
    if let Err(error) = state_from(host).send_key_event(
        event.type_ as u32,
        event.modifiers,
        event.windows_key_code,
        event.native_key_code,
        event.is_system_key != 0,
        event.character,
        event.unmodified_character,
    ) {
        eprintln!("firefox-cef: key event failed: {error}");
    }
}

unsafe extern "C" fn host_get_window_handle(host: *mut _cef_browser_host_t) -> cef_window_handle_t {
    state_from(host).window() as cef_window_handle_t
}

unsafe extern "C" fn host_get_client(host: *mut _cef_browser_host_t) -> *mut _cef_client_t {
    let client = state_from(host).client.load(Ordering::Acquire);
    unsafe { add_ref_raw(client) };
    client
}

unsafe extern "C" fn host_notify_resize(host: *mut _cef_browser_host_t) {
    if let Err(error) = state_from(host).sync_from_parent(false) {
        eprintln!("firefox-cef: resize failed: {error}");
    }
}

unsafe extern "C" fn host_was_hidden(host: *mut _cef_browser_host_t, hidden: c_int) {
    if let Err(error) = state_from(host).set_visible(hidden == 0) {
        eprintln!("firefox-cef: visibility update failed: {error}");
    }
}

unsafe extern "C" fn host_send_mouse_move(
    host: *mut _cef_browser_host_t,
    event: *const cef_mouse_event_t,
    mouse_leave: c_int,
) {
    let Some(event) = (unsafe { event.as_ref() }) else {
        return;
    };
    if let Err(error) =
        state_from(host).send_mouse_move(event.x, event.y, event.modifiers, mouse_leave != 0)
    {
        eprintln!("firefox-cef: mouse move failed: {error}");
    }
}

unsafe extern "C" fn host_send_mouse_click(
    host: *mut _cef_browser_host_t,
    event: *const cef_mouse_event_t,
    button: cef_mouse_button_type_t,
    mouse_up: c_int,
    click_count: c_int,
) {
    let Some(event) = (unsafe { event.as_ref() }) else {
        return;
    };
    if let Err(error) = state_from(host).send_mouse_click(
        event.x,
        event.y,
        event.modifiers,
        button as u32,
        mouse_up != 0,
        click_count,
    ) {
        eprintln!("firefox-cef: mouse click failed: {error}");
    }
}

unsafe extern "C" fn host_send_mouse_wheel(
    host: *mut _cef_browser_host_t,
    event: *const cef_mouse_event_t,
    delta_x: c_int,
    delta_y: c_int,
) {
    let Some(event) = (unsafe { event.as_ref() }) else {
        return;
    };
    if let Err(error) =
        state_from(host).send_mouse_wheel(event.x, event.y, event.modifiers, delta_x, delta_y)
    {
        eprintln!("firefox-cef: mouse wheel failed: {error}");
    }
}

unsafe extern "C" fn frame_is_valid(frame: *mut _cef_frame_t) -> c_int {
    i32::from(state_from(frame).is_valid())
}

unsafe extern "C" fn frame_load_url(frame: *mut _cef_frame_t, url: *const cef_string_t) {
    let Some(url) = (unsafe { cef_string_to_string(url) }) else {
        return;
    };
    if let Err(error) = state_from(frame).navigate(&url) {
        eprintln!("firefox-cef: navigation failed: {error}");
    }
}

unsafe extern "C" fn frame_true(_frame: *mut _cef_frame_t) -> c_int {
    1
}

unsafe extern "C" fn frame_get_browser(frame: *mut _cef_frame_t) -> *mut _cef_browser_t {
    let browser = state_from(frame).browser.load(Ordering::Acquire);
    unsafe { add_ref_raw(browser) };
    browser
}

fn make_browser_objects(state: Arc<BrowserState>) -> *mut _cef_browser_t {
    let mut host = unsafe { std::mem::zeroed::<_cef_browser_host_t>() };
    host.get_browser = Some(host_get_browser);
    host.close_browser = Some(host_close_browser);
    host.try_close_browser = Some(host_try_close_browser);
    host.is_ready_to_be_closed = Some(host_is_ready_to_close);
    host.set_focus = Some(host_set_focus);
    host.send_key_event = Some(host_send_key_event);
    host.get_window_handle = Some(host_get_window_handle);
    host.get_client = Some(host_get_client);
    host.notify_move_or_resize_started = Some(host_notify_resize);
    host.was_hidden = Some(host_was_hidden);
    host.send_mouse_move_event = Some(host_send_mouse_move);
    host.send_mouse_click_event = Some(host_send_mouse_click);
    host.send_mouse_wheel_event = Some(host_send_mouse_wheel);
    let host = RefObject::allocate(host, state.clone());
    state.host.store(host, Ordering::Release);

    let mut frame = unsafe { std::mem::zeroed::<_cef_frame_t>() };
    frame.is_valid = Some(frame_is_valid);
    frame.load_url = Some(frame_load_url);
    frame.is_main = Some(frame_true);
    frame.is_focused = Some(frame_true);
    frame.get_browser = Some(frame_get_browser);
    let frame = RefObject::allocate(frame, state.clone());
    state.frame.store(frame, Ordering::Release);

    let mut browser = unsafe { std::mem::zeroed::<_cef_browser_t>() };
    browser.is_valid = Some(browser_is_valid);
    browser.get_host = Some(browser_get_host);
    browser.is_loading = Some(browser_is_loading);
    browser.reload = Some(browser_reload);
    browser.reload_ignore_cache = Some(browser_reload);
    browser.get_identifier = Some(browser_get_identifier);
    browser.is_same = Some(browser_is_same);
    browser.is_popup = Some(browser_false);
    browser.has_document = Some(browser_has_document);
    browser.get_main_frame = Some(browser_get_main_frame);
    browser.get_focused_frame = Some(browser_get_main_frame);
    browser.get_frame_count = Some(browser_get_frame_count);
    let browser = RefObject::allocate(browser, state.clone());
    state.browser.store(browser, Ordering::Release);
    unsafe { add_ref_raw(browser) };
    browser
}

#[unsafe(no_mangle)]
pub extern "C" fn cef_api_hash(version: c_int, entry: c_int) -> *const c_char {
    let hash: *const c_char = match version {
        15000 | 999998 => API_HASH_15000_LINUX.as_ptr().cast(),
        cef_cookie::CEF_API_VERSION_EXPERIMENTAL => {
            cef_cookie::CEF_API_HASH_EXPERIMENTAL_LINUX.as_ptr().cast()
        }
        _ => return ptr::null(),
    };
    if CONFIGURED_API_VERSION
        .compare_exchange(0, version, Ordering::AcqRel, Ordering::Acquire)
        .is_err_and(|configured| configured != version)
    {
        return ptr::null();
    }
    match entry {
        0 | 1 => hash,
        2 => CEF_COMMIT_HASH.as_ptr().cast(),
        3 => CEF_SANDBOX_COMPAT_HASH.as_ptr().cast(),
        _ => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cef_api_version() -> c_int {
    CONFIGURED_API_VERSION.load(Ordering::Acquire)
}

#[unsafe(no_mangle)]
/// Registers the build-owned custom scheme used by the shared helper.
///
/// # Safety
///
/// String pointers must be null or valid CEF UTF-16 strings. `factory` must be
/// null or carry the reference transferred by the CEF caller.
pub unsafe extern "C" fn cef_register_scheme_handler_factory(
    scheme_name: *const cef_string_t,
    domain_name: *const cef_string_t,
    factory: *mut _cef_scheme_handler_factory_t,
) -> c_int {
    let scheme_name = unsafe { cef_string_to_string(scheme_name) };
    let domain_name = unsafe { cef_string_to_string(domain_name) };
    unsafe { release_raw(factory) };
    i32::from(
        scheme_name.as_deref() == Some("deb")
            && domain_name.as_deref() == Some("new-tab")
            && !factory.is_null(),
    )
}

#[unsafe(no_mangle)]
/// # Safety
///
/// The pointers must either be null or point to values that follow the CEF C
/// ABI for the duration of this call.
pub unsafe extern "C" fn cef_execute_process(
    args: *const cef_main_args_t,
    application: *mut _cef_app_t,
    _sandbox: *mut c_void,
) -> c_int {
    unsafe { refcount::release_raw(application) };
    if runtime::is_content_process(args) {
        return match runtime::execute_child(args) {
            Ok(code) => code,
            Err(error) => {
                eprintln!("firefox-cef: Gecko child startup failed: {error}");
                1
            }
        };
    }
    -1
}

#[unsafe(no_mangle)]
/// # Safety
///
/// The pointers must either be null or point to values that follow the CEF C
/// ABI for the duration of this call. `application` callbacks may be invoked.
pub unsafe extern "C" fn cef_initialize(
    _args: *const cef_main_args_t,
    settings: *const _cef_settings_t,
    application: *mut _cef_app_t,
    _sandbox: *mut c_void,
) -> c_int {
    let Some(settings) = (unsafe { settings.as_ref() }) else {
        unsafe { refcount::release_raw(application) };
        return 0;
    };
    if settings.multi_threaded_message_loop != 0 {
        eprintln!("firefox-cef: Gecko requires the CEF loop on the process main thread");
        unsafe { refcount::release_raw(application) };
        return 0;
    }
    let root_cache_path = unsafe { cef_string_to_string(&settings.root_cache_path) }
        .filter(|path| !path.is_empty())
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join(format!("firefox-cef-{}", std::process::id()))
                .to_string_lossy()
                .into_owned()
        });
    if let Err(error) = runtime::initialize(&root_cache_path) {
        eprintln!("firefox-cef: Gecko initialization failed: {error}");
        unsafe { refcount::release_raw(application) };
        return 0;
    }
    if let Some(application) = unsafe { application.as_mut() }
        && let Some(get_handler) = application.get_browser_process_handler
    {
        let handler = unsafe { get_handler(application) };
        if !handler.is_null() {
            if let Some(callback) = unsafe { (*handler).on_context_initialized } {
                unsafe { callback(handler) };
            }
            unsafe { refcount::release_raw(handler) };
        }
    }
    unsafe { refcount::release_raw(application) };
    eprintln!("firefox-cef: initialized the in-process Gecko runtime");
    1
}

#[unsafe(no_mangle)]
/// # Safety
///
/// The arguments must follow the CEF C ABI. Non-null pointers must remain
/// valid for the duration of this call, including any invoked client callbacks.
pub unsafe extern "C" fn cef_browser_host_create_browser_sync(
    window_info: *const cef_window_info_t,
    client: *mut _cef_client_t,
    url: *const cef_string_t,
    _settings: *const _cef_browser_settings_t,
    extra_info: *mut _cef_dictionary_value_t,
    request_context: *mut _cef_request_context_t,
) -> *mut _cef_browser_t {
    unsafe {
        refcount::release_raw(extra_info);
        refcount::release_raw(request_context);
    }
    let Some(window_info) = (unsafe { window_info.as_ref() }) else {
        unsafe { refcount::release_raw(client) };
        return ptr::null_mut();
    };
    let Some(url) = (unsafe { cef_string_to_string(url) }) else {
        unsafe { refcount::release_raw(client) };
        return ptr::null_mut();
    };
    let parent = window_info.parent_window as u32;
    let width = window_info.bounds.width.max(2) as u32;
    let height = window_info.bounds.height.max(2) as u32;
    let state = match BrowserState::create(parent, width, height, &url) {
        Ok(state) => state,
        Err(error) => {
            eprintln!("firefox-cef: browser creation failed: {error}");
            unsafe { refcount::release_raw(client) };
            return ptr::null_mut();
        }
    };
    state.take_client(client);
    let browser = make_browser_objects(state.clone());
    eprintln!("firefox-cef: queued Gecko browser {} for {url}", state.id);
    browser
}

#[unsafe(no_mangle)]
/// # Safety
///
/// The arguments must follow the CEF C ABI. Non-null pointers must remain
/// valid for the duration of this call, including any invoked client callbacks.
pub unsafe extern "C" fn cef_browser_host_create_browser(
    window_info: *const cef_window_info_t,
    client: *mut _cef_client_t,
    url: *const cef_string_t,
    settings: *const _cef_browser_settings_t,
    extra_info: *mut _cef_dictionary_value_t,
    request_context: *mut _cef_request_context_t,
) -> c_int {
    let browser = unsafe {
        cef_browser_host_create_browser_sync(
            window_info,
            client,
            url,
            settings,
            extra_info,
            request_context,
        )
    };
    if browser.is_null() {
        0
    } else {
        unsafe { refcount::release_raw(browser) };
        1
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cef_do_message_loop_work() {}

#[unsafe(no_mangle)]
unsafe extern "C" fn cef_post_task(_thread: cef_thread_id_t, task: *mut _cef_task_t) -> c_int {
    if task.is_null() {
        return 0;
    }
    match runtime::post_task(Some(runtime::execute_cef_task), task.cast()) {
        Ok(()) => 1,
        Err(error) => {
            unsafe { refcount::release_raw(task) };
            eprintln!("firefox-cef: UI task dispatch failed: {error}");
            0
        }
    }
}

#[unsafe(no_mangle)]
extern "C" fn cef_run_message_loop() {
    match runtime::run_message_loop() {
        Ok(0) => {}
        Ok(code) => eprintln!("firefox-cef: Gecko message loop exited with {code}"),
        Err(error) => eprintln!("firefox-cef: Gecko message loop failed: {error}"),
    }
}

#[unsafe(no_mangle)]
extern "C" fn cef_quit_message_loop() {
    runtime::quit_message_loop();
}

#[unsafe(no_mangle)]
pub extern "C" fn cef_shutdown() {
    cookies::shutdown();
    shutdown_all();
    eprintln!("firefox-cef: shutdown complete");
}

unsafe fn notify_cookie_changed(cookie: *const runtime::FirefoxCefCookie, action: u8) {
    unsafe { cookies::notify_changed(cookie, action) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configures_the_patched_experimental_linux_api_once() {
        let value = cef_api_hash(cef_cookie::CEF_API_VERSION_EXPERIMENTAL, 0);
        assert!(!value.is_null());
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(value) }.to_bytes_with_nul(),
            cef_cookie::CEF_API_HASH_EXPERIMENTAL_LINUX
        );
        assert_eq!(cef_api_version(), cef_cookie::CEF_API_VERSION_EXPERIMENTAL);
        assert!(cef_api_hash(15000, 0).is_null());
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(cef_api_hash(999999, 2)) }
                .to_str()
                .unwrap(),
            "7c1aa68455db1f1fad159c2b83070ad318212b3d"
        );
    }
}
