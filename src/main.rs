use qtbridge::{QApp, QObjectHolder, invoke_method, qobject};
use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

const DEFAULT_URL: &str = "https://www.google.com/";

struct Backend {
    url: String,
    chromium_image: String,
    firefox_image: String,
    chromium_status: String,
    firefox_status: String,
    render_generation: u64,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            url: std::env::var("DUAL_ENGINE_URL").unwrap_or_else(|_| DEFAULT_URL.to_owned()),
            chromium_image: String::new(),
            firefox_image: String::new(),
            chromium_status: "Waiting".to_owned(),
            firefox_status: "Waiting".to_owned(),
            render_generation: 0,
        }
    }
}

#[qobject]
impl Backend {
    qproperty!("url", Member = url, Write = set_url, Notify = url_changed);
    qproperty!(
        "chromiumImage",
        Member = chromium_image,
        Notify = chromium_image_changed
    );
    qproperty!(
        "firefoxImage",
        Member = firefox_image,
        Notify = firefox_image_changed
    );
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
    qproperty!(
        "renderGeneration",
        Member = render_generation,
        Notify = render_generation_changed
    );

    fn set_url(&mut self, url: String) {
        self.url = normalize_url(&url);
        self.url_changed();
    }

    #[qsignal]
    fn url_changed(&mut self);

    #[qsignal]
    fn chromium_image_changed(&mut self);

    #[qsignal]
    fn firefox_image_changed(&mut self);

    #[qsignal]
    fn chromium_status_changed(&mut self);

    #[qsignal]
    fn firefox_status_changed(&mut self);

    #[qsignal]
    fn render_generation_changed(&mut self);

    #[qslot]
    fn render(&mut self) {
        let url = normalize_url(&self.url);
        self.set_url(url.clone());
        self.chromium_status = "Rendering with CEF / Chromium…".to_owned();
        self.chromium_status_changed();
        self.firefox_status = "Rendering with Firefox / Gecko…".to_owned();
        self.firefox_status_changed();

        let output_dir = std::env::temp_dir().join("dual-engine-browser");
        if let Err(error) = std::fs::create_dir_all(&output_dir) {
            self.chromium_status = format!("Could not create output directory: {error}");
            self.firefox_status = self.chromium_status.clone();
            self.chromium_status_changed();
            self.firefox_status_changed();
            return;
        }

        let chromium_invoker = self.get_qml_method_invoker();
        let chromium_url = url.clone();
        let chromium_output = output_dir.join("chromium.png");
        std::thread::spawn(move || {
            let (image, status) = render_chromium(&chromium_url, &chromium_output);
            invoke_method!(chromium_invoker, "update_chromium", image, status);
        });

        let firefox_invoker = self.get_qml_method_invoker();
        let firefox_output = output_dir.join("firefox.png");
        std::thread::spawn(move || {
            let (image, status) = render_firefox(&url, &firefox_output);
            invoke_method!(firefox_invoker, "update_firefox", image, status);
        });
    }

    #[qslot]
    fn update_chromium(&mut self, image: String, status: String) {
        self.chromium_image = image;
        self.chromium_status = status;
        self.render_generation += 1;
        self.chromium_image_changed();
        self.chromium_status_changed();
        self.render_generation_changed();
    }

    #[qslot]
    fn update_firefox(&mut self, image: String, status: String) {
        self.firefox_image = image;
        self.firefox_status = status;
        self.render_generation += 1;
        self.firefox_image_changed();
        self.firefox_status_changed();
        self.render_generation_changed();
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

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn output_error(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if stderr.is_empty() {
        format!("process exited with {}", output.status)
    } else {
        stderr
    }
}

fn cef_renderer_path() -> Result<PathBuf, String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let directory = executable
        .parent()
        .ok_or_else(|| "application executable has no parent directory".to_owned())?;
    Ok(directory.join("cef-renderer"))
}

fn render_chromium(url: &str, destination: &Path) -> (String, String) {
    let helper = match cef_renderer_path() {
        Ok(helper) => helper,
        Err(error) => return (String::new(), error),
    };

    let output = if std::env::var_os("DISPLAY").is_some() {
        Command::new(&helper)
            .arg("--url")
            .arg(url)
            .arg("--output")
            .arg(destination)
            .output()
    } else {
        Command::new("xvfb-run")
            .arg("-a")
            .arg(&helper)
            .arg("--url")
            .arg(url)
            .arg("--output")
            .arg(destination)
            .output()
    };

    match output {
        Ok(output) if output.status.success() && destination.exists() => (
            file_url(destination),
            "Rendered by CEF 150 / Chromium 150".to_owned(),
        ),
        Ok(output) => (
            String::new(),
            format!("CEF failed: {}", output_error(&output)),
        ),
        Err(error) => (String::new(), format!("Could not start CEF: {error}")),
    }
}

fn render_firefox(url: &str, destination: &Path) -> (String, String) {
    let output = Command::new("firefox")
        .arg("--headless")
        .arg("--no-remote")
        .arg("--screenshot")
        .arg(destination)
        .arg("--window-size")
        .arg("1200,800")
        .arg(url)
        .output();

    match output {
        Ok(output) if output.status.success() && destination.exists() => (
            file_url(destination),
            "Rendered by Firefox 153 / Gecko".to_owned(),
        ),
        Ok(output) => (
            String::new(),
            format!("Firefox failed: {}", output_error(&output)),
        ),
        Err(error) => (String::new(), format!("Could not start Firefox: {error}")),
    }
}

fn main() {
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
