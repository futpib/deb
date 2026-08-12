use crate::profile::ProfileDirectories;
use base64::Engine as _;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const CLI_CHROMIUM_EXTENSIONS: &str = "DEB_CLI_CHROMIUM_EXTENSIONS";
const CLI_FIREFOX_EXTENSIONS: &str = "DEB_CLI_FIREFOX_EXTENSIONS";
const CHROMIUM_EXTENSIONS: &str = "DEB_CHROMIUM_EXTENSIONS";
const FIREFOX_EXTENSIONS: &str = "DEB_FIREFOX_EXTENSIONS";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExtensionLaunch {
    pub application_arguments: Vec<OsString>,
    chromium: Vec<PathBuf>,
    firefox: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionEngine {
    Chromium,
    Firefox,
}

#[derive(Clone, Copy)]
enum ExtensionTarget {
    Both,
    Chromium,
    Firefox,
}

#[derive(Debug, PartialEq, Eq)]
struct ExtensionEntry {
    id: String,
    path: PathBuf,
}

impl ExtensionEngine {
    fn directory(self) -> &'static str {
        match self {
            Self::Chromium => "chromium",
            Self::Firefox => "firefox",
        }
    }

    fn cli_environment(self) -> &'static str {
        match self {
            Self::Chromium => CLI_CHROMIUM_EXTENSIONS,
            Self::Firefox => CLI_FIREFOX_EXTENSIONS,
        }
    }

    fn runtime_environment(self) -> &'static str {
        match self {
            Self::Chromium => CHROMIUM_EXTENSIONS,
            Self::Firefox => FIREFOX_EXTENSIONS,
        }
    }
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
        let chromium = validate_paths(&self.chromium, ExtensionEngine::Chromium)?;
        let firefox = validate_paths(&self.firefox, ExtensionEngine::Firefox)?;
        let chromium = serde_json::to_string(&chromium).map_err(|error| error.to_string())?;
        let firefox = serde_json::to_string(&firefox).map_err(|error| error.to_string())?;
        unsafe {
            std::env::set_var(CLI_CHROMIUM_EXTENSIONS, chromium);
            std::env::set_var(CLI_FIREFOX_EXTENSIONS, firefox);
        }
        Ok(())
    }
}

pub fn configure_child_environment(
    command: &mut Command,
    directories: &ProfileDirectories,
    engine: ExtensionEngine,
) -> Result<(), String> {
    let paths = profile_extensions(directories, engine)?;
    if paths.is_empty() {
        command.env_remove(engine.runtime_environment());
        return Ok(());
    }
    let encoded = match engine {
        ExtensionEngine::Chromium => paths
            .iter()
            .map(|path| path.to_str().expect("validated UTF-8 path"))
            .collect::<Vec<_>>()
            .join(","),
        ExtensionEngine::Firefox => {
            serde_json::to_string(&paths).map_err(|error| error.to_string())?
        }
    };
    command.env(engine.runtime_environment(), encoded);
    Ok(())
}

fn profile_extensions(
    directories: &ProfileDirectories,
    engine: ExtensionEngine,
) -> Result<Vec<PathBuf>, String> {
    let global = directories
        .app_data
        .join("extensions/all")
        .join(engine.directory());
    let profile = directories
        .shared_data
        .join("extensions")
        .join(engine.directory());
    let cli = cli_paths(engine)?;
    resolve_extensions(&global, &profile, engine, &cli)
}

fn cli_paths(engine: ExtensionEngine) -> Result<Vec<PathBuf>, String> {
    let Some(encoded) = std::env::var_os(engine.cli_environment()) else {
        return Ok(Vec::new());
    };
    serde_json::from_str(
        encoded
            .to_str()
            .ok_or("CLI extension environment is not valid UTF-8")?,
    )
    .map_err(|error| format!("invalid CLI extension environment: {error}"))
}

fn resolve_extensions(
    global: &Path,
    profile: &Path,
    engine: ExtensionEngine,
    cli: &[PathBuf],
) -> Result<Vec<PathBuf>, String> {
    let mut selected = collect_scope(global, engine)?;
    for (id, entry) in collect_scope(profile, engine)? {
        selected.insert(id, entry);
    }
    let mut cli_ids = HashSet::new();
    for path in cli {
        let fallback = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("extension path {} has no UTF-8 name", path.display()))?;
        let entry = extension_entry(path, engine, fallback)?;
        if !cli_ids.insert(entry.id.clone()) {
            return Err(format!(
                "CLI extension paths repeat native extension ID {}",
                entry.id
            ));
        }
        selected.insert(entry.id.clone(), entry);
    }
    Ok(selected.into_values().map(|entry| entry.path).collect())
}

fn collect_scope(
    directory: &Path,
    engine: ExtensionEngine,
) -> Result<BTreeMap<String, ExtensionEntry>, String> {
    if !directory.exists() {
        return Ok(BTreeMap::new());
    }
    if !directory.is_dir() {
        return Err(format!(
            "extension scope {} is not a directory",
            directory.display()
        ));
    }
    let mut children = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "cannot read extension scope {}: {error}",
                directory.display()
            )
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            format!(
                "cannot enumerate extension scope {}: {error}",
                directory.display()
            )
        })?;
    children.sort_by_key(fs::DirEntry::file_name);

    let mut entries = BTreeMap::new();
    for child in children {
        let name = child.file_name();
        let name = name.to_str().ok_or_else(|| {
            format!(
                "extension entry {} has a non-UTF-8 name",
                child.path().display()
            )
        })?;
        if name == "disabled" || name.starts_with('.') {
            continue;
        }
        let file_type = child.file_type().map_err(|error| {
            format!(
                "cannot inspect extension entry {}: {error}",
                child.path().display()
            )
        })?;
        if !file_type.is_dir() && !file_type.is_symlink() {
            continue;
        }
        let entry = extension_entry(&child.path(), engine, name)?;
        let id = entry.id.clone();
        if entries.insert(id.clone(), entry).is_some() {
            return Err(format!(
                "extension scope {} repeats native extension ID {id}",
                directory.display(),
            ));
        }
    }
    Ok(entries)
}

fn extension_entry(
    path: &Path,
    engine: ExtensionEngine,
    fallback_id: &str,
) -> Result<ExtensionEntry, String> {
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
    let manifest_path = canonical.join("manifest.json");
    if !manifest_path.is_file() {
        return Err(format!(
            "extension directory {} has no manifest.json",
            canonical.display()
        ));
    }
    let manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).map_err(|error| {
        format!(
            "cannot read extension manifest {}: {error}",
            manifest_path.display()
        )
    })?)
    .map_err(|error| {
        format!(
            "invalid extension manifest {}: {error}",
            manifest_path.display()
        )
    })?;
    validate_manifest(&manifest, &manifest_path)?;
    let id = extension_id(&manifest, engine, fallback_id, &manifest_path)?;
    let encoded = canonical.to_str().ok_or_else(|| {
        format!(
            "extension directory {} is not valid UTF-8",
            canonical.display()
        )
    })?;
    if engine == ExtensionEngine::Chromium && encoded.contains(',') {
        return Err(format!(
            "Chromium extension directory {} contains a comma",
            canonical.display()
        ));
    }
    Ok(ExtensionEntry {
        id,
        path: canonical,
    })
}

fn validate_manifest(manifest: &Value, path: &Path) -> Result<(), String> {
    let Some(manifest) = manifest.as_object() else {
        return Err(format!(
            "extension manifest {} is not an object",
            path.display()
        ));
    };
    for field in ["manifest_version", "name", "version"] {
        if !manifest.contains_key(field) {
            return Err(format!(
                "extension manifest {} has no {field}",
                path.display()
            ));
        }
    }
    if !manifest["manifest_version"].is_number()
        || !manifest["name"].is_string()
        || !manifest["version"].is_string()
    {
        return Err(format!(
            "extension manifest {} has invalid required fields",
            path.display()
        ));
    }
    Ok(())
}

fn extension_id(
    manifest: &Value,
    engine: ExtensionEngine,
    fallback: &str,
    path: &Path,
) -> Result<String, String> {
    match engine {
        ExtensionEngine::Firefox => Ok(manifest
            .pointer("/browser_specific_settings/gecko/id")
            .or_else(|| manifest.pointer("/applications/gecko/id"))
            .and_then(Value::as_str)
            .unwrap_or(fallback)
            .to_owned()),
        ExtensionEngine::Chromium => {
            let Some(key) = manifest.get("key").and_then(Value::as_str) else {
                return Ok(fallback.to_owned());
            };
            let public_key = base64::engine::general_purpose::STANDARD
                .decode(key)
                .map_err(|error| format!("invalid Chromium key in {}: {error}", path.display()))?;
            let digest = Sha256::digest(public_key);
            let mut id = String::with_capacity(32);
            for byte in &digest[..16] {
                id.push(char::from(b'a' + (byte >> 4)));
                id.push(char::from(b'a' + (byte & 0x0f)));
            }
            Ok(id)
        }
    }
}

fn validate_paths(paths: &[PathBuf], engine: ExtensionEngine) -> Result<Vec<PathBuf>, String> {
    let mut result = Vec::with_capacity(paths.len());
    let mut seen = HashSet::new();
    for path in paths {
        let fallback = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("extension path {} has no UTF-8 name", path.display()))?;
        let entry = extension_entry(path, engine, fallback)?;
        if seen.insert(entry.path.clone()) {
            result.push(entry.path);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::{ExtensionEngine, ExtensionLaunch, resolve_extensions};
    use serde_json::{Value, json};
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
    };
    use tempfile::TempDir;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn write_extension(root: &Path, name: &str, manifest: Value) -> PathBuf {
        let directory = root.join(name);
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        directory
    }

    fn manifest() -> Value {
        json!({
            "manifest_version": 3,
            "name": "test",
            "version": "1.0"
        })
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
        let root = TempDir::new().unwrap();
        let launch = ExtensionLaunch::parse(vec![
            OsString::from("deb"),
            OsString::from("--load-extension"),
            root.path().to_owned().into_os_string(),
        ])
        .unwrap();
        let error = launch
            .configure_environment()
            .expect_err("missing manifest should fail");
        assert!(error.contains("has no manifest.json"));
    }

    #[test]
    fn profile_scope_overrides_global_native_ids_and_ignores_disabled() {
        let root = TempDir::new().unwrap();
        let global = root.path().join("global");
        let profile = root.path().join("profile");
        let mut keyed = manifest();
        keyed["key"] = json!("AQID");
        let global_override = write_extension(&global, "global-name", keyed.clone());
        let profile_override = write_extension(&profile, "profile-name", keyed);
        let global_other = write_extension(&global, "other", manifest());
        let disabled = global.join("disabled/ignored");
        fs::create_dir_all(&disabled).unwrap();
        fs::write(disabled.join("manifest.json"), b"not json").unwrap();
        let hidden = global.join(".ignored");
        fs::create_dir_all(&hidden).unwrap();
        fs::write(hidden.join("manifest.json"), b"not json").unwrap();
        fs::write(global.join("README"), b"not an extension").unwrap();

        let resolved =
            resolve_extensions(&global, &profile, ExtensionEngine::Chromium, &[]).unwrap();
        assert_eq!(resolved.len(), 2);
        assert!(resolved.contains(&fs::canonicalize(profile_override).unwrap()));
        assert!(resolved.contains(&fs::canonicalize(global_other).unwrap()));
        assert!(!resolved.contains(&fs::canonicalize(global_override).unwrap()));
        assert_eq!(
            resolved,
            resolve_extensions(&global, &profile, ExtensionEngine::Chromium, &[]).unwrap()
        );
    }

    #[test]
    fn follows_extension_directory_symlinks() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new().unwrap();
        let global = root.path().join("global");
        let installed = write_extension(&root.path().join("installed"), "shared", manifest());
        fs::create_dir_all(&global).unwrap();
        symlink(&installed, global.join("linked")).unwrap();

        assert_eq!(
            resolve_extensions(
                &global,
                &root.path().join("profile"),
                ExtensionEngine::Chromium,
                &[],
            )
            .unwrap(),
            vec![fs::canonicalize(installed).unwrap()]
        );
    }

    #[test]
    fn firefox_native_id_and_cli_paths_override_xdg_entries() {
        let root = TempDir::new().unwrap();
        let global = root.path().join("global");
        let profile = root.path().join("profile");
        let cli_root = root.path().join("cli");
        let mut firefox = manifest();
        firefox["browser_specific_settings"] = json!({
            "gecko": {"id": "same@example.test"}
        });
        let global_path = write_extension(&global, "global", firefox.clone());
        let cli_path = write_extension(&cli_root, "cli", firefox);

        let resolved = resolve_extensions(
            &global,
            &profile,
            ExtensionEngine::Firefox,
            std::slice::from_ref(&cli_path),
        )
        .unwrap();
        assert_eq!(resolved, vec![fs::canonicalize(cli_path).unwrap()]);
        assert!(!resolved.contains(&fs::canonicalize(global_path).unwrap()));
    }

    #[test]
    fn another_profile_receives_only_global_extensions() {
        let root = TempDir::new().unwrap();
        let global = root.path().join("global");
        let first_profile = root.path().join("first");
        let second_profile = root.path().join("second");
        let global_path = write_extension(&global, "global", manifest());
        write_extension(&first_profile, "private", manifest());

        assert_eq!(
            resolve_extensions(&global, &second_profile, ExtensionEngine::Chromium, &[],).unwrap(),
            vec![fs::canonicalize(global_path).unwrap()]
        );
    }

    #[test]
    fn rejects_an_invalid_active_xdg_extension() {
        let root = TempDir::new().unwrap();
        let global = root.path().join("global/broken");
        fs::create_dir_all(&global).unwrap();
        fs::write(global.join("manifest.json"), b"not json").unwrap();
        let error = resolve_extensions(
            global.parent().unwrap(),
            &root.path().join("profile"),
            ExtensionEngine::Chromium,
            &[],
        )
        .unwrap_err();
        assert!(error.contains("invalid extension manifest"));
    }
}
