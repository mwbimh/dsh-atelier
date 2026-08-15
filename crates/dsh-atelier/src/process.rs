use std::{collections::BTreeMap, path::PathBuf, process::Stdio, time::Duration};

use thiserror::Error;
use tokio::{process::Command, time};

#[cfg(target_os = "macos")]
use std::os::unix::process::CommandExt as _;

#[cfg(windows)]
pub(crate) const WINDOWS_CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub current_dir: Option<PathBuf>,
    pub env: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Error)]
pub enum ProcessError {
    #[error("command timed out after {0:?}")]
    Timeout(Duration),
    #[error("failed to execute command: {0}")]
    Io(#[from] std::io::Error),
    #[error("command exited without a numeric status")]
    MissingStatus,
}

/// Runs a short-lived command and captures its output.
///
/// Arguments, environment overrides and the working directory remain structured
/// until process creation. On Windows, the standard library recognizes resolved
/// `.cmd` and `.bat` paths, invokes `cmd.exe`, and applies its dedicated batch
/// argument escaping rather than interpolating arguments into a shell string.
pub async fn run_once(
    spec: &CommandSpec,
    timeout: Duration,
) -> Result<CommandOutput, ProcessError> {
    let mut command = command_for(spec)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .envs(&spec.env);

    if let Some(current_dir) = &spec.current_dir {
        command.current_dir(current_dir);
    }

    let child = command.spawn()?;
    #[cfg(target_os = "macos")]
    let process_group = child.id();
    #[cfg(windows)]
    let _job = {
        let job = crate::windows_job::WindowsJob::create()?;
        job.assign(&child)?;
        job
    };
    let output = match time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => result?,
        Err(_) => {
            #[cfg(target_os = "macos")]
            if let Some(process_group) = process_group {
                let _ = signal_process_group(process_group, 9);
            }
            return Err(ProcessError::Timeout(timeout));
        }
    };
    let status = output.status.code().ok_or(ProcessError::MissingStatus)?;

    Ok(CommandOutput {
        status,
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn command_for(spec: &CommandSpec) -> Result<Command, ProcessError> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    #[cfg(windows)]
    command.creation_flags(WINDOWS_CREATE_NO_WINDOW);
    #[cfg(target_os = "macos")]
    command.as_std_mut().process_group(0);
    Ok(command)
}

#[cfg(target_os = "macos")]
fn signal_process_group(process_group: u32, signal: i32) -> std::io::Result<()> {
    unsafe extern "C" {
        fn kill(process: i32, signal: i32) -> i32;
    }

    let process_group = i32::try_from(process_group)
        .map_err(|_| std::io::Error::other("process group id exceeds i32"))?;
    // SAFETY: `kill` receives a negative child process-group id and a standard
    // signal number. It does not retain pointers or access Rust memory.
    if unsafe { kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(3) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf, time::Duration};

    #[cfg(windows)]
    use std::env;
    #[cfg(windows)]
    use std::fs;

    use tempfile::tempdir;

    use super::{CommandSpec, ProcessError, run_once};

    #[cfg(windows)]
    const CONSOLE_HELPER_ENV: &str = "ATELIER_CONSOLE_HELPER";

    #[cfg(windows)]
    #[test]
    fn console_presence_helper() {
        if env::var_os(CONSOLE_HELPER_ENV).is_none() {
            return;
        }
        let window = unsafe { windows::Win32::System::Console::GetConsoleWindow() };
        println!("ATELIER_HAS_CONSOLE={}", !window.is_invalid());
    }

    #[cfg(windows)]
    fn shell_program() -> PathBuf {
        PathBuf::from(std::env::var_os("ComSpec").unwrap_or_else(|| "cmd.exe".into()))
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn runs_a_command_with_structured_args_env_and_cwd() {
        let working_dir = tempdir().expect("temporary working directory");
        let mut env = BTreeMap::new();
        env.insert("ATELIER_PROCESS_TEST".into(), "structured value".into());
        let spec = CommandSpec {
            program: shell_program(),
            args: vec![
                "/d".into(),
                "/s".into(),
                "/c".into(),
                "echo [%ATELIER_PROCESS_TEST%] && cd".into(),
            ],
            current_dir: Some(working_dir.path().to_owned()),
            env,
        };

        let output = run_once(&spec, Duration::from_secs(5))
            .await
            .expect("command should run");

        assert_eq!(output.status, 0);
        let normalized_stdout = output.stdout.replace('\\', "/").to_lowercase();
        assert!(normalized_stdout.contains("[structured value]"));
        assert!(
            normalized_stdout.contains(
                &working_dir
                    .path()
                    .display()
                    .to_string()
                    .replace('\\', "/")
                    .to_lowercase()
            )
        );
        assert!(output.stderr.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_a_command_with_structured_args_env_and_cwd() {
        let working_dir = tempdir().expect("temporary working directory");
        let mut env = BTreeMap::new();
        env.insert("ATELIER_PROCESS_TEST".into(), "structured value".into());
        let spec = CommandSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                "printf '[%s]\\n%s' \"$ATELIER_PROCESS_TEST\" \"$PWD\"".into(),
            ],
            current_dir: Some(working_dir.path().to_owned()),
            env,
        };

        let output = run_once(&spec, Duration::from_secs(5))
            .await
            .expect("command should run");

        assert_eq!(output.status, 0);
        assert!(output.stdout.contains("[structured value]"));
        assert!(
            output
                .stdout
                .contains(&working_dir.path().display().to_string())
        );
        assert!(output.stderr.is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn terminates_a_timed_out_command() {
        let spec = CommandSpec {
            program: PathBuf::from("ping.exe"),
            args: vec!["127.0.0.1".into(), "-n".into(), "6".into()],
            current_dir: None,
            env: BTreeMap::new(),
        };
        let timeout = Duration::from_millis(25);

        let error = run_once(&spec, timeout)
            .await
            .expect_err("command should time out");

        assert!(matches!(error, ProcessError::Timeout(value) if value == timeout));
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn timed_out_command_cannot_leave_a_descendant_running() {
        let directory = tempdir().unwrap();
        let marker = directory.path().join("descendant-survived.txt");
        let spec = CommandSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec![
                "-c".into(),
                "(sleep 0.2; printf survived > \"$1\") & sleep 5".into(),
                "atelier-test".into(),
                marker.to_string_lossy().into_owned(),
            ],
            current_dir: None,
            env: BTreeMap::new(),
        };

        assert!(matches!(
            run_once(&spec, Duration::from_millis(25)).await,
            Err(ProcessError::Timeout(_))
        ));
        tokio::time::sleep(Duration::from_millis(350)).await;

        assert!(!marker.exists(), "a timed-out descendant kept running");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn terminates_a_timed_out_command() {
        let spec = CommandSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), "sleep 5".into()],
            current_dir: None,
            env: BTreeMap::new(),
        };
        let timeout = Duration::from_millis(25);

        let error = run_once(&spec, timeout)
            .await
            .expect_err("command should time out");

        assert!(matches!(error, ProcessError::Timeout(value) if value == timeout));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn invokes_cmd_shims_without_interpreting_argument_metacharacters() {
        let directory = tempdir().expect("temporary directory");
        let shim = directory.path().join("safe shim.cmd");
        let marker = directory.path().join("injected.txt");
        fs::write(&shim, "@echo off\r\n<nul set /p =shim-ok\r\nexit /b 7\r\n")
            .expect("write cmd shim");
        let injection = format!(
            "spaces \\\"quoted\\\" & echo injected>\\\"{}\\\" | < > ^ %PATH% !bang!",
            marker.display()
        );
        let spec = CommandSpec {
            program: shim,
            args: vec![injection],
            current_dir: Some(directory.path().to_owned()),
            env: BTreeMap::new(),
        };

        let output = run_once(&spec, Duration::from_secs(5))
            .await
            .expect("cmd shim should run");

        assert_eq!(output.status, 7);
        assert_eq!(output.stdout, "shim-ok");
        assert!(!marker.exists(), "argument escaped the cmd invocation");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn launches_console_tools_without_a_console_window() {
        let spec = CommandSpec {
            program: env::current_exe().unwrap(),
            args: vec![
                "--exact".into(),
                "process::tests::console_presence_helper".into(),
                "--nocapture".into(),
            ],
            current_dir: None,
            env: BTreeMap::from([(CONSOLE_HELPER_ENV.into(), "1".into())]),
        };

        let output = run_once(&spec, Duration::from_secs(5)).await.unwrap();

        assert!(
            output.stdout.contains("ATELIER_HAS_CONSOLE=false"),
            "child unexpectedly owned a console: {}",
            output.stdout
        );
    }
}
