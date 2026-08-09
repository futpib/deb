use crate::{
    refcount::{RefObject, add_ref_raw, release_raw},
    runtime::BrowserState,
    strings::{cef_string_to_string, cef_string_userfree_from_string},
};
use cef_dll_sys::{
    _cef_context_menu_params_t, _cef_menu_model_t, _cef_run_context_menu_callback_t,
    cef_context_menu_type_flags_t, cef_event_flags_t, cef_menu_item_type_t, cef_string_t,
};
use libc::c_int;
use std::{
    ptr,
    sync::{Arc, Mutex, atomic::Ordering},
};

const MENU_ID_BACK: c_int = 100;
const MENU_ID_FORWARD: c_int = 101;
const MENU_ID_RELOAD: c_int = 102;
const MENU_ID_VIEW_SOURCE: c_int = 132;

#[derive(Clone)]
struct MenuEntry {
    command_id: c_int,
    label: String,
    item_type: cef_menu_item_type_t,
    enabled: bool,
}

struct MenuState {
    entries: Mutex<Vec<MenuEntry>>,
}

fn menu_state(menu: *mut _cef_menu_model_t) -> &'static MenuState {
    &unsafe { RefObject::<_cef_menu_model_t, MenuState>::get(menu) }.state
}

unsafe extern "C" fn menu_count(menu: *mut _cef_menu_model_t) -> usize {
    menu_state(menu)
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .len()
}

unsafe extern "C" fn menu_add_item(
    menu: *mut _cef_menu_model_t,
    command_id: c_int,
    label: *const cef_string_t,
) -> c_int {
    let Some(label) = (unsafe { cef_string_to_string(label) }) else {
        return 0;
    };
    menu_state(menu)
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(MenuEntry {
            command_id,
            label,
            item_type: cef_menu_item_type_t::MENUITEMTYPE_COMMAND,
            enabled: true,
        });
    1
}

unsafe extern "C" fn menu_index_of(menu: *mut _cef_menu_model_t, command_id: c_int) -> c_int {
    menu_state(menu)
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .position(|entry| entry.command_id == command_id)
        .and_then(|index| c_int::try_from(index).ok())
        .unwrap_or(-1)
}

unsafe extern "C" fn menu_command_at(menu: *mut _cef_menu_model_t, index: usize) -> c_int {
    menu_state(menu)
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(index)
        .map_or(-1, |entry| entry.command_id)
}

unsafe extern "C" fn menu_label_at(
    menu: *mut _cef_menu_model_t,
    index: usize,
) -> cef_dll_sys::cef_string_userfree_t {
    let label = menu_state(menu)
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(index)
        .map(|entry| entry.label.clone())
        .unwrap_or_default();
    cef_string_userfree_from_string(&label)
}

unsafe extern "C" fn menu_type_at(
    menu: *mut _cef_menu_model_t,
    index: usize,
) -> cef_menu_item_type_t {
    menu_state(menu)
        .entries
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(index)
        .map_or(cef_menu_item_type_t::MENUITEMTYPE_NONE, |entry| {
            entry.item_type
        })
}

unsafe extern "C" fn menu_no_submenu(
    _menu: *mut _cef_menu_model_t,
    _index: usize,
) -> *mut _cef_menu_model_t {
    ptr::null_mut()
}

unsafe extern "C" fn menu_visible_at(menu: *mut _cef_menu_model_t, index: usize) -> c_int {
    i32::from(
        menu_state(menu)
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(index)
            .is_some(),
    )
}

unsafe extern "C" fn menu_enabled_at(menu: *mut _cef_menu_model_t, index: usize) -> c_int {
    i32::from(
        menu_state(menu)
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(index)
            .is_some_and(|entry| entry.enabled),
    )
}

unsafe extern "C" fn menu_unchecked_at(_menu: *mut _cef_menu_model_t, _index: usize) -> c_int {
    0
}

fn make_menu() -> *mut _cef_menu_model_t {
    let entries = vec![
        MenuEntry {
            command_id: MENU_ID_BACK,
            label: "Back".to_owned(),
            item_type: cef_menu_item_type_t::MENUITEMTYPE_COMMAND,
            enabled: false,
        },
        MenuEntry {
            command_id: MENU_ID_FORWARD,
            label: "Forward".to_owned(),
            item_type: cef_menu_item_type_t::MENUITEMTYPE_COMMAND,
            enabled: false,
        },
        MenuEntry {
            command_id: -1,
            label: String::new(),
            item_type: cef_menu_item_type_t::MENUITEMTYPE_SEPARATOR,
            enabled: false,
        },
        MenuEntry {
            command_id: MENU_ID_RELOAD,
            label: "Reload".to_owned(),
            item_type: cef_menu_item_type_t::MENUITEMTYPE_COMMAND,
            enabled: true,
        },
        MenuEntry {
            command_id: -1,
            label: String::new(),
            item_type: cef_menu_item_type_t::MENUITEMTYPE_SEPARATOR,
            enabled: false,
        },
        MenuEntry {
            command_id: MENU_ID_VIEW_SOURCE,
            label: "View page source".to_owned(),
            item_type: cef_menu_item_type_t::MENUITEMTYPE_COMMAND,
            enabled: true,
        },
    ];
    let mut menu = unsafe { std::mem::zeroed::<_cef_menu_model_t>() };
    menu.get_count = Some(menu_count);
    menu.add_item = Some(menu_add_item);
    menu.get_index_of = Some(menu_index_of);
    menu.get_command_id_at = Some(menu_command_at);
    menu.get_label_at = Some(menu_label_at);
    menu.get_type_at = Some(menu_type_at);
    menu.get_sub_menu_at = Some(menu_no_submenu);
    menu.is_visible_at = Some(menu_visible_at);
    menu.is_enabled_at = Some(menu_enabled_at);
    menu.is_checked_at = Some(menu_unchecked_at);
    RefObject::allocate(
        menu,
        MenuState {
            entries: Mutex::new(entries),
        },
    )
}

struct ParamsState {
    x: c_int,
    y: c_int,
}

fn params_state(params: *mut _cef_context_menu_params_t) -> &'static ParamsState {
    &unsafe { RefObject::<_cef_context_menu_params_t, ParamsState>::get(params) }.state
}

unsafe extern "C" fn params_x(params: *mut _cef_context_menu_params_t) -> c_int {
    params_state(params).x
}

unsafe extern "C" fn params_y(params: *mut _cef_context_menu_params_t) -> c_int {
    params_state(params).y
}

unsafe extern "C" fn params_type(
    _params: *mut _cef_context_menu_params_t,
) -> cef_context_menu_type_flags_t {
    cef_context_menu_type_flags_t::CM_TYPEFLAG_PAGE
}

fn make_params(x: c_int, y: c_int) -> *mut _cef_context_menu_params_t {
    let mut params = unsafe { std::mem::zeroed::<_cef_context_menu_params_t>() };
    params.get_xcoord = Some(params_x);
    params.get_ycoord = Some(params_y);
    params.get_type_flags = Some(params_type);
    RefObject::allocate(params, ParamsState { x, y })
}

unsafe extern "C" fn callback_continue(
    callback: *mut _cef_run_context_menu_callback_t,
    command_id: c_int,
    _event_flags: cef_event_flags_t,
) {
    let state =
        unsafe { RefObject::<_cef_run_context_menu_callback_t, Arc<BrowserState>>::get(callback) }
            .state
            .clone();
    let result = match command_id {
        MENU_ID_RELOAD => state.reload(),
        MENU_ID_VIEW_SOURCE => state.navigate(&format!("view-source:{}", state.current_url())),
        _ => Ok(()),
    };
    if let Err(error) = result {
        eprintln!("firefox-cef: context menu command failed: {error}");
    }
}

unsafe extern "C" fn callback_cancel(_callback: *mut _cef_run_context_menu_callback_t) {}

fn make_callback(state: Arc<BrowserState>) -> *mut _cef_run_context_menu_callback_t {
    let mut callback = unsafe { std::mem::zeroed::<_cef_run_context_menu_callback_t>() };
    callback.cont = Some(callback_continue);
    callback.cancel = Some(callback_cancel);
    RefObject::allocate(callback, state)
}

pub fn show(state: Arc<BrowserState>, x: c_int, y: c_int) {
    let client = state.client.load(Ordering::Acquire);
    let browser = state.browser.load(Ordering::Acquire);
    let frame = state.frame.load(Ordering::Acquire);
    if client.is_null() || browser.is_null() || frame.is_null() {
        return;
    }
    let Some(get_handler) = (unsafe { (*client).get_context_menu_handler }) else {
        return;
    };
    let handler = unsafe { get_handler(client) };
    if handler.is_null() {
        return;
    }
    let params = make_params(x, y);
    let model = make_menu();
    let callback = make_callback(state);
    unsafe {
        if let Some(on_before) = (*handler).on_before_context_menu {
            add_ref_raw(browser);
            add_ref_raw(frame);
            add_ref_raw(params);
            add_ref_raw(model);
            on_before(handler, browser, frame, params, model);
        }
        let handled = if let Some(run) = (*handler).run_context_menu {
            add_ref_raw(browser);
            add_ref_raw(frame);
            add_ref_raw(params);
            add_ref_raw(model);
            add_ref_raw(callback);
            run(handler, browser, frame, params, model, callback) != 0
        } else {
            false
        };
        if !handled && let Some(cancel) = (*callback).cancel {
            cancel(callback);
        }
        release_raw(handler);
        release_raw(params);
        release_raw(model);
        release_raw(callback);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_a_reloadable_page_menu_through_the_cef_model() {
        let menu = make_menu();
        unsafe {
            assert_eq!(((*menu).get_count.unwrap())(menu), 6);
            assert_eq!(((*menu).get_index_of.unwrap())(menu, MENU_ID_RELOAD), 3);
            assert_eq!(((*menu).is_enabled_at.unwrap())(menu, 0), 0);
            assert_eq!(((*menu).is_enabled_at.unwrap())(menu, 3), 1);
            assert_eq!(
                ((*menu).get_type_at.unwrap())(menu, 2),
                cef_menu_item_type_t::MENUITEMTYPE_SEPARATOR
            );
            release_raw(menu);
        }
    }

    #[test]
    fn exposes_context_coordinates_through_cef_params() {
        let params = make_params(23, 47);
        unsafe {
            assert_eq!(((*params).get_xcoord.unwrap())(params), 23);
            assert_eq!(((*params).get_ycoord.unwrap())(params), 47);
            release_raw(params);
        }
    }
}
