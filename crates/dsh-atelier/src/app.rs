use std::{
    env, fs,
    path::{Path, PathBuf},
    sync::Arc,
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
        discovery::{CandidateSource, DiscoveredDsh, discover_candidates, discover_dsh},
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
    process::CommandSpec,
    registry::{DSH_PACKAGE_NAME, REGISTRY_CONNECT_TIMEOUT, REGISTRY_REQUEST_TIMEOUT},
};

const SUPERVISOR_POLL_INTERVAL: Duration = Duration::from_secs(1);

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
        }
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
            version: selected.version.clone(),
            source: if selected.source == CandidateSource::Managed {
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
                    version = %selected.version,
                    error = %update_error,
                    "pending DSH failed readiness; rolling back"
                );
                clear_pending_dsh(&self.paths)?;
                let fallback = resolve_dsh(&self.paths)
                    .await
                    .context("resolve the previous DSH after update failure")?;
                if fallback.program == selected.program {
                    return Err(update_error).context("start pending DSH update");
                }
                self.updater
                    .check_after_start(current_dsh(&self.paths, &fallback));
                let (supervisor, url) = start_selected_dsh(&fallback).await.with_context(|| {
                    format!(
                        "pending DSH {} failed ({update_error:#}); rollback DSH {} also failed",
                        selected.version, fallback.version
                    )
                })?;
                let _ = self.notifier.notify(
                    "DSH update rolled back",
                    &format!(
                        "DSH {} could not start. Atelier restored DSH {}.",
                        selected.version, fallback.version
                    ),
                );
                (fallback, supervisor, url)
            }
            Err(error) => return Err(error).context("start managed DSH Web"),
        };
        if is_managed_selection(&self.paths, &selected) {
            commit_managed_activation(&self.paths, &selected.version)?;
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
        write_pending_dsh(&self.paths, &selected.version)?;
        Ok(InstalledDshUpdate {
            version: selected.version,
            program: selected.program,
        })
    }

    async fn notify(&self, title: &str, body: &str) -> Result<()> {
        self.notifier
            .notify(title, body)
            .map_err(anyhow::Error::from)
    }
}

pub async fn ensure_dsh(paths: &AtelierPaths) -> Result<PathBuf> {
    Ok(resolve_dsh(paths).await?.program)
}

async fn resolve_dsh(paths: &AtelierPaths) -> Result<DiscoveredDsh> {
    create_atelier_directories(paths)?;
    let cwd = env::current_dir().context("resolve current directory")?;
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
        env::var_os("PATH").as_deref(),
        env::var_os("PATHEXT").as_deref(),
        &cwd,
    );
    if let Some(mut discovered) = discover_dsh(&candidates, Duration::from_secs(5))
        .await
        .selected
    {
        if is_managed_selection(paths, &discovered) {
            discovered.source = CandidateSource::Managed;
        }
        return Ok(discovered);
    }

    install_managed_dsh(paths).await
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

fn is_managed_selection(paths: &AtelierPaths, selected: &DiscoveredDsh) -> bool {
    selected
        .program
        .starts_with(paths.dsh_installations_dir.join("versions"))
}

fn is_pending_managed_selection(paths: &AtelierPaths, selected: &DiscoveredDsh) -> Result<bool> {
    Ok(is_managed_selection(paths, selected)
        && read_pending_dsh(paths)?.as_ref() == Some(&selected.version))
}

fn current_dsh(paths: &AtelierPaths, selected: &DiscoveredDsh) -> CurrentDsh {
    CurrentDsh {
        version: selected.version.clone(),
        source: if is_managed_selection(paths, selected) {
            DshUpdateSource::Managed
        } else {
            DshUpdateSource::External
        },
    }
}

async fn start_selected_dsh(selected: &DiscoveredDsh) -> Result<(DshSupervisor, LoopbackUrl)> {
    let spec = CommandSpec {
        program: selected.program.clone(),
        args: vec![
            "web".to_owned(),
            "--host".to_owned(),
            "127.0.0.1".to_owned(),
            "--port".to_owned(),
            "0".to_owned(),
        ],
        current_dir: None,
        env: Default::default(),
    };
    let mut supervisor = DshSupervisor::new(spec, SupervisorOptions::default());
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

async fn install_managed_dsh(paths: &AtelierPaths) -> Result<DiscoveredDsh> {
    let located = lookup_latest_dsh_release().await?;
    install_managed_release(paths, &located).await
}

async fn install_managed_release(
    paths: &AtelierPaths,
    located: &LocatedRelease,
) -> Result<DiscoveredDsh> {
    let layout = ManagedDshLayout::from_paths(paths);
    let final_program = managed_dsh_program(&layout.version_dir(&located.release.version));
    if final_program.is_file() {
        return Ok(DiscoveredDsh {
            program: final_program,
            version: located.release.version.clone(),
            source: CandidateSource::Managed,
        });
    }

    let toolchain =
        ensure_node_toolchain_for(paths, located.release.engines_node.as_deref()).await?;

    let stale_staging = layout.staging_dir.join(located.release.version.to_string());
    if stale_staging.exists() {
        fs::remove_dir_all(&stale_staging)
            .with_context(|| format!("remove stale DSH staging {}", stale_staging.display()))?;
    }
    let staged = layout.begin(&located.release.version)?;
    let npm = NpmCli::new(
        toolchain.node_executable,
        toolchain.npm_cli,
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
    let report = discover_dsh(
        &[crate::dsh::discovery::DshCandidate {
            program: program.clone(),
            source: crate::dsh::discovery::CandidateSource::Managed,
        }],
        Duration::from_secs(10),
    )
    .await;
    if report.selected.is_none() {
        bail!(
            "installed DSH failed version validation: {:?}",
            report.failures
        );
    }
    let installed = staged.promote()?;
    Ok(DiscoveredDsh {
        program: installed.managed_program(),
        version: installed.version,
        source: CandidateSource::Managed,
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
    if let Some(system_node) = find_in_path(if cfg!(windows) { "node.exe" } else { "node" }) {
        node_candidates.push(system_node);
    }
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

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
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

    #[test]
    fn selects_the_highest_valid_managed_dsh_version() {
        let directory = tempfile::tempdir().unwrap();
        for version in ["0.1.0-rc.5", "0.1.0-rc.6", "not-a-version"] {
            let root = directory.path().join("versions").join(version);
            fs::create_dir_all(&root).unwrap();
            fs::write(managed_dsh_program(&root), b"shim").unwrap();
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
