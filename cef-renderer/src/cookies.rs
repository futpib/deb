use crate::ProtocolEmitter;
use cef::{ImplCookieManager, cookie_manager_get_global_manager, sys};
use cef_cookie::{
    CefCookie, CefCookieChangeObserver, CookieChangeCause, RefCounted, add_change_observer,
    cef_string, cookie_from_ptr, release_registration, visit_all_cookies_with_completion,
};
use shell_protocol::wire;
use std::{
    error::Error,
    mem, ptr,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct CookieBridge {
    emitter: ProtocolEmitter,
    registration: Arc<Mutex<Option<usize>>>,
}

impl CookieBridge {
    pub fn new(emitter: ProtocolEmitter) -> Self {
        Self {
            emitter,
            registration: Arc::new(Mutex::new(None)),
        }
    }

    pub fn ensure_observer(&self) -> Result<(), Box<dyn Error>> {
        let mut registration = self
            .registration
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if registration.is_some() {
            return Ok(());
        }
        let manager = cookie_manager_get_global_manager(None).ok_or("CEF has no cookie manager")?;
        let manager = ImplCookieManager::get_raw(&manager);
        let observer = RefCounted::new(
            CefCookieChangeObserver {
                base: unsafe { mem::zeroed() },
                on_cookie_changed: Some(on_cookie_changed),
            },
            self.emitter.clone(),
        );
        unsafe { retain(observer) };
        let registered = unsafe { add_change_observer(manager, observer.cast()) };
        unsafe { release(observer) };
        let registered = registered.ok_or("CEF rejected the cookie change observer")?;
        *registration = Some(registered as usize);
        Ok(())
    }

    pub fn read_all(&self, request_id: u64) -> Result<(), Box<dyn Error>> {
        self.ensure_observer()?;
        let manager = cookie_manager_get_global_manager(None).ok_or("CEF has no cookie manager")?;
        let manager = ImplCookieManager::get_raw(&manager);
        let state = Arc::new(SnapshotState {
            emitter: self.emitter.clone(),
            request_id,
            error: Mutex::new(None),
        });
        let visitor = RefCounted::new(
            sys::_cef_cookie_visitor_t {
                base: unsafe { mem::zeroed() },
                visit: Some(visit_cookie),
            },
            state.clone(),
        );
        let completion = RefCounted::new(
            sys::_cef_completion_callback_t {
                base: unsafe { mem::zeroed() },
                on_complete: Some(snapshot_complete),
            },
            state,
        );
        unsafe {
            retain(visitor);
            retain(completion);
        }
        let accepted = unsafe {
            visit_all_cookies_with_completion(manager, visitor.cast(), completion.cast())
        };
        unsafe {
            release(visitor);
            release(completion);
        }
        if !accepted {
            return Err("CEF rejected the cookie snapshot request".into());
        }
        Ok(())
    }

    pub fn set(&self, request_id: u64, cookie: wire::Cookie) -> Result<(), Box<dyn Error>> {
        self.set_internal(request_id, cookie, false)
    }

    pub fn delete(&self, request_id: u64, cookie: wire::Cookie) -> Result<(), Box<dyn Error>> {
        self.set_internal(request_id, cookie, true)
    }

    fn set_internal(
        &self,
        request_id: u64,
        cookie: wire::Cookie,
        delete: bool,
    ) -> Result<(), Box<dyn Error>> {
        self.ensure_observer()?;
        let manager = cookie_manager_get_global_manager(None).ok_or("CEF has no cookie manager")?;
        let manager = ImplCookieManager::get_raw(&manager);
        let mut cookie = OwnedCookie::new(cookie, delete)?;
        let url = CefStringStorage::new(&cookie.source_url);
        let callback = RefCounted::new(
            sys::_cef_set_cookie_callback_t {
                base: unsafe { mem::zeroed() },
                on_complete: Some(set_complete),
            },
            SetState {
                emitter: self.emitter.clone(),
                request_id,
                operation: if delete { "delete" } else { "set" },
            },
        );
        unsafe { retain(callback) };
        let manager = unsafe { manager.as_mut() }.ok_or("CEF cookie manager disappeared")?;
        let set = manager
            .set_cookie
            .ok_or("CEF cookie manager cannot set cookies")?;
        let accepted = unsafe {
            set(
                manager,
                &url.raw,
                ptr::from_mut(&mut cookie.raw).cast(),
                callback.cast(),
            ) != 0
        };
        unsafe { release(callback) };
        if !accepted {
            return Err(format!(
                "CEF rejected the cookie {} request",
                if delete { "delete" } else { "set" }
            )
            .into());
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        let registration = self
            .registration
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(registration) = registration {
            unsafe { release_registration(registration as *mut _) };
        }
    }
}

struct SnapshotState {
    emitter: ProtocolEmitter,
    request_id: u64,
    error: Mutex<Option<String>>,
}

unsafe extern "C" fn visit_cookie(
    self_: *mut sys::_cef_cookie_visitor_t,
    cookie: *const sys::_cef_cookie_t,
    _count: i32,
    _total: i32,
    _delete_cookie: *mut i32,
) -> i32 {
    let visitor =
        unsafe { RefCounted::<sys::_cef_cookie_visitor_t, Arc<SnapshotState>>::from_cef(self_) };
    match wire_cookie(cookie) {
        Ok(cookie) => visitor
            .state
            .emitter
            .event(wire::event::Value::CookieSnapshotEntry(
                wire::CookieSnapshotEntry {
                    cookie: Some(cookie),
                },
            )),
        Err(error) => {
            *visitor
                .state
                .error
                .lock()
                .unwrap_or_else(|lock_error| lock_error.into_inner()) = Some(error);
            return 0;
        }
    }
    1
}

unsafe extern "C" fn snapshot_complete(self_: *mut sys::_cef_completion_callback_t) {
    let callback = unsafe {
        RefCounted::<sys::_cef_completion_callback_t, Arc<SnapshotState>>::from_cef(self_)
    };
    let error = callback
        .state
        .error
        .lock()
        .unwrap_or_else(|lock_error| lock_error.into_inner())
        .take();
    if let Some(error) = error {
        callback
            .state
            .emitter
            .error(callback.state.request_id, "COOKIE_CONVERSION_FAILED", error);
    } else {
        callback
            .state
            .emitter
            .event(wire::event::Value::CookieSnapshotComplete(
                wire::CookieSnapshotComplete {},
            ));
        callback.state.emitter.success(callback.state.request_id);
    }
}

struct SetState {
    emitter: ProtocolEmitter,
    request_id: u64,
    operation: &'static str,
}

unsafe extern "C" fn set_complete(self_: *mut sys::_cef_set_cookie_callback_t, success: i32) {
    let callback =
        unsafe { RefCounted::<sys::_cef_set_cookie_callback_t, SetState>::from_cef(self_) };
    if success != 0 {
        callback.state.emitter.success(callback.state.request_id);
    } else {
        callback.state.emitter.error(
            callback.state.request_id,
            "COOKIE_REJECTED",
            format!("engine rejected cookie {}", callback.state.operation),
        );
    }
}

unsafe extern "C" fn on_cookie_changed(
    self_: *mut CefCookieChangeObserver,
    cookie: *const sys::_cef_cookie_t,
    cause: CookieChangeCause,
) {
    let observer =
        unsafe { RefCounted::<CefCookieChangeObserver, ProtocolEmitter>::from_cef(self_) };
    match wire_cookie(cookie) {
        Ok(cookie) => {
            observer
                .state
                .event(wire::event::Value::CookieChanged(wire::CookieChanged {
                    cookie: Some(cookie),
                    cause: cause as i32,
                }))
        }
        Err(error) => eprintln!("cef-renderer: cookie change conversion failed: {error}"),
    }
}

fn wire_cookie(cookie: *const sys::_cef_cookie_t) -> Result<wire::Cookie, String> {
    let cookie = unsafe { cookie_from_ptr(cookie) }
        .ok_or_else(|| "backend supplied a pre-extension cef_cookie_t".to_owned())?;
    let partition_key = if cookie.has_partition_key != 0 {
        let top_level_site = unsafe { cef_string(&cookie.partition_key_top_level_site) };
        Some(wire::CookiePartitionKey {
            opaque: top_level_site.is_empty(),
            top_level_site,
            has_cross_site_ancestor: cookie.partition_key_has_cross_site_ancestor != 0,
        })
    } else {
        None
    };
    let same_site = match cookie.same_site {
        sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_UNSPECIFIED => {
            wire::CookieSameSite::Unspecified
        }
        sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_NO_RESTRICTION => {
            wire::CookieSameSite::None
        }
        sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_LAX_MODE => wire::CookieSameSite::Lax,
        sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_STRICT_MODE => {
            wire::CookieSameSite::Strict
        }
        _ => return Err("backend supplied an invalid cookie SameSite value".to_owned()),
    };
    let priority = match cookie.priority {
        sys::cef_cookie_priority_t::CEF_COOKIE_PRIORITY_LOW => wire::CookiePriority::Low,
        sys::cef_cookie_priority_t::CEF_COOKIE_PRIORITY_MEDIUM => wire::CookiePriority::Medium,
        sys::cef_cookie_priority_t::CEF_COOKIE_PRIORITY_HIGH => wire::CookiePriority::High,
        _ => return Err("backend supplied an invalid cookie priority".to_owned()),
    };
    Ok(wire::Cookie {
        key: Some(wire::CookieKey {
            name: unsafe { cef_string(&cookie.name) },
            domain: unsafe { cef_string(&cookie.domain) },
            path: unsafe { cef_string(&cookie.path) },
            partition_key,
        }),
        value: unsafe { cef_string(&cookie.value) },
        secure: cookie.secure != 0,
        http_only: cookie.httponly != 0,
        creation: cookie.creation.val,
        last_access: cookie.last_access.val,
        expires: (cookie.has_expires != 0).then_some(cookie.expires.val),
        same_site: same_site as i32,
        priority: priority as i32,
        last_update: cookie.last_update.val,
    })
}

struct CefStringStorage {
    raw: sys::cef_string_t,
    _value: Vec<u16>,
}

impl CefStringStorage {
    fn new(value: &str) -> Self {
        let mut value = value.encode_utf16().collect::<Vec<_>>();
        let raw = sys::cef_string_t {
            str_: value.as_mut_ptr(),
            length: value.len(),
            dtor: None,
        };
        Self { raw, _value: value }
    }
}

struct OwnedCookie {
    raw: CefCookie,
    source_url: String,
    _name: CefStringStorage,
    _value: CefStringStorage,
    _domain: CefStringStorage,
    _path: CefStringStorage,
    _partition_site: CefStringStorage,
}

impl OwnedCookie {
    fn new(cookie: wire::Cookie, delete: bool) -> Result<Self, Box<dyn Error>> {
        let key = cookie.key.ok_or("cookie key is required")?;
        if key.domain.is_empty() || key.path.is_empty() {
            return Err("cookie domain and path are required".into());
        }
        let partition = key.partition_key.unwrap_or_default();
        if partition.opaque {
            return Err("opaque cookie partition keys cannot cross engines".into());
        }
        if !partition.top_level_site.is_empty() && !partition.top_level_site.contains("://") {
            return Err("cookie partition key must be a schemeful site".into());
        }
        let same_site = match wire::CookieSameSite::try_from(cookie.same_site)? {
            wire::CookieSameSite::Unspecified => {
                sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_UNSPECIFIED
            }
            wire::CookieSameSite::None => {
                sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_NO_RESTRICTION
            }
            wire::CookieSameSite::Lax => sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_LAX_MODE,
            wire::CookieSameSite::Strict => {
                sys::cef_cookie_same_site_t::CEF_COOKIE_SAME_SITE_STRICT_MODE
            }
        };
        let priority = match wire::CookiePriority::try_from(cookie.priority)? {
            wire::CookiePriority::Low => sys::cef_cookie_priority_t::CEF_COOKIE_PRIORITY_LOW,
            wire::CookiePriority::Medium => sys::cef_cookie_priority_t::CEF_COOKIE_PRIORITY_MEDIUM,
            wire::CookiePriority::High => sys::cef_cookie_priority_t::CEF_COOKIE_PRIORITY_HIGH,
        };
        let host = key.domain.trim_start_matches('.');
        let host = if host.contains(':') && !host.starts_with('[') {
            format!("[{host}]")
        } else {
            host.to_owned()
        };
        let scheme = if cookie.secure { "https" } else { "http" };
        let source_url = format!("{scheme}://{host}{}", key.path);
        let name = CefStringStorage::new(&key.name);
        let value = CefStringStorage::new(if delete { "" } else { &cookie.value });
        let domain = CefStringStorage::new(&key.domain);
        let path = CefStringStorage::new(&key.path);
        let partition_site = CefStringStorage::new(&partition.top_level_site);
        let expires = if delete { Some(1) } else { cookie.expires };
        let raw = CefCookie {
            size: mem::size_of::<CefCookie>(),
            name: name.raw,
            value: value.raw,
            domain: domain.raw,
            path: path.raw,
            secure: i32::from(cookie.secure),
            httponly: i32::from(cookie.http_only),
            creation: sys::cef_basetime_t {
                val: cookie.creation,
            },
            last_access: sys::cef_basetime_t {
                val: cookie.last_access,
            },
            has_expires: i32::from(expires.is_some()),
            expires: sys::cef_basetime_t {
                val: expires.unwrap_or_default(),
            },
            same_site,
            priority,
            last_update: sys::cef_basetime_t {
                val: cookie.last_update,
            },
            has_partition_key: i32::from(!partition.top_level_site.is_empty()),
            partition_key_top_level_site: partition_site.raw,
            partition_key_has_cross_site_ancestor: i32::from(partition.has_cross_site_ancestor),
        };
        Ok(Self {
            raw,
            source_url,
            _name: name,
            _value: value,
            _domain: domain,
            _path: path,
            _partition_site: partition_site,
        })
    }
}

unsafe fn retain<T>(object: *mut T) {
    let Some(base) = (unsafe { object.cast::<sys::cef_base_ref_counted_t>().as_ref() }) else {
        return;
    };
    if let Some(add_ref) = base.add_ref {
        unsafe { add_ref(ptr::from_ref(base).cast_mut()) };
    }
}

unsafe fn release<T>(object: *mut T) {
    let Some(base) = (unsafe { object.cast::<sys::cef_base_ref_counted_t>().as_ref() }) else {
        return;
    };
    if let Some(release) = base.release {
        unsafe { release(ptr::from_ref(base).cast_mut()) };
    }
}

#[cfg(test)]
mod tests {
    use super::OwnedCookie;
    use shell_protocol::wire;

    fn cookie() -> wire::Cookie {
        wire::Cookie {
            key: Some(wire::CookieKey {
                name: "session".to_owned(),
                domain: ".example.com".to_owned(),
                path: "/account".to_owned(),
                partition_key: Some(wire::CookiePartitionKey {
                    top_level_site: "https://shop.test".to_owned(),
                    has_cross_site_ancestor: true,
                    opaque: false,
                }),
            }),
            value: "value".to_owned(),
            secure: true,
            http_only: true,
            creation: 10,
            last_access: 20,
            expires: Some(30),
            same_site: wire::CookieSameSite::Lax as i32,
            priority: wire::CookiePriority::High as i32,
            last_update: 25,
        }
    }

    #[test]
    fn builds_a_partitioned_cef_cookie() {
        let cookie = OwnedCookie::new(cookie(), false).unwrap();
        assert_eq!(cookie.source_url, "https://example.com/account");
        assert_eq!(cookie.raw.has_partition_key, 1);
        assert_eq!(cookie.raw.partition_key_has_cross_site_ancestor, 1);
        assert_eq!(cookie.raw.has_expires, 1);
        assert_eq!(cookie.raw.expires.val, 30);
    }

    #[test]
    fn represents_exact_deletion_as_an_expired_cookie() {
        let cookie = OwnedCookie::new(cookie(), true).unwrap();
        assert_eq!(cookie.raw.expires.val, 1);
    }

    #[test]
    fn rejects_opaque_partition_keys() {
        let mut cookie = cookie();
        cookie
            .key
            .as_mut()
            .unwrap()
            .partition_key
            .as_mut()
            .unwrap()
            .opaque = true;
        assert!(OwnedCookie::new(cookie, false).is_err());
    }
}
