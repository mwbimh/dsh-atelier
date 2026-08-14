use std::{
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use semver::Version;
use thiserror::Error;

use crate::process::{CommandOutput, CommandSpec, ProcessError, run_once};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateSource {
    LastUsed,
    Managed,
    Path,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DshCandidate {
    pub program: PathBuf,
    pub source: CandidateSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveredDsh {
    pub program: PathBuf,
    pub version: Version,
    pub source: CandidateSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscoveryReport {
    pub selected: Option<DiscoveredDsh>,
    pub failures: Vec<CandidateFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CandidateFailure {
    pub candidate: DshCandidate,
    pub reason: CandidateFailureReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CandidateFailureReason {
    Execution(String),
    ExitStatus { status: i32, stderr: String },
    InvalidVersion(String),
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum VersionParseError {
    #[error("DSH version output is empty")]
    Empty,
    #[error("DSH version output contains multiple lines")]
    MultipleLines,
    #[error("invalid DSH semantic version: {0}")]
    Invalid(String),
}

/// Builds the ordered executable candidates used by DSH discovery.
///
/// `path` and `pathext` are explicit so tests and callers do not need to mutate
/// the process environment. Relative PATH entries are resolved against `cwd`.
pub fn discover_candidates(
    last_used: Option<&Path>,
    managed: Option<&Path>,
    path: Option<&OsStr>,
    pathext: Option<&OsStr>,
    cwd: &Path,
) -> Vec<DshCandidate> {
    let mut candidates = Vec::new();

    if let Some(program) = last_used {
        push_if_file(
            &mut candidates,
            absolute_from(program, cwd),
            CandidateSource::LastUsed,
        );
    }

    if let Some(program) = managed {
        push_if_file(
            &mut candidates,
            absolute_from(program, cwd),
            CandidateSource::Managed,
        );
    }

    if let Some(path) = path {
        for directory in std::env::split_paths(path) {
            let directory = absolute_from(&directory, cwd);
            for name in path_command_names(pathext) {
                push_if_file(&mut candidates, directory.join(name), CandidateSource::Path);
            }
        }
    }

    deduplicate_candidates(candidates)
}

pub fn parse_dsh_version(output: &str) -> Result<Version, VersionParseError> {
    let output = output.trim();
    if output.is_empty() {
        return Err(VersionParseError::Empty);
    }
    if output.contains(['\r', '\n']) {
        return Err(VersionParseError::MultipleLines);
    }

    let version = output.strip_prefix('v').unwrap_or(output);
    Version::parse(version).map_err(|_| VersionParseError::Invalid(output.to_owned()))
}

#[async_trait]
pub trait DshVersionProbe: Send + Sync {
    async fn run(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput, ProcessError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessDshVersionProbe;

#[async_trait]
impl DshVersionProbe for ProcessDshVersionProbe {
    async fn run(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput, ProcessError> {
        run_once(spec, timeout).await
    }
}

pub async fn discover_dsh(candidates: &[DshCandidate], timeout: Duration) -> DiscoveryReport {
    discover_dsh_with(&ProcessDshVersionProbe, candidates, timeout).await
}

pub async fn discover_dsh_with<P: DshVersionProbe + ?Sized>(
    probe: &P,
    candidates: &[DshCandidate],
    timeout: Duration,
) -> DiscoveryReport {
    let mut failures = Vec::new();

    for candidate in candidates {
        let spec = CommandSpec {
            program: candidate.program.clone(),
            args: vec!["--version".to_owned()],
            current_dir: None,
            env: Default::default(),
        };

        let output = match probe.run(&spec, timeout).await {
            Ok(output) => output,
            Err(error) => {
                failures.push(CandidateFailure {
                    candidate: candidate.clone(),
                    reason: CandidateFailureReason::Execution(error.to_string()),
                });
                continue;
            }
        };

        if output.status != 0 {
            failures.push(CandidateFailure {
                candidate: candidate.clone(),
                reason: CandidateFailureReason::ExitStatus {
                    status: output.status,
                    stderr: output.stderr,
                },
            });
            continue;
        }

        match parse_dsh_version(&output.stdout) {
            Ok(version) => {
                return DiscoveryReport {
                    selected: Some(DiscoveredDsh {
                        program: candidate.program.clone(),
                        version,
                        source: candidate.source,
                    }),
                    failures,
                };
            }
            Err(error) => failures.push(CandidateFailure {
                candidate: candidate.clone(),
                reason: CandidateFailureReason::InvalidVersion(error.to_string()),
            }),
        }
    }

    DiscoveryReport {
        selected: None,
        failures,
    }
}

fn absolute_from(path: &Path, cwd: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

fn push_if_file(candidates: &mut Vec<DshCandidate>, program: PathBuf, source: CandidateSource) {
    if is_executable_file(&program) {
        candidates.push(DshCandidate { program, source });
    }
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn path_command_names(pathext: Option<&OsStr>) -> Vec<String> {
    #[cfg(windows)]
    {
        const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";
        let value = pathext
            .and_then(OsStr::to_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(DEFAULT_PATHEXT);
        let mut names = Vec::new();
        let mut seen = HashSet::new();

        for extension in value.split(';') {
            let extension = extension.trim();
            if extension.is_empty() {
                continue;
            }
            let extension = if extension.starts_with('.') {
                extension.to_owned()
            } else {
                format!(".{extension}")
            };
            let name = format!("dsh{}", extension.to_ascii_lowercase());
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
        names
    }

    #[cfg(not(windows))]
    {
        let _ = pathext;
        vec!["dsh".to_owned()]
    }
}

fn deduplicate_candidates(candidates: Vec<DshCandidate>) -> Vec<DshCandidate> {
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| seen.insert(path_identity(&candidate.program)))
        .collect()
}

fn path_identity(path: &Path) -> String {
    let resolved = path.canonicalize().unwrap_or_else(|_| path.to_owned());
    let identity = resolved.to_string_lossy().into_owned();
    if cfg!(windows) {
        identity.to_lowercase()
    } else {
        identity
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
    };

    use super::*;

    fn executable(path: &Path) {
        fs::write(path, b"fixture").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(path).unwrap().permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(path, permissions).unwrap();
        }
    }

    fn joined_path(paths: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(paths).unwrap()
    }

    #[test]
    fn parses_semver_with_whitespace_and_optional_v_prefix() {
        assert_eq!(
            parse_dsh_version("  v0.1.0-rc.6 \r\n").unwrap(),
            Version::parse("0.1.0-rc.6").unwrap()
        );
        assert_eq!(
            parse_dsh_version("1.2.3+build.4").unwrap(),
            Version::parse("1.2.3+build.4").unwrap()
        );
    }

    #[test]
    fn rejects_empty_multiline_and_decorated_versions() {
        assert_eq!(parse_dsh_version("  \n"), Err(VersionParseError::Empty));
        assert_eq!(
            parse_dsh_version("0.1.0\n0.2.0"),
            Err(VersionParseError::MultipleLines)
        );
        assert_eq!(
            parse_dsh_version("dsh 0.1.0"),
            Err(VersionParseError::Invalid("dsh 0.1.0".to_owned()))
        );
    }

    #[test]
    fn candidates_are_ordered_last_used_managed_then_path_and_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let cwd = temp.path();
        let last = cwd.join(command_name());
        let managed_dir = cwd.join("managed");
        let path_dir = cwd.join("path-bin");
        fs::create_dir_all(&managed_dir).unwrap();
        fs::create_dir_all(&path_dir).unwrap();
        let managed = managed_dir.join(command_name());
        let path_program = path_dir.join(command_name());
        executable(&last);
        executable(&managed);
        executable(&path_program);

        let path = joined_path(&[cwd, &path_dir]);
        let candidates = discover_candidates(
            Some(&last),
            Some(&managed),
            Some(&path),
            test_pathext(),
            cwd,
        );

        assert_eq!(
            candidates,
            vec![
                DshCandidate {
                    program: last,
                    source: CandidateSource::LastUsed,
                },
                DshCandidate {
                    program: managed,
                    source: CandidateSource::Managed,
                },
                DshCandidate {
                    program: path_program,
                    source: CandidateSource::Path,
                },
            ]
        );
    }

    #[test]
    fn ignores_stale_explicit_candidates_and_non_executable_path_entries() {
        let temp = tempfile::tempdir().unwrap();
        let path_dir = temp.path().join("bin");
        fs::create_dir_all(&path_dir).unwrap();
        fs::create_dir_all(path_dir.join(command_name())).unwrap();
        let path = joined_path(&[&path_dir]);

        let candidates = discover_candidates(
            Some(&temp.path().join("missing-dsh")),
            None,
            Some(&path),
            test_pathext(),
            temp.path(),
        );

        assert!(candidates.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn windows_pathext_finds_dsh_cmd_without_using_real_path() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let cmd = bin.join("dsh.cmd");
        executable(&cmd);
        let path = joined_path(&[&bin]);

        let candidates = discover_candidates(
            None,
            None,
            Some(&path),
            Some(OsStr::new(".EXE;.CMD")),
            temp.path(),
        );

        assert_eq!(
            candidates,
            vec![DshCandidate {
                program: cmd,
                source: CandidateSource::Path,
            }]
        );
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_requires_executable_permission() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        fs::create_dir_all(&bin).unwrap();
        let program = bin.join("dsh");
        fs::write(&program, b"fixture").unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o644)).unwrap();
        let path = joined_path(&[&bin]);

        assert!(discover_candidates(None, None, Some(&path), None, temp.path()).is_empty());
    }

    fn command_name() -> &'static str {
        if cfg!(windows) { "dsh.cmd" } else { "dsh" }
    }

    fn test_pathext() -> Option<&'static OsStr> {
        if cfg!(windows) {
            Some(OsStr::new(".CMD"))
        } else {
            None
        }
    }

    struct FakeProbe {
        outputs: BTreeMap<PathBuf, FakeOutput>,
        calls: Mutex<Vec<CommandSpec>>,
    }

    enum FakeOutput {
        Output(CommandOutput),
        Error,
    }

    #[async_trait]
    impl DshVersionProbe for FakeProbe {
        async fn run(
            &self,
            spec: &CommandSpec,
            _timeout: Duration,
        ) -> Result<CommandOutput, ProcessError> {
            self.calls.lock().unwrap().push(spec.clone());
            match self.outputs.get(&spec.program).unwrap() {
                FakeOutput::Output(output) => Ok(output.clone()),
                FakeOutput::Error => Err(ProcessError::MissingStatus),
            }
        }
    }

    fn output(status: i32, stdout: &str, stderr: &str) -> FakeOutput {
        FakeOutput::Output(CommandOutput {
            status,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        })
    }

    #[tokio::test]
    async fn probes_in_priority_order_and_selects_first_valid_version() {
        let last = PathBuf::from("last-dsh");
        let managed = PathBuf::from("managed-dsh");
        let path = PathBuf::from("path-dsh");
        let candidates = vec![
            DshCandidate {
                program: last.clone(),
                source: CandidateSource::LastUsed,
            },
            DshCandidate {
                program: managed.clone(),
                source: CandidateSource::Managed,
            },
            DshCandidate {
                program: path.clone(),
                source: CandidateSource::Path,
            },
        ];
        let probe = FakeProbe {
            outputs: BTreeMap::from([
                (last.clone(), output(0, "not-a-version", "")),
                (managed.clone(), output(0, "0.1.0-rc.6\n", "")),
                (path, output(0, "9.9.9", "")),
            ]),
            calls: Mutex::new(Vec::new()),
        };

        let report = discover_dsh_with(&probe, &candidates, Duration::from_secs(2)).await;

        assert_eq!(
            report.selected,
            Some(DiscoveredDsh {
                program: managed,
                version: Version::parse("0.1.0-rc.6").unwrap(),
                source: CandidateSource::Managed,
            })
        );
        assert_eq!(report.failures.len(), 1);
        let calls = probe.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| {
            call.args == ["--version"] && call.current_dir.is_none() && call.env.is_empty()
        }));
    }

    #[tokio::test]
    async fn reports_execution_exit_and_parse_failures_when_none_are_usable() {
        let programs = [
            PathBuf::from("io-error"),
            PathBuf::from("bad-exit"),
            PathBuf::from("bad-version"),
        ];
        let candidates: Vec<_> = programs
            .iter()
            .cloned()
            .map(|program| DshCandidate {
                program,
                source: CandidateSource::Path,
            })
            .collect();
        let probe = FakeProbe {
            outputs: BTreeMap::from([
                (programs[0].clone(), FakeOutput::Error),
                (programs[1].clone(), output(1, "", "broken")),
                (programs[2].clone(), output(0, "DSH v1", "")),
            ]),
            calls: Mutex::new(Vec::new()),
        };

        let report = discover_dsh_with(&probe, &candidates, Duration::from_secs(2)).await;

        assert_eq!(report.selected, None);
        assert!(matches!(
            report.failures[0].reason,
            CandidateFailureReason::Execution(_)
        ));
        assert_eq!(
            report.failures[1].reason,
            CandidateFailureReason::ExitStatus {
                status: 1,
                stderr: "broken".to_owned(),
            }
        );
        assert!(matches!(
            report.failures[2].reason,
            CandidateFailureReason::InvalidVersion(_)
        ));
    }
}
