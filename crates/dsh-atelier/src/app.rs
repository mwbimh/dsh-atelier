use std::{
    collections::{BTreeMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, mpsc::Sender},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::{
    controller::{ControllerHandle, ControllerServices},
    dsh::{
        discovery::{
            CandidateSource, DiscoveredDsh, DshCandidate, desktop_search_path, discover_candidates,
            parse_dsh_version,
        },
        readiness::LoopbackUrl,
        supervisor::{DshState, DshSupervisor, SupervisorOptions},
        update::{
            CurrentDsh, DshUpdateBackend, DshUpdateManager, DshUpdateSource, InstalledDshUpdate,
        },
    },
    install::{
        LocatedRelease, ManagedDshLayout, NPM_INSTALL_TIMEOUT, NpmCli, ProcessCommandRunner,
        RegistryRetryPolicy, ReqwestPackumentSource, default_registry_endpoints,
        install_exact_with_registry_fallback, lookup_latest_release_with,
        node_supports_current_dsh, probe_node_toolchain,
    },
    node::{
        MAX_NODE_ARCHIVE_BYTES, MAX_SHASUMS_BYTES, NodeDistribution, download_to,
        node_version_is_compatible, probe_node, promote_staged_node, stage_downloaded_node,
    },
    paths::AtelierPaths,
    platform::{NativeBrowser, NativeNotifier},
    ports::{Browser, Notifier},
    process::{CommandSpec, run_once},
    registry::{DSH_PACKAGE_NAME, REGISTRY_CONNECT_TIMEOUT, REGISTRY_REQUEST_TIMEOUT},
    surface::SurfaceRequest,
};

const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Eq, PartialEq)]
struct DshCommand {
    program: PathBuf,
    leading_args: Vec<String>,
    env: BTreeMap<String, String>,
}

impl DshCommand {
    fn external(dsh_program: impl Into<PathBuf>) -> Self {
        Self {
            program: dsh_program.into(),
            leading_args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    fn with_search_path(mut self, search_path: &std::ffi::OsStr) -> Self {
        self.env.insert(
            "PATH".to_owned(),
            search_path.to_string_lossy().into_owned(),
        );
        self
    }

    fn managed(node_program: impl Into<PathBuf>, dsh_script: impl Into<PathBuf>) -> Self {
        let node_program = node_program.into();
        let dsh_script = dsh_script.into();
        let inherited = desktop_search_path(env::var_os("PATH").as_deref());
        let mut directories = node_program
            .parent()
            .map(Path::to_path_buf)
            .into_iter()
            .collect::<Vec<_>>();
        directories.extend(env::split_paths(&inherited));
        deduplicate_paths(&mut directories);
        let search_path = env::join_paths(directories).unwrap_or(inherited);
        #[cfg(windows)]
        let command = Self::external(dsh_script);
        #[cfg(not(windows))]
        let command = Self {
            program: node_program,
            leading_args: vec![dsh_script.to_string_lossy().into_owned()],
            env: BTreeMap::new(),
        };
        command.with_search_path(&search_path)
    }

    fn version_spec(&self) -> CommandSpec {
        self.spec(&["--version"])
    }

    fn web_spec(&self) -> CommandSpec {
        self.spec(&["web", "--host", "127.0.0.1", "--port", "0"])
    }

    fn spec(&self, args: &[&str]) -> CommandSpec {
        let mut command_args = self.leading_args.clone();
        command_args.extend(args.iter().map(|argument| (*argument).to_owned()));
        CommandSpec {
            program: self.program.clone(),
            args: command_args,
            current_dir: None,
            env: self.env.clone(),
        }
    }
}

#[derive(Clone, Debug)]
struct SelectedDsh {
    discovered: DiscoveredDsh,
    command: DshCommand,
}

struct RunningDsh {
    supervisor: DshSupervisor,
    started_at: tokio::time::Instant,
}

type SupervisorSlot = Arc<Mutex<Option<RunningDsh>>>;

#[derive(Clone)]
pub struct RuntimeServices {
    paths: AtelierPaths,
    supervisor: SupervisorSlot,
    browser: NativeBrowser,
    notifier: NativeNotifier,
    updater: DshUpdateManager,
    surface_sender: Option<Sender<SurfaceRequest>>,
}

impl RuntimeServices {
    pub fn new(paths: AtelierPaths) -> Self {
        let notifier = NativeNotifier::default();
        let updater = DshUpdateManager::new(Arc::new(RuntimeUpdateBackend {
            paths: paths.clone(),
            notifier,
        }));
        Self {
            paths,
            supervisor: Arc::new(Mutex::new(None)),
            browser: NativeBrowser::default(),
            notifier,
            updater,
            surface_sender: None,
        }
    }

    #[must_use]
    pub fn with_surface_sender(mut self, sender: Sender<SurfaceRequest>) -> Self {
        self.surface_sender = Some(sender);
        self
    }

    #[must_use]
    pub fn updater(&self) -> DshUpdateManager {
        self.updater.clone()
    }

    pub fn spawn_monitor(&self, controller: ControllerHandle) {
        let supervisor = Arc::clone(&self.supervisor);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SUPERVISOR_POLL_INTERVAL).await;
                let crashed = {
                    let mut slot = supervisor.lock().await;
                    let Some(running) = slot.as_mut() else {
                        if matches!(
                            controller.snapshot().phase,
                            crate::controller::ControllerPhase::Shutdown
                        ) {
                            return;
                        }
                        continue;
                    };
                    match running.supervisor.poll() {
                        Ok(DshState::Crashed { code }) => {
                            let runtime = running.started_at.elapsed();
                            *slot = None;
                            Some((
                                runtime,
                                format!("DSH exited unexpectedly with code {code:?}"),
                            ))
                        }
                        Ok(_) => None,
                        Err(error) => {
                            let runtime = running.started_at.elapsed();
                            *slot = None;
                            Some((runtime, format!("failed to inspect DSH: {error}")))
                        }
                    }
                };
                if let Some((runtime, detail)) = crashed {
                    let _ = controller.report_unexpected_exit(runtime, detail).await;
                }
            }
        });
    }
}

#[async_trait]
impl ControllerServices for RuntimeServices {
    async fn start_dsh(&mut self) -> Result<LoopbackUrl> {
        if self.supervisor.lock().await.is_some() {
            bail!("DSH is already running");
        }
        let selected = resolve_dsh(&self.paths).await?;
        self.updater.check_after_start(CurrentDsh {
            version: selected.discovered.version.clone(),
            source: if selected.discovered.source == CandidateSource::Managed {
                DshUpdateSource::Managed
            } else {
                DshUpdateSource::External
            },
        });
        let is_pending_activation = is_pending_managed_selection(&self.paths, &selected)?;
        let (selected, supervisor, url) = match start_selected_dsh(&selected).await {
            Ok((supervisor, url)) => (selected, supervisor, url),
            Err(update_error) if is_pending_activation => {
                tracing::error!(
                    version = %selected.discovered.version,
                    error = %update_error,
                    "pending DSH failed readiness; rolling back"
                );
                clear_pending_dsh(&self.paths)?;
                let fallback = resolve_dsh(&self.paths)
                    .await
                    .context("resolve the previous DSH after update failure")?;
                if fallback.discovered.program == selected.discovered.program {
                    return Err(update_error).context("start pending DSH update");
                }
                self.updater
                    .check_after_start(current_dsh(&self.paths, &fallback));
                let (supervisor, url) = start_selected_dsh(&fallback).await.with_context(|| {
                    format!(
                        "pending DSH {} failed ({update_error:#}); rollback DSH {} also failed",
                        selected.discovered.version, fallback.discovered.version
                    )
                })?;
                let _ = self.notifier.notify(
                    "DSH update rolled back",
                    &format!(
                        "DSH {} could not start. Atelier restored DSH {}.",
                        selected.discovered.version, fallback.discovered.version
                    ),
                );
                (fallback, supervisor, url)
            }
            Err(error) => return Err(error).context("start managed DSH Web"),
        };
        if is_managed_selection(&self.paths, &selected) {
            commit_managed_activation(&self.paths, &selected.discovered.version)?;
        }
        *self.supervisor.lock().await = Some(RunningDsh {
            supervisor,
            started_at: tokio::time::Instant::now(),
        });
        Ok(url)
    }

    async fn stop_dsh(&mut self) -> Result<()> {
        let running = self.supervisor.lock().await.take();
        if let Some(mut running) = running {
            running
                .supervisor
                .stop()
                .await
                .context("stop managed DSH")?;
        }
        Ok(())
    }

    async fn show_surface_loading(&mut self) -> Result<()> {
        self.surface_sender
            .as_ref()
            .context("the DSH Surface host is unavailable")?
            .send(SurfaceRequest::ShowLoading)
            .context("send the loading request to the DSH Surface host")
    }

    async fn show_surface(&mut self, url: &LoopbackUrl) -> Result<()> {
        self.surface_sender
            .as_ref()
            .context("the DSH Surface host is unavailable")?
            .send(SurfaceRequest::Show(url.clone()))
            .context("send the validated DSH URL to the Surface host")
    }

    async fn open_web(&mut self, url: &LoopbackUrl) -> Result<()> {
        self.browser.open(url).map_err(anyhow::Error::from)
    }

    async fn notify(&mut self, title: &str, message: &str) -> Result<()> {
        self.notifier
            .notify(title, message)
            .map_err(anyhow::Error::from)
    }
}

#[derive(Clone)]
struct RuntimeUpdateBackend {
    paths: AtelierPaths,
    notifier: NativeNotifier,
}

#[async_trait]
impl DshUpdateBackend for RuntimeUpdateBackend {
    async fn latest_release(&self) -> Result<LocatedRelease> {
        lookup_latest_dsh_release().await
    }

    async fn install_release(&self, release: &LocatedRelease) -> Result<InstalledDshUpdate> {
        let selected = install_managed_release(&self.paths, release).await?;
        write_pending_dsh(&self.paths, &selected.discovered.version)?;
        Ok(InstalledDshUpdate {
            version: selected.discovered.version,
            program: selected.discovered.program,
        })
    }

    async fn notify(&self, title: &str, body: &str) -> Result<()> {
        self.notifier
            .notify(title, body)
            .map_err(anyhow::Error::from)
    }
}

pub async fn ensure_dsh(paths: &AtelierPaths) -> Result<PathBuf> {
    Ok(resolve_dsh(paths).await?.discovered.program)
}

async fn resolve_dsh(paths: &AtelierPaths) -> Result<SelectedDsh> {
    create_atelier_directories(paths)?;
    let cwd = env::current_dir().context("resolve current directory")?;
    let search_path = desktop_search_path(env::var_os("PATH").as_deref());
    let pending = read_pending_dsh(paths)?
        .map(|version| managed_dsh_program(&managed_dsh_version_dir(paths, &version)))
        .filter(|program| program.is_file());
    let active = read_active_dsh(paths)?
        .map(|state| managed_dsh_program(&managed_dsh_version_dir(paths, &state.active)))
        .filter(|program| program.is_file())
        .or_else(|| newest_managed_dsh(&paths.dsh_installations_dir));
    let candidates = discover_candidates(
        pending.as_deref(),
        active.as_deref(),
        Some(search_path.as_os_str()),
        env::var_os("PATHEXT").as_deref(),
        &cwd,
    );
    let managed_nodes = existing_nodes_for_managed_dsh(paths, search_path.as_os_str());
    for mut candidate in candidates {
        if is_managed_program(paths, &candidate.program) {
            candidate.source = CandidateSource::Managed;
            if managed_nodes.is_empty() {
                tracing::warn!(
                    dsh = %candidate.program.display(),
                    "cannot probe managed DSH without a selected Node executable"
                );
                continue;
            }
            for node_program in &managed_nodes {
                let Ok(probed_node) = probe_node(node_program).await else {
                    continue;
                };
                if !node_version_is_compatible(&probed_node.version) {
                    continue;
                }
                let command = DshCommand::managed(node_program, &candidate.program);
                if let Some(selected) =
                    discover_dsh_with_command(&candidate, command, Duration::from_secs(5)).await
                {
                    return Ok(selected);
                }
            }
        } else {
            let command =
                DshCommand::external(&candidate.program).with_search_path(search_path.as_os_str());
            if let Some(selected) =
                discover_dsh_with_command(&candidate, command, Duration::from_secs(5)).await
            {
                return Ok(selected);
            }
        }
    }

    install_managed_dsh(paths).await
}

async fn discover_dsh_with_command(
    candidate: &DshCandidate,
    command: DshCommand,
    timeout: Duration,
) -> Option<SelectedDsh> {
    let output = match run_once(&command.version_spec(), timeout).await {
        Ok(output) => output,
        Err(error) => {
            tracing::warn!(
                dsh = %candidate.program.display(),
                %error,
                "failed to probe DSH candidate"
            );
            return None;
        }
    };
    if output.status != 0 {
        tracing::warn!(
            dsh = %candidate.program.display(),
            status = output.status,
            stderr = %output.stderr.trim(),
            "DSH candidate version probe failed"
        );
        return None;
    }
    let version = match parse_dsh_version(&output.stdout) {
        Ok(version) => version,
        Err(error) => {
            tracing::warn!(
                dsh = %candidate.program.display(),
                %error,
                "DSH candidate returned an invalid version"
            );
            return None;
        }
    };
    Some(SelectedDsh {
        discovered: DiscoveredDsh {
            program: candidate.program.clone(),
            version,
            source: candidate.source,
        },
        command,
    })
}

fn create_atelier_directories(paths: &AtelierPaths) -> Result<()> {
    for path in [
        &paths.config_dir,
        &paths.state_dir,
        &paths.logs_dir,
        &paths.runtime_dir,
        &paths.tools_dir,
        &paths.npm_dir,
        &paths.dsh_installations_dir,
    ] {
        fs::create_dir_all(path)
            .with_context(|| format!("create Atelier directory {}", path.display()))?;
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ManagedDshActive {
    active: Version,
    rollback: Option<Version>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct ManagedDshPending {
    pending: Version,
}

fn managed_dsh_version_dir(paths: &AtelierPaths, version: &Version) -> PathBuf {
    paths
        .dsh_installations_dir
        .join("versions")
        .join(version.to_string())
}

fn active_dsh_path(paths: &AtelierPaths) -> PathBuf {
    paths.dsh_installations_dir.join("active.json")
}

fn pending_dsh_path(paths: &AtelierPaths) -> PathBuf {
    paths.dsh_installations_dir.join("pending.json")
}

fn read_active_dsh(paths: &AtelierPaths) -> Result<Option<ManagedDshActive>> {
    read_optional_json(&active_dsh_path(paths))
}

fn read_pending_dsh(paths: &AtelierPaths) -> Result<Option<Version>> {
    Ok(
        read_optional_json::<ManagedDshPending>(&pending_dsh_path(paths))?
            .map(|state| state.pending),
    )
}

fn write_pending_dsh(paths: &AtelierPaths, version: &Version) -> Result<()> {
    write_json_atomically(
        &pending_dsh_path(paths),
        &ManagedDshPending {
            pending: version.clone(),
        },
    )
}

fn clear_pending_dsh(paths: &AtelierPaths) -> Result<()> {
    match fs::remove_file(pending_dsh_path(paths)) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).context("clear pending DSH activation"),
    }
}

fn commit_managed_activation(paths: &AtelierPaths, version: &Version) -> Result<()> {
    let previous = read_active_dsh(paths)?;
    if previous
        .as_ref()
        .is_some_and(|state| &state.active == version)
    {
        if read_pending_dsh(paths)?.as_ref() == Some(version) {
            clear_pending_dsh(paths)?;
        }
        return Ok(());
    }
    write_json_atomically(
        &active_dsh_path(paths),
        &ManagedDshActive {
            active: version.clone(),
            rollback: previous.map(|state| state.active),
        },
    )?;
    if read_pending_dsh(paths)?.as_ref() == Some(version) {
        clear_pending_dsh(paths)?;
    }
    Ok(())
}

fn read_optional_json<T>(path: &Path) -> Result<Option<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("read {}", path.display()));
        }
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {}", path.display()))
        .map(Some)
}

fn write_json_atomically<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("state path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let bytes = serde_json::to_vec_pretty(value)?;
    fs::write(&temporary, bytes)
        .with_context(|| format!("write temporary state file {}", temporary.display()))?;
    if path.exists() {
        fs::remove_file(path).with_context(|| format!("replace state file {}", path.display()))?;
    }
    fs::rename(&temporary, path).with_context(|| format!("activate state file {}", path.display()))
}

fn is_managed_program(paths: &AtelierPaths, program: &Path) -> bool {
    program.starts_with(paths.dsh_installations_dir.join("versions"))
}

fn is_managed_selection(paths: &AtelierPaths, selected: &SelectedDsh) -> bool {
    selected
        .discovered
        .program
        .starts_with(paths.dsh_installations_dir.join("versions"))
}

fn is_pending_managed_selection(paths: &AtelierPaths, selected: &SelectedDsh) -> Result<bool> {
    Ok(is_managed_selection(paths, selected)
        && read_pending_dsh(paths)?.as_ref() == Some(&selected.discovered.version))
}

fn current_dsh(paths: &AtelierPaths, selected: &SelectedDsh) -> CurrentDsh {
    CurrentDsh {
        version: selected.discovered.version.clone(),
        source: if is_managed_selection(paths, selected) {
            DshUpdateSource::Managed
        } else {
            DshUpdateSource::External
        },
    }
}

async fn start_selected_dsh(selected: &SelectedDsh) -> Result<(DshSupervisor, LoopbackUrl)> {
    let mut supervisor =
        DshSupervisor::new(selected.command.web_spec(), SupervisorOptions::default());
    let url = supervisor.start().await?;
    Ok((supervisor, url))
}

fn newest_managed_dsh(installations_root: &Path) -> Option<PathBuf> {
    let versions = installations_root.join("versions");
    let mut candidates = fs::read_dir(versions)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version = Version::parse(entry.file_name().to_string_lossy().as_ref()).ok()?;
            let program = managed_dsh_program(&entry.path());
            program.is_file().then_some((version, program))
        })
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| right.0.cmp(&left.0));
    candidates.into_iter().next().map(|(_, program)| program)
}

fn managed_dsh_program(prefix: &Path) -> PathBuf {
    if cfg!(windows) {
        prefix.join("dsh.cmd")
    } else {
        prefix.join("bin/dsh")
    }
}

async fn lookup_latest_dsh_release() -> Result<LocatedRelease> {
    let source = ReqwestPackumentSource::new(REGISTRY_CONNECT_TIMEOUT, REGISTRY_REQUEST_TIMEOUT)
        .context("build npm registry client")?;
    lookup_latest_release_with(
        &source,
        &default_registry_endpoints(),
        DSH_PACKAGE_NAME,
        &RegistryRetryPolicy::default(),
    )
    .await
    .context("resolve latest DSH from npm")
}

async fn install_managed_dsh(paths: &AtelierPaths) -> Result<SelectedDsh> {
    let located = lookup_latest_dsh_release().await?;
    install_managed_release(paths, &located).await
}

async fn install_managed_release(
    paths: &AtelierPaths,
    located: &LocatedRelease,
) -> Result<SelectedDsh> {
    let layout = ManagedDshLayout::from_paths(paths);
    let final_program = managed_dsh_program(&layout.version_dir(&located.release.version));
    let toolchain =
        ensure_node_toolchain_for(paths, located.release.engines_node.as_deref()).await?;
    let final_command =
        DshCommand::managed(toolchain.node_executable.clone(), final_program.clone());
    if final_program.is_file() {
        return Ok(SelectedDsh {
            discovered: DiscoveredDsh {
                program: final_program,
                version: located.release.version.clone(),
                source: CandidateSource::Managed,
            },
            command: final_command,
        });
    }

    let stale_staging = layout.staging_dir.join(located.release.version.to_string());
    if stale_staging.exists() {
        fs::remove_dir_all(&stale_staging)
            .with_context(|| format!("remove stale DSH staging {}", stale_staging.display()))?;
    }
    let staged = layout.begin(&located.release.version)?;
    let npm = NpmCli::new(
        toolchain.node_executable.clone(),
        toolchain.npm_cli.clone(),
        layout.npm.clone(),
    );
    let registries = default_registry_endpoints();
    let first_install_registry = registries
        .iter()
        .position(|registry| registry == &located.registry)
        .unwrap_or(0);
    install_exact_with_registry_fallback(
        &npm,
        &ProcessCommandRunner,
        &staged.root,
        DSH_PACKAGE_NAME,
        &located.release.version,
        &registries[first_install_registry..],
        NPM_INSTALL_TIMEOUT,
    )
    .await
    .context("install DSH through npm")?;
    let program = staged.managed_program();
    let command = DshCommand::managed(toolchain.node_executable.clone(), program.clone());
    discover_dsh_with_command(
        &DshCandidate {
            program,
            source: CandidateSource::Managed,
        },
        command,
        Duration::from_secs(10),
    )
    .await
    .ok_or_else(|| anyhow::anyhow!("installed DSH failed version validation"))?;
    let installed = staged.promote()?;
    let installed_program = installed.managed_program();
    Ok(SelectedDsh {
        discovered: DiscoveredDsh {
            program: installed_program.clone(),
            version: installed.version,
            source: CandidateSource::Managed,
        },
        command: DshCommand::managed(toolchain.node_executable, installed_program),
    })
}

async fn ensure_node_toolchain_for(
    paths: &AtelierPaths,
    engines_node: Option<&str>,
) -> Result<crate::install::NodeToolchain> {
    let distribution = NodeDistribution::default_for_current_platform()?;
    let managed_root = managed_node_root(paths, &distribution);
    let managed_node = managed_root.join(distribution.node_relative_path());
    let mut node_candidates = Vec::new();
    if managed_node.is_file() {
        node_candidates.push(managed_node.clone());
    }
    let search_path = desktop_search_path(env::var_os("PATH").as_deref());
    node_candidates.extend(find_all_in_path(
        if cfg!(windows) { "node.exe" } else { "node" },
        search_path.as_os_str(),
    ));
    deduplicate_paths(&mut node_candidates);
    for candidate in node_candidates {
        if let Ok(probed) = probe_node(&candidate).await
            && node_version_is_compatible(&probed.version)
            && node_satisfies_engine_requirement(&probed.version, engines_node)?
            && let Ok(toolchain) = probe_node_toolchain(&candidate).await
        {
            return Ok(toolchain);
        }
    }

    install_managed_node(paths, &distribution, &managed_root).await?;
    let toolchain = probe_node_toolchain(&managed_node)
        .await
        .context("validate installed managed Node/npm")?;
    if !node_satisfies_engine_requirement(&toolchain.node_version, engines_node)? {
        bail!(
            "Atelier-managed Node {} does not satisfy DSH engines.node {:?}",
            toolchain.node_version,
            engines_node
        );
    }
    Ok(toolchain)
}

fn node_satisfies_engine_requirement(
    version: &Version,
    engines_node: Option<&str>,
) -> Result<bool> {
    let Some(requirement) = engines_node else {
        return Ok(node_supports_current_dsh(version));
    };
    requirement
        .split("||")
        .map(str::trim)
        .filter(|alternative| !alternative.is_empty())
        .map(|alternative| {
            VersionReq::parse(alternative)
                .with_context(|| format!("parse DSH engines.node requirement {requirement:?}"))
        })
        .collect::<Result<Vec<_>>>()
        .map(|alternatives| {
            !alternatives.is_empty()
                && alternatives
                    .iter()
                    .any(|alternative| alternative.matches(version))
        })
}

fn existing_nodes_for_managed_dsh(
    paths: &AtelierPaths,
    search_path: &std::ffi::OsStr,
) -> Vec<PathBuf> {
    let Ok(distribution) = NodeDistribution::default_for_current_platform() else {
        return Vec::new();
    };
    let managed_node =
        managed_node_root(paths, &distribution).join(distribution.node_relative_path());
    let mut candidates = Vec::new();
    if managed_node.is_file() {
        candidates.push(managed_node);
    }
    candidates.extend(find_all_in_path(
        if cfg!(windows) { "node.exe" } else { "node" },
        search_path,
    ));
    deduplicate_paths(&mut candidates);
    candidates
}

fn deduplicate_paths(paths: &mut Vec<PathBuf>) {
    let mut seen = HashSet::new();
    paths.retain(|path| seen.insert(path.clone()));
}

fn find_all_in_path(name: &str, path: &std::ffi::OsStr) -> Vec<PathBuf> {
    env::split_paths(path)
        .map(|directory| directory.join(name))
        .filter(|candidate| candidate.is_file())
        .collect()
}

fn managed_node_root(paths: &AtelierPaths, distribution: &NodeDistribution) -> PathBuf {
    let platform = match distribution.platform {
        crate::node::NodePlatform::Windows => "windows",
        crate::node::NodePlatform::MacOs => "macos",
    };
    let architecture = match distribution.architecture {
        crate::node::NodeArchitecture::X64 => "x64",
        crate::node::NodeArchitecture::Arm64 => "arm64",
    };
    paths
        .tools_dir
        .join("node/versions")
        .join(distribution.version.to_string())
        .join(format!("{platform}-{architecture}"))
}

async fn install_managed_node(
    paths: &AtelierPaths,
    distribution: &NodeDistribution,
    destination: &Path,
) -> Result<()> {
    if destination.is_dir() {
        return Ok(());
    }
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let staging_root = paths.tools_dir.join("node/staging").join(format!(
        "{}-{}-{stamp}",
        distribution.version,
        std::process::id()
    ));
    fs::create_dir_all(&staging_root)?;
    let archive = staging_root.join(&distribution.artifact_name);
    let shasums = staging_root.join("SHASUMS256.txt");
    let extracted = staging_root.join("extracted");
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5 * 60))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    download_to(
        &client,
        &distribution.shasums_url,
        &shasums,
        MAX_SHASUMS_BYTES,
    )
    .await?;
    download_to(
        &client,
        &distribution.artifact_url,
        &archive,
        MAX_NODE_ARCHIVE_BYTES,
    )
    .await?;
    let shasums_text = fs::read_to_string(&shasums)?;
    stage_downloaded_node(&archive, &shasums_text, distribution, &extracted)?;
    promote_staged_node(&extracted, destination)?;
    let _ = fs::remove_dir_all(&staging_root);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[test]
    fn managed_dsh_specs_run_the_script_through_the_selected_node() {
        let command = DshCommand::managed("/atelier/node/bin/node", "/atelier/dsh/bin/dsh");
        let version = command.version_spec();
        let web = command.web_spec();

        assert_eq!(version.program, PathBuf::from("/atelier/node/bin/node"));
        assert_eq!(version.args, ["/atelier/dsh/bin/dsh", "--version"]);
        assert_eq!(web.program, version.program);
        assert_eq!(
            web.args,
            [
                "/atelier/dsh/bin/dsh",
                "web",
                "--host",
                "127.0.0.1",
                "--port",
                "0",
            ]
        );
        assert_eq!(&web.env, &version.env);
        let search_path = version.env["PATH"].clone();
        assert_eq!(
            env::split_paths(std::ffi::OsStr::new(&search_path)).next(),
            Some(PathBuf::from("/atelier/node/bin"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn managed_windows_dsh_specs_keep_executing_the_cmd_shim_directly() {
        let command = DshCommand::managed("C:/atelier/node/node.exe", "C:/atelier/dsh/dsh.cmd");

        assert_eq!(
            command.version_spec().program,
            PathBuf::from("C:/atelier/dsh/dsh.cmd")
        );
        assert_eq!(command.version_spec().args, ["--version"]);
        let search_path = command.version_spec().env["PATH"].clone();
        assert_eq!(
            env::split_paths(std::ffi::OsStr::new(&search_path)).next(),
            Some(PathBuf::from("C:/atelier/node"))
        );
    }

    #[test]
    fn external_dsh_specs_execute_the_discovered_program_directly() {
        let search_path = std::ffi::OsStr::new("/usr/bin:/opt/homebrew/bin:/usr/local/bin");
        let command = DshCommand::external("/opt/homebrew/bin/dsh").with_search_path(search_path);

        assert_eq!(
            command.version_spec(),
            CommandSpec {
                program: PathBuf::from("/opt/homebrew/bin/dsh"),
                args: vec!["--version".into()],
                current_dir: None,
                env: BTreeMap::from([("PATH".into(), search_path.to_string_lossy().into_owned(),)]),
            }
        );
        assert_eq!(command.web_spec().program, command.version_spec().program);
        assert_eq!(
            command.web_spec().args,
            ["web", "--host", "127.0.0.1", "--port", "0"]
        );
        assert_eq!(command.web_spec().env, command.version_spec().env);
    }

    #[test]
    fn node_search_keeps_all_candidates_in_path_order() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("old");
        let compatible = directory.path().join("compatible");
        fs::create_dir_all(&old).unwrap();
        fs::create_dir_all(&compatible).unwrap();
        let node_name = if cfg!(windows) { "node.exe" } else { "node" };
        fs::write(old.join(node_name), b"old node").unwrap();
        fs::write(compatible.join(node_name), b"compatible node").unwrap();
        let search_path = env::join_paths([&old, &compatible]).unwrap();

        assert_eq!(
            find_all_in_path(node_name, &search_path),
            vec![old.join(node_name), compatible.join(node_name)]
        );
    }

    #[test]
    fn selects_the_highest_valid_managed_dsh_version() {
        let directory = tempfile::tempdir().unwrap();
        for version in ["0.1.0-rc.5", "0.1.0-rc.6", "not-a-version"] {
            let root = directory.path().join("versions").join(version);
            let program = managed_dsh_program(&root);
            fs::create_dir_all(program.parent().unwrap()).unwrap();
            fs::write(program, b"shim").unwrap();
        }

        let selected = newest_managed_dsh(directory.path()).unwrap();

        assert!(selected.to_string_lossy().contains("0.1.0-rc.6"));
    }

    #[test]
    fn managed_node_layout_is_platform_and_architecture_specific() {
        let paths = AtelierPaths::from_root(Path::new("atelier"));
        let distribution = NodeDistribution::new(
            &Version::parse("24.19.0").unwrap(),
            crate::node::NodePlatform::Windows,
            crate::node::NodeArchitecture::X64,
        )
        .unwrap();

        assert_eq!(
            managed_node_root(&paths, &distribution),
            Path::new("atelier/tools/node/versions/24.19.0/windows-x64")
        );
    }

    #[test]
    fn npm_node_engine_alternatives_are_checked_against_the_selected_node() {
        let requirement = Some("^22.19.0 || >=24.0.0");

        assert!(
            node_satisfies_engine_requirement(&Version::parse("22.19.1").unwrap(), requirement,)
                .unwrap()
        );
        assert!(
            !node_satisfies_engine_requirement(&Version::parse("23.11.1").unwrap(), requirement,)
                .unwrap()
        );
        assert!(
            node_satisfies_engine_requirement(&Version::parse("24.19.0").unwrap(), requirement,)
                .unwrap()
        );
    }

    #[test]
    fn malformed_node_engine_metadata_fails_closed() {
        assert!(
            node_satisfies_engine_requirement(
                &Version::parse("24.19.0").unwrap(),
                Some("not a range")
            )
            .is_err()
        );
    }

    #[test]
    fn managed_activation_commits_pending_and_retains_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AtelierPaths::from_root(directory.path());
        let old = Version::parse("0.1.0").unwrap();
        let new = Version::parse("0.2.0").unwrap();

        commit_managed_activation(&paths, &old).unwrap();
        write_pending_dsh(&paths, &new).unwrap();
        assert_eq!(read_pending_dsh(&paths).unwrap(), Some(new.clone()));

        commit_managed_activation(&paths, &new).unwrap();

        assert_eq!(
            read_active_dsh(&paths).unwrap(),
            Some(ManagedDshActive {
                active: new,
                rollback: Some(old),
            })
        );
        assert_eq!(read_pending_dsh(&paths).unwrap(), None);
    }

    #[test]
    fn clearing_a_failed_pending_activation_preserves_the_active_version() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AtelierPaths::from_root(directory.path());
        let active = Version::parse("0.1.0").unwrap();
        commit_managed_activation(&paths, &active).unwrap();
        write_pending_dsh(&paths, &Version::parse("0.2.0").unwrap()).unwrap();

        clear_pending_dsh(&paths).unwrap();

        assert_eq!(read_active_dsh(&paths).unwrap().unwrap().active, active);
        assert_eq!(read_pending_dsh(&paths).unwrap(), None);
    }
}
