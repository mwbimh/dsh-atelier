use std::{
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use atelier_bootstrap::{
    ActiveRuntime, BootstrapConfig, BootstrapError, ChildProbe, HealthOutcome, LaunchSource,
    PendingRuntime, RUNTIME_ALREADY_RUNNING_EXIT_CODE, RUNTIME_UPDATE_RESTART_EXIT_CODE,
    RollbackRuntime, RunContinuation, RuntimeEntry, RuntimeState, await_runtime_health,
    choose_launch, choose_launch_for_generation, decide_runtime_exit, decide_switch,
    persist_pending_runtime, persist_runtime_state, plan_health_failure, read_runtime_state,
    resolve_atelier_home, resolve_launch, resolve_launch_for_generation, run,
    sanitize_runtime_state_for_generation, try_acquire_bootstrap_lock, validate_health,
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

#[test]
fn macos_portable_marker_places_state_beside_the_app_bundle() {
    let root = tempfile::tempdir().unwrap();
    let portable = root.path().join("DSH Atelier Portable");
    let executable = portable.join("DSH Atelier.app/Contents/MacOS/DSH Atelier");
    fs::create_dir_all(executable.parent().unwrap()).unwrap();
    fs::write(portable.join("atelier.portable"), []).unwrap();

    let home = resolve_atelier_home(&executable, Path::new("/Users/test/.atelier"));

    assert_eq!(home, portable.join("data"));
}

#[test]
fn app_without_portable_marker_uses_the_installed_state_root() {
    let root = tempfile::tempdir().unwrap();
    let executable = root
        .path()
        .join("Applications/DSH Atelier.app/Contents/MacOS/DSH Atelier");
    let installed = root.path().join("Users/test/.atelier");

    assert_eq!(resolve_atelier_home(&executable, &installed), installed);
}

fn runtime(version: &str) -> RuntimeEntry {
    runtime_for_generation(version, 1)
}

fn runtime_for_generation(version: &str, bootstrap_generation: u32) -> RuntimeEntry {
    RuntimeEntry {
        version: version.to_owned(),
        executable: PathBuf::from(format!("versions/{version}/dsh-atelier-runtime.exe")),
        bootstrap_generation,
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

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

#[cfg(unix)]
fn healthy_runtime_script(marker: &Path, before_exit: &str, exit_code: i32) -> String {
    format!(
        r#"#!/bin/sh
health_file=''
nonce=''
while [ "$#" -gt 0 ]; do
    case "$1" in
        --bootstrap-health-file) health_file="$2"; shift 2 ;;
        --bootstrap-health-nonce) nonce="$2"; shift 2 ;;
        *) shift ;;
    esac
done
printf '{{"schema":1,"nonce":"%s"}}' "$nonce" > "$health_file"
: > '{}'
{before_exit}
exit {exit_code}
"#,
        marker.display()
    )
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
    let pending = state.pending.unwrap();
    assert_eq!(pending.entry.version, "0.2.0");
    assert_eq!(pending.entry.bootstrap_generation, 1);
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
fn generation_two_skips_legacy_generation_one_managed_runtimes() {
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.1.0"))),
        pending: Some(PendingRuntime::new(runtime("0.2.0"))),
        rollback: Some(RollbackRuntime::new(runtime("0.0.9"))),
    };

    assert_eq!(choose_launch_for_generation(&state, 2), None);
}

#[test]
fn generation_selection_skips_an_incompatible_pending_but_can_use_a_compatible_active() {
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime_for_generation("0.3.0", 2))),
        pending: Some(PendingRuntime::new(runtime("0.4.0"))),
        rollback: None,
    };

    let plan = choose_launch_for_generation(&state, 2).unwrap();

    assert_eq!(plan.source, LaunchSource::Active);
    assert_eq!(plan.entry.version, "0.3.0");
}

#[test]
fn sanitizing_generation_discards_only_an_incompatible_pending_pointer() {
    let active = ActiveRuntime::new(runtime("0.1.0"));
    let rollback = RollbackRuntime::new(runtime("0.0.9"));
    let state = RuntimeState {
        active: Some(active.clone()),
        pending: Some(PendingRuntime::new(runtime("0.2.0"))),
        rollback: Some(rollback.clone()),
    };

    let sanitized = sanitize_runtime_state_for_generation(&state, 2);

    assert_eq!(sanitized.pending, None);
    assert_eq!(sanitized.active, Some(active));
    assert_eq!(sanitized.rollback, Some(rollback));
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
fn pending_health_failure_requests_an_immediate_relaunch_of_the_previous_runtime() {
    let old = runtime("0.1.0");
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(old.clone())),
        pending: Some(PendingRuntime::new(runtime("0.2.0"))),
        rollback: None,
    };

    let recovery = plan_health_failure(&state, LaunchSource::Pending).unwrap();

    assert_eq!(recovery.continuation, RunContinuation::Relaunch);
    assert_eq!(recovery.state.active, Some(ActiveRuntime::new(old)));
    assert_eq!(recovery.state.pending, None);
}

#[test]
fn active_health_failure_immediately_relaunches_the_rollback_runtime() {
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.2.0"))),
        rollback: Some(RollbackRuntime::new(runtime("0.1.0"))),
        ..RuntimeState::default()
    };

    let recovery = plan_health_failure(&state, LaunchSource::Active).unwrap();

    assert_eq!(recovery.continuation, RunContinuation::Relaunch);
    assert_eq!(
        recovery.state.active,
        Some(ActiveRuntime::new(runtime("0.1.0")))
    );
    assert_eq!(recovery.state.rollback, None);
}

#[test]
fn active_health_failure_without_rollback_clears_active_and_relaunches_baseline() {
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.2.0"))),
        ..RuntimeState::default()
    };

    let recovery = plan_health_failure(&state, LaunchSource::Active).unwrap();

    assert_eq!(recovery.continuation, RunContinuation::Relaunch);
    assert_eq!(recovery.state.active, None);
}

#[test]
fn retried_pending_promotion_does_not_overwrite_the_real_rollback_with_the_candidate() {
    let candidate = runtime("0.2.0");
    let old = runtime("0.1.0");
    let interrupted = RuntimeState {
        active: Some(ActiveRuntime::new(candidate.clone())),
        pending: Some(PendingRuntime::new(candidate.clone())),
        rollback: Some(RollbackRuntime::new(old.clone())),
    };

    let promoted =
        decide_switch(&interrupted, LaunchSource::Pending, HealthOutcome::Healthy).unwrap();

    assert_eq!(promoted.active, Some(ActiveRuntime::new(candidate)));
    assert_eq!(promoted.pending, None);
    assert_eq!(promoted.rollback, Some(RollbackRuntime::new(old)));
}

#[test]
fn dedicated_update_exit_requests_relaunch_after_runtime_stages_pending() {
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.1.0"))),
        pending: Some(PendingRuntime::new(runtime("0.2.0"))),
        rollback: None,
    };

    let continuation = decide_runtime_exit(RUNTIME_UPDATE_RESTART_EXIT_CODE, &state).unwrap();

    assert_eq!(continuation, RunContinuation::Relaunch);
}

#[test]
fn update_exit_without_a_staged_pending_runtime_is_rejected() {
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.1.0"))),
        ..RuntimeState::default()
    };

    let error = decide_runtime_exit(RUNTIME_UPDATE_RESTART_EXIT_CODE, &state).unwrap_err();

    assert!(matches!(error, BootstrapError::UpdateRestartWithoutPending));
}

#[test]
fn ordinary_runtime_exit_does_not_request_a_relaunch() {
    assert_eq!(
        decide_runtime_exit(0, &RuntimeState::default()).unwrap(),
        RunContinuation::Stop
    );
}

#[test]
fn bootstrap_lock_allows_only_one_supervisor_for_an_atelier_home() {
    let root = tempfile::tempdir().unwrap();
    let runtime_dir = root.path().join("runtime");

    let first = try_acquire_bootstrap_lock(&runtime_dir)
        .unwrap()
        .expect("first Bootstrap owns the lock");
    assert!(try_acquire_bootstrap_lock(&runtime_dir).unwrap().is_none());

    drop(first);
    assert!(try_acquire_bootstrap_lock(&runtime_dir).unwrap().is_some());
}

#[test]
fn second_bootstrap_exits_without_resolving_or_launching_a_pending_runtime() {
    let root = tempfile::tempdir().unwrap();
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: None,
        baseline_version: Some("0.1.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(5),
    };
    let _owner = try_acquire_bootstrap_lock(&config.runtime_dir())
        .unwrap()
        .expect("existing Bootstrap owns the lock");
    fs::write(
        config.runtime_dir().join("pending.json"),
        r#"{"schema":1,"version":"0.2.0","executable":"missing-runtime"}"#,
    )
    .unwrap();

    assert_eq!(run(&config).unwrap(), 0);
}

#[cfg(unix)]
#[test]
fn update_restart_launches_the_staged_pending_runtime_in_the_same_bootstrap_process() {
    let root = tempfile::tempdir().unwrap();
    let baseline = root.path().join("baseline-runtime");
    let candidate = root.path().join("candidate-runtime");
    let baseline_marker = root.path().join("baseline-ran");
    let candidate_marker = root.path().join("candidate-ran");
    let pending = PendingRuntime::new(RuntimeEntry {
        version: "0.2.0".to_owned(),
        executable: candidate.clone(),
        bootstrap_generation: 1,
    });
    let staged_pointer = serde_json::to_string(&pending).unwrap();
    let stage_pending = format!(
        "printf '%s' '{}' > \"$atelier_home/runtime/pending.json\"",
        staged_pointer
    );
    let baseline_script = healthy_runtime_script(
        &baseline_marker,
        &format!("atelier_home='{}'\n{stage_pending}", root.path().display()),
        RUNTIME_UPDATE_RESTART_EXIT_CODE,
    );
    write_executable(&baseline, &baseline_script);
    write_executable(
        &candidate,
        &healthy_runtime_script(&candidate_marker, "", 0),
    );
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: Some(baseline),
        baseline_version: Some("0.1.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(5),
    };

    assert_eq!(run(&config).unwrap(), 0);
    assert!(baseline_marker.is_file());
    assert!(candidate_marker.is_file());
    let state = read_runtime_state(&config.runtime_dir()).unwrap();
    assert_eq!(state.active, Some(ActiveRuntime::new(pending.entry)));
    assert_eq!(state.pending, None);
}

#[cfg(unix)]
#[test]
fn failed_pending_health_check_immediately_launches_the_previous_active_runtime() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("old-runtime");
    let candidate = root.path().join("candidate-runtime");
    let old_marker = root.path().join("old-ran");
    write_executable(&old, &healthy_runtime_script(&old_marker, "", 0));
    write_executable(&candidate, "#!/bin/sh\nexit 2\n");
    let old_entry = RuntimeEntry {
        version: "0.1.0".to_owned(),
        executable: old,
        bootstrap_generation: 1,
    };
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(old_entry.clone())),
        pending: Some(PendingRuntime::new(RuntimeEntry {
            version: "0.2.0".to_owned(),
            executable: candidate,
            bootstrap_generation: 1,
        })),
        rollback: None,
    };
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: None,
        baseline_version: Some("0.1.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(5),
    };
    persist_runtime_state(&config.runtime_dir(), &state).unwrap();

    assert_eq!(run(&config).unwrap(), 0);
    assert!(old_marker.is_file());
    let state = read_runtime_state(&config.runtime_dir()).unwrap();
    assert_eq!(state.active, Some(ActiveRuntime::new(old_entry)));
    assert_eq!(state.pending, None);
}

#[cfg(unix)]
#[test]
fn already_running_exit_before_health_leaves_all_runtime_pointers_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let active = root.path().join("active-runtime");
    write_executable(
        &active,
        &format!("#!/bin/sh\nexit {RUNTIME_ALREADY_RUNNING_EXIT_CODE}\n"),
    );
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(RuntimeEntry {
            version: "0.1.0".to_owned(),
            executable: active,
            bootstrap_generation: 1,
        })),
        pending: Some(PendingRuntime::new(RuntimeEntry {
            version: "0.2.0".to_owned(),
            executable: root.path().join("incompatible-candidate"),
            bootstrap_generation: 2,
        })),
        rollback: Some(RollbackRuntime::new(runtime("0.0.9"))),
    };
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: None,
        baseline_version: Some("0.1.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(5),
    };
    persist_runtime_state(&config.runtime_dir(), &state).unwrap();

    assert_eq!(run(&config).unwrap(), 0);
    assert_eq!(read_runtime_state(&config.runtime_dir()).unwrap(), state);
}

#[cfg(unix)]
#[test]
fn missing_pending_executable_immediately_launches_the_previous_active_runtime() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("old-runtime");
    let old_marker = root.path().join("old-ran");
    write_executable(&old, &healthy_runtime_script(&old_marker, "", 0));
    let old_entry = RuntimeEntry {
        version: "0.1.0".to_owned(),
        executable: old,
        bootstrap_generation: 1,
    };
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(old_entry.clone())),
        pending: Some(PendingRuntime::new(RuntimeEntry {
            version: "0.2.0".to_owned(),
            executable: root.path().join("missing-candidate"),
            bootstrap_generation: 1,
        })),
        rollback: None,
    };
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: None,
        baseline_version: Some("0.1.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(5),
    };
    persist_runtime_state(&config.runtime_dir(), &state).unwrap();

    assert_eq!(run(&config).unwrap(), 0);
    assert!(old_marker.is_file());
    let state = read_runtime_state(&config.runtime_dir()).unwrap();
    assert_eq!(state.active, Some(ActiveRuntime::new(old_entry)));
    assert_eq!(state.pending, None);
}

#[cfg(unix)]
#[test]
fn pending_spawn_error_immediately_launches_the_previous_active_runtime() {
    let root = tempfile::tempdir().unwrap();
    let old = root.path().join("old-runtime");
    let candidate = root.path().join("candidate-runtime");
    let old_marker = root.path().join("old-ran");
    write_executable(&old, &healthy_runtime_script(&old_marker, "", 0));
    fs::write(&candidate, "not executable").unwrap();
    let old_entry = RuntimeEntry {
        version: "0.1.0".to_owned(),
        executable: old,
        bootstrap_generation: 1,
    };
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(old_entry.clone())),
        pending: Some(PendingRuntime::new(RuntimeEntry {
            version: "0.2.0".to_owned(),
            executable: candidate,
            bootstrap_generation: 1,
        })),
        rollback: None,
    };
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: None,
        baseline_version: Some("0.1.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(5),
    };
    persist_runtime_state(&config.runtime_dir(), &state).unwrap();

    assert_eq!(run(&config).unwrap(), 0);
    assert!(old_marker.is_file());
    let state = read_runtime_state(&config.runtime_dir()).unwrap();
    assert_eq!(state.active, Some(ActiveRuntime::new(old_entry)));
    assert_eq!(state.pending, None);
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
    let active_json = fs::read_to_string(root.path().join("active.json")).unwrap();
    assert!(active_json.contains(r#""bootstrap_generation": 1"#));
}

#[test]
fn staging_pending_does_not_rewrite_active_or_rollback_pointer_files() {
    let root = tempfile::tempdir().unwrap();
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.1.0"))),
        pending: None,
        rollback: Some(RollbackRuntime::new(runtime("0.0.9"))),
    };
    persist_runtime_state(root.path(), &state).unwrap();
    let active_before = fs::read(root.path().join("active.json")).unwrap();
    let rollback_before = fs::read(root.path().join("rollback.json")).unwrap();
    let pending = PendingRuntime::new(runtime("0.2.0"));

    persist_pending_runtime(root.path(), &pending).unwrap();

    assert_eq!(
        fs::read(root.path().join("active.json")).unwrap(),
        active_before
    );
    assert_eq!(
        fs::read(root.path().join("rollback.json")).unwrap(),
        rollback_before
    );
    assert_eq!(
        read_runtime_state(root.path()).unwrap().pending,
        Some(pending)
    );
}

#[cfg(windows)]
#[test]
fn failed_atomic_pointer_replace_preserves_the_old_pointer_and_cleans_the_temporary() {
    use std::os::windows::fs::OpenOptionsExt;

    let root = tempfile::tempdir().unwrap();
    let old_state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.1.0"))),
        ..RuntimeState::default()
    };
    persist_runtime_state(root.path(), &old_state).unwrap();
    let active_path = root.path().join("active.json");
    let old_bytes = fs::read(&active_path).unwrap();
    let _held = fs::OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&active_path)
        .unwrap();
    let next_state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.2.0"))),
        ..RuntimeState::default()
    };

    assert!(persist_runtime_state(root.path(), &next_state).is_err());
    assert_eq!(fs::read(&active_path).unwrap(), old_bytes);
    let temporary_files = fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with("active.json.") && name.ends_with(".tmp")
        })
        .count();
    assert_eq!(temporary_files, 0);
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
        baseline_version: Some("0.1.0".to_owned()),
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
        baseline_version: Some("0.1.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(15),
    };

    let resolved = resolve_launch(&config, &RuntimeState::default()).unwrap();

    assert_eq!(resolved.source, LaunchSource::Baseline);
    assert_eq!(resolved.executable, baseline);
}

#[test]
fn newer_full_package_baseline_supersedes_an_older_compatible_active_runtime() {
    let root = tempfile::tempdir().unwrap();
    let baseline = root.path().join("baseline-runtime");
    let active = root.path().join("runtime/versions/0.1.0/runtime");
    fs::create_dir_all(active.parent().unwrap()).unwrap();
    fs::write(&baseline, []).unwrap();
    fs::write(&active, []).unwrap();
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: Some(baseline.clone()),
        baseline_version: Some("0.2.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(15),
    };
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(RuntimeEntry {
            version: "0.1.0".to_owned(),
            executable: active,
            bootstrap_generation: 1,
        })),
        ..RuntimeState::default()
    };

    let resolved = resolve_launch_for_generation(&config, &state, 1).unwrap();

    assert_eq!(resolved.source, LaunchSource::Baseline);
    assert_eq!(resolved.version.as_deref(), Some("0.2.0"));
    assert_eq!(resolved.executable, baseline);
}

#[test]
fn newer_self_updated_active_runtime_still_supersedes_an_older_package_baseline() {
    let root = tempfile::tempdir().unwrap();
    let baseline = root.path().join("baseline-runtime");
    let active = root.path().join("runtime/versions/0.3.0/runtime");
    fs::create_dir_all(active.parent().unwrap()).unwrap();
    fs::write(&baseline, []).unwrap();
    fs::write(&active, []).unwrap();
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: Some(baseline),
        baseline_version: Some("0.2.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(15),
    };
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(RuntimeEntry {
            version: "0.3.0".to_owned(),
            executable: active.clone(),
            bootstrap_generation: 1,
        })),
        ..RuntimeState::default()
    };

    let resolved = resolve_launch_for_generation(&config, &state, 1).unwrap();

    assert_eq!(resolved.source, LaunchSource::Active);
    assert_eq!(resolved.executable, active);
}

#[test]
fn new_generation_full_package_uses_baseline_when_retained_active_is_incompatible() {
    let root = tempfile::tempdir().unwrap();
    let baseline = root.path().join("baseline-runtime");
    fs::write(&baseline, []).unwrap();
    let config = BootstrapConfig {
        atelier_home: root.path().to_owned(),
        bootstrap_executable: root.path().join("dsh-atelier"),
        baseline_runtime: Some(baseline.clone()),
        baseline_version: Some("0.2.0".to_owned()),
        runtime_arguments: Vec::new(),
        health_timeout: Duration::from_secs(15),
    };
    let state = RuntimeState {
        active: Some(ActiveRuntime::new(runtime("0.1.0"))),
        ..RuntimeState::default()
    };

    let resolved = resolve_launch_for_generation(&config, &state, 2).unwrap();

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

#[test]
fn recognizes_an_existing_runtime_before_the_health_handshake() {
    let error = await_runtime_health(
        &mut ExitedChild(RUNTIME_ALREADY_RUNNING_EXIT_CODE),
        Path::new("missing-health.json"),
        "fresh",
        Duration::from_millis(100),
        Duration::from_millis(1),
    )
    .unwrap_err();

    assert!(matches!(error, BootstrapError::RuntimeAlreadyRunning));
}
