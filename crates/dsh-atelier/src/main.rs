#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

use std::{
    env,
    fs::OpenOptions,
    sync::{Mutex, mpsc},
    thread,
};

use anyhow::{Context, Result};
use dsh_atelier::{
    app::RuntimeServices,
    config::Config,
    controller::{Controller, ControllerCommand, ControllerSnapshot, RestartPolicy},
    paths::AtelierPaths,
    platform::{NativeAutostart, portable_bootstrap_sibling},
    ports::Autostart,
    runtime::{InstanceLock, RuntimeArguments, write_bootstrap_health},
    surface::SurfaceRequest,
    tray::{TrayCommand, TrayStateUpdate, TrayStatus, load_tray_icon, run_tray_with_ready},
};
use tokio::runtime::{Builder, Runtime};
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    let arguments = RuntimeArguments::parse(env::args_os().skip(1))?;
    let paths = AtelierPaths::discover()?;
    paths
        .create_directories()
        .context("create the Atelier home layout")?;

    let lock_path = paths.state_dir.join("atelier.lock");
    let Some(_instance_lock) = InstanceLock::try_acquire(&lock_path)
        .with_context(|| format!("acquire the instance lock at {}", lock_path.display()))?
    else {
        if let Some(health) = &arguments.health {
            write_bootstrap_health(health, env!("CARGO_PKG_VERSION"))
                .context("acknowledge the already-running Runtime")?;
        }
        return Ok(());
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

    if let Err(error) = synchronize_autostart(&config) {
        tracing::error!(%error, "failed to synchronize launch-at-login configuration");
    }

    run_desktop(runtime, paths, config, arguments)
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

fn synchronize_autostart(config: &Config) -> Result<()> {
    let runtime_executable = env::current_exe().context("resolve the Runtime executable")?;
    let bootstrap = portable_bootstrap_sibling(&runtime_executable);
    if config.atelier.launch_at_login && bootstrap.is_none() {
        anyhow::bail!("cannot enable launch at login without a portable Bootstrap sibling");
    }
    let target = bootstrap.unwrap_or(runtime_executable);
    let adapter = NativeAutostart::new(target).map_err(anyhow::Error::from)?;
    adapter
        .set_enabled(config.atelier.launch_at_login)
        .map_err(anyhow::Error::from)
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
) -> Result<()> {
    let (tray_command_sender, tray_command_receiver) = mpsc::channel();
    let (tray_state_sender, tray_state_receiver) = mpsc::channel();
    let (surface_sender, surface_receiver) = mpsc::channel();
    let tray_icon = load_tray_icon(&paths.root)?;
    let surface_directory = paths.dsh_surface_dir.clone();
    let theme_preference = config.atelier.theme;
    let services = RuntimeServices::new(paths).with_surface_sender(surface_sender.clone());
    let updater = services.updater();
    let monitor = services.clone();

    let controller = runtime.block_on(async {
        let controller = Controller::spawn(services, RestartPolicy::default());
        monitor.spawn_monitor(controller.clone());

        let startup_controller = controller.clone();
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
                if tray_state_sender
                    .send(TrayStateUpdate::Update(snapshot))
                    .is_err()
                {
                    return;
                }
                if update_state.changed().await.is_err() {
                    return;
                }
            }
        });
        controller
    });

    let runtime_handle = runtime.handle().clone();
    let command_controller = controller.clone();
    let command_updater = updater.clone();
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
        surface_directory,
        theme_preference,
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
