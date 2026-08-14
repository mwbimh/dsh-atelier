use std::{fs, path::Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

const DEFAULT_REGISTRIES: [&str; 4] = [
    "https://registry.npmjs.org/",
    "https://registry.npmmirror.com/",
    "https://mirrors.cloud.tencent.com/npm/",
    "https://repo.huaweicloud.com/repository/npm/",
];

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub dsh: DshConfig,
    pub atelier: AtelierConfig,
}

impl Config {
    pub fn from_toml(source: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(source)?)
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match fs::read_to_string(path) {
            Ok(source) => Self::from_toml(&source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DshConfig {
    pub auto_start: bool,
    pub first_launch: FirstLaunch,
    pub startup_timeout_seconds: u64,
    pub health_check_interval_seconds: u64,
    pub install: DshInstallConfig,
}

impl Default for DshConfig {
    fn default() -> Self {
        Self {
            auto_start: true,
            first_launch: FirstLaunch::Web,
            startup_timeout_seconds: 30,
            health_check_interval_seconds: 30,
            install: DshInstallConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FirstLaunch {
    None,
    #[default]
    Web,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DshInstallConfig {
    pub version: String,
    pub registries: Vec<String>,
}

impl Default for DshInstallConfig {
    fn default() -> Self {
        Self {
            version: String::new(),
            registries: DEFAULT_REGISTRIES
                .iter()
                .map(|registry| (*registry).to_owned())
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AtelierConfig {
    pub launch_at_login: bool,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read configuration: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid configuration: {0}")]
    Toml(#[from] toml::de::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_starting_dsh_and_opening_the_web_ui() {
        let config = Config::default();

        assert!(config.dsh.auto_start);
        assert_eq!(config.dsh.first_launch, FirstLaunch::Web);
        assert!(!config.atelier.launch_at_login);
    }

    #[test]
    fn accepts_none_as_the_first_launch_target() {
        let config = Config::from_toml(
            r#"
                [dsh]
                first_launch = "none"
            "#,
        )
        .expect("none is a supported first-launch target");

        assert_eq!(config.dsh.first_launch, FirstLaunch::None);
        assert!(config.dsh.auto_start);
    }

    #[test]
    fn rejects_an_unknown_first_launch_target() {
        let error = Config::from_toml(
            r#"
                [dsh]
                first_launch = "surface:custom"
            "#,
        )
        .expect_err("unsupported surfaces must not silently fall back");

        assert!(error.to_string().contains("first_launch"));
    }

    #[test]
    fn a_missing_file_uses_defaults_without_creating_it() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("missing.toml");

        assert_eq!(Config::load(&path).unwrap(), Config::default());
        assert!(!path.exists());
    }
}
