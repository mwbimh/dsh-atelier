use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use atelier_bootstrap::{
    ActiveRuntime, BootstrapConfig, BootstrapError, ChildProbe, HealthOutcome, LaunchSource,
    PendingRuntime, RollbackRuntime, RuntimeEntry, RuntimeState, await_runtime_health,
    choose_launch, decide_switch, persist_runtime_state, read_runtime_state, resolve_launch,
    validate_health,
};

#[cfg(windows)]
#[test]
fn bootstrap_binary_embeds_an_application_icon() {
    use windows::{
        Win32::UI::Shell::ExtractIconExW,
        core::{HSTRING, PCWSTR},
    };

    let executable = HSTRING::from(env!("CARGO_BIN_EXE_dsh-atelier"));
    let count = unsafe { ExtractIconExW(PCWSTR(executable.as_ptr()), -1, None, None, 0) };

    assert!(
        count > 0,
        "Bootstrap executable does not contain an icon resource"
    );
}

fn runtime(version: &str) -> RuntimeEntry {
    RuntimeEntry {
        version: version.to_owned(),
        executable: PathBuf::from(format!("versions/{version}/dsh-atelier-runtime.exe")),
    }
}

#[derive(Default)]
struct RunningChild;

impl ChildProbe for RunningChild {
    fn try_exit_code(&mut self) -> io::Result<Option<i32>> {
        Ok(None)
    }
}

struct ExitedChild(i32);

impl ChildProbe for ExitedChild {
    fn try_exit_code(&mut self) -> io::Result<Option<i32>> {
        Ok(Some(self.0))
    }
}

#[test]
fn reads_schema_one_active_pending_and_rollback_files() {
    let root = tempfile::tempdir().unwrap();
    let runtime_dir = root.path();
    fs::write(
        runtime_dir.join("active.json"),
        r#"{"schema":1,"version":"0.1.0","executable":"versions/0.1.0/runtime"}"#,
    )
    .unwrap();
    fs::write(
        runtime_dir.join("pending.json"),
        r#"{"schema":1,"version":"0.2.0","executable":"versions/0.2.0/runtime"}"#,
    )
    .unwrap();
    fs::write(
        runtime_dir.join("rollback.json"),
        r#"{"schema":1,"version":"0.0.9","executable":"versions/0.0.9/runtime"}"#,
    )
    .unwrap();

    let state = read_runtime_state(runtime_dir).unwrap();

    assert_eq!(state.active.unwrap().entry.version, "0.1.0");
    assert_eq!(state.pending.unwrap().entry.version, "0.2.0");
    assert_eq!(state.rollback.unwrap().entry.version, "0.0.9");
}

#[test]
fn pending_runtime_is_tried_before_active_runtime() {
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.1.0"))),
        pending: Some(PendingRuntime::new(runtime("0.2.0"))),
        rollback: None,
    };

    let plan = choose_launch(&state).unwrap();

    assert_eq!(plan.source, LaunchSource::Pending);
    assert_eq!(plan.entry.version, "0.2.0");
}

#[test]
fn healthy_pending_runtime_becomes_active_and_preserves_previous_active_as_rollback() {
    let old = runtime("0.1.0");
    let candidate = runtime("0.2.0");
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(old.clone())),
        pending: Some(PendingRuntime::new(candidate.clone())),
        rollback: None,
    };

    let next = decide_switch(&state, LaunchSource::Pending, HealthOutcome::Healthy).unwrap();

    assert_eq!(next.active, Some(ActiveRuntime::new(candidate)));
    assert_eq!(next.pending, None);
    assert_eq!(next.rollback, Some(RollbackRuntime::new(old)));
}

#[test]
fn unhealthy_pending_runtime_is_discarded_without_changing_active() {
    let old = runtime("0.1.0");
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(old.clone())),
        pending: Some(PendingRuntime::new(runtime("0.2.0"))),
        rollback: None,
    };

    let next = decide_switch(&state, LaunchSource::Pending, HealthOutcome::Unhealthy).unwrap();

    assert_eq!(next.active, Some(ActiveRuntime::new(old)));
    assert_eq!(next.pending, None);
}

#[test]
fn runtime_state_transition_is_persisted_as_schema_one_pointer_files() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("pending.json"), "obsolete").unwrap();
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.2.0"))),
        pending: None,
        rollback: Some(RollbackRuntime::new(runtime("0.1.0"))),
    };

    persist_runtime_state(root.path(), &state).unwrap();
    let loaded = read_runtime_state(root.path()).unwrap();

    assert_eq!(loaded, state);
    assert!(!root.path().join("pending.json").exists());
}

#[test]
fn resolves_a_relative_active_executable_below_the_runtime_directory() {
    let root = tempfile::tempdir().unwrap();
    let executable = root
        .path()
        .join("runtime/versions/0.1.0/dsh-atelier-runtime.exe");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(&executable, []).unwrap();
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier.exe"),
        baseline_runtime: None,
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(15),
    };
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.1.0"))),
        ..RuntimeState::default()
    };

    let resolved = resolve_launch(&config, &state).unwrap();

    assert_eq!(resolved.source, LaunchSource::Active);
    assert_eq!(resolved.version.as_deref(), Some("0.1.0"));
    assert_eq!(resolved.executable, executable);
}

#[test]
fn uses_configured_baseline_when_no_pointer_or_sibling_runtime_exists() {
    let root = tempfile::tempdir().unwrap();
    let baseline = root.path().join("baseline-runtime.exe");
    fs::write(&baseline, []).unwrap();
    let config = BootstrapConfig {
        atelier_home: root.path().join("home"),
        bootstrap_executable: root.path().join("bin/dsh-atelier.exe"),
        baseline_runtime: Some(baseline.clone()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(15),
    };

    let resolved = resolve_launch(&config, &RuntimeState::default()).unwrap();

    assert_eq!(resolved.source, LaunchSource::Baseline);
    assert_eq!(resolved.executable, baseline);
}

#[test]
fn rejects_an_unknown_pointer_schema() {
    let root = tempfile::tempdir().unwrap();
    fs::write(
        root.path().join("active.json"),
        r#"{"schema":9,"version":"0.1.0","executable":"runtime"}"#,
    )
    .unwrap();

    let error = read_runtime_state(root.path()).unwrap_err();

    assert!(matches!(
        error,
        BootstrapError::UnsupportedPointerSchema { schema: 9, .. }
    ));
}

#[test]
fn health_file_requires_schema_one_and_the_expected_nonce() {
    validate_health(
        br#"{"schema":1,"nonce":"expected","pid":42,"runtime_version":"0.2.0"}"#,
        "expected",
    )
    .unwrap();

    let wrong_nonce = validate_health(br#"{"schema":1,"nonce":"stale"}"#, "expected");
    assert!(matches!(
        wrong_nonce,
        Err(BootstrapError::HealthNonceMismatch)
    ));

    let wrong_schema = validate_health(br#"{"schema":2,"nonce":"expected"}"#, "expected");
    assert!(matches!(
        wrong_schema,
        Err(BootstrapError::UnsupportedHealthSchema(2))
    ));
}

#[test]
fn accepts_a_matching_health_file() {
    let root = tempfile::tempdir().unwrap();
    let health_file = root.path().join("health.json");
    fs::write(
        &health_file,
        r#"{"schema":1,"nonce":"fresh","extra":"allowed"}"#,
    )
    .unwrap();

    await_runtime_health(
        &mut RunningChild,
        &health_file,
        "fresh",
        Duration::from_millis(100),
        Duration::from_millis(1),
    )
    .unwrap();
}

#[test]
fn reports_a_runtime_that_exits_before_becoming_healthy() {
    let error = await_runtime_health(
        &mut ExitedChild(23),
        Path::new("missing-health.json"),
        "fresh",
        Duration::from_millis(100),
        Duration::from_millis(1),
    )
    .unwrap_err();

    assert!(matches!(
        error,
        BootstrapError::RuntimeExitedBeforeHealthy(23)
    ));
}
