#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{
    env,
    fs::OpenOptions,
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread,
};

use anyhow::{Context, Result};
use dsh_atelier::{
    app::RuntimeServices,
    atelier_update::{
        AtelierUpdateChecker, AtelierUpdatePhase, AtelierUpdateSnapshot, Ed25519ManifestVerifier,
        HttpUpdateFeed, HttpUpdateSource, UpdatePlatform,
    },
    config::{AtelierUpdateConfig, Config},
    controller::{Controller, ControllerCommand, ControllerSnapshot, RestartPolicy},
    paths::AtelierPaths,
    platform::{NativeAutostart, NativeNotifier, portable_bootstrap_sibling},
    ports::{Autostart, Notifier},
    runtime::{
        BOOTSTRAP_GENERATION, InstanceLock, RUNTIME_ALREADY_RUNNING_EXIT_CODE,
        RUNTIME_UPDATE_RESTART_EXIT_CODE, RuntimeArguments, write_bootstrap_health,
        write_pending_atelier_runtime,
    },
    surface::SurfaceRequest,
    tray::{
        SurfaceHostConfig, TrayCommand, TrayExitReason, TrayStateUpdate, TrayStatus,
        load_tray_icon, run_tray_with_ready,
    },
};
use semver::Version;
use tokio::runtime::{Builder, Runtime};
use tracing_subscriber::EnvFilter;
use url::Url;

fn main() -> Result<()> {
    let arguments = RuntimeArguments::parse(env::args_os().skip(1))?;
    let paths = if let Some(root) = arguments.atelier_home.as_deref() {
        AtelierPaths::from_root(root)
    } else {
        AtelierPaths::discover()?
    };
    paths
        .create_directories()
        .context("create the Atelier home layout")?;

    let lock_path = paths.state_dir.join("atelier.lock");
    let Some(_instance_lock) = InstanceLock::try_acquire(&lock_path)
        .with_context(|| format!("acquire the instance lock at {}", lock_path.display()))?
    else {
        std::process::exit(RUNTIME_ALREADY_RUNNING_EXIT_CODE);
    };

    initialize_logging(&paths.logs_dir.join("atelier.log"))?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "Atelier Runtime starting"
    );
    let config = Config::load(&paths.config_file)
        .with_context(|| format!("load configuration from {}", paths.config_file.display()))?;
    let runtime = build_async_runtime()?;

    if arguments.smoke_test {
        return run_smoke_test(&runtime, paths, arguments.health.as_ref());
    }

    if let Err(error) = synchronize_autostart(&config, arguments.bootstrap_executable.as_deref()) {
        tracing::error!(%error, "failed to synchronize launch-at-login configuration");
    }

    let exit_reason = run_desktop(runtime, paths, config, arguments)?;
    if exit_reason == TrayExitReason::RestartForUpdate {
        std::process::exit(RUNTIME_UPDATE_RESTART_EXIT_CODE);
    }
    Ok(())
}

fn initialize_logging(path: &std::path::Path) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open log file {}", path.display()))?;
    let filter =
        EnvFilter::try_from_env("DSH_ATELIER_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize file logging: {error}"))
}

fn build_async_runtime() -> Result<Runtime> {
    Builder::new_multi_thread()
        .enable_all()
        .thread_name("atelier-runtime")
        .build()
        .context("create the asynchronous Runtime")
}

fn synchronize_autostart(config: &Config, bootstrap_executable: Option<&Path>) -> Result<()> {
    let runtime_executable = env::current_exe().context("resolve the Runtime executable")?;
    let bootstrap = bootstrap_executable
        .map(Path::to_path_buf)
        .or_else(|| portable_bootstrap_sibling(&runtime_executable));
    if config.atelier.launch_at_login && bootstrap.is_none() {
        anyhow::bail!("cannot enable launch at login without a portable Bootstrap sibling");
    }
    let target = bootstrap.unwrap_or(runtime_executable);
    let adapter = NativeAutostart::new(target).map_err(anyhow::Error::from)?;
    adapter
        .set_enabled(config.atelier.launch_at_login)
        .map_err(anyhow::Error::from)
}

fn configure_atelier_updater(
    config: &AtelierUpdateConfig,
    bootstrap_executable: Option<&Path>,
) -> Result<AtelierUpdateChecker> {
    if !config.enabled {
        anyhow::bail!("disabled by configuration");
    }
    if bootstrap_executable.is_none() {
        anyhow::bail!("the Runtime was not launched by Atelier Bootstrap");
    }
    let platform = UpdatePlatform::current().context("this platform has no update asset")?;
    let current_version =
        Version::parse(env!("CARGO_PKG_VERSION")).context("parse the current Atelier version")?;
    let verifier = Ed25519ManifestVerifier::from_base64(&config.public_key)
        .context("load the Atelier update verification key")?;
    let feeds = config
        .feeds
        .iter()
        .map(|feed| {
            let base =
                Url::parse(feed).with_context(|| format!("parse Atelier update feed {feed}"))?;
            HttpUpdateFeed::from_base_url(base).map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    let source = HttpUpdateSource::new(feeds).context("configure Atelier update feeds")?;
    Ok(AtelierUpdateChecker::new(
        Arc::new(source),
        Arc::new(verifier),
        platform,
        current_version,
        BOOTSTRAP_GENERATION,
    ))
}

fn run_smoke_test(
    runtime: &Runtime,
    paths: AtelierPaths,
    health: Option<&dsh_atelier::runtime::BootstrapHealthRequest>,
) -> Result<()> {
    runtime.block_on(async {
        let services = RuntimeServices::new(paths);
        let monitor = services.clone();
        let controller = Controller::spawn(services, RestartPolicy::default());
        monitor.spawn_monitor(controller.clone());

        let start_result = controller.command(ControllerCommand::Start).await;
        if let Ok(snapshot) = &start_result {
            tracing::info!(url = ?snapshot.web_url, "DSH smoke test became ready");
            if let Some(health) = health {
                write_bootstrap_health(health, env!("CARGO_PKG_VERSION"))
                    .context("write Bootstrap health after smoke test readiness")?;
            }
        }
        let shutdown_result = controller.command(ControllerCommand::Shutdown).await;
        start_result.context("start DSH for the smoke test")?;
        shutdown_result.context("stop DSH after the smoke test")?;
        Ok(())
    })
}

fn run_desktop(
    runtime: Runtime,
    paths: AtelierPaths,
    config: Config,
    arguments: RuntimeArguments,
) -> Result<TrayExitReason> {
    let (tray_command_sender, tray_command_receiver) = mpsc::channel();
    let (tray_state_sender, tray_state_receiver) = mpsc::channel();
    let (surface_sender, surface_receiver) = mpsc::channel();
    let tray_icon = load_tray_icon(&paths.root)?;
    let surface_directory = paths.dsh_surface_dir.clone();
    let atelier_root = paths.root.clone();
    let runtime_directory = paths.runtime_dir.clone();
    let surface_config = config.atelier.surface.clone();
    let theme_preference = config.atelier.theme;
    let current_atelier_version =
        Version::parse(env!("CARGO_PKG_VERSION")).context("parse current Atelier version")?;
    let (atelier_updater, initial_atelier_update) = match configure_atelier_updater(
        &config.atelier.update,
        arguments.bootstrap_executable.as_deref(),
    ) {
        Ok(updater) => {
            let snapshot = updater.snapshot();
            (Some(updater), snapshot)
        }
        Err(error) => {
            tracing::info!(%error, "Atelier self-update is disabled");
            (
                None,
                AtelierUpdateSnapshot::disabled(current_atelier_version, format!("{error:#}")),
            )
        }
    };
    let _ = tray_state_sender.send(TrayStateUpdate::AtelierUpdate(initial_atelier_update));
    let services = RuntimeServices::new(paths).with_surface_sender(surface_sender.clone());
    let updater = services.updater();
    let monitor = services.clone();

    let controller = runtime.block_on(async {
        let controller = Controller::spawn(services, RestartPolicy::default());
        monitor.spawn_monitor(controller.clone());

        let startup_controller = controller.clone();
        let dsh_update_sender = tray_state_sender.clone();
        tokio::spawn(async move {
            if let Err(error) = startup_controller
                .apply_startup(&config, arguments.launch_kind)
                .await
            {
                tracing::error!(%error, "automatic DSH startup failed");
            }
        });

        let mut state = controller.subscribe();
        let dsh_state_sender = tray_state_sender.clone();
        let surface_state_sender = surface_sender.clone();
        tokio::spawn(async move {
            let mut initial_snapshot = true;
            loop {
                let snapshot = state.borrow().clone();
                let status = TrayStatus::from(&snapshot);
                tracing::info!(
                    phase = ?snapshot.phase,
                    url = ?snapshot.web_url,
                    restart_attempts = snapshot.restart_attempts,
                    error = ?snapshot.last_error,
                    "DSH state changed"
                );
                if dsh_state_sender.send(TrayStateUpdate::Dsh(status)).is_err() {
                    return;
                }
                if let Some(request) = surface_request_for_snapshot(&snapshot, initial_snapshot)
                    && surface_state_sender.send(request).is_err()
                {
                    return;
                }
                initial_snapshot = false;
                if state.changed().await.is_err() {
                    return;
                }
            }
        });

        let mut update_state = updater.subscribe();
        #[cfg(target_os = "macos")]
        {
            let termination_sender = tray_state_sender.clone();
            tokio::spawn(async move {
                use tokio::signal::unix::{SignalKind, signal};

                match signal(SignalKind::terminate()) {
                    Ok(mut terminate) => {
                        if terminate.recv().await.is_some() {
                            let _ = termination_sender.send(TrayStateUpdate::Terminate);
                        }
                    }
                    Err(error) => {
                        tracing::error!(%error, "failed to install the macOS termination handler");
                    }
                }
            });
        }
        tokio::spawn(async move {
            loop {
                let snapshot = update_state.borrow().clone();
                tracing::info!(
                    phase = ?snapshot.phase,
                    current = ?snapshot.current_version,
                    available = ?snapshot.available_version,
                    can_install = snapshot.can_install,
                    error = ?snapshot.last_error,
                    "DSH update state changed"
                );
                if dsh_update_sender
                    .send(TrayStateUpdate::DshUpdate(snapshot))
                    .is_err()
                {
                    return;
                }
                if update_state.changed().await.is_err() {
                    return;
                }
            }
        });

        if let Some(atelier_updater) = atelier_updater.as_ref() {
            let mut atelier_update_state = atelier_updater.subscribe();
            let atelier_update_sender = tray_state_sender.clone();
            let notifier = NativeNotifier::default();
            tokio::spawn(async move {
                let mut notified_version = None;
                loop {
                    let snapshot = atelier_update_state.borrow().clone();
                    tracing::info!(
                        phase = ?snapshot.phase,
                        current = ?snapshot.current_version,
                        available = ?snapshot.available_version,
                        error = ?snapshot.last_error,
                        "Atelier update state changed"
                    );
                    if matches!(
                        snapshot.phase,
                        AtelierUpdatePhase::Available
                            | AtelierUpdatePhase::FullPackageRequired
                    ) && snapshot.available_version != notified_version
                        && let Some(version) = snapshot.available_version.as_ref()
                    {
                        let body = if snapshot.phase == AtelierUpdatePhase::Available {
                            format!(
                                "Atelier {version} is available. Install it from the Tray when convenient."
                            )
                        } else {
                            format!(
                                "Atelier {version} requires a full package update from the configured release source."
                            )
                        };
                        if let Err(error) = notifier.notify("Atelier update available", &body) {
                            tracing::warn!(%error, "failed to show Atelier update notification");
                        }
                        notified_version = Some(version.clone());
                    }
                    if atelier_update_sender
                        .send(TrayStateUpdate::AtelierUpdate(snapshot))
                        .is_err()
                    {
                        return;
                    }
                    if atelier_update_state.changed().await.is_err() {
                        return;
                    }
                }
            });
            atelier_updater.check_in_background();
        }
        controller
    });

    let runtime_handle = runtime.handle().clone();
    let command_controller = controller.clone();
    let command_updater = updater.clone();
    let command_atelier_updater = atelier_updater.clone();
    let update_runtime_directory = runtime_directory.clone();
    let update_restart_sender = tray_state_sender.clone();
    let command_bridge = thread::Builder::new()
        .name("atelier-tray-commands".to_owned())
        .spawn(move || {
            while let Ok(command) = tray_command_receiver.recv() {
                match command {
                    TrayCommand::CheckDshUpdate => {
                        if !command_updater.force_check() {
                            tracing::warn!("cannot check for a DSH update before DSH is selected");
                        }
                    }
                    TrayCommand::InstallDshUpdate => {
                        let updater = command_updater.clone();
                        let controller = command_controller.clone();
                        runtime_handle.spawn(async move {
                            match updater.install_available().await {
                                Ok(installed) => {
                                    tracing::info!(
                                        version = %installed.version,
                                        "restarting DSH to apply the selected update"
                                    );
                                    if let Err(error) =
                                        controller.command(ControllerCommand::Restart).await
                                    {
                                        tracing::error!(%error, "failed to restart the updated DSH");
                                    }
                                }
                                Err(error) => {
                                    tracing::error!(%error, "DSH update installation failed");
                                }
                            }
                        });
                    }
                    TrayCommand::CheckAtelierUpdate => {
                        if let Some(updater) = &command_atelier_updater {
                            updater.check_in_background();
                        } else {
                            tracing::warn!("Atelier self-update is disabled");
                        }
                    }
                    TrayCommand::InstallAtelierUpdate => {
                        let Some(updater) = command_atelier_updater.clone() else {
                            tracing::warn!("Atelier self-update is disabled");
                            continue;
                        };
                        let runtime_directory = update_runtime_directory.clone();
                        let restart_sender = update_restart_sender.clone();
                        runtime_handle.spawn(async move {
                            match updater.stage_available(&runtime_directory).await {
                                Ok(staged) => {
                                    if let Err(error) = write_pending_atelier_runtime(
                                        &runtime_directory,
                                        &staged.version,
                                        &staged.executable,
                                    ) {
                                        let _ = updater.mark_install_failed(&error);
                                        tracing::error!(%error, "failed to activate the staged Atelier Runtime");
                                        return;
                                    }
                                    if let Err(error) = updater.mark_restart_required(&staged) {
                                        tracing::error!(%error, "failed to publish the Atelier restart state");
                                    }
                                    tracing::info!(
                                        version = %staged.version,
                                        "restarting Atelier to apply the selected Runtime update"
                                    );
                                    let _ = restart_sender.send(TrayStateUpdate::RestartAtelier);
                                }
                                Err(error) => {
                                    tracing::error!(%error, "Atelier Runtime update installation failed");
                                }
                            }
                        });
                    }
                    TrayCommand::Exit => return,
                    command => {
                        let command = match command {
                            TrayCommand::OpenDsh => ControllerCommand::OpenSurface,
                            TrayCommand::OpenDshInBrowser => ControllerCommand::OpenBrowser,
                            TrayCommand::Start => ControllerCommand::Start,
                            TrayCommand::Stop => ControllerCommand::Stop,
                            TrayCommand::Restart => ControllerCommand::Restart,
                            TrayCommand::CheckDshUpdate
                            | TrayCommand::InstallDshUpdate
                            | TrayCommand::CheckAtelierUpdate
                            | TrayCommand::InstallAtelierUpdate
                            | TrayCommand::Exit => unreachable!(),
                        };
                        let controller = command_controller.clone();
                        runtime_handle.spawn(async move {
                            if let Err(error) = controller.command(command).await {
                                tracing::error!(?command, %error, "tray command failed");
                            }
                        });
                    }
                }
            }
        })
        .context("start the Tray command bridge")?;

    let health = arguments.health;
    let tray_result = run_tray_with_ready(
        tray_command_sender,
        tray_state_receiver,
        surface_receiver,
        SurfaceHostConfig::new(
            surface_directory,
            atelier_root,
            surface_config,
            theme_preference,
        ),
        tray_icon,
        move || match health {
            Some(ref request) => write_bootstrap_health(request, env!("CARGO_PKG_VERSION"))
                .context("report Runtime health to Bootstrap"),
            None => Ok(()),
        },
    );

    let shutdown_result = runtime.block_on(controller.command(ControllerCommand::Shutdown));
    if command_bridge.join().is_err() {
        tracing::error!("Tray command bridge panicked");
    }
    shutdown_result.context("shut down DSH before exiting Atelier")?;
    tray_result
}

fn surface_request_for_snapshot(
    snapshot: &ControllerSnapshot,
    initial_snapshot: bool,
) -> Option<SurfaceRequest> {
    use dsh_atelier::controller::ControllerPhase;

    if initial_snapshot {
        return None;
    }

    match snapshot.phase {
        ControllerPhase::Running => snapshot
            .web_url
            .as_ref()
            .map(|url| SurfaceRequest::Navigate(url.clone())),
        ControllerPhase::Stopped | ControllerPhase::Failed | ControllerPhase::Shutdown => {
            Some(SurfaceRequest::Hide)
        }
        ControllerPhase::Starting
        | ControllerPhase::RestartBackoff
        | ControllerPhase::ShuttingDown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_atelier::dsh::readiness::LoopbackUrl;

    #[test]
    fn surface_follows_the_validated_controller_url_and_hides_when_stopped() {
        let mut snapshot = ControllerSnapshot::default();
        assert_eq!(
            surface_request_for_snapshot(&snapshot, false),
            Some(SurfaceRequest::Hide)
        );

        let url = LoopbackUrl::parse("http://127.0.0.1:43127").expect("valid loopback URL");
        snapshot.phase = dsh_atelier::controller::ControllerPhase::Running;
        snapshot.web_url = Some(url.clone());
        assert_eq!(
            surface_request_for_snapshot(&snapshot, false),
            Some(SurfaceRequest::Navigate(url))
        );
    }

    #[test]
    fn surface_remains_visible_while_dsh_is_in_a_transitional_phase() {
        use dsh_atelier::controller::ControllerPhase;

        for phase in [
            ControllerPhase::Starting,
            ControllerPhase::RestartBackoff,
            ControllerPhase::ShuttingDown,
        ] {
            let snapshot = ControllerSnapshot {
                phase,
                ..ControllerSnapshot::default()
            };

            assert_eq!(surface_request_for_snapshot(&snapshot, false), None);
        }
    }

    #[test]
    fn surface_hides_after_terminal_non_running_states() {
        use dsh_atelier::controller::ControllerPhase;

        for phase in [
            ControllerPhase::Stopped,
            ControllerPhase::Failed,
            ControllerPhase::Shutdown,
        ] {
            let snapshot = ControllerSnapshot {
                phase,
                ..ControllerSnapshot::default()
            };

            assert_eq!(
                surface_request_for_snapshot(&snapshot, false),
                Some(SurfaceRequest::Hide)
            );
        }
    }

    #[test]
    fn initial_stopped_snapshot_does_not_race_the_startup_loading_request() {
        let snapshot = ControllerSnapshot::default();

        assert_eq!(surface_request_for_snapshot(&snapshot, true), None);
        assert_eq!(
            surface_request_for_snapshot(&snapshot, false),
            Some(SurfaceRequest::Hide)
        );
    }
}
