use cef_dll_sys::{_cef_browser_host_t, _cef_browser_t, _cef_frame_t, cef_base_ref_counted_t};
use std::{
    mem::size_of,
    sync::atomic::{AtomicUsize, Ordering, fence},
};

pub trait CefRefCounted {
    fn base_mut(&mut self) -> &mut cef_base_ref_counted_t;
}

impl CefRefCounted for _cef_browser_t {
    fn base_mut(&mut self) -> &mut cef_base_ref_counted_t {
        &mut self.base
    }
}

impl CefRefCounted for _cef_browser_host_t {
    fn base_mut(&mut self) -> &mut cef_base_ref_counted_t {
        &mut self.base
    }
}

impl CefRefCounted for _cef_frame_t {
    fn base_mut(&mut self) -> &mut cef_base_ref_counted_t {
        &mut self.base
    }
}

#[repr(C)]
pub struct RefObject<T, S> {
    pub raw: T,
    refs: AtomicUsize,
    pub state: S,
}

impl<T: CefRefCounted, S> RefObject<T, S> {
    pub fn allocate(mut raw: T, state: S) -> *mut T {
        let base = raw.base_mut();
        base.size = size_of::<T>();
        base.add_ref = Some(add_ref::<T, S>);
        base.release = Some(release::<T, S>);
        base.has_one_ref = Some(has_one_ref::<T, S>);
        base.has_at_least_one_ref = Some(has_at_least_one_ref::<T, S>);
        Box::into_raw(Box::new(Self {
            raw,
            refs: AtomicUsize::new(1),
            state,
        }))
        .cast()
    }

    pub unsafe fn get<'a>(raw: *mut T) -> &'a Self {
        unsafe { &*raw.cast::<Self>() }
    }
}

pub unsafe fn add_ref_raw<T>(raw: *mut T) {
    if raw.is_null() {
        return;
    }
    let base = raw.cast::<cef_base_ref_counted_t>();
    if let Some(add_ref) = unsafe { (*base).add_ref } {
        unsafe { add_ref(base) };
    }
}

pub unsafe fn release_raw<T>(raw: *mut T) {
    if raw.is_null() {
        return;
    }
    let base = raw.cast::<cef_base_ref_counted_t>();
    if let Some(release) = unsafe { (*base).release } {
        unsafe { release(base) };
    }
}

unsafe extern "C" fn add_ref<T, S>(base: *mut cef_base_ref_counted_t) {
    let object = unsafe { &*base.cast::<RefObject<T, S>>() };
    object.refs.fetch_add(1, Ordering::Relaxed);
}

unsafe extern "C" fn release<T, S>(base: *mut cef_base_ref_counted_t) -> i32 {
    let object = unsafe { &*base.cast::<RefObject<T, S>>() };
    if object.refs.fetch_sub(1, Ordering::Release) != 1 {
        return 0;
    }
    fence(Ordering::Acquire);
    drop(unsafe { Box::from_raw(base.cast::<RefObject<T, S>>()) });
    1
}

unsafe extern "C" fn has_one_ref<T, S>(base: *mut cef_base_ref_counted_t) -> i32 {
    let object = unsafe { &*base.cast::<RefObject<T, S>>() };
    i32::from(object.refs.load(Ordering::Acquire) == 1)
}

unsafe extern "C" fn has_at_least_one_ref<T, S>(base: *mut cef_base_ref_counted_t) -> i32 {
    let object = unsafe { &*base.cast::<RefObject<T, S>>() };
    i32::from(object.refs.load(Ordering::Acquire) >= 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn implements_cef_reference_counting() {
        let raw = RefObject::allocate(
            unsafe { std::mem::zeroed::<_cef_browser_t>() },
            String::from("state"),
        );
        let base = raw.cast::<cef_base_ref_counted_t>();
        unsafe {
            ((*base).add_ref.unwrap())(base);
            assert_eq!(((*base).has_one_ref.unwrap())(base), 0);
            assert_eq!(((*base).release.unwrap())(base), 0);
            assert_eq!(((*base).has_one_ref.unwrap())(base), 1);
            assert_eq!(((*base).release.unwrap())(base), 1);
        }
    }
}
