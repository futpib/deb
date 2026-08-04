mod refcount;
mod runtime;
mod strings;

use cef_dll_sys::{
    _cef_app_t, _cef_browser_host_t, _cef_browser_settings_t, _cef_browser_t, _cef_client_t,
    _cef_dictionary_value_t, _cef_frame_t, _cef_request_context_t, _cef_settings_t, _cef_task_t,
    cef_main_args_t, cef_string_t, cef_thread_id_t, cef_window_handle_t, cef_window_info_t,
};
use libc::{c_char, c_int, c_void};
use refcount::{CefRefCounted, RefObject, add_ref_raw};
use runtime::{BrowserState, shutdown_all};
use std::{
    ptr,
    sync::{Arc, atomic::Ordering},
};
use strings::cef_string_to_string;

const API_HASH_15000_LINUX: &[u8] = b"210767725a6feb2e4becd3956b648cab6a006712\0";
const API_HASH_EXPERIMENTAL_LINUX: &[u8] = b"a5d187477e0cbe23eb1043c2f1868582b7018260\0";

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

unsafe extern "C" fn host_close_browser(host: *mut _cef_browser_host_t, _force: c_int) {
    state_from(host).close();
}

unsafe extern "C" fn host_try_close_browser(host: *mut _cef_browser_host_t) -> c_int {
    state_from(host).close();
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
    host.get_window_handle = Some(host_get_window_handle);
    host.get_client = Some(host_get_client);
    host.notify_move_or_resize_started = Some(host_notify_resize);
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
    if entry > 1 {
        return ptr::null();
    }
    match version {
        15000 | 999998 => API_HASH_15000_LINUX.as_ptr().cast(),
        999999 => API_HASH_EXPERIMENTAL_LINUX.as_ptr().cast(),
        _ => ptr::null(),
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn cef_api_version() -> c_int {
    15000
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
    shutdown_all();
    eprintln!("firefox-cef: shutdown complete");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_the_cef_150_linux_api_hash() {
        let value = cef_api_hash(15000, 0);
        assert!(!value.is_null());
        assert_eq!(
            unsafe { std::ffi::CStr::from_ptr(value) }.to_str().unwrap(),
            "210767725a6feb2e4becd3956b648cab6a006712"
        );
    }
}
