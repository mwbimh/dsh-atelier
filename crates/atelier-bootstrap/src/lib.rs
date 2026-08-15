//! Stable bootstrap for selecting and starting an Atelier Runtime.
//!
//! This crate deliberately has no knowledge of DSH, the tray, or browser
//! integration. Its protocol with the Runtime consists only of versioned
//! pointer files and a one-shot health file.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[cfg(windows)]
use std::os::windows::{ffi::OsStrExt, process::CommandExt};

#[cfg(windows)]
use windows::{
    Win32::Storage::FileSystem::{MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    core::PCWSTR,
};

use directories::BaseDirs;
use fs2::FileExt;
use semver::Version;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const RUNTIME_POINTER_SCHEMA: u32 = 1;
pub const BOOTSTRAP_GENERATION: u32 = 1;
pub const HEALTH_SCHEMA: u32 = 1;
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(15);
pub const PORTABLE_MARKER: &str = "atelier.portable";
/// Runtime exit code reserved for a staged self-update. The Runtime must write
/// `pending.json` before exiting with this code.
pub const RUNTIME_UPDATE_RESTART_EXIT_CODE: i32 = 75;
/// Runtime exit code indicating that another Runtime already owns the app
/// instance lock. No health file is written for this terminal condition.
pub const RUNTIME_ALREADY_RUNNING_EXIT_CODE: i32 = 76;
const LEGACY_BOOTSTRAP_GENERATION: u32 = 1;
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
    #[error("another Atelier Runtime is already running")]
    RuntimeAlreadyRunning,
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
    #[error("Runtime requested an update restart without staging pending.json")]
    UpdateRestartWithoutPending,
    #[error("missing value after {0}")]
    MissingArgumentValue(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeEntry {
    pub version: String,
    pub executable: PathBuf,
    #[serde(default = "legacy_bootstrap_generation")]
    pub bootstrap_generation: u32,
}

const fn legacy_bootstrap_generation() -> u32 {
    LEGACY_BOOTSTRAP_GENERATION
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunContinuation {
    Stop,
    Relaunch,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthFailurePlan {
    pub state: RuntimeState,
    pub continuation: RunContinuation,
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

/// Atomically publishes only a staged candidate pointer. This intentionally
/// leaves active and rollback byte-for-byte unchanged so update staging cannot
/// rewrite recovery state captured by the Bootstrap supervisor.
pub fn persist_pending_runtime(
    runtime_dir: &Path,
    pending: &PendingRuntime,
) -> Result<(), BootstrapError> {
    fs::create_dir_all(runtime_dir).map_err(|source| BootstrapError::Io {
        action: "create directory",
        path: runtime_dir.to_owned(),
        source,
    })?;
    write_pointer(&runtime_dir.join("pending.json"), Some(pending))
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

    if let Err(source) = replace_pointer_file(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(BootstrapError::Io {
            action: "activate",
            path: path.to_owned(),
            source,
        });
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_pointer_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_pointer_file(temporary: &Path, destination: &Path) -> io::Result<()> {
    let temporary = temporary
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR(temporary.as_ptr()),
            PCWSTR(destination.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(io::Error::other)
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
    choose_launch_for_generation(state, BOOTSTRAP_GENERATION)
}

/// Selects only managed Runtimes that implement the protocol generation used
/// by the current Bootstrap. Schema-one pointers written before this field was
/// introduced deserialize as generation one.
pub fn choose_launch_for_generation(
    state: &RuntimeState,
    bootstrap_generation: u32,
) -> Option<LaunchPlan> {
    if let Some(pending) = &state.pending
        && pending.entry.bootstrap_generation == bootstrap_generation
    {
        return Some(LaunchPlan {
            source: LaunchSource::Pending,
            entry: pending.entry.clone(),
        });
    }
    if let Some(active) = &state.active
        && active.entry.bootstrap_generation == bootstrap_generation
    {
        return Some(LaunchPlan {
            source: LaunchSource::Active,
            entry: active.entry.clone(),
        });
    }
    state
        .rollback
        .as_ref()
        .filter(|rollback| rollback.entry.bootstrap_generation == bootstrap_generation)
        .map(|rollback| LaunchPlan {
            source: LaunchSource::Rollback,
            entry: rollback.entry.clone(),
        })
}

/// Removes an incompatible candidate pointer so a Runtime restart request
/// cannot keep selecting it. Active and rollback pointers are retained as
/// metadata for their compatible Bootstrap; version directories are untouched.
#[must_use]
pub fn sanitize_runtime_state_for_generation(
    state: &RuntimeState,
    bootstrap_generation: u32,
) -> RuntimeState {
    let mut sanitized = state.clone();
    if sanitized
        .pending
        .as_ref()
        .is_some_and(|pending| pending.entry.bootstrap_generation != bootstrap_generation)
    {
        sanitized.pending = None;
    }
    sanitized
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
                .filter(|active| active.entry != candidate.entry)
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
                next.rollback = None;
            }
        }
        (LaunchSource::Active, HealthOutcome::Unhealthy) => {
            if state.active.is_none() {
                return Err(BootstrapError::InvalidSwitch("active Runtime is absent"));
            }
            if let Some(rollback) = &state.rollback {
                next.active = Some(ActiveRuntime::new(rollback.entry.clone()));
                next.rollback = None;
            } else {
                next.active = None;
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
                next.rollback = None;
            } else {
                next.rollback = None;
            }
        }
        (LaunchSource::Baseline, _) => {}
    }
    Ok(next)
}

/// Plans the pointer transition after a Runtime fails its launch health check.
/// A failed managed Runtime is recoverable immediately; a failed package
/// baseline preserves the stop-and-report behavior.
pub fn plan_health_failure(
    state: &RuntimeState,
    launched: LaunchSource,
) -> Result<HealthFailurePlan, BootstrapError> {
    Ok(HealthFailurePlan {
        state: decide_switch(state, launched, HealthOutcome::Unhealthy)?,
        continuation: if matches!(
            launched,
            LaunchSource::Pending | LaunchSource::Active | LaunchSource::Rollback
        ) {
            RunContinuation::Relaunch
        } else {
            RunContinuation::Stop
        },
    })
}

/// Interprets the Runtime's process exit protocol. A restart request is valid
/// only after the Runtime has staged a candidate pointer.
pub fn decide_runtime_exit(
    exit_code: i32,
    refreshed_state: &RuntimeState,
) -> Result<RunContinuation, BootstrapError> {
    if exit_code != RUNTIME_UPDATE_RESTART_EXIT_CODE {
        return Ok(RunContinuation::Stop);
    }
    if refreshed_state.pending.is_none() {
        return Err(BootstrapError::UpdateRestartWithoutPending);
    }
    Ok(RunContinuation::Relaunch)
}

/// Acquires the per-state-root Bootstrap supervisor lock. The returned file
/// must remain alive for the entire supervised Runtime lifetime. `None` means
/// another Bootstrap already owns that lifecycle.
pub fn try_acquire_bootstrap_lock(runtime_dir: &Path) -> Result<Option<File>, BootstrapError> {
    fs::create_dir_all(runtime_dir).map_err(|source| BootstrapError::Io {
        action: "create directory",
        path: runtime_dir.to_owned(),
        source,
    })?;
    let path = runtime_dir.join("bootstrap.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)
        .map_err(|source| BootstrapError::Io {
            action: "open",
            path: path.clone(),
            source,
        })?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(file)),
        Err(error) if lock_is_contended(&error) => Ok(None),
        Err(source) => Err(BootstrapError::Io {
            action: "lock",
            path,
            source,
        }),
    }
}

fn lock_is_contended(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        // fs2 exposes LockFileEx's ERROR_LOCK_VIOLATION directly.
        const ERROR_LOCK_VIOLATION: i32 = 33;
        error.raw_os_error() == Some(ERROR_LOCK_VIOLATION)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[derive(Debug, Clone)]
pub struct BootstrapConfig {
    pub atelier_home: PathBuf,
    pub bootstrap_executable: PathBuf,
    pub baseline_runtime: Option<PathBuf>,
    pub baseline_version: Option<String>,
    pub runtime_arguments: Vec<OsString>,
    pub health_timeout: Duration,
}

impl BootstrapConfig {
    pub fn from_current_process() -> Result<Self, BootstrapError> {
        let base_dirs = BaseDirs::new().ok_or(BootstrapError::HomeDirectoryUnavailable)?;
        let bootstrap_executable =
            std::env::current_exe().map_err(BootstrapError::CurrentExecutable)?;
        Ok(Self {
            atelier_home: resolve_atelier_home(
                &bootstrap_executable,
                &base_dirs.home_dir().join(".atelier"),
            ),
            bootstrap_executable,
            baseline_runtime: None,
            baseline_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            runtime_arguments: Vec::new(),
            health_timeout: DEFAULT_HEALTH_TIMEOUT,
        })
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.atelier_home.join("runtime")
    }
}

/// Resolves the state root for installed and macOS portable deployments.
///
/// A portable archive places the marker and writable `data` directory beside
/// the `.app`, never inside the application bundle.
#[must_use]
pub fn resolve_atelier_home(bootstrap_executable: &Path, installed_home: &Path) -> PathBuf {
    portable_deployment_root(bootstrap_executable)
        .filter(|root| root.join(PORTABLE_MARKER).is_file())
        .map(|root| root.join("data"))
        .unwrap_or_else(|| installed_home.to_owned())
}

fn portable_deployment_root(bootstrap_executable: &Path) -> Option<&Path> {
    let macos_directory = bootstrap_executable.parent()?;
    if macos_directory.file_name()? != "MacOS" {
        return None;
    }
    let contents_directory = macos_directory.parent()?;
    if contents_directory.file_name()? != "Contents" {
        return None;
    }
    let app_bundle = contents_directory.parent()?;
    if app_bundle.extension()? != "app" {
        return None;
    }
    app_bundle.parent()
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
    resolve_launch_for_generation(config, state, BOOTSTRAP_GENERATION)
}

pub fn resolve_launch_for_generation(
    config: &BootstrapConfig,
    state: &RuntimeState,
    bootstrap_generation: u32,
) -> Result<ResolvedLaunch, BootstrapError> {
    let baseline = resolve_baseline(config);
    if let Some(plan) = choose_launch_for_generation(state, bootstrap_generation) {
        if plan.source != LaunchSource::Pending
            && baseline
                .as_ref()
                .is_some_and(|baseline| baseline_supersedes_managed(baseline, &plan.entry))
        {
            return Ok(baseline.expect("baseline was checked as present"));
        }
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

    if let Some(baseline) = baseline {
        return Ok(baseline);
    }
    if let Some(configured) = &config.baseline_runtime {
        ensure_executable_exists(configured)?;
    }
    Err(BootstrapError::RuntimeNotFound)
}

fn resolve_baseline(config: &BootstrapConfig) -> Option<ResolvedLaunch> {
    let sibling = config
        .bootstrap_executable
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(format!(
            "dsh-atelier-runtime{}",
            std::env::consts::EXE_SUFFIX
        ));
    if sibling.is_file() {
        return Some(ResolvedLaunch {
            source: LaunchSource::Baseline,
            version: config.baseline_version.clone(),
            executable: sibling,
        });
    }
    config
        .baseline_runtime
        .as_ref()
        .filter(|baseline| baseline.is_file())
        .map(|baseline| ResolvedLaunch {
            source: LaunchSource::Baseline,
            version: config.baseline_version.clone(),
            executable: baseline.clone(),
        })
}

fn baseline_supersedes_managed(baseline: &ResolvedLaunch, managed: &RuntimeEntry) -> bool {
    let Some(baseline_version) = baseline.version.as_deref() else {
        return false;
    };
    let (Ok(baseline_version), Ok(managed_version)) = (
        Version::parse(baseline_version),
        Version::parse(&managed.version),
    ) else {
        return false;
    };
    baseline_version > managed_version
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
            if code == RUNTIME_ALREADY_RUNNING_EXIT_CODE {
                return Err(BootstrapError::RuntimeAlreadyRunning);
            }
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
    let Some(_bootstrap_lock) = try_acquire_bootstrap_lock(&runtime_dir)? else {
        return Ok(0);
    };
    let health_dir = runtime_dir.join("health");
    fs::create_dir_all(&health_dir).map_err(|source| BootstrapError::Io {
        action: "create directory",
        path: health_dir.clone(),
        source,
    })?;

    loop {
        let persisted_state = read_runtime_state(&runtime_dir)?;
        let state = sanitize_runtime_state_for_generation(&persisted_state, BOOTSTRAP_GENERATION);
        let launch = match resolve_launch(config, &state) {
            Ok(launch) => launch,
            Err(error) => {
                let Some(plan) = choose_launch(&state) else {
                    return Err(error);
                };
                if recover_managed_launch_failure(
                    &runtime_dir,
                    &persisted_state,
                    &state,
                    plan.source,
                )? {
                    eprintln!(
                        "dsh-atelier bootstrap: managed Runtime could not be resolved; relaunching fallback Runtime: {error}"
                    );
                    continue;
                }
                return Err(error);
            }
        };
        let nonce = fresh_nonce();
        let health_file = health_dir.join(format!("bootstrap-{nonce}.json"));

        let mut command = Command::new(&launch.executable);
        command
            .arg("--bootstrap-generation")
            .arg(BOOTSTRAP_GENERATION.to_string())
            .arg("--atelier-home")
            .arg(&config.atelier_home)
            .arg("--atelier-bootstrap")
            .arg(&config.bootstrap_executable)
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
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(source) => {
                let error = BootstrapError::Launch {
                    program: launch.executable.clone(),
                    source,
                };
                if recover_managed_launch_failure(
                    &runtime_dir,
                    &persisted_state,
                    &state,
                    launch.source,
                )? {
                    eprintln!(
                        "dsh-atelier bootstrap: managed Runtime could not be launched; relaunching fallback Runtime: {error}"
                    );
                    continue;
                }
                return Err(error);
            }
        };

        if let Err(error) = await_runtime_health(
            &mut child,
            &health_file,
            &nonce,
            config.health_timeout,
            DEFAULT_POLL_INTERVAL,
        ) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&health_file);
            if matches!(error, BootstrapError::RuntimeAlreadyRunning) {
                return Ok(0);
            }
            let recovery = plan_health_failure(&state, launch.source)?;
            if recovery.state != persisted_state {
                persist_runtime_state(&runtime_dir, &recovery.state)?;
            }
            if recovery.continuation == RunContinuation::Relaunch {
                eprintln!(
                    "dsh-atelier bootstrap: pending Runtime failed health check; relaunching previous Runtime: {error}"
                );
                continue;
            }
            return Err(error);
        }

        let next = decide_switch(&state, launch.source, HealthOutcome::Healthy)?;
        if next != persisted_state
            && let Err(error) = persist_runtime_state(&runtime_dir, &next)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        let _ = fs::remove_file(&health_file);

        let status = child.wait().map_err(BootstrapError::WaitForRuntime)?;
        let code = status.code().unwrap_or(-1);
        if code == RUNTIME_UPDATE_RESTART_EXIT_CODE {
            let refreshed_state = read_runtime_state(&runtime_dir)?;
            let sanitized =
                sanitize_runtime_state_for_generation(&refreshed_state, BOOTSTRAP_GENERATION);
            if sanitized != refreshed_state {
                persist_runtime_state(&runtime_dir, &sanitized)?;
            }
            let refreshed_state = sanitized;
            if decide_runtime_exit(code, &refreshed_state)? == RunContinuation::Relaunch {
                continue;
            }
        }
        if status.success() {
            return Ok(code);
        }
        return Err(BootstrapError::RuntimeExited(code));
    }
}

fn recover_managed_launch_failure(
    runtime_dir: &Path,
    persisted_state: &RuntimeState,
    state: &RuntimeState,
    source: LaunchSource,
) -> Result<bool, BootstrapError> {
    if source == LaunchSource::Baseline {
        return Ok(false);
    }
    let recovery = plan_health_failure(state, source)?;
    if recovery.state != *persisted_state {
        persist_runtime_state(runtime_dir, &recovery.state)?;
    }
    Ok(recovery.continuation == RunContinuation::Relaunch)
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
