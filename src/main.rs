mod cookie_store;
mod native;
mod profile;
mod tab_controller;

use native::NativeRect;
use profile::ProfileStore;
use qtbridge::{QApp, QObjectHolder, qobject};
use shell_protocol::is_valid_profile_id;
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
    active_tab_id: String,
    controller: Option<TabController>,
    last_bounds: Option<NativeRect>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            url: std::env::var("DEB_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned()),
            profile_id: "default".to_owned(),
            status: "Waiting for Qt native host…".to_owned(),
            tabs_json: "[]".to_owned(),
            active_tab_id: String::new(),
            controller: None,
            last_bounds: None,
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
        self.last_bounds = None;
        self.status = "Waiting for Qt native host…".to_owned();
        self.tabs_json = "[]".to_owned();
        self.active_tab_id.clear();
        self.profile_id_changed();
        self.status_changed();
        self.tabs_json_changed();
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
    fn active_tab_id_changed(&mut self);

    #[qslot]
    fn navigate(&mut self) {
        let url = normalize_url(&self.url);
        self.set_url(url.clone());
        if self.controller.is_none() {
            return;
        }
        self.status = "Navigating…".to_owned();
        self.status_changed();
        let send_failed = self
            .controller
            .as_ref()
            .is_none_or(|controller| controller.send(TabCommand::Navigate(url)).is_err());
        if send_failed {
            self.status = "Native controller stopped".to_owned();
            self.status_changed();
        }
    }

    #[qslot]
    fn sync_geometry(&mut self, host_id: String, width: i32, height: i32) {
        let Ok(host_id) = host_id.parse::<u32>() else {
            return;
        };
        if host_id == 0 {
            return;
        }
        let Some(bounds) = NativeRect::new(0, 0, width, height) else {
            return;
        };
        if self.last_bounds == Some(bounds) {
            return;
        }
        self.last_bounds = Some(bounds);

        if let Some(controller) = &self.controller {
            let _ = controller.send(TabCommand::Layout(bounds));
            return;
        }

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
            self.url.clone(),
            host_id,
            bounds,
            self.get_qml_method_invoker(),
        ));
    }

    #[qslot]
    fn reload(&mut self) {
        if let Some(controller) = &self.controller {
            let _ = controller.send(TabCommand::Reload);
        }
    }

    #[qslot]
    fn new_tab(&mut self, engine: String) {
        if let (Some(controller), Some(engine)) = (&self.controller, TabEngine::parse(&engine)) {
            let _ = controller.send(TabCommand::NewTab(engine));
        }
    }

    #[qslot]
    fn select_tab(&mut self, tab_id: String) {
        if let (Some(controller), Ok(tab_id)) = (&self.controller, tab_id.parse::<u64>()) {
            let _ = controller.send(TabCommand::Select(tab_id));
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
