use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use serde::Serialize;
use thiserror::Error;

use crate::controller::LaunchKind;

pub const BOOTSTRAP_GENERATION: u32 = 1;
const HEALTH_SCHEMA: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BootstrapHealthRequest {
    pub file: PathBuf,
    pub nonce: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeArguments {
    pub launch_kind: LaunchKind,
    pub health: Option<BootstrapHealthRequest>,
    pub smoke_test: bool,
}

impl RuntimeArguments {
    pub fn parse(
        arguments: impl IntoIterator<Item = OsString>,
    ) -> Result<Self, RuntimeArgumentError> {
        let arguments = arguments.into_iter().collect::<Vec<_>>();
        let mut launch_kind = LaunchKind::Explicit;
        let mut smoke_test = false;
        let mut generation = None;
        let mut health_file = None;
        let mut health_nonce = None;
        let mut index = 0;

        while index < arguments.len() {
            let argument = arguments[index].to_string_lossy();
            match argument.as_ref() {
                "--autostart" => launch_kind = LaunchKind::Login,
                "--smoke-test" => smoke_test = true,
                "--bootstrap-generation" => {
                    generation = Some(parse_generation(required_value(&arguments, index)?)?);
                    index += 1;
                }
                "--bootstrap-health-file" => {
                    health_file = Some(PathBuf::from(required_value(&arguments, index)?));
                    index += 1;
                }
                "--bootstrap-health-nonce" => {
                    health_nonce = Some(
                        required_value(&arguments, index)?
                            .to_str()
                            .ok_or(RuntimeArgumentError::NonUnicodeNonce)?
                            .to_owned(),
                    );
                    index += 1;
                }
                _ => return Err(RuntimeArgumentError::Unknown(arguments[index].clone())),
            }
            index += 1;
        }

        let health = match (generation, health_file, health_nonce) {
            (None, None, None) => None,
            (Some(value), Some(file), Some(nonce)) if value == BOOTSTRAP_GENERATION => {
                Some(BootstrapHealthRequest { file, nonce })
            }
            (Some(value), Some(_), Some(_)) => {
                return Err(RuntimeArgumentError::UnsupportedGeneration(value));
            }
            _ => return Err(RuntimeArgumentError::IncompleteHealthHandshake),
        };

        Ok(Self {
            launch_kind,
            health,
            smoke_test,
        })
    }
}

fn required_value(arguments: &[OsString], index: usize) -> Result<&OsString, RuntimeArgumentError> {
    arguments
        .get(index + 1)
        .ok_or_else(|| RuntimeArgumentError::MissingValue(arguments[index].clone()))
}

fn parse_generation(value: &OsString) -> Result<u32, RuntimeArgumentError> {
    value
        .to_str()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| RuntimeArgumentError::InvalidGeneration(value.clone()))
}

#[derive(Debug, Error)]
pub enum RuntimeArgumentError {
    #[error("missing value after {0:?}")]
    MissingValue(OsString),
    #[error("unknown Runtime argument {0:?}")]
    Unknown(OsString),
    #[error("invalid Bootstrap generation {0:?}")]
    InvalidGeneration(OsString),
    #[error("unsupported Bootstrap generation {0}; expected generation 1")]
    UnsupportedGeneration(u32),
    #[error("Bootstrap health arguments must be provided together")]
    IncompleteHealthHandshake,
    #[error("Bootstrap health nonce is not valid Unicode")]
    NonUnicodeNonce,
}

#[derive(Serialize)]
struct RuntimeHealth<'a> {
    schema: u32,
    nonce: &'a str,
    runtime_version: &'a str,
    pid: u32,
}

pub fn write_bootstrap_health(
    request: &BootstrapHealthRequest,
    runtime_version: &str,
) -> io::Result<()> {
    if let Some(parent) = request.file.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = request.file.with_extension(format!(
        "{}.{}.tmp",
        request
            .file
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("health"),
        std::process::id()
    ));
    let payload = serde_json::to_vec(&RuntimeHealth {
        schema: HEALTH_SCHEMA,
        nonce: &request.nonce,
        runtime_version,
        pid: std::process::id(),
    })
    .map_err(io::Error::other)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    if let Err(error) = file.write_all(&payload).and_then(|()| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, &request.file) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    pub fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if lock_is_held(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn lock_is_held(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // `LockFileEx` reports ERROR_LOCK_VIOLATION, which Rust currently maps
        // to `Uncategorized` rather than `WouldBlock`.
        error.raw_os_error() == Some(33)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs};

    use super::*;

    #[test]
    fn parses_an_explicit_launch_and_complete_bootstrap_handshake() {
        let args = RuntimeArguments::parse([
            OsString::from("--bootstrap-generation"),
            OsString::from("1"),
            OsString::from("--bootstrap-health-file"),
            OsString::from("health.json"),
            OsString::from("--bootstrap-health-nonce"),
            OsString::from("nonce-123"),
        ])
        .expect("valid Runtime arguments");

        assert_eq!(args.launch_kind, crate::controller::LaunchKind::Explicit);
        assert_eq!(args.health.expect("health request").nonce, "nonce-123");
    }

    #[test]
    fn parses_login_and_smoke_launch_modes() {
        let args = RuntimeArguments::parse([
            OsString::from("--autostart"),
            OsString::from("--smoke-test"),
        ])
        .expect("valid Runtime arguments");

        assert_eq!(args.launch_kind, crate::controller::LaunchKind::Login);
        assert!(args.smoke_test);
    }

    #[test]
    fn rejects_partial_or_unknown_runtime_arguments() {
        assert!(RuntimeArguments::parse([OsString::from("--bootstrap-generation")]).is_err());
        assert!(
            RuntimeArguments::parse([
                OsString::from("--bootstrap-generation"),
                OsString::from("1"),
            ])
            .is_err()
        );
        assert!(RuntimeArguments::parse([OsString::from("--unknown")]).is_err());
    }

    #[test]
    fn writes_a_nonce_bound_health_file_atomically() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let request = BootstrapHealthRequest {
            file: directory.path().join("health/runtime.json"),
            nonce: "expected-nonce".to_owned(),
        };

        write_bootstrap_health(&request, "0.1.0").expect("write Runtime health");

        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&request.file).unwrap()).unwrap();
        assert_eq!(value["schema"], 1);
        assert_eq!(value["nonce"], "expected-nonce");
        assert_eq!(value["runtime_version"], "0.1.0");
        assert_eq!(value["pid"], std::process::id());
        assert_eq!(
            fs::read_dir(request.file.parent().unwrap())
                .unwrap()
                .count(),
            1
        );
    }

    #[test]
    fn holds_an_exclusive_instance_lock() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let lock_path = directory.path().join("state/atelier.lock");

        let first = InstanceLock::try_acquire(&lock_path)
            .expect("acquire first lock")
            .expect("first process owns lock");
        let second = InstanceLock::try_acquire(&lock_path).expect("inspect second lock");

        assert!(second.is_none());
        drop(first);
        assert!(
            InstanceLock::try_acquire(&lock_path)
                .expect("reacquire released lock")
                .is_some()
        );
    }
}
