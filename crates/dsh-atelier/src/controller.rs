use std::{future::pending, pin::Pin, time::Duration};

use async_trait::async_trait;
use thiserror::Error;
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::{Instant, Sleep},
};

use crate::{
    config::{Config, FirstLaunch},
    dsh::readiness::LoopbackUrl,
};

const CONTROLLER_CHANNEL_CAPACITY: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchKind {
    Explicit,
    Login,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresentationTarget {
    SurfaceDsh,
    Web,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StartupDecision {
    pub start_dsh: bool,
    pub presentation: Option<PresentationTarget>,
}

#[must_use]
pub fn startup_decision(config: &Config, launch_kind: LaunchKind) -> StartupDecision {
    match launch_kind {
        LaunchKind::Explicit => {
            let presentation = match config.dsh.first_launch {
                FirstLaunch::SurfaceDsh => Some(PresentationTarget::SurfaceDsh),
                FirstLaunch::Web => Some(PresentationTarget::Web),
                FirstLaunch::None => None,
            };
            StartupDecision {
                start_dsh: config.dsh.auto_start || presentation.is_some(),
                presentation,
            }
        }
        LaunchKind::Login => StartupDecision {
            start_dsh: config.dsh.auto_start,
            presentation: None,
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RestartPolicy {
    pub maximum_attempts: u8,
    pub initial_delay: Duration,
    pub stable_runtime: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            maximum_attempts: 5,
            initial_delay: Duration::from_secs(1),
            stable_runtime: Duration::from_secs(5 * 60),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartDecision {
    Retry { attempt: u8, after: Duration },
    Exhausted { attempts: u8 },
}

#[derive(Clone, Debug)]
pub struct RestartTracker {
    policy: RestartPolicy,
    attempts: u8,
}

impl RestartTracker {
    #[must_use]
    pub const fn new(policy: RestartPolicy) -> Self {
        Self {
            policy,
            attempts: 0,
        }
    }

    #[must_use]
    pub const fn attempts(&self) -> u8 {
        self.attempts
    }

    pub fn reset(&mut self) {
        self.attempts = 0;
    }

    pub fn on_unexpected_exit(&mut self, runtime: Duration) -> RestartDecision {
        if runtime >= self.policy.stable_runtime {
            self.reset();
        }

        if self.attempts >= self.policy.maximum_attempts {
            return RestartDecision::Exhausted {
                attempts: self.attempts,
            };
        }

        self.attempts += 1;
        let exponent = u32::from(self.attempts.saturating_sub(1));
        let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
        RestartDecision::Retry {
            attempt: self.attempts,
            after: self.policy.initial_delay.saturating_mul(multiplier),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerCommand {
    Start,
    OpenSurface,
    OpenBrowser,
    Stop,
    Restart,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControllerPhase {
    Stopped,
    Starting,
    Running,
    RestartBackoff,
    Failed,
    ShuttingDown,
    Shutdown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControllerSnapshot {
    pub phase: ControllerPhase,
    pub web_url: Option<LoopbackUrl>,
    pub restart_attempts: u8,
    pub restart_after: Option<Duration>,
    pub last_error: Option<String>,
}

impl Default for ControllerSnapshot {
    fn default() -> Self {
        Self {
            phase: ControllerPhase::Stopped,
            web_url: None,
            restart_attempts: 0,
            restart_after: None,
            last_error: None,
        }
    }
}

#[async_trait]
pub trait ControllerServices: Send + 'static {
    /// Discovers or installs DSH, starts it and returns its validated readiness URL.
    async fn start_dsh(&mut self) -> anyhow::Result<LoopbackUrl>;

    /// Stops the complete DSH process tree. Calling this while stopped must be safe.
    async fn stop_dsh(&mut self) -> anyhow::Result<()>;

    async fn show_surface_loading(&mut self) -> anyhow::Result<()>;

    async fn show_surface(&mut self, url: &LoopbackUrl) -> anyhow::Result<()>;

    async fn open_web(&mut self, url: &LoopbackUrl) -> anyhow::Result<()>;

    async fn notify(&mut self, title: &str, message: &str) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct ControllerHandle {
    sender: mpsc::Sender<ControllerMessage>,
    state: watch::Receiver<ControllerSnapshot>,
}

impl ControllerHandle {
    pub async fn apply_startup(
        &self,
        config: &Config,
        launch_kind: LaunchKind,
    ) -> Result<ControllerSnapshot, ControllerError> {
        let decision = startup_decision(config, launch_kind);
        match decision.presentation {
            Some(PresentationTarget::SurfaceDsh) => {
                self.command(ControllerCommand::OpenSurface).await
            }
            Some(PresentationTarget::Web) => self.command(ControllerCommand::OpenBrowser).await,
            None if decision.start_dsh => self.command(ControllerCommand::Start).await,
            None => Ok(self.snapshot()),
        }
    }

    pub async fn command(
        &self,
        command: ControllerCommand,
    ) -> Result<ControllerSnapshot, ControllerError> {
        self.request(ControllerRequest::Command(command)).await
    }

    /// Reports a supervisor-observed exit. `runtime` is the duration of the
    /// process instance that exited, not the age of the controller.
    pub async fn report_unexpected_exit(
        &self,
        runtime: Duration,
        detail: impl Into<String>,
    ) -> Result<ControllerSnapshot, ControllerError> {
        self.request(ControllerRequest::UnexpectedExit {
            runtime,
            detail: detail.into(),
        })
        .await
    }

    #[must_use]
    pub fn snapshot(&self) -> ControllerSnapshot {
        self.state.borrow().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<ControllerSnapshot> {
        self.state.clone()
    }

    async fn request(
        &self,
        request: ControllerRequest,
    ) -> Result<ControllerSnapshot, ControllerError> {
        let (response_sender, response_receiver) = oneshot::channel();
        self.sender
            .send(ControllerMessage {
                request,
                response: response_sender,
            })
            .await
            .map_err(|_| ControllerError::Unavailable)?;
        response_receiver
            .await
            .map_err(|_| ControllerError::Unavailable)?
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ControllerError {
    #[error("the controller is unavailable")]
    Unavailable,
    #[error("{operation} failed: {message}")]
    Operation {
        operation: &'static str,
        message: String,
    },
}

enum ControllerRequest {
    Command(ControllerCommand),
    UnexpectedExit { runtime: Duration, detail: String },
}

struct ControllerMessage {
    request: ControllerRequest,
    response: oneshot::Sender<Result<ControllerSnapshot, ControllerError>>,
}

pub struct Controller;

impl Controller {
    #[must_use]
    pub fn spawn<S>(services: S, restart_policy: RestartPolicy) -> ControllerHandle
    where
        S: ControllerServices,
    {
        let (sender, receiver) = mpsc::channel(CONTROLLER_CHANNEL_CAPACITY);
        let (state_sender, state) = watch::channel(ControllerSnapshot::default());
        let actor = ControllerActor {
            services,
            receiver,
            state_sender,
            state: ControllerSnapshot::default(),
            restart_tracker: RestartTracker::new(restart_policy),
            retry_sleep: None,
        };
        tokio::spawn(actor.run());
        ControllerHandle { sender, state }
    }
}

struct ControllerActor<S> {
    services: S,
    receiver: mpsc::Receiver<ControllerMessage>,
    state_sender: watch::Sender<ControllerSnapshot>,
    state: ControllerSnapshot,
    restart_tracker: RestartTracker,
    retry_sleep: Option<Pin<Box<Sleep>>>,
}

impl<S: ControllerServices> ControllerActor<S> {
    async fn run(mut self) {
        loop {
            tokio::select! {
                message = self.receiver.recv() => {
                    let Some(message) = message else {
                        self.stop_for_shutdown().await;
                        break;
                    };
                    let (result, shutdown) = self.handle_request(message.request).await;
                    let _ = message.response.send(result);
                    if shutdown {
                        break;
                    }
                }
                () = Self::wait_for_retry(&mut self.retry_sleep), if self.retry_sleep.is_some() => {
                    self.retry_sleep = None;
                    if let Err(error) = self.start(None).await {
                        self.schedule_restart(Duration::ZERO, error.to_string()).await;
                    }
                }
            }
        }
    }

    async fn wait_for_retry(retry_sleep: &mut Option<Pin<Box<Sleep>>>) {
        match retry_sleep {
            Some(sleep) => sleep.as_mut().await,
            None => pending().await,
        }
    }

    async fn handle_request(
        &mut self,
        request: ControllerRequest,
    ) -> (Result<ControllerSnapshot, ControllerError>, bool) {
        match request {
            ControllerRequest::Command(command) => self.handle_command(command).await,
            ControllerRequest::UnexpectedExit { runtime, detail } => {
                if matches!(
                    self.state.phase,
                    ControllerPhase::Running | ControllerPhase::Starting
                ) {
                    self.schedule_restart(runtime, detail).await;
                }
                (Ok(self.state.clone()), false)
            }
        }
    }

    async fn handle_command(
        &mut self,
        command: ControllerCommand,
    ) -> (Result<ControllerSnapshot, ControllerError>, bool) {
        let result = match command {
            ControllerCommand::Start => {
                self.reset_manual_restart();
                self.start(None).await
            }
            ControllerCommand::OpenSurface => self.open(PresentationTarget::SurfaceDsh).await,
            ControllerCommand::OpenBrowser => self.open(PresentationTarget::Web).await,
            ControllerCommand::Stop => self.stop().await,
            ControllerCommand::Restart => self.restart().await,
            ControllerCommand::Shutdown => {
                self.stop_for_shutdown().await;
                return (Ok(self.state.clone()), true);
            }
        };
        (result.map(|()| self.state.clone()), false)
    }

    async fn start(
        &mut self,
        presentation: Option<PresentationTarget>,
    ) -> Result<(), ControllerError> {
        if self.state.phase == ControllerPhase::Running {
            if let Some(target) = presentation {
                return self.present_running(target).await;
            }
            return Ok(());
        }

        self.retry_sleep = None;
        self.state.phase = ControllerPhase::Starting;
        self.state.web_url = None;
        self.state.restart_after = None;
        self.state.last_error = None;
        self.publish();

        match self.services.start_dsh().await {
            Ok(url) => {
                self.state.phase = ControllerPhase::Running;
                self.state.web_url = Some(url);
                self.state.restart_attempts = self.restart_tracker.attempts();
                self.publish();
                if let Some(target) = presentation {
                    self.present_running(target).await?;
                }
                Ok(())
            }
            Err(error) => {
                let error = self.operation_error("start DSH", error);
                self.state.phase = ControllerPhase::Failed;
                self.state.last_error = Some(error.to_string());
                self.publish();
                Err(error)
            }
        }
    }

    async fn open(&mut self, target: PresentationTarget) -> Result<(), ControllerError> {
        if self.state.phase != ControllerPhase::Running {
            if target == PresentationTarget::SurfaceDsh {
                self.services
                    .show_surface_loading()
                    .await
                    .map_err(|error| self.operation_error("show DSH Surface loading", error))?;
            }
            self.reset_manual_restart();
            self.start(Some(target)).await
        } else {
            self.present_running(target).await
        }
    }

    async fn present_running(&mut self, target: PresentationTarget) -> Result<(), ControllerError> {
        let url = self
            .state
            .web_url
            .as_ref()
            .expect("a running controller must retain its validated URL")
            .clone();
        match target {
            PresentationTarget::SurfaceDsh => self
                .services
                .show_surface(&url)
                .await
                .map_err(|error| self.operation_error("show DSH Surface", error)),
            PresentationTarget::Web => self
                .services
                .open_web(&url)
                .await
                .map_err(|error| self.operation_error("open DSH Web UI", error)),
        }
    }

    async fn stop(&mut self) -> Result<(), ControllerError> {
        self.retry_sleep = None;
        let should_stop = !matches!(
            self.state.phase,
            ControllerPhase::Stopped | ControllerPhase::Shutdown
        );
        if should_stop {
            self.services
                .stop_dsh()
                .await
                .map_err(|error| self.operation_error("stop DSH", error))?;
        }
        self.restart_tracker.reset();
        self.state = ControllerSnapshot::default();
        self.publish();
        Ok(())
    }

    async fn restart(&mut self) -> Result<(), ControllerError> {
        self.stop().await?;
        self.start(None).await
    }

    async fn stop_for_shutdown(&mut self) {
        self.retry_sleep = None;
        self.state.phase = ControllerPhase::ShuttingDown;
        self.publish();
        if let Err(error) = self.services.stop_dsh().await {
            self.state.last_error = Some(format!("stop DSH failed: {error}"));
        }
        self.state.phase = ControllerPhase::Shutdown;
        self.state.web_url = None;
        self.state.restart_after = None;
        self.publish();
    }

    async fn schedule_restart(&mut self, runtime: Duration, detail: String) {
        self.state.web_url = None;
        let decision = self.restart_tracker.on_unexpected_exit(runtime);
        self.state.restart_attempts = self.restart_tracker.attempts();
        self.state.last_error = Some(detail.clone());

        match decision {
            RestartDecision::Retry { attempt, after } => {
                self.state.phase = ControllerPhase::RestartBackoff;
                self.state.restart_after = Some(after);
                self.retry_sleep = Some(Box::pin(tokio::time::sleep_until(Instant::now() + after)));
                let message = format!(
                    "DSH exited unexpectedly. Restart attempt {attempt} of {} in {} seconds. {detail}",
                    self.restart_tracker.policy.maximum_attempts,
                    after.as_secs_f32()
                );
                self.publish();
                let _ = self.services.notify("DSH stopped", &message).await;
            }
            RestartDecision::Exhausted { attempts } => {
                self.state.phase = ControllerPhase::Failed;
                self.state.restart_after = None;
                self.retry_sleep = None;
                let message = format!(
                    "DSH could not be restarted after {attempts} attempts. Open Atelier to try again. {detail}"
                );
                self.publish();
                let _ = self.services.notify("DSH restart failed", &message).await;
            }
        }
    }

    fn reset_manual_restart(&mut self) {
        self.retry_sleep = None;
        self.restart_tracker.reset();
        self.state.restart_attempts = 0;
        self.state.restart_after = None;
    }

    fn operation_error(&self, operation: &'static str, error: anyhow::Error) -> ControllerError {
        ControllerError::Operation {
            operation,
            message: format!("{error:#}"),
        }
    }

    fn publish(&self) {
        self.state_sender.send_replace(self.state.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;

    use crate::config::{Config, FirstLaunch};

    use super::{
        Controller, ControllerCommand, ControllerPhase, ControllerServices, LaunchKind,
        PresentationTarget, RestartDecision, RestartPolicy, RestartTracker, startup_decision,
    };

    struct FakeServices {
        actions: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl ControllerServices for FakeServices {
        async fn start_dsh(&mut self) -> anyhow::Result<crate::dsh::readiness::LoopbackUrl> {
            self.actions.lock().unwrap().push("start".into());
            Ok(crate::dsh::readiness::LoopbackUrl::parse(
                "http://127.0.0.1:43127",
            )?)
        }

        async fn stop_dsh(&mut self) -> anyhow::Result<()> {
            self.actions.lock().unwrap().push("stop".into());
            Ok(())
        }

        async fn show_surface_loading(&mut self) -> anyhow::Result<()> {
            self.actions.lock().unwrap().push("loading".into());
            Ok(())
        }

        async fn show_surface(
            &mut self,
            url: &crate::dsh::readiness::LoopbackUrl,
        ) -> anyhow::Result<()> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("surface {}", url.as_str()));
            Ok(())
        }

        async fn open_web(
            &mut self,
            url: &crate::dsh::readiness::LoopbackUrl,
        ) -> anyhow::Result<()> {
            self.actions
                .lock()
                .unwrap()
                .push(format!("open {}", url.as_str()));
            Ok(())
        }

        async fn notify(&mut self, title: &str, _message: &str) -> anyhow::Result<()> {
            self.actions.lock().unwrap().push(format!("notify {title}"));
            Ok(())
        }
    }

    fn spawn_controller(
        policy: RestartPolicy,
    ) -> (super::ControllerHandle, Arc<Mutex<Vec<String>>>) {
        let actions = Arc::new(Mutex::new(Vec::new()));
        let controller = Controller::spawn(
            FakeServices {
                actions: Arc::clone(&actions),
            },
            policy,
        );
        (controller, actions)
    }

    #[test]
    fn explicit_launch_starts_dsh_and_presents_the_configured_surface_target() {
        let decision = startup_decision(&Config::default(), LaunchKind::Explicit);

        assert!(decision.start_dsh);
        assert_eq!(decision.presentation, Some(PresentationTarget::SurfaceDsh));
    }

    #[test]
    fn login_launch_never_presents_a_target() {
        let decision = startup_decision(&Config::default(), LaunchKind::Login);

        assert!(decision.start_dsh);
        assert_eq!(decision.presentation, None);
    }

    #[test]
    fn login_launch_respects_disabled_auto_start() {
        let mut config = Config::default();
        config.dsh.auto_start = false;

        let decision = startup_decision(&config, LaunchKind::Login);

        assert!(!decision.start_dsh);
        assert_eq!(decision.presentation, None);
    }

    #[test]
    fn an_explicit_surface_target_starts_dsh_even_when_auto_start_is_disabled() {
        let mut config = Config::default();
        config.dsh.auto_start = false;

        let decision = startup_decision(&config, LaunchKind::Explicit);

        assert!(decision.start_dsh);
        assert_eq!(decision.presentation, Some(PresentationTarget::SurfaceDsh));
    }

    #[test]
    fn an_explicit_web_target_starts_dsh_even_when_auto_start_is_disabled() {
        let mut config = Config::default();
        config.dsh.auto_start = false;
        config.dsh.first_launch = FirstLaunch::Web;

        let decision = startup_decision(&config, LaunchKind::Explicit);

        assert!(decision.start_dsh);
        assert_eq!(decision.presentation, Some(PresentationTarget::Web));
    }

    #[test]
    fn none_and_disabled_auto_start_leave_dsh_stopped() {
        let mut config = Config::default();
        config.dsh.auto_start = false;
        config.dsh.first_launch = FirstLaunch::None;

        let decision = startup_decision(&config, LaunchKind::Explicit);

        assert!(!decision.start_dsh);
        assert_eq!(decision.presentation, None);
    }

    #[test]
    fn none_with_auto_start_starts_dsh_without_presenting_it() {
        let mut config = Config::default();
        config.dsh.first_launch = FirstLaunch::None;

        let decision = startup_decision(&config, LaunchKind::Explicit);

        assert!(decision.start_dsh);
        assert_eq!(decision.presentation, None);
    }

    #[test]
    fn retries_five_times_with_exponential_backoff() {
        let mut tracker = RestartTracker::new(RestartPolicy::default());
        let expected = [1, 2, 4, 8, 16];

        for (index, seconds) in expected.into_iter().enumerate() {
            assert_eq!(
                tracker.on_unexpected_exit(Duration::from_secs(1)),
                RestartDecision::Retry {
                    attempt: index as u8 + 1,
                    after: Duration::from_secs(seconds),
                }
            );
        }
        assert_eq!(
            tracker.on_unexpected_exit(Duration::from_secs(1)),
            RestartDecision::Exhausted { attempts: 5 }
        );
    }

    #[test]
    fn five_minutes_of_stable_runtime_resets_the_retry_budget() {
        let mut tracker = RestartTracker::new(RestartPolicy::default());
        assert!(matches!(
            tracker.on_unexpected_exit(Duration::from_secs(1)),
            RestartDecision::Retry { attempt: 1, .. }
        ));
        assert!(matches!(
            tracker.on_unexpected_exit(Duration::from_secs(1)),
            RestartDecision::Retry { attempt: 2, .. }
        ));

        assert_eq!(
            tracker.on_unexpected_exit(Duration::from_secs(300)),
            RestartDecision::Retry {
                attempt: 1,
                after: Duration::from_secs(1),
            }
        );
    }

    #[test]
    fn manual_stop_resets_the_retry_budget() {
        let mut tracker = RestartTracker::new(RestartPolicy::default());
        let _ = tracker.on_unexpected_exit(Duration::ZERO);
        let _ = tracker.on_unexpected_exit(Duration::ZERO);

        tracker.reset();

        assert!(matches!(
            tracker.on_unexpected_exit(Duration::ZERO),
            RestartDecision::Retry { attempt: 1, .. }
        ));
    }

    #[tokio::test]
    async fn open_surface_shows_loading_before_starting_dsh_and_then_shows_the_validated_url() {
        let (controller, actions) = spawn_controller(RestartPolicy::default());

        let state = controller
            .command(ControllerCommand::OpenSurface)
            .await
            .unwrap();

        assert_eq!(state.phase, ControllerPhase::Running);
        assert_eq!(
            *actions.lock().unwrap(),
            ["loading", "start", "surface http://127.0.0.1:43127/"]
        );
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn open_browser_starts_dsh_before_opening_the_validated_url() {
        let (controller, actions) = spawn_controller(RestartPolicy::default());

        let state = controller
            .command(ControllerCommand::OpenBrowser)
            .await
            .unwrap();

        assert_eq!(state.phase, ControllerPhase::Running);
        assert_eq!(
            *actions.lock().unwrap(),
            ["start", "open http://127.0.0.1:43127/"]
        );
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opening_each_target_while_running_reuses_the_managed_dsh_process() {
        let (controller, actions) = spawn_controller(RestartPolicy::default());
        controller
            .command(ControllerCommand::OpenSurface)
            .await
            .unwrap();

        controller
            .command(ControllerCommand::OpenBrowser)
            .await
            .unwrap();

        assert_eq!(
            *actions.lock().unwrap(),
            [
                "loading",
                "start",
                "surface http://127.0.0.1:43127/",
                "open http://127.0.0.1:43127/"
            ]
        );
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn opening_the_surface_while_running_does_not_return_to_loading() {
        let (controller, actions) = spawn_controller(RestartPolicy::default());
        controller.command(ControllerCommand::Start).await.unwrap();

        controller
            .command(ControllerCommand::OpenSurface)
            .await
            .unwrap();

        assert_eq!(
            *actions.lock().unwrap(),
            ["start", "surface http://127.0.0.1:43127/"]
        );
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn apply_startup_routes_the_surface_target() {
        let (controller, actions) = spawn_controller(RestartPolicy::default());

        controller
            .apply_startup(&Config::default(), LaunchKind::Explicit)
            .await
            .unwrap();

        assert_eq!(
            *actions.lock().unwrap(),
            ["loading", "start", "surface http://127.0.0.1:43127/"]
        );
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn apply_startup_routes_the_browser_target() {
        let (controller, actions) = spawn_controller(RestartPolicy::default());
        let mut config = Config::default();
        config.dsh.first_launch = FirstLaunch::Web;

        controller
            .apply_startup(&config, LaunchKind::Explicit)
            .await
            .unwrap();

        assert_eq!(
            *actions.lock().unwrap(),
            ["start", "open http://127.0.0.1:43127/"]
        );
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn apply_startup_can_start_without_presenting_a_target() {
        let (controller, actions) = spawn_controller(RestartPolicy::default());
        let mut config = Config::default();
        config.dsh.first_launch = FirstLaunch::None;

        controller
            .apply_startup(&config, LaunchKind::Explicit)
            .await
            .unwrap();

        assert_eq!(*actions.lock().unwrap(), ["start"]);
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn restart_and_shutdown_stop_the_managed_process() {
        let (controller, actions) = spawn_controller(RestartPolicy::default());
        controller.command(ControllerCommand::Start).await.unwrap();

        controller
            .command(ControllerCommand::Restart)
            .await
            .unwrap();
        let state = controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();

        assert_eq!(state.phase, ControllerPhase::Shutdown);
        assert_eq!(*actions.lock().unwrap(), ["start", "stop", "start", "stop"]);
    }

    #[tokio::test]
    async fn an_unexpected_exit_notifies_and_restarts_after_the_backoff() {
        let policy = RestartPolicy {
            maximum_attempts: 5,
            initial_delay: Duration::from_millis(1),
            stable_runtime: Duration::from_secs(300),
        };
        let (controller, actions) = spawn_controller(policy);
        controller.command(ControllerCommand::Start).await.unwrap();

        let state = controller
            .report_unexpected_exit(Duration::from_secs(1), "exit status 1")
            .await
            .unwrap();
        assert_eq!(state.phase, ControllerPhase::RestartBackoff);
        assert_eq!(state.restart_attempts, 1);
        assert_eq!(state.restart_after, Some(Duration::from_millis(1)));

        tokio::time::timeout(Duration::from_secs(1), async {
            let mut states = controller.subscribe();
            while states.borrow().phase != ControllerPhase::Running
                || states.borrow().restart_attempts != 1
            {
                states.changed().await.unwrap();
            }
        })
        .await
        .expect("automatic restart should complete");

        assert_eq!(
            *actions.lock().unwrap(),
            ["start", "notify DSH stopped", "start"]
        );
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stop_cancels_a_pending_automatic_restart() {
        let policy = RestartPolicy {
            maximum_attempts: 5,
            initial_delay: Duration::from_millis(100),
            stable_runtime: Duration::from_secs(300),
        };
        let (controller, actions) = spawn_controller(policy);
        controller.command(ControllerCommand::Start).await.unwrap();
        controller
            .report_unexpected_exit(Duration::from_secs(1), "exit status 1")
            .await
            .unwrap();

        let state = controller.command(ControllerCommand::Stop).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;

        assert_eq!(state.phase, ControllerPhase::Stopped);
        assert_eq!(
            *actions.lock().unwrap(),
            ["start", "notify DSH stopped", "stop"]
        );
        controller
            .command(ControllerCommand::Shutdown)
            .await
            .unwrap();
    }
}
