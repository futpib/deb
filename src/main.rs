mod cookie_store;
mod native;
mod profile;
mod tab_controller;

use native::NativeRect;
use profile::ProfileStore;
use qtbridge::{QApp, QObjectHolder, qobject};
use shell_protocol::is_valid_profile_id;
use std::collections::{HashMap, HashSet};
use tab_controller::{TabCommand, TabController, TabEngine};

const DEFAULT_URL: &str = "deb://new-tab/";

unsafe extern "C" {
    fn register_native_window_factory();
}

struct Backend {
    url: String,
    profile_id: String,
    status: String,
    tabs_json: String,
    window_state_json: String,
    smoke_test: bool,
    active_tab_id: String,
    controller: Option<TabController>,
    window_bounds: HashMap<u64, NativeRect>,
    window_states: HashMap<u64, (bool, bool)>,
    registered_windows: HashSet<u64>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            url: std::env::var("DEB_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned()),
            profile_id: "default".to_owned(),
            status: "Waiting for Qt native host…".to_owned(),
            tabs_json: "[]".to_owned(),
            window_state_json: "{\"windows\":[]}".to_owned(),
            smoke_test: std::env::var("DEB_AUTOMATED_SMOKE_TEST").as_deref() == Ok("1"),
            active_tab_id: String::new(),
            controller: None,
            window_bounds: HashMap::new(),
            window_states: HashMap::new(),
            registered_windows: HashSet::new(),
        }
    }
}

#[qobject]
impl Backend {
    qproperty!("url", Member = url, Write = set_url, Notify = url_changed);
    qproperty!(
        "profileId",
        Member = profile_id,
        Write = set_profile_id,
        Notify = profile_id_changed
    );
    qproperty!("status", Member = status, Notify = status_changed);
    qproperty!("tabsJson", Member = tabs_json, Notify = tabs_json_changed);
    qproperty!(
        "windowStateJson",
        Member = window_state_json,
        Notify = window_state_json_changed
    );
    qproperty!("smokeTest", Member = smoke_test);
    qproperty!(
        "activeTabId",
        Member = active_tab_id,
        Notify = active_tab_id_changed
    );

    fn set_url(&mut self, url: String) {
        self.url = normalize_url(&url);
        self.url_changed();
    }

    fn set_profile_id(&mut self, profile_id: String) {
        if self.profile_id == profile_id {
            return;
        }
        if !is_valid_profile_id(&profile_id) {
            self.status = format!("Invalid profile ID {profile_id:?}");
            self.status_changed();
            return;
        }
        if let Some(controller) = self.controller.take() {
            controller.stop();
        }
        self.profile_id = profile_id;
        self.window_bounds.clear();
        self.window_states.clear();
        self.registered_windows.clear();
        self.status = "Waiting for Qt native host…".to_owned();
        self.tabs_json = "[]".to_owned();
        self.window_state_json = "{\"windows\":[]}".to_owned();
        self.active_tab_id.clear();
        self.profile_id_changed();
        self.status_changed();
        self.tabs_json_changed();
        self.window_state_json_changed();
        self.active_tab_id_changed();
    }

    #[qsignal]
    fn url_changed(&mut self);

    #[qsignal]
    fn profile_id_changed(&mut self);

    #[qsignal]
    fn status_changed(&mut self);

    #[qsignal]
    fn tabs_json_changed(&mut self);

    #[qsignal]
    fn window_state_json_changed(&mut self);

    #[qsignal]
    fn active_tab_id_changed(&mut self);

    #[qslot]
    fn navigate(&mut self, window_id: String, input: String) {
        let Ok(window_id) = window_id.parse::<u64>() else {
            return;
        };
        let url = normalize_url(&input);
        if self.controller.is_none() {
            return;
        }
        self.status = "Navigating…".to_owned();
        self.status_changed();
        let send_failed = self.controller.as_ref().is_none_or(|controller| {
            controller
                .send(TabCommand::Navigate(window_id, url))
                .is_err()
        });
        if send_failed {
            self.status = "Native controller stopped".to_owned();
            self.status_changed();
        }
    }

    #[qslot]
    #[allow(clippy::too_many_arguments)]
    fn sync_geometry(
        &mut self,
        window_id: String,
        host_id: String,
        width: i32,
        height: i32,
        label: String,
        visible: bool,
        focused: bool,
    ) {
        let Ok(window_id) = window_id.parse::<u64>() else {
            return;
        };
        let Ok(host_id) = host_id.parse::<u32>() else {
            return;
        };
        if host_id == 0 {
            return;
        }
        let Some(bounds) = NativeRect::new(0, 0, width, height) else {
            return;
        };
        if self.controller.is_none() {
            self.status = "Starting Chromium through CEF…".to_owned();
            self.status_changed();
            let directories = match profile::profile_directories(&self.profile_id) {
                Ok(directories) => directories,
                Err(error) => {
                    self.status = format!("Profile storage failed: {error}");
                    self.status_changed();
                    return;
                }
            };
            self.controller = Some(tab_controller::spawn(
                self.profile_id.clone(),
                directories,
                self.get_qml_method_invoker(),
            ));
        }
        if let Some(controller) = &self.controller {
            if self.registered_windows.insert(window_id) {
                let _ = controller.send(TabCommand::AddWindow {
                    id: window_id,
                    parent: host_id,
                    bounds,
                    label,
                    initial_url: self.url.clone(),
                });
            } else if self.window_bounds.get(&window_id) != Some(&bounds) {
                let _ = controller.send(TabCommand::Layout(window_id, bounds));
            }
            if self.window_states.get(&window_id) != Some(&(visible, focused)) {
                let _ = controller.send(TabCommand::SetWindowState {
                    id: window_id,
                    visible,
                    focused,
                });
            }
        }
        self.window_bounds.insert(window_id, bounds);
        self.window_states.insert(window_id, (visible, focused));
    }

    #[qslot]
    fn unregister_window(&mut self, window_id: String) {
        let Ok(window_id) = window_id.parse::<u64>() else {
            return;
        };
        self.registered_windows.remove(&window_id);
        self.window_bounds.remove(&window_id);
        self.window_states.remove(&window_id);
        if let Some(controller) = &self.controller {
            let _ = controller.send(TabCommand::RemoveWindow(window_id));
        }
    }

    #[qslot]
    fn reload(&mut self, window_id: String) {
        if let (Some(controller), Ok(window_id)) = (&self.controller, window_id.parse::<u64>()) {
            let _ = controller.send(TabCommand::Reload(window_id));
        }
    }

    #[qslot]
    fn new_tab(&mut self, window_id: String, engine: String) {
        if let (Some(controller), Ok(window_id), Some(engine)) = (
            &self.controller,
            window_id.parse::<u64>(),
            TabEngine::parse(&engine),
        ) {
            let _ = controller.send(TabCommand::NewTab(window_id, engine));
        }
    }

    #[qslot]
    fn select_tab(&mut self, window_id: String, tab_id: String) {
        if let (Some(controller), Ok(window_id), Ok(tab_id)) = (
            &self.controller,
            window_id.parse::<u64>(),
            tab_id.parse::<u64>(),
        ) {
            let _ = controller.send(TabCommand::Select(window_id, tab_id));
        }
    }

    #[qslot]
    fn close_tab(&mut self, tab_id: String) {
        if let (Some(controller), Ok(tab_id)) = (&self.controller, tab_id.parse::<u64>()) {
            let _ = controller.send(TabCommand::Close(tab_id));
        }
    }

    #[qslot]
    fn switch_engine(&mut self, tab_id: String, engine: String) {
        if let (Some(controller), Ok(tab_id), Some(engine)) = (
            &self.controller,
            tab_id.parse::<u64>(),
            TabEngine::parse(&engine),
        ) {
            let _ = controller.send(TabCommand::SwitchEngine(tab_id, engine));
        }
    }

    #[qslot]
    fn move_tab(&mut self, tab_id: String, target_window_id: String) {
        if let (Some(controller), Ok(tab_id), Ok(target_window_id)) = (
            &self.controller,
            tab_id.parse::<u64>(),
            target_window_id.parse::<u64>(),
        ) {
            let _ = controller.send(TabCommand::Move(tab_id, target_window_id));
        }
    }

    #[qslot]
    fn pointer_move(&mut self, window_id: String, x: i32, y: i32, modifiers: i32, leaving: bool) {
        if let (Some(controller), Ok(window_id), Ok(modifiers)) = (
            &self.controller,
            window_id.parse::<u64>(),
            u32::try_from(modifiers),
        ) {
            let _ = controller.send(TabCommand::MouseMove {
                window_id,
                x,
                y,
                modifiers,
                leaving,
            });
        }
    }

    #[qslot]
    #[allow(clippy::too_many_arguments)]
    fn pointer_button(
        &mut self,
        window_id: String,
        x: i32,
        y: i32,
        modifiers: i32,
        button: i32,
        mouse_up: bool,
        click_count: i32,
    ) {
        let button = match button {
            0 => shell_protocol::wire::MouseButton::Left,
            1 => shell_protocol::wire::MouseButton::Middle,
            2 => shell_protocol::wire::MouseButton::Right,
            _ => return,
        };
        if let (Some(controller), Ok(window_id), Ok(modifiers), Ok(click_count)) = (
            &self.controller,
            window_id.parse::<u64>(),
            u32::try_from(modifiers),
            u32::try_from(click_count),
        ) {
            let _ = controller.send(TabCommand::MouseClick {
                window_id,
                x,
                y,
                modifiers,
                button,
                mouse_up,
                click_count,
            });
        }
    }

    #[qslot]
    #[allow(clippy::too_many_arguments)]
    fn pointer_wheel(
        &mut self,
        window_id: String,
        x: i32,
        y: i32,
        modifiers: i32,
        delta_x: i32,
        delta_y: i32,
    ) {
        if let (Some(controller), Ok(window_id), Ok(modifiers)) = (
            &self.controller,
            window_id.parse::<u64>(),
            u32::try_from(modifiers),
        ) {
            let _ = controller.send(TabCommand::MouseWheel {
                window_id,
                x,
                y,
                modifiers,
                delta_x,
                delta_y,
            });
        }
    }

    #[qslot]
    #[allow(clippy::too_many_arguments)]
    fn key_event(
        &mut self,
        window_id: String,
        event_type: i32,
        modifiers: i32,
        windows_key_code: i32,
        native_key_code: i32,
        is_system_key: bool,
        character: i32,
        unmodified_character: i32,
    ) {
        if shell_protocol::wire::KeyEventType::try_from(event_type).is_err() {
            return;
        }
        if let (
            Some(controller),
            Ok(window_id),
            Ok(modifiers),
            Ok(character),
            Ok(unmodified_character),
        ) = (
            &self.controller,
            window_id.parse::<u64>(),
            u32::try_from(modifiers),
            u32::try_from(character),
            u32::try_from(unmodified_character),
        ) {
            let _ = controller.send(TabCommand::KeyEvent {
                window_id,
                event: shell_protocol::wire::KeyEvent {
                    event_type,
                    modifiers,
                    windows_key_code,
                    native_key_code,
                    is_system_key,
                    character,
                    unmodified_character,
                },
            });
        }
    }

    #[qslot]
    fn update_window_state(&mut self, state_json: String) {
        self.window_state_json = state_json;
        self.window_state_json_changed();
        let Ok(state) = serde_json::from_str::<serde_json::Value>(&self.window_state_json) else {
            return;
        };
        let Some(window) = state
            .get("windows")
            .and_then(serde_json::Value::as_array)
            .and_then(|windows| windows.iter().find(|window| window["id"] == "1"))
        else {
            return;
        };
        self.tabs_json = serde_json::to_string(&window["tabs"]).unwrap_or_else(|_| "[]".to_owned());
        self.active_tab_id = window["activeTabId"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        if let Some(active) = window["tabs"]
            .as_array()
            .and_then(|tabs| tabs.iter().find(|tab| tab["id"] == self.active_tab_id))
        {
            self.url = active["url"].as_str().unwrap_or_default().to_owned();
            self.status = active["status"].as_str().unwrap_or_default().to_owned();
        }
        self.tabs_json_changed();
        self.active_tab_id_changed();
        self.url_changed();
        self.status_changed();
    }

    #[qslot]
    fn update_tab_state(
        &mut self,
        tabs_json: String,
        active_tab_id: String,
        url: String,
        status: String,
    ) {
        self.tabs_json = tabs_json;
        self.active_tab_id = active_tab_id;
        self.url = url;
        self.status = status;
        self.tabs_json_changed();
        self.active_tab_id_changed();
        self.url_changed();
        self.status_changed();
    }

    #[qslot]
    fn finish_smoke_test(&mut self, outcome: String, details: String) {
        if let Some(controller) = self.controller.take() {
            controller.stop();
        }
        eprintln!("deb-smoke: {outcome}: {details}");
        std::process::exit(i32::from(outcome != "PASS"));
    }

    #[qslot]
    fn stop(&mut self) {
        if let Some(controller) = self.controller.take() {
            controller.stop();
        }
    }
}

struct ProfileManager {
    profiles_json: String,
    last_created_profile_json: String,
    error: String,
    store: Option<ProfileStore>,
}

impl Default for ProfileManager {
    fn default() -> Self {
        match ProfileStore::load() {
            Ok(store) => {
                let profiles_json =
                    serde_json::to_string(store.profiles()).unwrap_or_else(|_| "[]".to_owned());
                Self {
                    profiles_json,
                    last_created_profile_json: String::new(),
                    error: String::new(),
                    store: Some(store),
                }
            }
            Err(error) => Self {
                profiles_json: r#"[{"id":"default","name":"Default"}]"#.to_owned(),
                last_created_profile_json: String::new(),
                error: format!("Profile registry failed: {error}"),
                store: None,
            },
        }
    }
}

#[qobject]
impl ProfileManager {
    qproperty!(
        "profilesJson",
        Member = profiles_json,
        Notify = profiles_json_changed
    );
    qproperty!(
        "lastCreatedProfileJson",
        Member = last_created_profile_json,
        Notify = last_created_profile_json_changed
    );
    qproperty!("error", Member = error, Notify = error_changed);

    #[qsignal]
    fn profiles_json_changed(&mut self);

    #[qsignal]
    fn last_created_profile_json_changed(&mut self);

    #[qsignal]
    fn error_changed(&mut self);

    #[qslot]
    fn create_profile(&mut self, name: String) {
        let Some(store) = &mut self.store else {
            self.error = "Profile registry is unavailable".to_owned();
            self.error_changed();
            return;
        };
        match store.create(&name) {
            Ok(profile) => {
                self.profiles_json =
                    serde_json::to_string(store.profiles()).unwrap_or_else(|_| "[]".to_owned());
                self.last_created_profile_json =
                    serde_json::to_string(&profile).unwrap_or_default();
                self.error.clear();
                self.profiles_json_changed();
                self.last_created_profile_json_changed();
                self.error_changed();
            }
            Err(error) => {
                self.error = error.to_string();
                self.error_changed();
            }
        }
    }
}

fn normalize_url(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.contains("://") || trimmed.starts_with("data:") {
        trimmed.to_owned()
    } else {
        format!("https://{trimmed}")
    }
}

fn main() {
    if std::env::var_os("DISPLAY").is_some() && std::env::var_os("QT_QPA_PLATFORM").is_none() {
        unsafe {
            std::env::set_var("QT_QPA_PLATFORM", "xcb");
        }
    }
    if std::env::var_os("QSG_RHI_BACKEND").is_none() {
        unsafe {
            std::env::set_var("QSG_RHI_BACKEND", "opengl");
        }
    }
    if std::env::var_os("QT_XCB_GL_INTEGRATION").is_none() {
        unsafe {
            std::env::set_var("QT_XCB_GL_INTEGRATION", "xcb_glx");
        }
    }
    unsafe {
        register_native_window_factory();
    }

    QApp::new()
        .register::<ProfileManager>()
        .register::<Backend>()
        .load_qml(include_bytes!("Main.qml"))
        .run();
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn supplies_https_when_scheme_is_missing() {
        assert_eq!(normalize_url("google.com"), "https://google.com");
    }

    #[test]
    fn preserves_explicit_scheme() {
        assert_eq!(normalize_url("https://example.com"), "https://example.com");
    }
}
