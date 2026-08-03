mod native;

use native::{ControllerCommand, Layout, NativeRect};
use qtbridge::{QApp, QObjectHolder, qobject};
use std::sync::mpsc::Sender;

const DEFAULT_URL: &str = "https://www.google.com/";

struct Backend {
    url: String,
    chromium_status: String,
    firefox_status: String,
    controller: Option<Sender<ControllerCommand>>,
    last_layout: Option<Layout>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            url: std::env::var("DUAL_ENGINE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned()),
            chromium_status: "Waiting for native surface…".to_owned(),
            firefox_status: "Waiting for native surface…".to_owned(),
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
        if let Some(controller) = self.controller.clone() {
            self.chromium_status = "Navigating CEF / Chromium…".to_owned();
            self.firefox_status = "Restarting Firefox / Gecko…".to_owned();
            self.chromium_status_changed();
            self.firefox_status_changed();
            if controller.send(ControllerCommand::Navigate(url)).is_err() {
                self.chromium_status = "Native controller stopped".to_owned();
                self.firefox_status = self.chromium_status.clone();
                self.chromium_status_changed();
                self.firefox_status_changed();
            }
        }
    }

    #[qslot]
    #[allow(clippy::too_many_arguments)]
    fn sync_geometry(
        &mut self,
        chromium_x: i32,
        chromium_y: i32,
        chromium_width: i32,
        chromium_height: i32,
        firefox_x: i32,
        firefox_y: i32,
        firefox_width: i32,
        firefox_height: i32,
    ) {
        let Some(chromium) =
            NativeRect::new(chromium_x, chromium_y, chromium_width, chromium_height)
        else {
            return;
        };
        let Some(firefox) = NativeRect::new(firefox_x, firefox_y, firefox_width, firefox_height)
        else {
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

        self.chromium_status = "Starting native CEF child…".to_owned();
        self.firefox_status = "Starting Firefox X11 child…".to_owned();
        self.chromium_status_changed();
        self.firefox_status_changed();
        self.controller = Some(native::spawn_controller(
            self.url.clone(),
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
            let _ = controller.send(ControllerCommand::Stop);
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
    if std::env::var_os("QT_QUICK_BACKEND").is_none() {
        unsafe {
            std::env::set_var("QT_QUICK_BACKEND", "software");
        }
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
