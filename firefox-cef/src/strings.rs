use cef_dll_sys::{
    cef_string_list_t, cef_string_t, cef_string_utf8_t, cef_string_utf16_t, char16_t,
};
use libc::{c_char, c_void};
use std::{mem::size_of, ptr, slice};

unsafe extern "C" fn free_utf8(value: *mut c_char) {
    unsafe { libc::free(value.cast::<c_void>()) };
}

unsafe extern "C" fn free_utf16(value: *mut char16_t) {
    unsafe { libc::free(value.cast::<c_void>()) };
}

#[unsafe(no_mangle)]
pub extern "C" fn cef_string_userfree_utf16_alloc() -> *mut cef_string_utf16_t {
    unsafe { libc::calloc(1, size_of::<cef_string_utf16_t>()) }.cast()
}

pub fn cef_string_userfree_from_string(value: &str) -> *mut cef_string_utf16_t {
    let output = cef_string_userfree_utf16_alloc();
    if output.is_null() {
        return ptr::null_mut();
    }
    let encoded = value.encode_utf16().collect::<Vec<_>>();
    if unsafe { cef_string_utf16_set(encoded.as_ptr(), encoded.len(), output, 1) } == 0 {
        unsafe { cef_string_userfree_utf16_free(output) };
        return ptr::null_mut();
    }
    output
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_utf16_set(
    source: *const char16_t,
    source_len: usize,
    output: *mut cef_string_utf16_t,
    copy: i32,
) -> i32 {
    let Some(output) = (unsafe { output.as_mut() }) else {
        return 0;
    };
    unsafe { cef_string_utf16_clear(output) };
    if source.is_null() {
        return i32::from(source_len == 0);
    }

    if copy == 0 {
        output.str_ = source.cast_mut();
        output.length = source_len;
        output.dtor = None;
        return 1;
    }

    let allocation =
        unsafe { libc::malloc((source_len + 1) * size_of::<char16_t>()) }.cast::<char16_t>();
    if allocation.is_null() {
        return 0;
    }
    unsafe {
        ptr::copy_nonoverlapping(source, allocation, source_len);
        allocation.add(source_len).write(0);
    }
    output.str_ = allocation;
    output.length = source_len;
    output.dtor = Some(free_utf16);
    1
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
pub unsafe extern "C" fn cef_string_userfree_utf16_free(value: *mut cef_string_utf16_t) {
    if value.is_null() {
        return;
    }
    unsafe {
        cef_string_utf16_clear(value);
        libc::free(value.cast::<c_void>());
    }
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

type StringList = Vec<Vec<char16_t>>;

unsafe fn string_list<'a>(list: cef_string_list_t) -> Option<&'a mut StringList> {
    unsafe { list.cast::<StringList>().as_mut() }
}

#[unsafe(no_mangle)]
pub extern "C" fn cef_string_list_alloc() -> cef_string_list_t {
    Box::into_raw(Box::new(StringList::new())).cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_list_size(list: cef_string_list_t) -> usize {
    unsafe { string_list(list) }.map_or(0, |list| list.len())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_list_value(
    list: cef_string_list_t,
    index: usize,
    value: *mut cef_string_t,
) -> i32 {
    let Some(entry) = (unsafe { string_list(list) }).and_then(|list| list.get(index)) else {
        return 0;
    };
    unsafe { cef_string_utf16_set(entry.as_ptr(), entry.len(), value, 1) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_list_append(
    list: cef_string_list_t,
    value: *const cef_string_t,
) {
    let Some(list) = (unsafe { string_list(list) }) else {
        return;
    };
    let Some(value) = (unsafe { value.as_ref() }) else {
        return;
    };
    if value.str_.is_null() {
        if value.length == 0 {
            list.push(Vec::new());
        }
        return;
    }
    list.push(unsafe { slice::from_raw_parts(value.str_, value.length) }.to_vec());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_list_clear(list: cef_string_list_t) {
    if let Some(list) = unsafe { string_list(list) } {
        list.clear();
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_list_free(list: cef_string_list_t) {
    if !list.is_null() {
        drop(unsafe { Box::from_raw(list.cast::<StringList>()) });
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn cef_string_list_copy(list: cef_string_list_t) -> cef_string_list_t {
    let Some(list) = (unsafe { string_list(list) }) else {
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(list.clone())).cast()
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

    #[test]
    fn copies_and_borrows_utf16_strings() {
        let source = "deb://new-tab/".encode_utf16().collect::<Vec<_>>();
        let mut copied = unsafe { std::mem::zeroed::<cef_string_utf16_t>() };
        let mut borrowed = unsafe { std::mem::zeroed::<cef_string_utf16_t>() };
        unsafe {
            assert_eq!(
                cef_string_utf16_set(source.as_ptr(), source.len(), &mut copied, 1),
                1
            );
            assert_ne!(copied.str_, source.as_ptr().cast_mut());
            assert_eq!(
                cef_string_to_string(&copied).as_deref(),
                Some("deb://new-tab/")
            );
            assert_eq!(
                cef_string_utf16_set(source.as_ptr(), source.len(), &mut borrowed, 0),
                1
            );
            assert_eq!(borrowed.str_, source.as_ptr().cast_mut());
            assert!(borrowed.dtor.is_none());
            cef_string_utf16_clear(&mut copied);
            cef_string_utf16_clear(&mut borrowed);
        }
    }

    #[test]
    fn allocates_userfree_utf16_strings() {
        let output = cef_string_userfree_from_string("deb://new-tab/#firefox");
        assert!(!output.is_null());
        unsafe {
            assert_eq!(
                cef_string_to_string(output).as_deref(),
                Some("deb://new-tab/#firefox")
            );
            cef_string_userfree_utf16_free(output);
        }
    }

    #[test]
    fn owns_and_copies_string_lists() {
        unsafe {
            let list = cef_string_list_alloc();
            let encoded = "first".encode_utf16().collect::<Vec<_>>();
            let value = cef_string_t {
                str_: encoded.as_ptr().cast_mut(),
                length: encoded.len(),
                dtor: None,
            };
            cef_string_list_append(list, &value);
            assert_eq!(cef_string_list_size(list), 1);

            let copy = cef_string_list_copy(list);
            cef_string_list_clear(list);
            assert_eq!(cef_string_list_size(list), 0);
            assert_eq!(cef_string_list_size(copy), 1);

            let mut output = std::mem::zeroed::<cef_string_t>();
            assert_eq!(cef_string_list_value(copy, 0, &mut output), 1);
            assert_eq!(cef_string_to_string(&output).as_deref(), Some("first"));
            cef_string_utf16_clear(&mut output);
            cef_string_list_free(copy);
            cef_string_list_free(list);
        }
    }
}
