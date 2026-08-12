use std::{collections::HashSet, ffi::OsString, fs, path::PathBuf};

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExtensionLaunch {
    pub application_arguments: Vec<OsString>,
    chromium: Vec<PathBuf>,
    firefox: Vec<PathBuf>,
}

#[derive(Clone, Copy)]
enum ExtensionTarget {
    Both,
    Chromium,
    Firefox,
}

impl ExtensionLaunch {
    pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut arguments = arguments.into_iter();
        let Some(program) = arguments.next() else {
            return Err("process argument vector is empty".to_owned());
        };
        let mut launch = Self {
            application_arguments: vec![program],
            ..Self::default()
        };
        while let Some(argument) = arguments.next() {
            let Some(argument_text) = argument.to_str() else {
                launch.application_arguments.push(argument);
                continue;
            };
            let matched = [
                ("--load-extension", ExtensionTarget::Both),
                ("--load-chromium-extension", ExtensionTarget::Chromium),
                ("--load-firefox-extension", ExtensionTarget::Firefox),
            ]
            .into_iter()
            .find(|(option, _)| {
                argument_text == *option || argument_text.starts_with(&format!("{option}="))
            });
            let Some((option, target)) = matched else {
                launch.application_arguments.push(argument);
                continue;
            };
            let path = if argument_text == option {
                arguments
                    .next()
                    .ok_or_else(|| format!("{option} requires an extension directory"))?
            } else {
                OsString::from(&argument_text[option.len() + 1..])
            };
            if path.is_empty() {
                return Err(format!("{option} requires an extension directory"));
            }
            launch.add(target, PathBuf::from(path));
        }
        Ok(launch)
    }

    fn add(&mut self, target: ExtensionTarget, path: PathBuf) {
        if matches!(target, ExtensionTarget::Both | ExtensionTarget::Chromium) {
            self.chromium.push(path.clone());
        }
        if matches!(target, ExtensionTarget::Both | ExtensionTarget::Firefox) {
            self.firefox.push(path);
        }
    }

    pub fn configure_environment(&self) -> Result<(), String> {
        let chromium = validate_paths(&self.chromium, true)?;
        let firefox = validate_paths(&self.firefox, false)?;
        if !chromium.is_empty() {
            let encoded = chromium
                .iter()
                .map(|path| path.to_str().expect("validated UTF-8 path"))
                .collect::<Vec<_>>()
                .join(",");
            unsafe {
                std::env::set_var("DEB_CHROMIUM_EXTENSIONS", encoded);
            }
        }
        if !firefox.is_empty() {
            let encoded = serde_json::to_string(&firefox).map_err(|error| error.to_string())?;
            unsafe {
                std::env::set_var("DEB_FIREFOX_EXTENSIONS", encoded);
            }
        }
        Ok(())
    }
}

fn validate_paths(paths: &[PathBuf], chromium: bool) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::with_capacity(paths.len());
    let mut seen = HashSet::new();
    for path in paths {
        let canonical = fs::canonicalize(path).map_err(|error| {
            format!(
                "extension directory {} is unavailable: {error}",
                path.display()
            )
        })?;
        if !canonical.is_dir() {
            return Err(format!(
                "extension path {} is not a directory",
                canonical.display()
            ));
        }
        if !canonical.join("manifest.json").is_file() {
            return Err(format!(
                "extension directory {} has no manifest.json",
                canonical.display()
            ));
        }
        let encoded = canonical.to_str().ok_or_else(|| {
            format!(
                "extension directory {} is not valid UTF-8",
                canonical.display()
            )
        })?;
        if chromium && encoded.contains(',') {
            return Err(format!(
                "Chromium extension directory {} contains a comma",
                canonical.display()
            ));
        }
        if seen.insert(canonical.clone()) {
            result.push(canonical);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::ExtensionLaunch;
    use std::{ffi::OsString, fs, path::PathBuf};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn extracts_shared_and_engine_specific_extensions() {
        let launch = ExtensionLaunch::parse(args(&[
            "deb",
            "--style",
            "Breeze",
            "--load-extension=shared",
            "--load-chromium-extension",
            "chromium",
            "--load-firefox-extension=firefox",
        ]))
        .unwrap();
        assert_eq!(
            launch.application_arguments,
            args(&["deb", "--style", "Breeze"])
        );
        assert_eq!(launch.chromium, ["shared", "chromium"].map(PathBuf::from));
        assert_eq!(launch.firefox, ["shared", "firefox"].map(PathBuf::from));
    }

    #[test]
    fn rejects_an_option_without_a_path() {
        let error = ExtensionLaunch::parse(args(&["deb", "--load-extension"]))
            .expect_err("missing path should fail");
        assert!(error.contains("requires an extension directory"));
    }

    #[test]
    fn validates_the_unpacked_manifest() {
        let root = std::env::temp_dir().join(format!(
            "deb-extension-test-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("unnamed")
        ));
        fs::create_dir_all(&root).unwrap();
        let launch = ExtensionLaunch::parse(vec![
            OsString::from("deb"),
            OsString::from("--load-extension"),
            root.clone().into_os_string(),
        ])
        .unwrap();
        let error = launch
            .configure_environment()
            .expect_err("missing manifest should fail");
        assert!(error.contains("has no manifest.json"));
        fs::remove_dir(&root).unwrap();
    }
}
