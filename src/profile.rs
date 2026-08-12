use serde::{Deserialize, Serialize};
use shell_protocol::is_valid_profile_id;
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};
use xdg::BaseDirectories;

type ProfileResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const APP_DIRECTORY: &str = "deb";
const DEFAULT_PROFILE_ID: &str = "default";
const DEFAULT_PROFILE_NAME: &str = "Default";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ProfileSummary {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineDirectories {
    pub data: PathBuf,
    pub cache: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProfileDirectories {
    pub app_data: PathBuf,
    pub shared_data: PathBuf,
    pub chromium: EngineDirectories,
    pub firefox: EngineDirectories,
}

#[derive(Debug, Deserialize, Serialize)]
struct ProfileRegistry {
    profiles: Vec<ProfileSummary>,
}

pub struct ProfileStore {
    registry_path: PathBuf,
    profiles: Vec<ProfileSummary>,
}

impl ProfileStore {
    pub fn load() -> ProfileResult<Self> {
        let registry_path =
            BaseDirectories::with_prefix(APP_DIRECTORY).place_config_file("profiles.json")?;
        let profiles = if registry_path.is_file() {
            let registry: ProfileRegistry =
                serde_json::from_str(&fs::read_to_string(&registry_path)?)?;
            validate_profiles(registry.profiles)?
        } else {
            vec![default_profile()]
        };
        let mut store = Self {
            registry_path,
            profiles,
        };
        store.ensure_directories()?;
        if !store.registry_path.is_file() {
            store.save()?;
        }
        Ok(store)
    }

    pub fn profiles(&self) -> &[ProfileSummary] {
        &self.profiles
    }

    pub fn create(&mut self, requested_name: &str) -> ProfileResult<ProfileSummary> {
        let name = requested_name.trim();
        if name.is_empty() {
            return Err("profile name must not be empty".into());
        }
        if name.chars().count() > 80 {
            return Err("profile name must be at most 80 characters".into());
        }
        let base = profile_slug(name);
        let mut id = base.clone();
        let mut suffix = 2;
        while self.profiles.iter().any(|profile| profile.id == id) {
            id = format!("{base}-{suffix}");
            suffix += 1;
        }
        let profile = ProfileSummary {
            id,
            name: name.to_owned(),
        };
        profile_directories(&profile.id)?;
        self.profiles.push(profile.clone());
        if let Err(error) = self.save() {
            self.profiles.pop();
            return Err(error);
        }
        Ok(profile)
    }

    fn ensure_directories(&mut self) -> ProfileResult<()> {
        fs::create_dir_all(
            self.registry_path
                .parent()
                .ok_or("profile registry has no parent directory")?,
        )?;
        for profile in &self.profiles {
            profile_directories(&profile.id)?;
        }
        Ok(())
    }

    fn save(&self) -> ProfileResult<()> {
        let registry = ProfileRegistry {
            profiles: self.profiles.clone(),
        };
        let encoded = serde_json::to_vec_pretty(&registry)?;
        let temporary = self
            .registry_path
            .with_extension(format!("json.tmp-{}", std::process::id()));
        fs::write(&temporary, encoded)?;
        fs::rename(&temporary, &self.registry_path)?;
        Ok(())
    }
}

pub fn profile_directories(profile_id: &str) -> ProfileResult<ProfileDirectories> {
    if !is_valid_profile_id(profile_id) {
        return Err(format!("invalid profile ID {profile_id:?}").into());
    }
    let directories =
        BaseDirectories::with_profile(APP_DIRECTORY, Path::new("profiles").join(profile_id));
    let app_data = BaseDirectories::with_prefix(APP_DIRECTORY)
        .get_data_home()
        .ok_or("XDG application data home is unavailable")?;
    let profile_data = directories
        .get_data_home()
        .ok_or("XDG data home is unavailable")?;
    let profile_cache = directories
        .get_cache_home()
        .ok_or("XDG cache home is unavailable")?;
    let directories = ProfileDirectories {
        app_data,
        shared_data: profile_data.clone(),
        chromium: EngineDirectories {
            data: profile_data.join("chromium"),
            cache: profile_cache.join("chromium"),
        },
        firefox: EngineDirectories {
            data: profile_data.join("firefox"),
            cache: profile_cache.join("firefox"),
        },
    };
    for directory in [
        &directories.shared_data,
        &directories.chromium.data,
        &directories.chromium.cache,
        &directories.firefox.data,
        &directories.firefox.cache,
        &directories.app_data.join("extensions/all/chromium"),
        &directories.app_data.join("extensions/all/firefox"),
        &directories.shared_data.join("extensions/chromium"),
        &directories.shared_data.join("extensions/firefox"),
    ] {
        fs::create_dir_all(directory)?;
    }
    Ok(directories)
}

fn validate_profiles(profiles: Vec<ProfileSummary>) -> ProfileResult<Vec<ProfileSummary>> {
    if profiles.is_empty() {
        return Ok(vec![default_profile()]);
    }
    for (index, profile) in profiles.iter().enumerate() {
        if !is_valid_profile_id(&profile.id) {
            return Err(format!("profile registry has invalid ID {:?}", profile.id).into());
        }
        if profile.name.trim().is_empty() {
            return Err(format!("profile {:?} has an empty name", profile.id).into());
        }
        if profiles[..index]
            .iter()
            .any(|existing| existing.id == profile.id)
        {
            return Err(format!("profile registry repeats ID {:?}", profile.id).into());
        }
    }
    Ok(profiles)
}

fn default_profile() -> ProfileSummary {
    ProfileSummary {
        id: DEFAULT_PROFILE_ID.to_owned(),
        name: DEFAULT_PROFILE_NAME.to_owned(),
    }
}

fn profile_slug(name: &str) -> String {
    let mut slug = String::new();
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
            separator = false;
        } else {
            separator = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    if slug.is_empty() {
        "profile".to_owned()
    } else {
        slug
    }
}

#[cfg(test)]
mod tests {
    use super::{ProfileSummary, profile_slug, validate_profiles};

    #[test]
    fn creates_safe_stable_slugs() {
        assert_eq!(profile_slug("Work Account"), "work-account");
        assert_eq!(profile_slug("  Personal / Main  "), "personal-main");
        assert_eq!(profile_slug("仕事"), "profile");
    }

    #[test]
    fn rejects_duplicate_profile_ids() {
        let profiles = vec![
            ProfileSummary {
                id: "work".to_owned(),
                name: "Work".to_owned(),
            },
            ProfileSummary {
                id: "work".to_owned(),
                name: "Also work".to_owned(),
            },
        ];
        assert!(validate_profiles(profiles).is_err());
    }
}
