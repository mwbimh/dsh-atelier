use std::{
    fs,
    path::{Path, PathBuf},
};

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
            first_launch: FirstLaunch::SurfaceDsh,
            startup_timeout_seconds: 30,
            health_check_interval_seconds: 30,
            install: DshInstallConfig::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum FirstLaunch {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "surface:dsh")]
    #[default]
    SurfaceDsh,
    #[serde(rename = "web")]
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
    pub theme: ThemePreference,
    pub surface: SurfaceConfig,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct SurfaceConfig {
    pub title: String,
    pub title_icon: Option<PathBuf>,
    pub loading_icon: Option<PathBuf>,
    pub loading_title: String,
    pub loading_starting_text: String,
    pub loading_started_text: String,
}

impl Default for SurfaceConfig {
    fn default() -> Self {
        Self {
            title: "DeepSeek Harness".to_owned(),
            title_icon: None,
            loading_icon: None,
            loading_title: "DeepSeek Harness".to_owned(),
            loading_starting_text: "正在启动…".to_owned(),
            loading_started_text: "已启动".to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemePreference {
    Light,
    #[default]
    Dark,
    System,
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
    use std::path::PathBuf;

    #[test]
    fn defaults_to_starting_dsh_and_presenting_the_built_in_surface() {
        let config = Config::default();

        assert!(config.dsh.auto_start);
        assert_eq!(config.dsh.first_launch, FirstLaunch::SurfaceDsh);
        assert!(!config.atelier.launch_at_login);
        assert_eq!(config.atelier.theme, ThemePreference::Dark);
        assert_eq!(config.atelier.surface, SurfaceConfig::default());
    }

    #[test]
    fn defaults_the_surface_branding_and_loading_copy() {
        let surface = SurfaceConfig::default();

        assert_eq!(surface.title, "DeepSeek Harness");
        assert_eq!(surface.title_icon, None);
        assert_eq!(surface.loading_icon, None);
        assert_eq!(surface.loading_title, "DeepSeek Harness");
        assert_eq!(surface.loading_starting_text, "正在启动…");
        assert_eq!(surface.loading_started_text, "已启动");
    }

    #[test]
    fn accepts_custom_surface_branding_and_loading_copy() {
        let config = Config::from_toml(
            r#"
                [atelier.surface]
                title = "My Harness"
                title_icon = "branding/title.png"
                loading_icon = "branding/loading.svg"
                loading_title = "My Harness"
                loading_starting_text = "正在准备服务…"
                loading_started_text = "准备完毕"
            "#,
        )
        .expect("custom Surface presentation parses");

        assert_eq!(config.atelier.surface.title, "My Harness");
        assert_eq!(
            config.atelier.surface.title_icon,
            Some(PathBuf::from("branding/title.png"))
        );
        assert_eq!(
            config.atelier.surface.loading_icon,
            Some(PathBuf::from("branding/loading.svg"))
        );
        assert_eq!(config.atelier.surface.loading_title, "My Harness");
        assert_eq!(
            config.atelier.surface.loading_starting_text,
            "正在准备服务…"
        );
        assert_eq!(config.atelier.surface.loading_started_text, "准备完毕");
    }

    #[test]
    fn an_existing_atelier_section_without_surface_uses_default_presentation() {
        let config = Config::from_toml(
            r#"
                [atelier]
                launch_at_login = true
                theme = "system"
            "#,
        )
        .expect("older configuration remains valid");

        assert_eq!(config.atelier.surface, SurfaceConfig::default());
    }

    #[test]
    fn rejects_an_unknown_surface_presentation_field() {
        let error = Config::from_toml(
            r#"
                [atelier.surface]
                subtitle = "unsupported"
            "#,
        )
        .expect_err("unknown Surface fields must not be ignored");

        assert!(error.to_string().contains("subtitle"));
    }

    #[test]
    fn an_existing_atelier_section_without_theme_uses_dark() {
        let config = Config::from_toml(
            r#"
                [atelier]
                launch_at_login = true
            "#,
        )
        .expect("older configuration remains valid");

        assert!(config.atelier.launch_at_login);
        assert_eq!(config.atelier.theme, ThemePreference::Dark);
    }

    #[test]
    fn accepts_all_supported_atelier_themes() {
        for (value, expected) in [
            ("light", ThemePreference::Light),
            ("dark", ThemePreference::Dark),
            ("system", ThemePreference::System),
        ] {
            let config = Config::from_toml(&format!(
                r#"
                    [atelier]
                    theme = "{value}"
                "#
            ))
            .expect("supported theme parses");

            assert_eq!(config.atelier.theme, expected);
        }
    }

    #[test]
    fn rejects_an_unknown_atelier_theme() {
        let error = Config::from_toml(
            r#"
                [atelier]
                theme = "sepia"
            "#,
        )
        .expect_err("unknown themes must not silently fall back");

        assert!(error.to_string().contains("theme"));
    }

    #[test]
    fn serializes_the_default_atelier_theme_as_dark() {
        let serialized = toml::to_string(&Config::default()).expect("configuration serializes");

        assert!(serialized.contains("theme = \"dark\""));
    }

    #[test]
    fn accepts_the_dsh_surface_as_the_first_launch_target() {
        let config = Config::from_toml(
            r#"
                [dsh]
                first_launch = "surface:dsh"
            "#,
        )
        .expect("the built-in DSH surface is supported");

        assert_eq!(config.dsh.first_launch, FirstLaunch::SurfaceDsh);
    }

    #[test]
    fn serializes_the_dsh_surface_with_its_namespaced_identifier() {
        let serialized = toml::to_string(&Config::default()).expect("configuration serializes");

        assert!(serialized.contains("first_launch = \"surface:dsh\""));
    }

    #[test]
    fn accepts_web_as_an_explicit_first_launch_target() {
        let config = Config::from_toml(
            r#"
                [dsh]
                first_launch = "web"
            "#,
        )
        .expect("the system browser remains supported");

        assert_eq!(config.dsh.first_launch, FirstLaunch::Web);
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
