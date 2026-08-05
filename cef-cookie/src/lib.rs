use cef_dll_sys::{
    _cef_completion_callback_t, _cef_cookie_manager_t, _cef_cookie_t, _cef_cookie_visitor_t,
    _cef_registration_t, cef_base_ref_counted_t, cef_basetime_t, cef_cookie_priority_t,
    cef_cookie_same_site_t, cef_string_t,
};
use std::{
    mem,
    os::raw::c_int,
    ptr,
    sync::atomic::{AtomicUsize, Ordering, fence},
};

pub const CEF_API_VERSION_EXPERIMENTAL: c_int = 999_999;
pub const CEF_API_HASH_EXPERIMENTAL_LINUX: &[u8] = b"9c4f3ddc9baede09fb12229355d593dd60565bee\0";

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CefCookie {
    pub size: usize,
    pub name: cef_string_t,
    pub value: cef_string_t,
    pub domain: cef_string_t,
    pub path: cef_string_t,
    pub secure: c_int,
    pub httponly: c_int,
    pub creation: cef_basetime_t,
    pub last_access: cef_basetime_t,
    pub has_expires: c_int,
    pub expires: cef_basetime_t,
    pub same_site: cef_cookie_same_site_t,
    pub priority: cef_cookie_priority_t,
    pub last_update: cef_basetime_t,
    pub has_partition_key: c_int,
    pub partition_key_top_level_site: cef_string_t,
    pub partition_key_has_cross_site_ancestor: c_int,
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CookieChangeCause {
    Inserted = 0,
    Explicit = 1,
    UnknownDeletion = 2,
    Overwrite = 3,
    Expired = 4,
    Evicted = 5,
    ExpiredOverwrite = 6,
    InsertedNoChangeOverwrite = 7,
    InsertedNoValueChangeOverwrite = 8,
    NumValues = 9,
}

impl CookieChangeCause {
    pub fn is_removal(self) -> bool {
        matches!(
            self,
            Self::Explicit
                | Self::UnknownDeletion
                | Self::Overwrite
                | Self::Expired
                | Self::Evicted
                | Self::ExpiredOverwrite
        )
    }
}

#[repr(C)]
pub struct CefCookieChangeObserver {
    pub base: cef_base_ref_counted_t,
    pub on_cookie_changed: Option<
        unsafe extern "C" fn(*mut CefCookieChangeObserver, *const _cef_cookie_t, CookieChangeCause),
    >,
}

#[repr(C)]
pub struct CefCookieManager {
    pub base: _cef_cookie_manager_t,
    pub add_change_observer: Option<
        unsafe extern "C" fn(
            *mut _cef_cookie_manager_t,
            *mut CefCookieChangeObserver,
        ) -> *mut _cef_registration_t,
    >,
    pub visit_all_cookies_with_completion: Option<
        unsafe extern "C" fn(
            *mut _cef_cookie_manager_t,
            *mut _cef_cookie_visitor_t,
            *mut _cef_completion_callback_t,
        ) -> c_int,
    >,
}

#[repr(C)]
pub struct RefCounted<T, S> {
    pub cef_object: T,
    pub state: S,
    references: AtomicUsize,
}

impl<T, S> RefCounted<T, S> {
    pub fn new(mut cef_object: T, state: S) -> *mut Self {
        let base = unsafe { &mut *ptr::from_mut(&mut cef_object).cast::<cef_base_ref_counted_t>() };
        base.size = mem::size_of::<T>();
        base.add_ref = Some(add_ref::<T, S>);
        base.release = Some(release::<T, S>);
        base.has_one_ref = Some(has_one_ref::<T, S>);
        base.has_at_least_one_ref = Some(has_at_least_one_ref::<T, S>);
        Box::into_raw(Box::new(Self {
            cef_object,
            state,
            references: AtomicUsize::new(1),
        }))
    }

    pub unsafe fn from_cef<'a>(object: *mut T) -> &'a Self {
        unsafe { &*object.cast() }
    }
}

unsafe extern "C" fn add_ref<T, S>(base: *mut cef_base_ref_counted_t) {
    let object = unsafe { RefCounted::<T, S>::from_cef(base.cast()) };
    object.references.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn release<T, S>(base: *mut cef_base_ref_counted_t) -> c_int {
    let object = unsafe { RefCounted::<T, S>::from_cef(base.cast()) };
    if object.references.fetch_sub(1, Ordering::Release) != 1 {
        return 0;
    }
    fence(Ordering::Acquire);
    drop(unsafe { Box::from_raw(base.cast::<RefCounted<T, S>>()) });
    1
}

unsafe extern "C" fn has_one_ref<T, S>(base: *mut cef_base_ref_counted_t) -> c_int {
    let object = unsafe { RefCounted::<T, S>::from_cef(base.cast()) };
    c_int::from(object.references.load(Ordering::Acquire) == 1)
}

unsafe extern "C" fn has_at_least_one_ref<T, S>(base: *mut cef_base_ref_counted_t) -> c_int {
    let object = unsafe { RefCounted::<T, S>::from_cef(base.cast()) };
    c_int::from(object.references.load(Ordering::Acquire) != 0)
}

pub unsafe fn add_change_observer(
    manager: *mut _cef_cookie_manager_t,
    observer: *mut CefCookieChangeObserver,
) -> Option<*mut _cef_registration_t> {
    let manager = unsafe { manager.cast::<CefCookieManager>().as_ref()? };
    if manager.base.base.size < mem::size_of::<CefCookieManager>() {
        return None;
    }
    let register = manager.add_change_observer?;
    ptr::NonNull::new(unsafe { register(ptr::from_ref(&manager.base).cast_mut(), observer) })
        .map(ptr::NonNull::as_ptr)
}

pub unsafe fn visit_all_cookies_with_completion(
    manager: *mut _cef_cookie_manager_t,
    visitor: *mut _cef_cookie_visitor_t,
    callback: *mut _cef_completion_callback_t,
) -> bool {
    let Some(manager) = (unsafe { manager.cast::<CefCookieManager>().as_ref() }) else {
        return false;
    };
    if manager.base.base.size < mem::size_of::<CefCookieManager>() {
        return false;
    }
    let Some(visit) = manager.visit_all_cookies_with_completion else {
        return false;
    };
    unsafe { visit(ptr::from_ref(&manager.base).cast_mut(), visitor, callback) != 0 }
}

pub unsafe fn release_registration(registration: *mut _cef_registration_t) {
    let Some(registration) = (unsafe { registration.as_ref() }) else {
        return;
    };
    if let Some(release) = registration.base.release {
        unsafe { release(ptr::from_ref(&registration.base).cast_mut()) };
    }
}

pub unsafe fn cookie_from_ptr<'a>(cookie: *const _cef_cookie_t) -> Option<&'a CefCookie> {
    let stock = unsafe { cookie.as_ref()? };
    if stock.size < mem::size_of::<CefCookie>() {
        return None;
    }
    unsafe { cookie.cast::<CefCookie>().as_ref() }
}

pub unsafe fn cef_string(value: &cef_string_t) -> String {
    if value.str_.is_null() || value.length == 0 {
        return String::new();
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(value.str_, value.length) })
}

const _: () = {
    assert!(mem::size_of::<_cef_cookie_t>() == 152);
    assert!(mem::offset_of!(CefCookie, last_update) == mem::size_of::<_cef_cookie_t>());
    assert!(
        mem::offset_of!(CefCookieManager, add_change_observer)
            == mem::size_of::<_cef_cookie_manager_t>()
    );
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_cookie_preserves_the_upstream_prefix() {
        assert_eq!(
            mem::offset_of!(CefCookie, name),
            mem::offset_of!(_cef_cookie_t, name)
        );
        assert_eq!(
            mem::offset_of!(CefCookie, priority),
            mem::offset_of!(_cef_cookie_t, priority)
        );
        assert_eq!(mem::size_of::<CefCookie>(), 200);
    }

    #[test]
    fn classifies_deletion_notifications() {
        assert!(!CookieChangeCause::Inserted.is_removal());
        assert!(CookieChangeCause::Explicit.is_removal());
        assert!(CookieChangeCause::Overwrite.is_removal());
        assert!(!CookieChangeCause::InsertedNoChangeOverwrite.is_removal());
    }
}
