mod native;

use native::{ControllerCommand, Layout, NativeRect};
use qtbridge::{QApp, QObjectHolder, qobject};

const DEFAULT_URL: &str = "deb://new-tab/";

unsafe extern "C" {
    fn register_native_window_factory();
}

struct Backend {
    url: String,
    chromium_status: String,
    firefox_status: String,
    controller: Option<native::Controller>,
    last_layout: Option<Layout>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            url: std::env::var("DEB_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned()),
            chromium_status: "Waiting for Qt native host…".to_owned(),
            firefox_status: "Waiting for Qt native host…".to_owned(),
            controller: None,
            last_layout: None,
        }
    }
}

#[qobject]
impl Backend {
    qproperty!("url", Member = url, Write = set_url, Notify = url_changed);
    qproperty!(
        "chromiumStatus",
        Member = chromium_status,
        Notify = chromium_status_changed
    );
    qproperty!(
        "firefoxStatus",
        Member = firefox_status,
        Notify = firefox_status_changed
    );

    fn set_url(&mut self, url: String) {
        self.url = normalize_url(&url);
        self.url_changed();
    }

    #[qsignal]
    fn url_changed(&mut self);

    #[qsignal]
    fn chromium_status_changed(&mut self);

    #[qsignal]
    fn firefox_status_changed(&mut self);

    #[qslot]
    fn navigate(&mut self) {
        let url = normalize_url(&self.url);
        self.set_url(url.clone());
        if self.controller.is_none() {
            return;
        }
        self.chromium_status = "Navigating CEF / Chromium…".to_owned();
        self.firefox_status = "Navigating Firefox / Gecko through CEF…".to_owned();
        self.chromium_status_changed();
        self.firefox_status_changed();
        let send_failed = self
            .controller
            .as_ref()
            .is_none_or(|controller| controller.send(ControllerCommand::Navigate(url)).is_err());
        if send_failed {
            self.chromium_status = "Native controller stopped".to_owned();
            self.firefox_status = self.chromium_status.clone();
            self.chromium_status_changed();
            self.firefox_status_changed();
        }
    }

    #[qslot]
    fn sync_geometry(
        &mut self,
        chromium_host_id: String,
        chromium_width: i32,
        chromium_height: i32,
        firefox_host_id: String,
        firefox_width: i32,
        firefox_height: i32,
    ) {
        let Ok(chromium_host_id) = chromium_host_id.parse::<u32>() else {
            return;
        };
        if chromium_host_id == 0 {
            return;
        }
        let Ok(firefox_host_id) = firefox_host_id.parse::<u32>() else {
            return;
        };
        if firefox_host_id == 0 {
            return;
        }
        let Some(chromium) = NativeRect::new(0, 0, chromium_width, chromium_height) else {
            return;
        };
        let Some(firefox) = NativeRect::new(0, 0, firefox_width, firefox_height) else {
            return;
        };
        let layout = Layout { chromium, firefox };
        if self.last_layout == Some(layout) {
            return;
        }
        self.last_layout = Some(layout);

        if let Some(controller) = &self.controller {
            let _ = controller.send(ControllerCommand::Layout(layout));
            return;
        }

        self.chromium_status = "Starting CEF inside its Qt host…".to_owned();
        self.firefox_status = "Starting Firefox through the CEF ABI…".to_owned();
        self.chromium_status_changed();
        self.firefox_status_changed();
        self.controller = Some(native::spawn_controller(
            self.url.clone(),
            chromium_host_id,
            firefox_host_id,
            layout,
            self.get_qml_method_invoker(),
        ));
    }

    #[qslot]
    fn update_statuses(&mut self, chromium: String, firefox: String) {
        self.chromium_status = chromium;
        self.firefox_status = firefox;
        self.chromium_status_changed();
        self.firefox_status_changed();
    }

    #[qslot]
    fn stop(&mut self) {
        if let Some(controller) = self.controller.take() {
            controller.stop();
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
