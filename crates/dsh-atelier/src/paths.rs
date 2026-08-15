use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::UserDirs;
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AtelierPaths {
    pub root: PathBuf,
    pub config_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub tools_dir: PathBuf,
    pub npm_dir: PathBuf,
    pub dsh_installations_dir: PathBuf,
    pub dsh_surface_dir: PathBuf,
}

impl AtelierPaths {
    pub fn discover() -> Result<Self, PathError> {
        let user_dirs = UserDirs::new().ok_or(PathError::HomeDirectoryUnavailable)?;
        Ok(Self::from_home(user_dirs.home_dir()))
    }

    pub fn from_home(home: &Path) -> Self {
        Self::from_root(&home.join(".atelier"))
    }

    pub fn from_root(root: &Path) -> Self {
        let root = root.to_path_buf();
        let config_dir = root.join("config");

        Self {
            config_file: config_dir.join("atelier.toml"),
            state_dir: root.join("state"),
            logs_dir: root.join("logs"),
            runtime_dir: root.join("runtime"),
            tools_dir: root.join("tools"),
            npm_dir: root.join("npm"),
            dsh_installations_dir: root.join("installations").join("dsh"),
            dsh_surface_dir: root.join("surfaces").join("dsh"),
            config_dir,
            root,
        }
    }

    pub fn create_directories(&self) -> io::Result<()> {
        for path in [
            &self.config_dir,
            &self.state_dir,
            &self.logs_dir,
            &self.runtime_dir,
            &self.tools_dir,
            &self.npm_dir,
            &self.dsh_installations_dir,
            &self.dsh_surface_dir,
        ] {
            fs::create_dir_all(path)?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum PathError {
    #[error("the current user's home directory is unavailable")]
    HomeDirectoryUnavailable,
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn resolves_the_atelier_root_from_an_injected_home() {
        let paths = AtelierPaths::from_home(Path::new("C:/Users/tester"));

        assert_eq!(paths.root, Path::new("C:/Users/tester/.atelier"));
        assert_eq!(
            paths.config_file,
            Path::new("C:/Users/tester/.atelier/config/atelier.toml")
        );
    }

    #[test]
    fn accepts_an_explicit_root_for_isolated_tests() {
        let paths = AtelierPaths::from_root(Path::new("test-state"));

        assert_eq!(paths.root, Path::new("test-state"));
        assert_eq!(paths.state_dir, Path::new("test-state/state"));
        assert_eq!(paths.dsh_surface_dir, Path::new("test-state/surfaces/dsh"));
        assert_eq!(
            paths.dsh_installations_dir,
            Path::new("test-state/installations/dsh")
        );
    }

    #[test]
    fn creates_only_the_owned_atelier_directories() {
        let directory = tempfile::tempdir().expect("temporary home");
        let paths = AtelierPaths::from_home(directory.path());

        paths.create_directories().expect("create Atelier layout");

        for owned in [
            &paths.config_dir,
            &paths.state_dir,
            &paths.logs_dir,
            &paths.runtime_dir,
            &paths.tools_dir,
            &paths.npm_dir,
            &paths.dsh_installations_dir,
            &paths.dsh_surface_dir,
        ] {
            assert!(owned.is_dir(), "missing {}", owned.display());
        }
        assert!(!directory.path().join(".dsh").exists());
    }
}
