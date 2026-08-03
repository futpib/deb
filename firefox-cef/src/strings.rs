use cef_dll_sys::{cef_string_utf8_t, cef_string_utf16_t, char16_t};
use libc::{c_char, c_void};
use std::{mem::size_of, ptr, slice};

unsafe extern "C" fn free_utf8(value: *mut c_char) {
    unsafe { libc::free(value.cast::<c_void>()) };
}

unsafe extern "C" fn free_utf16(value: *mut char16_t) {
    unsafe { libc::free(value.cast::<c_void>()) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_utf8_clear(value: *mut cef_string_utf8_t) {
    let Some(value) = (unsafe { value.as_mut() }) else {
        return;
    };
    if let Some(dtor) = value.dtor
        && !value.str_.is_null()
    {
        unsafe { dtor(value.str_) };
    }
    value.str_ = ptr::null_mut();
    value.length = 0;
    value.dtor = None;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_utf16_clear(value: *mut cef_string_utf16_t) {
    let Some(value) = (unsafe { value.as_mut() }) else {
        return;
    };
    if let Some(dtor) = value.dtor
        && !value.str_.is_null()
    {
        unsafe { dtor(value.str_) };
    }
    value.str_ = ptr::null_mut();
    value.length = 0;
    value.dtor = None;
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_utf8_to_utf16(
    source: *const c_char,
    source_len: usize,
    output: *mut cef_string_utf16_t,
) -> i32 {
    if source.is_null() || output.is_null() {
        return 0;
    }
    let bytes = unsafe { slice::from_raw_parts(source.cast::<u8>(), source_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return 0;
    };
    let encoded = text.encode_utf16().collect::<Vec<_>>();
    unsafe { cef_string_utf16_clear(output) };
    let allocation =
        unsafe { libc::malloc((encoded.len() + 1) * size_of::<char16_t>()) }.cast::<char16_t>();
    if allocation.is_null() {
        return 0;
    }
    unsafe {
        ptr::copy_nonoverlapping(encoded.as_ptr(), allocation, encoded.len());
        allocation.add(encoded.len()).write(0);
        (*output).str_ = allocation;
        (*output).length = encoded.len();
        (*output).dtor = Some(free_utf16);
    }
    1
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_utf16_to_utf8(
    source: *const char16_t,
    source_len: usize,
    output: *mut cef_string_utf8_t,
) -> i32 {
    if source.is_null() || output.is_null() {
        return 0;
    }
    let units = unsafe { slice::from_raw_parts(source, source_len) };
    let Ok(text) = String::from_utf16(units) else {
        return 0;
    };
    let encoded = text.as_bytes();
    unsafe { cef_string_utf8_clear(output) };
    let allocation = unsafe { libc::malloc(encoded.len() + 1) }.cast::<c_char>();
    if allocation.is_null() {
        return 0;
    }
    unsafe {
        ptr::copy_nonoverlapping(encoded.as_ptr(), allocation.cast::<u8>(), encoded.len());
        allocation.add(encoded.len()).write(0);
        (*output).str_ = allocation;
        (*output).length = encoded.len();
        (*output).dtor = Some(free_utf8);
    }
    1
}

pub unsafe fn cef_string_to_string(value: *const cef_string_utf16_t) -> Option<String> {
    let value = unsafe { value.as_ref() }?;
    if value.str_.is_null() {
        return Some(String::new());
    }
    String::from_utf16(unsafe { slice::from_raw_parts(value.str_, value.length) }).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_utf8_and_utf16() {
        let source = "Firefox via CEF 🦊";
        let mut utf16 = unsafe { std::mem::zeroed::<cef_string_utf16_t>() };
        let mut utf8 = unsafe { std::mem::zeroed::<cef_string_utf8_t>() };
        unsafe {
            assert_eq!(
                cef_string_utf8_to_utf16(source.as_ptr().cast(), source.len(), &mut utf16,),
                1
            );
            assert_eq!(cef_string_to_string(&utf16).as_deref(), Some(source));
            assert_eq!(
                cef_string_utf16_to_utf8(utf16.str_, utf16.length, &mut utf8),
                1
            );
            let result = slice::from_raw_parts(utf8.str_.cast::<u8>(), utf8.length);
            assert_eq!(result, source.as_bytes());
            cef_string_utf8_clear(&mut utf8);
            cef_string_utf16_clear(&mut utf16);
        }
    }
}
