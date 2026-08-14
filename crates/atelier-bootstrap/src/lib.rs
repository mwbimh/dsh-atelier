//! Stable bootstrap for selecting and starting an Atelier Runtime.
//!
//! This crate deliberately has no knowledge of DSH, the tray, or browser
//! integration. Its protocol with the Runtime consists only of versioned
//! pointer files and a one-shot health file.

use std::{
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use directories::BaseDirs;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const RUNTIME_POINTER_SCHEMA: u32 = 1;
pub const BOOTSTRAP_GENERATION: u32 = 1;
pub const HEALTH_SCHEMA: u32 = 1;
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(50);
#[cfg(windows)]
const WINDOWS_CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Error)]
pub enum BootstrapError {
    #[error("could not determine the current user's home directory")]
    HomeDirectoryUnavailable,
    #[error("could not determine the bootstrap executable path: {0}")]
    CurrentExecutable(io::Error),
    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid JSON in {path}: {source}")]
    InvalidJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to serialize Runtime state for {path}: {source}")]
    SerializeJson {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("{path} uses unsupported schema {schema}; expected schema 1")]
    UnsupportedPointerSchema { path: PathBuf, schema: u32 },
    #[error("{path} contains an empty runtime {field}")]
    EmptyRuntimeField { path: PathBuf, field: &'static str },
    #[error("no Atelier Runtime is configured or present beside the bootstrap")]
    RuntimeNotFound,
    #[error("configured Runtime executable does not exist: {0}")]
    RuntimeExecutableMissing(PathBuf),
    #[error("Runtime state cannot apply the requested switch: {0}")]
    InvalidSwitch(&'static str),
    #[error("failed to launch Runtime {program}: {source}")]
    Launch {
        program: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to inspect the Runtime process: {0}")]
    InspectChild(io::Error),
    #[error("Runtime exited with code {0} before reporting healthy")]
    RuntimeExitedBeforeHealthy(i32),
    #[error("Runtime did not report healthy within {0:?}")]
    HealthTimeout(Duration),
    #[error("Runtime health file is invalid JSON: {0}")]
    InvalidHealthJson(serde_json::Error),
    #[error("Runtime health file uses unsupported schema {0}; expected schema 1")]
    UnsupportedHealthSchema(u32),
    #[error("Runtime health file nonce does not match this launch")]
    HealthNonceMismatch,
    #[error("failed to wait for Runtime: {0}")]
    WaitForRuntime(io::Error),
    #[error("Runtime exited with code {0}")]
    RuntimeExited(i32),
    #[error("missing value after {0}")]
    MissingArgumentValue(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEntry {
    pub version: String,
    pub executable: PathBuf,
}

macro_rules! runtime_pointer {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
        pub struct $name {
            pub schema: u32,
            #[serde(flatten)]
            pub entry: RuntimeEntry,
        }

        impl $name {
            pub fn new(entry: RuntimeEntry) -> Self {
                Self {
                    schema: RUNTIME_POINTER_SCHEMA,
                    entry,
                }
            }
        }

        impl RuntimePointer for $name {
            fn schema(&self) -> u32 {
                self.schema
            }

            fn entry(&self) -> &RuntimeEntry {
                &self.entry
            }
        }
    };
}

runtime_pointer!(ActiveRuntime);
runtime_pointer!(PendingRuntime);
runtime_pointer!(RollbackRuntime);

trait RuntimePointer {
    fn schema(&self) -> u32;
    fn entry(&self) -> &RuntimeEntry;
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeState {
    pub active: Option<ActiveRuntime>,
    pub pending: Option<PendingRuntime>,
    pub rollback: Option<RollbackRuntime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchSource {
    Pending,
    Active,
    Rollback,
    Baseline,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    pub source: LaunchSource,
    pub entry: RuntimeEntry,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthOutcome {
    Healthy,
    Unhealthy,
}

/// Reads the three optional schema-one Runtime pointer files.
pub fn read_runtime_state(runtime_dir: &Path) -> Result<RuntimeState, BootstrapError> {
    Ok(RuntimeState {
        active: read_pointer(&runtime_dir.join("active.json"))?,
        pending: read_pointer(&runtime_dir.join("pending.json"))?,
        rollback: read_pointer(&runtime_dir.join("rollback.json"))?,
    })
}

/// Persists each pointer independently through a same-directory temporary
/// file. A missing value removes its pointer file.
pub fn persist_runtime_state(
    runtime_dir: &Path,
    state: &RuntimeState,
) -> Result<(), BootstrapError> {
    fs::create_dir_all(runtime_dir).map_err(|source| BootstrapError::Io {
        action: "create directory",
        path: runtime_dir.to_owned(),
        source,
    })?;

    // Keep a known rollback on disk before changing active. Pending is removed
    // last, making an interrupted commit retryable.
    write_pointer(&runtime_dir.join("rollback.json"), state.rollback.as_ref())?;
    write_pointer(&runtime_dir.join("active.json"), state.active.as_ref())?;
    write_pointer(&runtime_dir.join("pending.json"), state.pending.as_ref())?;
    Ok(())
}

fn write_pointer<T: Serialize>(path: &Path, pointer: Option<&T>) -> Result<(), BootstrapError> {
    let Some(pointer) = pointer else {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(BootstrapError::Io {
                action: "remove",
                path: path.to_owned(),
                source,
            }),
        };
    };

    let bytes =
        serde_json::to_vec_pretty(pointer).map_err(|source| BootstrapError::SerializeJson {
            path: path.to_owned(),
            source,
        })?;
    let temporary = path.with_extension(format!("json.{}.tmp", fresh_nonce()));
    fs::write(&temporary, bytes).map_err(|source| BootstrapError::Io {
        action: "write",
        path: temporary.clone(),
        source,
    })?;

    // std::fs::rename cannot replace a file on Windows. The fully written temp
    // file limits the non-atomic window to the replacement itself.
    if path.exists() {
        fs::remove_file(path).map_err(|source| BootstrapError::Io {
            action: "replace",
            path: path.to_owned(),
            source,
        })?;
    }
    if let Err(source) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(BootstrapError::Io {
            action: "activate",
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

fn read_pointer<T>(path: &Path) -> Result<Option<T>, BootstrapError>
where
    T: DeserializeOwned + RuntimePointer,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(BootstrapError::Io {
                action: "read",
                path: path.to_owned(),
                source,
            });
        }
    };
    let pointer: T =
        serde_json::from_slice(&bytes).map_err(|source| BootstrapError::InvalidJson {
            path: path.to_owned(),
            source,
        })?;
    validate_pointer(path, &pointer)?;
    Ok(Some(pointer))
}

fn validate_pointer(path: &Path, pointer: &impl RuntimePointer) -> Result<(), BootstrapError> {
    if pointer.schema() != RUNTIME_POINTER_SCHEMA {
        return Err(BootstrapError::UnsupportedPointerSchema {
            path: path.to_owned(),
            schema: pointer.schema(),
        });
    }
    if pointer.entry().version.trim().is_empty() {
        return Err(BootstrapError::EmptyRuntimeField {
            path: path.to_owned(),
            field: "version",
        });
    }
    if pointer.entry().executable.as_os_str().is_empty() {
        return Err(BootstrapError::EmptyRuntimeField {
            path: path.to_owned(),
            field: "executable",
        });
    }
    Ok(())
}

/// Selects a managed Runtime. Pending is tried first so an update can be
/// health-checked before it replaces active. Rollback is the last managed
/// recovery option.
pub fn choose_launch(state: &RuntimeState) -> Option<LaunchPlan> {
    if let Some(pending) = &state.pending {
        return Some(LaunchPlan {
            source: LaunchSource::Pending,
            entry: pending.entry.clone(),
        });
    }
    if let Some(active) = &state.active {
        return Some(LaunchPlan {
            source: LaunchSource::Active,
            entry: active.entry.clone(),
        });
    }
    state.rollback.as_ref().map(|rollback| LaunchPlan {
        source: LaunchSource::Rollback,
        entry: rollback.entry.clone(),
    })
}

/// Pure Runtime pointer transition. Filesystem persistence is intentionally
/// separate, making update and rollback policy exhaustively testable.
pub fn decide_switch(
    state: &RuntimeState,
    launched: LaunchSource,
    outcome: HealthOutcome,
) -> Result<RuntimeState, BootstrapError> {
    let mut next = state.clone();
    match (launched, outcome) {
        (LaunchSource::Pending, HealthOutcome::Healthy) => {
            let candidate = state
                .pending
                .as_ref()
                .ok_or(BootstrapError::InvalidSwitch("pending Runtime is absent"))?;
            next.active = Some(ActiveRuntime::new(candidate.entry.clone()));
            next.rollback = state
                .active
                .as_ref()
                .map(|active| RollbackRuntime::new(active.entry.clone()))
                .or_else(|| state.rollback.clone());
            next.pending = None;
        }
        (LaunchSource::Pending, HealthOutcome::Unhealthy) => {
            if state.pending.is_none() {
                return Err(BootstrapError::InvalidSwitch("pending Runtime is absent"));
            }
            next.pending = None;
            if next.active.is_none()
                && let Some(rollback) = &state.rollback
            {
                next.active = Some(ActiveRuntime::new(rollback.entry.clone()));
            }
        }
        (LaunchSource::Active, HealthOutcome::Unhealthy) => {
            if state.active.is_none() {
                return Err(BootstrapError::InvalidSwitch("active Runtime is absent"));
            }
            if let Some(rollback) = &state.rollback {
                next.active = Some(ActiveRuntime::new(rollback.entry.clone()));
            }
        }
        (LaunchSource::Active, HealthOutcome::Healthy) => {
            if state.active.is_none() {
                return Err(BootstrapError::InvalidSwitch("active Runtime is absent"));
            }
        }
        (LaunchSource::Rollback, _) => {
            let rollback = state
                .rollback
                .as_ref()
                .ok_or(BootstrapError::InvalidSwitch("rollback Runtime is absent"))?;
            if outcome == HealthOutcome::Healthy {
                next.active = Some(ActiveRuntime::new(rollback.entry.clone()));
            }
        }
        (LaunchSource::Baseline, _) => {}
    }
    Ok(next)
}

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub atelier_home: PathBuf,
    pub bootstrap_executable: PathBuf,
    pub baseline_runtime: Option<PathBuf>,
    pub runtime_arguments: Vec<OsString>,
    pub health_timeout: Duration,
}

impl BootstrapConfig {
    pub fn from_current_process() -> Result<Self, BootstrapError> {
        let base_dirs = BaseDirs::new().ok_or(BootstrapError::HomeDirectoryUnavailable)?;
        let bootstrap_executable =
            std::env::current_exe().map_err(BootstrapError::CurrentExecutable)?;
        Ok(Self {
            atelier_home: base_dirs.home_dir().join(".atelier"),
            bootstrap_executable,
            baseline_runtime: None,
            runtime_arguments: Vec::new(),
            health_timeout: DEFAULT_HEALTH_TIMEOUT,
        })
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.atelier_home.join("runtime")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedLaunch {
    pub source: LaunchSource,
    pub version: Option<String>,
    pub executable: PathBuf,
}

pub fn resolve_launch(
    config: &BootstrapConfig,
    state: &RuntimeState,
) -> Result<ResolvedLaunch, BootstrapError> {
    if let Some(plan) = choose_launch(state) {
        let executable = if plan.entry.executable.is_absolute() {
            plan.entry.executable
        } else {
            config.runtime_dir().join(plan.entry.executable)
        };
        ensure_executable_exists(&executable)?;
        return Ok(ResolvedLaunch {
            source: plan.source,
            version: Some(plan.entry.version),
            executable,
        });
    }

    let sibling = config
        .bootstrap_executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            "dsh-atelier-runtime{}",
            std::env::consts::EXE_SUFFIX
        ));
    if sibling.is_file() {
        return Ok(ResolvedLaunch {
            source: LaunchSource::Baseline,
            version: None,
            executable: sibling,
        });
    }
    if let Some(baseline) = &config.baseline_runtime {
        ensure_executable_exists(baseline)?;
        return Ok(ResolvedLaunch {
            source: LaunchSource::Baseline,
            version: None,
            executable: baseline.clone(),
        });
    }
    Err(BootstrapError::RuntimeNotFound)
}

fn ensure_executable_exists(path: &Path) -> Result<(), BootstrapError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(BootstrapError::RuntimeExecutableMissing(path.to_owned()))
    }
}

#[derive(Debug, Deserialize)]
struct HealthFile {
    schema: u32,
    nonce: String,
}

pub fn validate_health(bytes: &[u8], expected_nonce: &str) -> Result<(), BootstrapError> {
    let health: HealthFile =
        serde_json::from_slice(bytes).map_err(BootstrapError::InvalidHealthJson)?;
    if health.schema != HEALTH_SCHEMA {
        return Err(BootstrapError::UnsupportedHealthSchema(health.schema));
    }
    if health.nonce != expected_nonce {
        return Err(BootstrapError::HealthNonceMismatch);
    }
    Ok(())
}

pub trait ChildProbe {
    fn try_exit_code(&mut self) -> io::Result<Option<i32>>;
}

impl ChildProbe for Child {
    fn try_exit_code(&mut self) -> io::Result<Option<i32>> {
        self.try_wait()
            .map(|status| status.map(|status| status.code().unwrap_or(-1)))
    }
}

pub fn await_runtime_health(
    child: &mut impl ChildProbe,
    health_file: &Path,
    expected_nonce: &str,
    timeout: Duration,
    poll_interval: Duration,
) -> Result<(), BootstrapError> {
    let deadline = Instant::now() + timeout;
    loop {
        match fs::read(health_file) {
            Ok(bytes) => return validate_health(&bytes, expected_nonce),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(BootstrapError::Io {
                    action: "read",
                    path: health_file.to_owned(),
                    source,
                });
            }
        }

        if let Some(code) = child
            .try_exit_code()
            .map_err(BootstrapError::InspectChild)?
        {
            return Err(BootstrapError::RuntimeExitedBeforeHealthy(code));
        }
        if Instant::now() >= deadline {
            return Err(BootstrapError::HealthTimeout(timeout));
        }
        thread::sleep(poll_interval.min(deadline.saturating_duration_since(Instant::now())));
    }
}

static NONCE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn fresh_nonce() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = NONCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{timestamp:032x}-{:08x}-{sequence:016x}",
        std::process::id()
    )
}

pub fn run(config: &BootstrapConfig) -> Result<i32, BootstrapError> {
    let runtime_dir = config.runtime_dir();
    let state = read_runtime_state(&runtime_dir)?;
    let launch = resolve_launch(config, &state)?;
    let nonce = fresh_nonce();
    let health_dir = runtime_dir.join("health");
    fs::create_dir_all(&health_dir).map_err(|source| BootstrapError::Io {
        action: "create directory",
        path: health_dir.clone(),
        source,
    })?;
    let health_file = health_dir.join(format!("bootstrap-{nonce}.json"));

    let mut command = Command::new(&launch.executable);
    command
        .arg("--bootstrap-generation")
        .arg(BOOTSTRAP_GENERATION.to_string())
        .arg("--bootstrap-health-file")
        .arg(&health_file)
        .arg("--bootstrap-health-nonce")
        .arg(&nonce)
        .args(&config.runtime_arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(windows)]
    command.creation_flags(WINDOWS_CREATE_NO_WINDOW);
    let mut child = command.spawn().map_err(|source| BootstrapError::Launch {
        program: launch.executable.clone(),
        source,
    })?;

    if let Err(error) = await_runtime_health(
        &mut child,
        &health_file,
        &nonce,
        config.health_timeout,
        DEFAULT_POLL_INTERVAL,
    ) {
        let _ = child.kill();
        let _ = child.wait();
        if let Ok(next) = decide_switch(&state, launch.source, HealthOutcome::Unhealthy)
            && next != state
        {
            persist_runtime_state(&runtime_dir, &next)?;
        }
        return Err(error);
    }

    let next = decide_switch(&state, launch.source, HealthOutcome::Healthy)?;
    if next != state
        && let Err(error) = persist_runtime_state(&runtime_dir, &next)
    {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
    let _ = fs::remove_file(&health_file);

    let status = child.wait().map_err(BootstrapError::WaitForRuntime)?;
    let code = status.code().unwrap_or(-1);
    if status.success() {
        Ok(code)
    } else {
        Err(BootstrapError::RuntimeExited(code))
    }
}

pub fn run_from_environment() -> Result<i32, BootstrapError> {
    let mut config = BootstrapConfig::from_current_process()?;
    let mut args = std::env::args_os().skip(1);
    while let Some(argument) = args.next() {
        if argument == "--baseline-runtime" {
            config.baseline_runtime = Some(PathBuf::from(
                args.next()
                    .ok_or(BootstrapError::MissingArgumentValue("--baseline-runtime"))?,
            ));
        } else {
            config.runtime_arguments.push(argument);
        }
    }
    run(&config)
}
