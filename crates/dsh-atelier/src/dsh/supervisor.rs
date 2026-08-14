use std::{
    collections::VecDeque,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use reqwest::redirect::Policy;
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::mpsc,
    task::JoinHandle,
    time,
};

use crate::{
    dsh::readiness::{LoopbackUrl, ReadinessError, parse_readiness_line},
    process::CommandSpec,
};

#[cfg(windows)]
use crate::process::WINDOWS_CREATE_NO_WINDOW;

const OUTPUT_LINE_LIMIT: usize = 512;
#[cfg(windows)]
const WINDOWS_JOB_STOP_CODE: u32 = 0xA7E1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DshState {
    Stopped,
    Starting,
    Ready { url: LoopbackUrl },
    Failed { message: String },
    Crashed { code: Option<i32> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisorOptions {
    pub readiness_timeout: Duration,
    pub http_timeout: Duration,
    pub stop_timeout: Duration,
}

impl Default for SupervisorOptions {
    fn default() -> Self {
        Self {
            readiness_timeout: Duration::from_secs(30),
            http_timeout: Duration::from_secs(5),
            stop_timeout: Duration::from_secs(5),
        }
    }
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("DSH is already running")]
    AlreadyRunning,
    #[error("failed to spawn DSH: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("DSH stdout was not captured")]
    MissingStdout,
    #[error("DSH stderr was not captured")]
    MissingStderr,
    #[error("DSH readiness timed out after {0:?}")]
    ReadinessTimeout(Duration),
    #[error("DSH exited before it became ready (code: {code:?})")]
    ExitedBeforeReady { code: Option<i32> },
    #[error("DSH emitted an invalid readiness signal: {0}")]
    InvalidReadiness(#[source] ReadinessError),
    #[error("failed to build the DSH health-check client: {0}")]
    HttpClient(#[source] reqwest::Error),
    #[error("DSH readiness endpoint did not become healthy: {0}")]
    HealthCheck(String),
    #[error("failed to inspect DSH: {0}")]
    Inspect(#[source] std::io::Error),
    #[error("failed to stop DSH: {0}")]
    Stop(#[source] std::io::Error),
    #[cfg(windows)]
    #[error("failed to create or assign the DSH Job Object: {0}")]
    WindowsJobSetup(#[source] std::io::Error),
    #[error("DSH did not stop after {0:?}")]
    StopTimeout(Duration),
}

pub struct DshSupervisor {
    spec: CommandSpec,
    options: SupervisorOptions,
    state: DshState,
    child: Option<Child>,
    #[cfg(windows)]
    windows_job: Option<crate::windows_job::WindowsJob>,
    stdout_lines: Arc<Mutex<VecDeque<String>>>,
    stderr_lines: Arc<Mutex<VecDeque<String>>>,
    readiness_rx: Option<mpsc::UnboundedReceiver<String>>,
    output_tasks: Vec<JoinHandle<()>>,
}

impl DshSupervisor {
    #[must_use]
    pub fn new(spec: CommandSpec, options: SupervisorOptions) -> Self {
        Self {
            spec,
            options,
            state: DshState::Stopped,
            child: None,
            #[cfg(windows)]
            windows_job: None,
            stdout_lines: Arc::new(Mutex::new(VecDeque::new())),
            stderr_lines: Arc::new(Mutex::new(VecDeque::new())),
            readiness_rx: None,
            output_tasks: Vec::new(),
        }
    }

    #[must_use]
    pub fn state(&self) -> &DshState {
        &self.state
    }

    #[must_use]
    pub fn stdout_lines(&self) -> Vec<String> {
        self.stdout_lines
            .lock()
            .expect("stdout log lock poisoned")
            .iter()
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn stderr_lines(&self) -> Vec<String> {
        self.stderr_lines
            .lock()
            .expect("stderr log lock poisoned")
            .iter()
            .cloned()
            .collect()
    }

    pub async fn start(&mut self) -> Result<LoopbackUrl, SupervisorError> {
        if self.child.is_some() || matches!(self.state, DshState::Starting | DshState::Ready { .. })
        {
            return Err(SupervisorError::AlreadyRunning);
        }

        self.clear_output_tasks();
        self.stdout_lines
            .lock()
            .expect("stdout log lock poisoned")
            .clear();
        self.stderr_lines
            .lock()
            .expect("stderr log lock poisoned")
            .clear();
        self.state = DshState::Starting;

        #[cfg(windows)]
        if let Some(job) = self.windows_job.take() {
            let _ = job.terminate(WINDOWS_JOB_STOP_CODE);
        }

        if let Err(error) = self.spawn().await {
            return Err(self.fail(error));
        }

        match self.wait_until_ready().await {
            Ok(url) => {
                // Readiness is a one-shot signal. Dropping the receiver avoids
                // retaining an unbounded duplicate of long-running stdout.
                self.readiness_rx = None;
                self.state = DshState::Ready { url: url.clone() };
                Ok(url)
            }
            Err(error) => {
                self.force_stop_after_failed_start().await;
                Err(self.fail(error))
            }
        }
    }

    /// Reaps an exited child and updates lifecycle state without waiting.
    pub fn poll(&mut self) -> Result<DshState, SupervisorError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(self.state.clone());
        };
        let Some(status) = child.try_wait().map_err(SupervisorError::Inspect)? else {
            return Ok(self.state.clone());
        };

        self.child = None;
        self.clear_output_tasks();
        self.state = if matches!(self.state, DshState::Ready { .. }) {
            DshState::Crashed {
                code: status.code(),
            }
        } else {
            DshState::Failed {
                message: format!("DSH exited while starting (code: {:?})", status.code()),
            }
        };
        Ok(self.state.clone())
    }

    /// Stops the current DSH child. This is force-only on Windows until the
    /// process adapter supplies the planned in-process stop hook and Job Object.
    pub async fn stop(&mut self) -> Result<(), SupervisorError> {
        let mut child = self.child.take();
        #[cfg(windows)]
        let job = self.windows_job.take();

        if child.is_none() {
            #[cfg(windows)]
            if let Some(job) = job {
                job.terminate(WINDOWS_JOB_STOP_CODE)
                    .map_err(SupervisorError::Stop)?;
            }
            self.state = DshState::Stopped;
            self.clear_output_tasks();
            return Ok(());
        }
        let child = child.as_mut().expect("checked child exists");

        #[cfg(windows)]
        let job_termination = if let Some(job) = job.as_ref() {
            job.terminate(WINDOWS_JOB_STOP_CODE)
        } else {
            Ok(())
        };

        if child.try_wait().map_err(SupervisorError::Stop)?.is_none() {
            #[cfg(not(windows))]
            child.start_kill().map_err(SupervisorError::Stop)?;
            #[cfg(windows)]
            if job.is_none() || job_termination.is_err() {
                child.start_kill().map_err(SupervisorError::Stop)?;
            }
            match time::timeout(self.options.stop_timeout, child.wait()).await {
                Ok(result) => {
                    result.map_err(SupervisorError::Stop)?;
                }
                Err(_) => {
                    self.clear_output_tasks();
                    self.state = DshState::Failed {
                        message: SupervisorError::StopTimeout(self.options.stop_timeout)
                            .to_string(),
                    };
                    return Err(SupervisorError::StopTimeout(self.options.stop_timeout));
                }
            }
        }

        #[cfg(windows)]
        if let Err(error) = job_termination {
            self.clear_output_tasks();
            self.state = DshState::Failed {
                message: format!("failed to stop DSH: {error}"),
            };
            return Err(SupervisorError::Stop(error));
        }

        self.clear_output_tasks();
        self.state = DshState::Stopped;
        Ok(())
    }

    async fn spawn(&mut self) -> Result<(), SupervisorError> {
        let mut command = Command::new(&self.spec.program);
        command
            .args(&self.spec.args)
            .envs(&self.spec.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(WINDOWS_CREATE_NO_WINDOW);
        if let Some(current_dir) = &self.spec.current_dir {
            command.current_dir(current_dir);
        }

        #[cfg(windows)]
        let job =
            crate::windows_job::WindowsJob::create().map_err(SupervisorError::WindowsJobSetup)?;
        let mut child = command.spawn().map_err(SupervisorError::Spawn)?;
        #[cfg(windows)]
        if let Err(error) = job.assign(&child) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(SupervisorError::WindowsJobSetup(error));
        }
        let stdout = child.stdout.take().ok_or(SupervisorError::MissingStdout)?;
        let stderr = child.stderr.take().ok_or(SupervisorError::MissingStderr)?;
        let (readiness_tx, readiness_rx) = mpsc::unbounded_channel();

        let stdout_lines = Arc::clone(&self.stdout_lines);
        self.output_tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                record_line(&stdout_lines, line.clone());
                let _ = readiness_tx.send(line);
            }
        }));
        let stderr_lines = Arc::clone(&self.stderr_lines);
        self.output_tasks.push(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                record_line(&stderr_lines, line);
            }
        }));

        self.child = Some(child);
        #[cfg(windows)]
        {
            self.windows_job = Some(job);
        }
        self.readiness_rx = Some(readiness_rx);
        Ok(())
    }

    async fn wait_until_ready(&mut self) -> Result<LoopbackUrl, SupervisorError> {
        let deadline = Instant::now() + self.options.readiness_timeout;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .connect_timeout(self.options.http_timeout)
            .timeout(self.options.http_timeout)
            .build()
            .map_err(SupervisorError::HttpClient)?;
        let mut advertised_url: Option<LoopbackUrl> = None;
        let mut last_health_error = None;

        loop {
            if let Some(status) = self
                .child
                .as_mut()
                .expect("child exists while starting")
                .try_wait()
                .map_err(SupervisorError::Inspect)?
            {
                self.child = None;
                return Err(SupervisorError::ExitedBeforeReady {
                    code: status.code(),
                });
            }

            if let Some(url) = advertised_url.as_ref() {
                match client.get(url.as_url().clone()).send().await {
                    Ok(response) if response.status().is_success() => return Ok(url.clone()),
                    Ok(response) => last_health_error = Some(format!("HTTP {}", response.status())),
                    Err(error) => last_health_error = Some(error.to_string()),
                }
            }

            let now = Instant::now();
            if now >= deadline {
                return if advertised_url.is_some() {
                    Err(SupervisorError::HealthCheck(
                        last_health_error.unwrap_or_else(|| {
                            "readiness endpoint did not return a successful response".to_owned()
                        }),
                    ))
                } else {
                    Err(SupervisorError::ReadinessTimeout(
                        self.options.readiness_timeout,
                    ))
                };
            }

            let wait = (deadline - now).min(Duration::from_millis(100));
            let (line, stream_closed) = if let Some(receiver) = self.readiness_rx.as_mut() {
                match time::timeout(wait, receiver.recv()).await {
                    Ok(Some(line)) => (Some(line), false),
                    Ok(None) => (None, true),
                    Err(_) => (None, false),
                }
            } else {
                time::sleep(wait).await;
                (None, false)
            };
            if stream_closed {
                // A child may close stdout yet remain alive. Stop polling a
                // closed channel in a hot loop while the startup deadline runs.
                self.readiness_rx = None;
            }
            if let Some(line) = line {
                match parse_readiness_line(&line) {
                    Ok(Some(url)) => advertised_url = Some(url),
                    Ok(None) => {}
                    Err(error) => return Err(SupervisorError::InvalidReadiness(error)),
                }
            }
        }
    }

    async fn force_stop_after_failed_start(&mut self) {
        if let Some(mut child) = self.child.take() {
            #[cfg(windows)]
            let terminated_job = self
                .windows_job
                .as_ref()
                .is_some_and(|job| job.terminate(WINDOWS_JOB_STOP_CODE).is_ok());
            #[cfg(not(windows))]
            let terminated_job = false;
            if !terminated_job {
                let _ = child.start_kill();
            }
            let _ = time::timeout(self.options.stop_timeout, child.wait()).await;
        }
        #[cfg(windows)]
        {
            self.windows_job = None;
        }
        self.clear_output_tasks();
    }

    fn fail(&mut self, error: SupervisorError) -> SupervisorError {
        self.state = DshState::Failed {
            message: error.to_string(),
        };
        error
    }

    fn clear_output_tasks(&mut self) {
        self.readiness_rx = None;
        for task in self.output_tasks.drain(..) {
            task.abort();
        }
    }
}

fn record_line(lines: &Mutex<VecDeque<String>>, line: String) {
    let mut lines = lines.lock().expect("output log lock poisoned");
    if lines.len() >= OUTPUT_LINE_LIMIT {
        lines.pop_front();
    }
    lines.push_back(line);
}

impl Drop for DshSupervisor {
    fn drop(&mut self) {
        #[cfg(windows)]
        let terminated_job = self
            .windows_job
            .as_ref()
            .is_some_and(|job| job.terminate(WINDOWS_JOB_STOP_CODE).is_ok());
        #[cfg(not(windows))]
        let terminated_job = false;
        if let Some(child) = self.child.as_mut()
            && !terminated_job
        {
            let _ = child.start_kill();
        }
        #[cfg(windows)]
        if let Some(child) = self.child.as_mut() {
            let deadline = Instant::now() + self.options.stop_timeout;
            while matches!(child.try_wait(), Ok(None)) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        #[cfg(windows)]
        {
            self.windows_job = None;
        }
        self.clear_output_tasks();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        env, fs,
        io::{Read, Write},
        net::TcpListener,
        process, thread,
    };

    use super::*;

    const HELPER_ENV: &str = "ATELIER_SUPERVISOR_HELPER";
    const HELPER_MARKER_ENV: &str = "ATELIER_SUPERVISOR_HELPER_MARKER";

    fn helper_spec(mode: &str) -> CommandSpec {
        CommandSpec {
            program: env::current_exe().unwrap(),
            args: vec![
                "--exact".into(),
                "dsh::supervisor::tests::fake_dsh_helper".into(),
                "--nocapture".into(),
            ],
            current_dir: None,
            env: BTreeMap::from([(HELPER_ENV.into(), mode.into())]),
        }
    }

    fn short_options() -> SupervisorOptions {
        SupervisorOptions {
            readiness_timeout: Duration::from_secs(3),
            http_timeout: Duration::from_millis(300),
            stop_timeout: Duration::from_secs(2),
        }
    }

    #[test]
    fn fake_dsh_helper() {
        let Ok(mode) = env::var(HELPER_ENV) else {
            return;
        };
        if mode == "tree-grandchild" {
            thread::sleep(Duration::from_millis(750));
            fs::write(env::var_os(HELPER_MARKER_ENV).unwrap(), b"survived").unwrap();
            process::exit(0);
        }
        if mode == "exit-before-ready" {
            eprintln!("fake DSH exited before readiness");
            process::exit(19);
        }

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        eprintln!("fake DSH stderr marker");
        if mode == "delayed-ready" {
            thread::sleep(Duration::from_millis(150));
        }
        if mode == "invalid-url" {
            println!("\ndsh web: http://example.invalid:{port}");
            thread::sleep(Duration::from_secs(10));
            return;
        }
        println!("\ndsh web: http://127.0.0.1:{port}");
        std::io::stdout().flush().unwrap();

        if mode == "tree-parent" {
            let marker = env::var_os(HELPER_MARKER_ENV).unwrap();
            let mut grandchild = process::Command::new(env::current_exe().unwrap())
                .args([
                    "--exact",
                    "dsh::supervisor::tests::fake_dsh_helper",
                    "--nocapture",
                ])
                .env(HELPER_ENV, "tree-grandchild")
                .env(HELPER_MARKER_ENV, marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            thread::spawn(move || {
                let _ = grandchild.wait();
            });
        }
        if mode == "crash" {
            thread::spawn(|| {
                thread::sleep(Duration::from_millis(350));
                process::exit(23);
            });
        }
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            if mode == "http-error" {
                stream
                    .write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
            } else {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                    )
                    .unwrap();
            }
        }
    }

    #[test]
    fn starts_stopped_with_bounded_production_timeouts() {
        let supervisor = DshSupervisor::new(helper_spec("normal"), SupervisorOptions::default());

        assert_eq!(supervisor.state(), &DshState::Stopped);
        assert_eq!(
            SupervisorOptions::default(),
            SupervisorOptions {
                readiness_timeout: Duration::from_secs(30),
                http_timeout: Duration::from_secs(5),
                stop_timeout: Duration::from_secs(5),
            }
        );
    }

    #[test]
    fn retains_only_the_most_recent_output_lines() {
        let lines = Mutex::new(VecDeque::new());

        for index in 0..OUTPUT_LINE_LIMIT + 3 {
            record_line(&lines, index.to_string());
        }

        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), OUTPUT_LINE_LIMIT);
        assert_eq!(lines.front().unwrap(), "3");
        assert_eq!(lines.back().unwrap(), &(OUTPUT_LINE_LIMIT + 2).to_string());
    }

    #[tokio::test]
    async fn starts_long_lived_dsh_reads_output_and_verifies_http() {
        let mut supervisor = DshSupervisor::new(helper_spec("normal"), short_options());

        let url = supervisor.start().await.unwrap();

        assert!(
            matches!(supervisor.state(), DshState::Ready { url: state_url } if state_url == &url)
        );
        assert!(
            supervisor
                .stdout_lines()
                .iter()
                .any(|line| line == &format!("dsh web: {}", url.as_str().trim_end_matches('/')))
        );
        assert!(
            supervisor
                .stderr_lines()
                .iter()
                .any(|line| line == "fake DSH stderr marker")
        );
        assert_eq!(supervisor.poll().unwrap(), supervisor.state().clone());
        supervisor.stop().await.unwrap();
        assert_eq!(supervisor.state(), &DshState::Stopped);
    }

    #[tokio::test]
    async fn rejects_a_second_start_while_ready() {
        let mut supervisor = DshSupervisor::new(helper_spec("normal"), short_options());
        supervisor.start().await.unwrap();

        assert!(matches!(
            supervisor.start().await,
            Err(SupervisorError::AlreadyRunning)
        ));
        supervisor.stop().await.unwrap();
    }

    #[tokio::test]
    async fn fails_on_an_unsafe_readiness_url_and_reaps_the_child() {
        let mut supervisor = DshSupervisor::new(helper_spec("invalid-url"), short_options());

        assert!(matches!(
            supervisor.start().await,
            Err(SupervisorError::InvalidReadiness(_))
        ));
        assert!(matches!(supervisor.state(), DshState::Failed { .. }));
        assert!(supervisor.child.is_none());
    }

    #[tokio::test]
    async fn fails_and_reaps_the_child_when_readiness_times_out() {
        let options = SupervisorOptions {
            readiness_timeout: Duration::from_millis(40),
            ..short_options()
        };
        let mut supervisor = DshSupervisor::new(helper_spec("delayed-ready"), options);

        assert!(matches!(
            supervisor.start().await,
            Err(SupervisorError::ReadinessTimeout(duration))
                if duration == Duration::from_millis(40)
        ));
        assert!(matches!(supervisor.state(), DshState::Failed { .. }));
        assert!(supervisor.child.is_none());
    }

    #[tokio::test]
    async fn reports_an_exit_before_readiness_as_failed() {
        let mut supervisor = DshSupervisor::new(helper_spec("exit-before-ready"), short_options());

        assert!(matches!(
            supervisor.start().await,
            Err(SupervisorError::ExitedBeforeReady { code: Some(19) })
        ));
        assert!(matches!(supervisor.state(), DshState::Failed { .. }));
        assert!(
            supervisor
                .stderr_lines()
                .iter()
                .any(|line| line.contains("exited before readiness"))
        );
    }

    #[tokio::test]
    async fn reports_an_unexpected_exit_after_ready_as_crashed() {
        let mut supervisor = DshSupervisor::new(helper_spec("crash"), short_options());
        supervisor.start().await.unwrap();
        time::sleep(Duration::from_millis(500)).await;

        assert_eq!(
            supervisor.poll().unwrap(),
            DshState::Crashed { code: Some(23) }
        );
    }

    #[tokio::test]
    async fn refuses_to_mark_a_non_successful_http_endpoint_ready() {
        let options = SupervisorOptions {
            readiness_timeout: Duration::from_millis(450),
            ..short_options()
        };
        let mut supervisor = DshSupervisor::new(helper_spec("http-error"), options);

        assert!(matches!(
            supervisor.start().await,
            Err(SupervisorError::HealthCheck(message)) if message.contains("503")
        ));
        assert!(matches!(supervisor.state(), DshState::Failed { .. }));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn stop_terminates_the_complete_windows_process_tree() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("grandchild-survived.txt");
        let mut spec = helper_spec("tree-parent");
        spec.env
            .insert(HELPER_MARKER_ENV.into(), path_as_environment_value(&marker));
        let mut supervisor = DshSupervisor::new(spec, short_options());
        supervisor.start().await.unwrap();

        supervisor.stop().await.unwrap();
        time::sleep(Duration::from_secs(1)).await;

        assert!(!marker.exists(), "a DSH descendant escaped the Job Object");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn drop_terminates_and_reaps_the_windows_process_tree() {
        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("grandchild-survived-drop.txt");
        let mut spec = helper_spec("tree-parent");
        spec.env
            .insert(HELPER_MARKER_ENV.into(), path_as_environment_value(&marker));
        let mut supervisor = DshSupervisor::new(spec, short_options());
        supervisor.start().await.unwrap();

        drop(supervisor);
        time::sleep(Duration::from_secs(1)).await;

        assert!(!marker.exists(), "a DSH descendant escaped Drop cleanup");
    }

    #[cfg(windows)]
    fn path_as_environment_value(path: &std::path::Path) -> String {
        path.to_str().unwrap().to_owned()
    }
}
