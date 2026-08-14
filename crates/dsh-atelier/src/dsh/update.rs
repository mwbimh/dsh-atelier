use std::{path::PathBuf, sync::Arc};

use async_trait::async_trait;
use semver::Version;
use tokio::sync::{Mutex, watch};

use crate::install::LocatedRelease;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DshUpdateSource {
    Managed,
    External,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CurrentDsh {
    pub version: Version,
    pub source: DshUpdateSource,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DshUpdatePhase {
    #[default]
    Idle,
    Checking,
    UpToDate,
    Available,
    Installing,
    RestartRequired,
    CheckFailed,
    InstallFailed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DshUpdateSnapshot {
    pub phase: DshUpdatePhase,
    pub current_version: Option<Version>,
    pub available_version: Option<Version>,
    pub can_install: bool,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InstalledDshUpdate {
    pub version: Version,
    pub program: PathBuf,
}

#[async_trait]
pub trait DshUpdateBackend: Send + Sync + 'static {
    async fn latest_release(&self) -> anyhow::Result<LocatedRelease>;

    async fn install_release(&self, release: &LocatedRelease)
    -> anyhow::Result<InstalledDshUpdate>;

    async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct DshUpdateManager {
    inner: Arc<DshUpdateManagerInner>,
}

struct DshUpdateManagerInner {
    backend: Arc<dyn DshUpdateBackend>,
    operation: Mutex<()>,
    current: std::sync::Mutex<Option<CurrentDsh>>,
    available: std::sync::Mutex<Option<LocatedRelease>>,
    state_sender: watch::Sender<DshUpdateSnapshot>,
}

impl DshUpdateManager {
    #[must_use]
    pub fn new(backend: Arc<dyn DshUpdateBackend>) -> Self {
        let (state_sender, _) = watch::channel(DshUpdateSnapshot::default());
        Self {
            inner: Arc::new(DshUpdateManagerInner {
                backend,
                operation: Mutex::new(()),
                current: std::sync::Mutex::new(None),
                available: std::sync::Mutex::new(None),
                state_sender,
            }),
        }
    }

    /// Starts an npm metadata check without adding network work to the DSH
    /// startup critical path. Repeated automatic starts for the same version do
    /// not trigger duplicate checks or notifications.
    pub fn check_after_start(&self, current: CurrentDsh) {
        let snapshot = self.snapshot();
        if snapshot.current_version.as_ref() == Some(&current.version)
            && matches!(
                snapshot.phase,
                DshUpdatePhase::Checking
                    | DshUpdatePhase::UpToDate
                    | DshUpdatePhase::Available
                    | DshUpdatePhase::Installing
                    | DshUpdatePhase::RestartRequired
            )
        {
            return;
        }
        self.begin_check(current);
    }

    /// Rechecks the latest selected DSH after an explicit Tray request.
    /// Returns false when no DSH has been selected yet.
    pub fn force_check(&self) -> bool {
        let current = self.inner.current.lock().expect("current DSH lock").clone();
        let Some(current) = current else {
            return false;
        };
        self.begin_check(current);
        true
    }

    fn begin_check(&self, current: CurrentDsh) {
        *self.inner.current.lock().expect("current DSH lock") = Some(current.clone());
        *self
            .inner
            .available
            .lock()
            .expect("available DSH update lock") = None;
        self.publish(DshUpdateSnapshot {
            phase: DshUpdatePhase::Checking,
            current_version: Some(current.version.clone()),
            ..DshUpdateSnapshot::default()
        });

        let manager = self.clone();
        tokio::spawn(async move {
            manager.perform_check(current).await;
        });
    }

    async fn perform_check(&self, current: CurrentDsh) {
        let _operation = self.inner.operation.lock().await;
        if self
            .inner
            .current
            .lock()
            .expect("current DSH lock")
            .as_ref()
            != Some(&current)
        {
            return;
        }

        let located = match self.inner.backend.latest_release().await {
            Ok(located) => located,
            Err(error) => {
                tracing::warn!(%error, "background DSH update check failed");
                self.publish(DshUpdateSnapshot {
                    phase: DshUpdatePhase::CheckFailed,
                    current_version: Some(current.version),
                    last_error: Some(format!("{error:#}")),
                    ..DshUpdateSnapshot::default()
                });
                return;
            }
        };

        if located.release.version <= current.version {
            self.publish(DshUpdateSnapshot {
                phase: DshUpdatePhase::UpToDate,
                current_version: Some(current.version),
                ..DshUpdateSnapshot::default()
            });
            return;
        }

        let can_install = current.source == DshUpdateSource::Managed;
        let available_version = located.release.version.clone();
        *self
            .inner
            .available
            .lock()
            .expect("available DSH update lock") = Some(located);
        self.publish(DshUpdateSnapshot {
            phase: DshUpdatePhase::Available,
            current_version: Some(current.version.clone()),
            available_version: Some(available_version.clone()),
            can_install,
            last_error: None,
        });

        let body = if can_install {
            format!(
                "DSH {available_version} is available. Install it from the Atelier Tray when convenient; the running DSH was not interrupted."
            )
        } else {
            format!(
                "DSH {available_version} is available. This DSH is externally managed, so update it with its original installation method."
            )
        };
        if let Err(error) = self
            .inner
            .backend
            .notify("DSH update available", &body)
            .await
        {
            tracing::warn!(%error, "failed to show DSH update notification");
        }
    }

    pub async fn install_available(&self) -> anyhow::Result<InstalledDshUpdate> {
        let _operation = self.inner.operation.lock().await;
        let snapshot = self.snapshot();
        if snapshot.phase != DshUpdatePhase::Available {
            anyhow::bail!("no DSH update is ready to install");
        }
        if !snapshot.can_install {
            anyhow::bail!("the selected DSH is managed by an external installation method");
        }
        let located = self
            .inner
            .available
            .lock()
            .expect("available DSH update lock")
            .clone()
            .ok_or_else(|| anyhow::anyhow!("the available DSH release metadata is missing"))?;
        self.publish(DshUpdateSnapshot {
            phase: DshUpdatePhase::Installing,
            current_version: snapshot.current_version.clone(),
            available_version: snapshot.available_version.clone(),
            can_install: false,
            last_error: None,
        });

        let installed = match self.inner.backend.install_release(&located).await {
            Ok(installed) => installed,
            Err(error) => {
                let message = format!("{error:#}");
                self.publish(DshUpdateSnapshot {
                    phase: DshUpdatePhase::InstallFailed,
                    current_version: snapshot.current_version,
                    available_version: snapshot.available_version,
                    can_install: true,
                    last_error: Some(message.clone()),
                });
                let _ = self
                    .inner
                    .backend
                    .notify("DSH update failed", &message)
                    .await;
                return Err(error);
            }
        };

        self.publish(DshUpdateSnapshot {
            phase: DshUpdatePhase::RestartRequired,
            current_version: snapshot.current_version,
            available_version: Some(installed.version.clone()),
            can_install: false,
            last_error: None,
        });
        let _ = self
            .inner
            .backend
            .notify(
                "DSH update installed",
                &format!(
                    "DSH {} was installed and will now restart.",
                    installed.version
                ),
            )
            .await;
        Ok(installed)
    }

    #[must_use]
    pub fn snapshot(&self) -> DshUpdateSnapshot {
        self.inner.state_sender.borrow().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<DshUpdateSnapshot> {
        self.inner.state_sender.subscribe()
    }

    fn publish(&self, snapshot: DshUpdateSnapshot) {
        self.inner.state_sender.send_replace(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::PathBuf,
        sync::{Arc, Mutex},
    };

    use async_trait::async_trait;
    use semver::Version;
    use tokio::sync::Notify;
    use url::Url;

    use super::{
        CurrentDsh, DshUpdateBackend, DshUpdateManager, DshUpdatePhase, DshUpdateSource,
        InstalledDshUpdate,
    };
    use crate::{
        install::{LocatedRelease, RegistryEndpoint},
        registry::LatestRelease,
    };

    struct FakeBackend {
        latest: LocatedRelease,
        check_started: Arc<Notify>,
        release_check: Arc<Notify>,
        notifications: Arc<Mutex<Vec<(String, String)>>>,
        installs: Arc<Mutex<Vec<Version>>>,
    }

    #[async_trait]
    impl DshUpdateBackend for FakeBackend {
        async fn latest_release(&self) -> anyhow::Result<LocatedRelease> {
            self.check_started.notify_waiters();
            self.release_check.notified().await;
            Ok(self.latest.clone())
        }

        async fn install_release(
            &self,
            release: &LocatedRelease,
        ) -> anyhow::Result<InstalledDshUpdate> {
            self.installs
                .lock()
                .unwrap()
                .push(release.release.version.clone());
            Ok(InstalledDshUpdate {
                version: release.release.version.clone(),
                program: PathBuf::from("managed-dsh"),
            })
        }

        async fn notify(&self, title: &str, body: &str) -> anyhow::Result<()> {
            self.notifications
                .lock()
                .unwrap()
                .push((title.to_owned(), body.to_owned()));
            Ok(())
        }
    }

    struct Fixture {
        manager: DshUpdateManager,
        check_started: Arc<Notify>,
        release_check: Arc<Notify>,
        notifications: Arc<Mutex<Vec<(String, String)>>>,
        installs: Arc<Mutex<Vec<Version>>>,
    }

    impl Fixture {
        fn new(latest: &str) -> Self {
            let check_started = Arc::new(Notify::new());
            let release_check = Arc::new(Notify::new());
            let notifications = Arc::new(Mutex::new(Vec::new()));
            let installs = Arc::new(Mutex::new(Vec::new()));
            let manager = DshUpdateManager::new(Arc::new(FakeBackend {
                latest: located_release(latest),
                check_started: Arc::clone(&check_started),
                release_check: Arc::clone(&release_check),
                notifications: Arc::clone(&notifications),
                installs: Arc::clone(&installs),
            }));
            Self {
                manager,
                check_started,
                release_check,
                notifications,
                installs,
            }
        }
    }

    #[tokio::test]
    async fn scheduling_a_check_returns_without_waiting_for_the_registry() {
        let fixture = Fixture::new("0.2.0");

        fixture.manager.check_after_start(CurrentDsh {
            version: Version::parse("0.1.0").unwrap(),
            source: DshUpdateSource::Managed,
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.check_started.notified(),
        )
        .await
        .expect("background registry check should start");
        assert_eq!(fixture.manager.snapshot().phase, DshUpdatePhase::Checking);
        assert!(fixture.notifications.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_newer_managed_release_notifies_and_waits_for_explicit_install() {
        let fixture = Fixture::new("0.2.0");
        fixture.manager.check_after_start(CurrentDsh {
            version: Version::parse("0.1.0").unwrap(),
            source: DshUpdateSource::Managed,
        });
        fixture.release_check.notify_one();

        wait_for_phase(&fixture.manager, DshUpdatePhase::Available).await;

        let snapshot = fixture.manager.snapshot();
        assert_eq!(snapshot.current_version.unwrap().to_string(), "0.1.0");
        assert_eq!(snapshot.available_version.unwrap().to_string(), "0.2.0");
        assert!(snapshot.can_install);
        assert!(fixture.installs.lock().unwrap().is_empty());
        assert_eq!(fixture.notifications.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_up_to_date_release_is_silent() {
        let fixture = Fixture::new("0.1.0");
        fixture.manager.check_after_start(CurrentDsh {
            version: Version::parse("0.1.0").unwrap(),
            source: DshUpdateSource::Managed,
        });
        fixture.release_check.notify_one();

        wait_for_phase(&fixture.manager, DshUpdatePhase::UpToDate).await;

        assert!(fixture.notifications.lock().unwrap().is_empty());
        assert!(fixture.manager.snapshot().available_version.is_none());
    }

    #[tokio::test]
    async fn external_dsh_updates_are_reported_but_never_installed_by_atelier() {
        let fixture = Fixture::new("0.2.0");
        fixture.manager.check_after_start(CurrentDsh {
            version: Version::parse("0.1.0").unwrap(),
            source: DshUpdateSource::External,
        });
        fixture.release_check.notify_one();
        wait_for_phase(&fixture.manager, DshUpdatePhase::Available).await;

        assert!(!fixture.manager.snapshot().can_install);
        assert!(fixture.manager.install_available().await.is_err());
        assert!(fixture.installs.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn install_only_runs_after_the_user_requests_it() {
        let fixture = Fixture::new("0.2.0");
        fixture.manager.check_after_start(CurrentDsh {
            version: Version::parse("0.1.0").unwrap(),
            source: DshUpdateSource::Managed,
        });
        fixture.release_check.notify_one();
        wait_for_phase(&fixture.manager, DshUpdatePhase::Available).await;

        let installed = fixture.manager.install_available().await.unwrap();

        assert_eq!(installed.version.to_string(), "0.2.0");
        assert_eq!(
            fixture.installs.lock().unwrap().as_slice(),
            [installed.version]
        );
        assert_eq!(
            fixture.manager.snapshot().phase,
            DshUpdatePhase::RestartRequired
        );
    }

    async fn wait_for_phase(manager: &DshUpdateManager, expected: DshUpdatePhase) {
        let mut updates = manager.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while updates.borrow().phase != expected {
                updates.changed().await.unwrap();
            }
        })
        .await
        .expect("update phase should change");
    }

    fn located_release(version: &str) -> LocatedRelease {
        LocatedRelease {
            release: LatestRelease {
                version: Version::parse(version).unwrap(),
                integrity: "sha512-fixture".to_owned(),
                tarball: Url::parse("https://registry.npmjs.org/dsh.tgz").unwrap(),
                engines_node: Some("^22.19.0 || >=24.0.0".to_owned()),
            },
            registry: RegistryEndpoint::parse("https://registry.npmjs.org/", true).unwrap(),
        }
    }
}
