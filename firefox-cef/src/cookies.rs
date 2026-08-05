use cef_cookie::{
    CefCookie, CefCookieChangeObserver, CefCookieManager, CookieChangeCause, RefCounted,
    cef_string, cookie_from_ptr,
};
use cef_dll_sys::{
    _cef_completion_callback_t, _cef_cookie_manager_t, _cef_cookie_t, _cef_cookie_visitor_t,
    _cef_delete_cookies_callback_t, _cef_registration_t, _cef_set_cookie_callback_t,
    cef_basetime_t, cef_cookie_priority_t, cef_cookie_same_site_t, cef_string_t,
};
use libc::{c_int, c_void};
use std::{
    collections::BTreeMap,
    ffi::{CStr, CString},
    mem, ptr,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use crate::{
    refcount::{add_ref_raw, release_raw},
    runtime::{self, FirefoxCefCookie},
};

const WINDOWS_EPOCH_MICROSECONDS: i64 = 11_644_473_600_000_000;
const SESSION_EXPIRY_MILLISECONDS: i64 = 9_007_199_254_740_991;

struct ManagerState {
    observers: Mutex<BTreeMap<u64, usize>>,
    next_observer: AtomicU64,
}

struct RegistrationState {
    manager: Arc<ManagerState>,
    id: u64,
}

impl Drop for RegistrationState {
    fn drop(&mut self) {
        let observer = self
            .manager
            .observers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.id);
        if let Some(observer) = observer {
            unsafe { release_raw(observer as *mut CefCookieChangeObserver) };
        }
    }
}

struct SnapshotContext {
    visitor: *mut _cef_cookie_visitor_t,
    completion: *mut _cef_completion_callback_t,
    cookies: Vec<CookieStorage>,
}

struct MutationContext {
    callback: *mut _cef_set_cookie_callback_t,
}

struct CookieStorage {
    name: Vec<u16>,
    value: Vec<u16>,
    domain: Vec<u16>,
    path: Vec<u16>,
    partition: Vec<u16>,
    raw: CefCookie,
}

struct GeckoCookieStorage {
    name: CString,
    value: CString,
    domain: CString,
    path: CString,
    partition: CString,
    raw: FirefoxCefCookie,
}

unsafe impl Send for SnapshotContext {}
unsafe impl Send for MutationContext {}

static MANAGER: OnceLock<Mutex<Option<usize>>> = OnceLock::new();

fn manager_slot() -> &'static Mutex<Option<usize>> {
    MANAGER.get_or_init(|| Mutex::new(None))
}

fn empty_string() -> cef_string_t {
    cef_string_t {
        str_: ptr::null_mut(),
        length: 0,
        dtor: None,
    }
}

fn borrowed_string(value: &mut [u16]) -> cef_string_t {
    cef_string_t {
        str_: value.as_mut_ptr(),
        length: value.len(),
        dtor: None,
    }
}

fn windows_time(unix_microseconds: i64) -> cef_basetime_t {
    cef_basetime_t {
        val: unix_microseconds.saturating_add(WINDOWS_EPOCH_MICROSECONDS),
    }
}

fn unix_time(time: cef_basetime_t) -> i64 {
    time.val.saturating_sub(WINDOWS_EPOCH_MICROSECONDS)
}

fn gecko_expiry_milliseconds(has_expires: c_int, expires: cef_basetime_t) -> i64 {
    if has_expires != 0 {
        unix_time(expires).div_euclid(1_000)
    } else {
        SESSION_EXPIRY_MILLISECONDS
    }
}

fn c_string(value: *const libc::c_char) -> String {
    if value.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(value) }
            .to_string_lossy()
            .into_owned()
    }
}

impl CookieStorage {
    fn from_gecko(cookie: &FirefoxCefCookie) -> Self {
        let mut storage = Self {
            name: c_string(cookie.name).encode_utf16().collect(),
            value: c_string(cookie.value).encode_utf16().collect(),
            domain: c_string(cookie.domain).encode_utf16().collect(),
            path: c_string(cookie.path).encode_utf16().collect(),
            partition: c_string(cookie.partition_key_top_level_site)
                .encode_utf16()
                .collect(),
            raw: CefCookie {
                size: mem::size_of::<CefCookie>(),
                name: empty_string(),
                value: empty_string(),
                domain: empty_string(),
                path: empty_string(),
                secure: c_int::from(cookie.secure != 0),
                httponly: c_int::from(cookie.http_only != 0),
                creation: windows_time(cookie.creation_microseconds),
                last_access: windows_time(cookie.last_access_microseconds),
                has_expires: c_int::from(cookie.session == 0),
                expires: windows_time(cookie.expires_milliseconds.saturating_mul(1_000)),
                same_site: match cookie.same_site {
                    0 => cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_NO_RESTRICTION,
                    1 => cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_LAX_MODE,
                    2 => cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_STRICT_MODE,
                    _ => cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_UNSPECIFIED,
                },
                priority: cef_cookie_priority_t::CEF_COOKIE_PRIORITY_MEDIUM,
                last_update: windows_time(cookie.update_microseconds),
                has_partition_key: c_int::from(cookie.partitioned != 0),
                partition_key_top_level_site: empty_string(),
                partition_key_has_cross_site_ancestor: c_int::from(
                    cookie.partition_key_has_cross_site_ancestor != 0,
                ),
            },
        };
        storage.refresh_strings();
        storage
    }

    fn refresh_strings(&mut self) {
        self.raw.name = borrowed_string(&mut self.name);
        self.raw.value = borrowed_string(&mut self.value);
        self.raw.domain = borrowed_string(&mut self.domain);
        self.raw.path = borrowed_string(&mut self.path);
        self.raw.partition_key_top_level_site = borrowed_string(&mut self.partition);
    }
}

impl GeckoCookieStorage {
    unsafe fn from_cef(cookie: *const _cef_cookie_t) -> Option<Self> {
        let cookie = unsafe { cookie_from_ptr(cookie)? };
        let name = CString::new(unsafe { cef_string(&cookie.name) }).ok()?;
        let value = CString::new(unsafe { cef_string(&cookie.value) }).ok()?;
        let domain = CString::new(unsafe { cef_string(&cookie.domain) }).ok()?;
        let path = CString::new(unsafe { cef_string(&cookie.path) }).ok()?;
        let partition = CString::new(if cookie.has_partition_key != 0 {
            unsafe { cef_string(&cookie.partition_key_top_level_site) }
        } else {
            String::new()
        })
        .ok()?;
        let mut storage = Self {
            name,
            value,
            domain,
            path,
            partition,
            raw: FirefoxCefCookie {
                name: ptr::null(),
                value: ptr::null(),
                domain: ptr::null(),
                path: ptr::null(),
                partition_key_top_level_site: ptr::null(),
                secure: u8::from(cookie.secure != 0),
                http_only: u8::from(cookie.httponly != 0),
                session: u8::from(cookie.has_expires == 0),
                partitioned: u8::from(cookie.has_partition_key != 0),
                partition_key_has_cross_site_ancestor: u8::from(
                    cookie.partition_key_has_cross_site_ancestor != 0,
                ),
                expires_milliseconds: gecko_expiry_milliseconds(cookie.has_expires, cookie.expires),
                creation_microseconds: unix_time(cookie.creation),
                last_access_microseconds: unix_time(cookie.last_access),
                update_microseconds: unix_time(cookie.last_update),
                same_site: match cookie.same_site {
                    cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_NO_RESTRICTION => 0,
                    cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_LAX_MODE => 1,
                    cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_STRICT_MODE => 2,
                    _ => 256,
                },
            },
        };
        storage.refresh_pointers();
        Some(storage)
    }

    fn refresh_pointers(&mut self) {
        self.raw.name = self.name.as_ptr();
        self.raw.value = self.value.as_ptr();
        self.raw.domain = self.domain.as_ptr();
        self.raw.path = self.path.as_ptr();
        self.raw.partition_key_top_level_site = self.partition.as_ptr();
    }
}

unsafe extern "C" fn visit_cookie(context: *mut c_void, cookie: *const FirefoxCefCookie) {
    let Some(context) = (unsafe { context.cast::<SnapshotContext>().as_mut() }) else {
        return;
    };
    let Some(cookie) = (unsafe { cookie.as_ref() }) else {
        return;
    };
    context.cookies.push(CookieStorage::from_gecko(cookie));
}

unsafe extern "C" fn finish_snapshot(context: *mut c_void, success: u8) {
    let mut context = unsafe { Box::from_raw(context.cast::<SnapshotContext>()) };
    if success != 0 && !context.visitor.is_null() {
        let total = c_int::try_from(context.cookies.len()).unwrap_or(c_int::MAX);
        for (index, cookie) in context.cookies.iter_mut().enumerate() {
            cookie.refresh_strings();
            let mut delete_cookie = 0;
            let keep_going = unsafe {
                (*context.visitor).visit.map_or(0, |visit| {
                    visit(
                        context.visitor,
                        ptr::from_ref(&cookie.raw).cast(),
                        c_int::try_from(index).unwrap_or(c_int::MAX),
                        total,
                        &mut delete_cookie,
                    )
                })
            };
            if delete_cookie != 0 {
                if let Some(gecko_cookie) =
                    unsafe { GeckoCookieStorage::from_cef(ptr::from_ref(&cookie.raw).cast()) }
                {
                    let _ = unsafe {
                        runtime::delete_cookie(
                            ptr::from_ref(&gecko_cookie.raw),
                            Some(ignore_completion),
                            ptr::null_mut(),
                        )
                    };
                }
            }
            if keep_going == 0 {
                break;
            }
        }
    }
    if !context.completion.is_null() {
        unsafe {
            if let Some(on_complete) = (*context.completion).on_complete {
                on_complete(context.completion);
            }
            release_raw(context.completion);
        }
    }
    unsafe { release_raw(context.visitor) };
}

unsafe extern "C" fn ignore_completion(_context: *mut c_void, _success: u8) {}

unsafe extern "C" fn finish_mutation(context: *mut c_void, success: u8) {
    let context = unsafe { Box::from_raw(context.cast::<MutationContext>()) };
    if !context.callback.is_null() {
        unsafe {
            if let Some(on_complete) = (*context.callback).on_complete {
                on_complete(context.callback, c_int::from(success != 0));
            }
            release_raw(context.callback);
        }
    }
}

unsafe extern "C" fn visit_all(
    _manager: *mut _cef_cookie_manager_t,
    visitor: *mut _cef_cookie_visitor_t,
) -> c_int {
    unsafe { visit_all_with_completion(_manager, visitor, ptr::null_mut()) }
}

unsafe extern "C" fn visit_all_with_completion(
    _manager: *mut _cef_cookie_manager_t,
    visitor: *mut _cef_cookie_visitor_t,
    completion: *mut _cef_completion_callback_t,
) -> c_int {
    if visitor.is_null() {
        unsafe {
            release_raw(visitor);
            release_raw(completion);
        }
        return 0;
    }
    let context = Box::new(SnapshotContext {
        visitor,
        completion,
        cookies: Vec::new(),
    });
    let context = Box::into_raw(context).cast();
    match unsafe { runtime::visit_cookies(Some(visit_cookie), Some(finish_snapshot), context) } {
        Ok(()) => 1,
        Err(error) => {
            eprintln!("firefox-cef: {error}");
            unsafe { finish_snapshot(context, 0) };
            0
        }
    }
}

unsafe extern "C" fn visit_url(
    _manager: *mut _cef_cookie_manager_t,
    _url: *const cef_string_t,
    _include_http_only: c_int,
    visitor: *mut _cef_cookie_visitor_t,
) -> c_int {
    unsafe { release_raw(visitor) };
    0
}

unsafe extern "C" fn set_cookie(
    _manager: *mut _cef_cookie_manager_t,
    _url: *const cef_string_t,
    cookie: *const _cef_cookie_t,
    callback: *mut _cef_set_cookie_callback_t,
) -> c_int {
    mutate_cookie(cookie, callback, false)
}

fn mutate_cookie(
    cookie: *const _cef_cookie_t,
    callback: *mut _cef_set_cookie_callback_t,
    delete: bool,
) -> c_int {
    let Some(storage) = (unsafe { GeckoCookieStorage::from_cef(cookie) }) else {
        unsafe { release_raw(callback) };
        return 0;
    };
    let context = Box::into_raw(Box::new(MutationContext { callback })).cast();
    let result = if delete {
        unsafe {
            runtime::delete_cookie(ptr::from_ref(&storage.raw), Some(finish_mutation), context)
        }
    } else {
        unsafe { runtime::set_cookie(ptr::from_ref(&storage.raw), Some(finish_mutation), context) }
    };
    match result {
        Ok(()) => 1,
        Err(error) => {
            eprintln!("firefox-cef: {error}");
            unsafe { finish_mutation(context, 0) };
            0
        }
    }
}

unsafe extern "C" fn delete_cookies(
    _manager: *mut _cef_cookie_manager_t,
    _url: *const cef_string_t,
    _name: *const cef_string_t,
    callback: *mut _cef_delete_cookies_callback_t,
) -> c_int {
    unsafe { release_raw(callback) };
    0
}

unsafe extern "C" fn flush_store(
    _manager: *mut _cef_cookie_manager_t,
    callback: *mut _cef_completion_callback_t,
) -> c_int {
    if !callback.is_null() {
        unsafe {
            if let Some(on_complete) = (*callback).on_complete {
                on_complete(callback);
            }
            release_raw(callback);
        }
    }
    1
}

unsafe extern "C" fn add_change_observer(
    manager: *mut _cef_cookie_manager_t,
    observer: *mut CefCookieChangeObserver,
) -> *mut _cef_registration_t {
    if manager.is_null() || observer.is_null() {
        unsafe { release_raw(observer) };
        return ptr::null_mut();
    }
    let manager =
        unsafe { RefCounted::<CefCookieManager, Arc<ManagerState>>::from_cef(manager.cast()) };
    let id = manager.state.next_observer.fetch_add(1, Ordering::Relaxed);
    manager
        .state
        .observers
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(id, observer as usize);
    let registration = unsafe { mem::zeroed::<_cef_registration_t>() };
    RefCounted::new(
        registration,
        RegistrationState {
            manager: manager.state.clone(),
            id,
        },
    )
    .cast()
}

fn create_manager() -> *mut CefCookieManager {
    let state = Arc::new(ManagerState {
        observers: Mutex::new(BTreeMap::new()),
        next_observer: AtomicU64::new(1),
    });
    let manager = CefCookieManager {
        base: _cef_cookie_manager_t {
            base: unsafe { mem::zeroed() },
            visit_all_cookies: Some(visit_all),
            visit_url_cookies: Some(visit_url),
            set_cookie: Some(set_cookie),
            delete_cookies: Some(delete_cookies),
            flush_store: Some(flush_store),
        },
        add_change_observer: Some(add_change_observer),
        visit_all_cookies_with_completion: Some(visit_all_with_completion),
    };
    RefCounted::new(manager, state).cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_cookie_manager_get_global_manager(
    callback: *mut _cef_completion_callback_t,
) -> *mut _cef_cookie_manager_t {
    let mut slot = manager_slot()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let manager = slot.get_or_insert_with(|| create_manager() as usize);
    let manager = *manager as *mut CefCookieManager;
    unsafe {
        add_ref_raw(manager);
        if !callback.is_null() {
            if let Some(on_complete) = (*callback).on_complete {
                on_complete(callback);
            }
            release_raw(callback);
        }
    }
    manager.cast()
}

pub unsafe fn notify_changed(cookie: *const FirefoxCefCookie, action: u8) {
    let Some(manager) = manager_slot()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .as_ref()
        .copied()
    else {
        return;
    };
    let manager = unsafe {
        RefCounted::<CefCookieManager, Arc<ManagerState>>::from_cef(
            (manager as *mut CefCookieManager).cast(),
        )
    };
    let observers = manager
        .state
        .observers
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .values()
        .copied()
        .collect::<Vec<_>>();
    if cookie.is_null() {
        eprintln!("firefox-cef: Gecko cleared all cookies; clients should resnapshot");
        return;
    }
    let mut storage = CookieStorage::from_gecko(unsafe { &*cookie });
    storage.refresh_strings();
    let causes: &[CookieChangeCause] = match action {
        0 => &[CookieChangeCause::Explicit],
        1 => &[CookieChangeCause::Inserted],
        2 => &[CookieChangeCause::Overwrite, CookieChangeCause::Inserted],
        4 => &[CookieChangeCause::Evicted],
        _ => &[CookieChangeCause::UnknownDeletion],
    };
    for observer in observers {
        let observer = observer as *mut CefCookieChangeObserver;
        unsafe { add_ref_raw(observer) };
        for cause in causes {
            unsafe {
                if let Some(on_cookie_changed) = (*observer).on_cookie_changed {
                    on_cookie_changed(observer, ptr::from_ref(&storage.raw).cast(), *cause);
                }
            }
        }
        unsafe { release_raw(observer) };
    }
}

pub fn shutdown() {
    if let Some(manager) = manager_slot()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        unsafe { release_raw(manager as *mut CefCookieManager) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_conversion_round_trips() {
        for value in [-1, 0, 1, 1_700_000_000_123_456] {
            assert_eq!(unix_time(windows_time(value)), value);
        }
    }

    #[test]
    fn session_cookies_get_a_future_gecko_expiry() {
        assert_eq!(
            gecko_expiry_milliseconds(0, cef_basetime_t { val: 0 }),
            SESSION_EXPIRY_MILLISECONDS
        );
        assert_eq!(gecko_expiry_milliseconds(1, windows_time(1_234_000)), 1_234);
    }

    #[test]
    fn gecko_cookie_layout_matches_cpp_contract() {
        assert_eq!(mem::size_of::<FirefoxCefCookie>(), 88);
        assert_eq!(mem::offset_of!(FirefoxCefCookie, expires_milliseconds), 48);
        assert_eq!(mem::offset_of!(FirefoxCefCookie, same_site), 80);
    }
}
