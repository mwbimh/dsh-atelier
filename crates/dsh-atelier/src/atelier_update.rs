use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, watch};
use url::Url;

const UPDATE_MANIFEST_SCHEMA: u32 = 1;
const MAX_MANIFEST_BYTES: usize = 256 * 1024;
const MAX_SIGNATURE_BYTES: usize = 16 * 1024;
const MAX_PORTABLE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RUNTIME_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum AtelierUpdateError {
    #[error("invalid update manifest JSON: {0}")]
    InvalidManifestJson(#[from] serde_json::Error),
    #[error("unsupported update manifest schema {0}; expected schema 1")]
    UnsupportedManifestSchema(u32),
    #[error("unsupported update channel {0:?}; expected stable")]
    UnsupportedChannel(String),
    #[error("stable update version {0} must not contain prerelease or build metadata")]
    UnstableVersion(Version),
    #[error("invalid release tag {actual:?}; expected {expected:?}")]
    InvalidReleaseTag { expected: String, actual: String },
    #[error("min_bootstrap_generation must be greater than zero")]
    InvalidBootstrapGeneration,
    #[error("invalid {platform} asset field {field}: {reason}")]
    InvalidAsset {
        platform: &'static str,
        field: &'static str,
        reason: &'static str,
    },
    #[error("update manifest signature is empty")]
    EmptySignature,
    #[error("update manifest signature verification failed: {0}")]
    SignatureVerification(String),
    #[error("invalid Atelier update public key: {0}")]
    InvalidPublicKey(String),
    #[error("invalid Atelier update signature encoding: {0}")]
    InvalidSignatureEncoding(String),
    #[error("unsupported update platform")]
    UnsupportedPlatform,
    #[error("no Atelier update is available")]
    NoAvailableUpdate,
    #[error("another Atelier update operation is already running")]
    OperationInProgress,
    #[error("invalid Atelier update state transition: {0}")]
    InvalidStateTransition(&'static str),
    #[error(
        "Atelier {version} requires bootstrap generation {required}, but this application provides {current}"
    )]
    BootstrapTooOld {
        version: Version,
        required: u32,
        current: u32,
    },
    #[error("portable archive size mismatch: expected {expected} bytes, got {actual} bytes")]
    ArchiveSizeMismatch { expected: u64, actual: u64 },
    #[error("portable archive SHA-256 mismatch: expected {expected}, got {actual}")]
    ArchiveHashMismatch { expected: String, actual: String },
    #[error("unsafe ZIP entry {0:?}")]
    UnsafeArchiveEntry(String),
    #[error("unsupported ZIP entry {0:?}")]
    UnsupportedArchiveEntry(String),
    #[error("portable archive does not contain Runtime entry {0:?}")]
    RuntimeEntryMissing(String),
    #[error("portable archive contains Runtime entry {0:?} more than once")]
    DuplicateRuntimeEntry(String),
    #[error("portable archive contains duplicate filenames")]
    DuplicateArchiveEntries,
    #[error("Runtime entry is too large: {0} bytes")]
    RuntimeTooLarge(u64),
    #[error("Runtime entry size mismatch: expected {expected} bytes, got {actual} bytes")]
    RuntimeSizeMismatch { expected: u64, actual: u64 },
    #[error("Runtime version destination already exists: {0}")]
    VersionAlreadyInstalled(PathBuf),
    #[error("failed to {action} {path}: {source}")]
    Io {
        action: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid ZIP archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("all update sources failed: {0}")]
    AllSourcesFailed(String),
    #[error("update response from {url} exceeded the {limit}-byte limit")]
    ResponseTooLarge { url: Url, limit: usize },
    #[error("invalid update feed URL {url}: {reason}")]
    InvalidFeedUrl { url: Url, reason: &'static str },
    #[error("manifest origin {0} is not a configured update source")]
    UnknownManifestOrigin(Url),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct UpdateManifest {
    pub schema: u32,
    pub channel: String,
    pub version: Version,
    pub tag: String,
    pub min_bootstrap_generation: u32,
    pub assets: ManifestAssets,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ManifestAssets {
    #[serde(rename = "windows-x64")]
    pub windows_x64: PlatformAsset,
    #[serde(rename = "macos-universal")]
    pub macos_universal: PlatformAsset,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PlatformAsset {
    pub path: String,
    pub format: String,
    pub runtime_path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdatePlatform {
    WindowsX64,
    MacosUniversal,
}

impl UpdatePlatform {
    #[must_use]
    pub fn current() -> Option<Self> {
        if cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            Some(Self::WindowsX64)
        } else if cfg!(target_os = "macos") {
            Some(Self::MacosUniversal)
        } else {
            None
        }
    }

    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::WindowsX64 => "windows-x64",
            Self::MacosUniversal => "macos-universal",
        }
    }

    const fn runtime_executable_name(self) -> &'static str {
        match self {
            Self::WindowsX64 => "dsh-atelier-runtime.exe",
            Self::MacosUniversal => "dsh-atelier-runtime",
        }
    }
}

impl UpdateManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, AtelierUpdateError> {
        let manifest: Self = serde_json::from_slice(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn asset_for(&self, platform: UpdatePlatform) -> &PlatformAsset {
        match platform {
            UpdatePlatform::WindowsX64 => &self.assets.windows_x64,
            UpdatePlatform::MacosUniversal => &self.assets.macos_universal,
        }
    }

    fn validate(&self) -> Result<(), AtelierUpdateError> {
        if self.schema != UPDATE_MANIFEST_SCHEMA {
            return Err(AtelierUpdateError::UnsupportedManifestSchema(self.schema));
        }
        if self.channel != "stable" {
            return Err(AtelierUpdateError::UnsupportedChannel(self.channel.clone()));
        }
        if !self.version.pre.is_empty() || !self.version.build.is_empty() {
            return Err(AtelierUpdateError::UnstableVersion(self.version.clone()));
        }
        let expected_tag = format!("v{}", self.version);
        if self.tag != expected_tag {
            return Err(AtelierUpdateError::InvalidReleaseTag {
                expected: expected_tag,
                actual: self.tag.clone(),
            });
        }
        if self.min_bootstrap_generation == 0 {
            return Err(AtelierUpdateError::InvalidBootstrapGeneration);
        }
        validate_asset("windows-x64", &self.assets.windows_x64)?;
        validate_asset("macos-universal", &self.assets.macos_universal)?;
        Ok(())
    }
}

fn validate_asset(platform: &'static str, asset: &PlatformAsset) -> Result<(), AtelierUpdateError> {
    if asset.format != "zip" {
        return Err(AtelierUpdateError::InvalidAsset {
            platform,
            field: "format",
            reason: "only zip archives are supported",
        });
    }
    if !is_safe_asset_filename(&asset.path) {
        return Err(AtelierUpdateError::InvalidAsset {
            platform,
            field: "path",
            reason: "expected a single portable archive filename",
        });
    }
    if !is_safe_zip_path(&asset.runtime_path, false) {
        return Err(AtelierUpdateError::InvalidAsset {
            platform,
            field: "runtime_path",
            reason: "expected a normalized relative ZIP path",
        });
    }
    if asset.size == 0 || asset.size > MAX_PORTABLE_BYTES as u64 {
        return Err(AtelierUpdateError::InvalidAsset {
            platform,
            field: "size",
            reason: "expected 1 to 536870912 bytes",
        });
    }
    if asset.sha256.len() != 64
        || !asset
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(AtelierUpdateError::InvalidAsset {
            platform,
            field: "sha256",
            reason: "expected 64 lowercase hexadecimal characters",
        });
    }
    Ok(())
}

fn is_safe_asset_filename(path: &str) -> bool {
    !path.is_empty() && path != "." && path != ".." && !path.contains(['/', '\\', '\0', '?', '#'])
}

fn is_safe_zip_path(path: &str, allow_trailing_slash: bool) -> bool {
    !path.contains(['\\', '\0']) && is_safe_slash_path(path, allow_trailing_slash)
}

fn is_safe_slash_path(path: &str, allow_trailing_slash: bool) -> bool {
    if path.is_empty() || path.starts_with('/') {
        return false;
    }
    let normalized = if allow_trailing_slash {
        path.strip_suffix('/').unwrap_or(path)
    } else {
        path
    };
    !normalized.is_empty()
        && normalized
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

#[derive(Clone, Debug)]
pub struct FetchedManifest {
    pub origin: Url,
    pub manifest: Vec<u8>,
    pub signature: Vec<u8>,
}

#[derive(Clone, Debug)]
pub enum ManifestCandidate {
    Fetched(FetchedManifest),
    Failed { origin: Url, error: String },
}

pub trait ManifestVerifier: Send + Sync + 'static {
    fn verify(&self, manifest: &[u8], signature: &[u8]) -> anyhow::Result<()>;
}

#[derive(Clone)]
pub struct Ed25519ManifestVerifier {
    key: VerifyingKey,
}

impl Ed25519ManifestVerifier {
    pub fn from_base64(public_key: &str) -> Result<Self, AtelierUpdateError> {
        let decoded = BASE64_STANDARD
            .decode(public_key.trim())
            .map_err(|error| AtelierUpdateError::InvalidPublicKey(error.to_string()))?;
        let key_bytes: [u8; 32] = decoded.try_into().map_err(|bytes: Vec<u8>| {
            AtelierUpdateError::InvalidPublicKey(format!(
                "expected 32 decoded bytes, got {}",
                bytes.len()
            ))
        })?;
        let key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|error| AtelierUpdateError::InvalidPublicKey(error.to_string()))?;
        Ok(Self { key })
    }
}

impl ManifestVerifier for Ed25519ManifestVerifier {
    fn verify(&self, manifest: &[u8], signature: &[u8]) -> anyhow::Result<()> {
        let encoded = std::str::from_utf8(signature)
            .map_err(|error| AtelierUpdateError::InvalidSignatureEncoding(error.to_string()))?
            .trim_end_matches(['\r', '\n']);
        let decoded = BASE64_STANDARD
            .decode(encoded)
            .map_err(|error| AtelierUpdateError::InvalidSignatureEncoding(error.to_string()))?;
        let signature_bytes: [u8; 64] = decoded.try_into().map_err(|bytes: Vec<u8>| {
            AtelierUpdateError::InvalidSignatureEncoding(format!(
                "expected 64 decoded bytes, got {}",
                bytes.len()
            ))
        })?;
        self.key
            .verify(manifest, &Signature::from_bytes(&signature_bytes))
            .map_err(|error| AtelierUpdateError::SignatureVerification(error.to_string()).into())
    }
}

#[async_trait]
pub trait UpdateSource: Send + Sync + 'static {
    async fn fetch_manifests(&self) -> Vec<ManifestCandidate>;

    async fn fetch_portable(&self, origin: &Url, path: &str) -> anyhow::Result<Vec<u8>>;
}

#[derive(Clone)]
pub struct HttpUpdateSource {
    client: reqwest::Client,
    feeds: Arc<[HttpUpdateFeed]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HttpUpdateFeed {
    pub manifest_url: Url,
    pub signature_url: Url,
    pub asset_base_url: Url,
}

impl HttpUpdateFeed {
    pub fn from_base_url(base_url: Url) -> Result<Self, AtelierUpdateError> {
        validate_https_url(&base_url, true)?;
        Ok(Self {
            manifest_url: base_url.join("update-manifest.json").map_err(|error| {
                AtelierUpdateError::InvalidFeedUrl {
                    url: base_url.clone(),
                    reason: if error == url::ParseError::RelativeUrlWithCannotBeABaseBase {
                        "expected an absolute hierarchical URL"
                    } else {
                        "could not derive manifest URL"
                    },
                }
            })?,
            signature_url: base_url.join("update-manifest.json.sig").map_err(|_| {
                AtelierUpdateError::InvalidFeedUrl {
                    url: base_url.clone(),
                    reason: "could not derive signature URL",
                }
            })?,
            asset_base_url: base_url,
        })
    }
}

impl HttpUpdateSource {
    pub fn new(feeds: Vec<HttpUpdateFeed>) -> Result<Self, AtelierUpdateError> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|error| AtelierUpdateError::AllSourcesFailed(error.to_string()))?;
        Self::with_client(client, feeds)
    }

    pub fn with_client(
        client: reqwest::Client,
        feeds: Vec<HttpUpdateFeed>,
    ) -> Result<Self, AtelierUpdateError> {
        if feeds.is_empty() {
            return Err(AtelierUpdateError::AllSourcesFailed(
                "no update feed was configured".to_owned(),
            ));
        }
        for feed in &feeds {
            validate_https_url(&feed.manifest_url, false)?;
            validate_https_url(&feed.signature_url, false)?;
            validate_https_url(&feed.asset_base_url, true)?;
        }
        Ok(Self {
            client,
            feeds: feeds.into(),
        })
    }

    async fn fetch_bytes(&self, url: Url, limit: usize) -> anyhow::Result<Vec<u8>> {
        let mut response = self
            .client
            .get(url.clone())
            .send()
            .await?
            .error_for_status()?;
        if response
            .content_length()
            .is_some_and(|length| length > limit as u64)
        {
            return Err(AtelierUpdateError::ResponseTooLarge { url, limit }.into());
        }
        let mut bytes = Vec::with_capacity(
            response
                .content_length()
                .unwrap_or_default()
                .min(limit as u64) as usize,
        );
        while let Some(chunk) = response.chunk().await? {
            if bytes
                .len()
                .checked_add(chunk.len())
                .is_none_or(|length| length > limit)
            {
                return Err(AtelierUpdateError::ResponseTooLarge { url, limit }.into());
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

#[async_trait]
impl UpdateSource for HttpUpdateSource {
    async fn fetch_manifests(&self) -> Vec<ManifestCandidate> {
        let mut candidates = Vec::with_capacity(self.feeds.len());
        for feed in self.feeds.iter() {
            let result = async {
                let manifest = self
                    .fetch_bytes(feed.manifest_url.clone(), MAX_MANIFEST_BYTES)
                    .await?;
                let signature = self
                    .fetch_bytes(feed.signature_url.clone(), MAX_SIGNATURE_BYTES)
                    .await?;
                Ok::<_, anyhow::Error>(FetchedManifest {
                    origin: feed.asset_base_url.clone(),
                    manifest,
                    signature,
                })
            }
            .await;
            match result {
                Ok(fetched) => candidates.push(ManifestCandidate::Fetched(fetched)),
                Err(error) => candidates.push(ManifestCandidate::Failed {
                    origin: feed.asset_base_url.clone(),
                    error: format!("{}: {error:#}", feed.manifest_url),
                }),
            }
        }
        candidates
    }

    async fn fetch_portable(&self, origin: &Url, path: &str) -> anyhow::Result<Vec<u8>> {
        if !self.feeds.iter().any(|feed| &feed.asset_base_url == origin) {
            return Err(AtelierUpdateError::UnknownManifestOrigin(origin.clone()).into());
        }
        if !is_safe_asset_filename(path) {
            return Err(AtelierUpdateError::InvalidAsset {
                platform: "selected",
                field: "path",
                reason: "expected a single portable archive filename",
            }
            .into());
        }
        self.fetch_bytes(origin.join(path)?, MAX_PORTABLE_BYTES)
            .await
    }
}

fn validate_https_url(url: &Url, require_trailing_slash: bool) -> Result<(), AtelierUpdateError> {
    let reason = if url.scheme() != "https" {
        Some("expected HTTPS")
    } else if url.cannot_be_a_base() {
        Some("expected an absolute hierarchical URL")
    } else if require_trailing_slash && !url.path().ends_with('/') {
        Some("expected an asset base URL ending in /")
    } else if !url.username().is_empty() || url.password().is_some() {
        Some("credentials are not allowed")
    } else if url.query().is_some() || url.fragment().is_some() {
        Some("query and fragment are not allowed")
    } else {
        None
    };
    if let Some(reason) = reason {
        return Err(AtelierUpdateError::InvalidFeedUrl {
            url: url.clone(),
            reason,
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AtelierUpdatePhase {
    #[default]
    Idle,
    Disabled,
    Checking,
    UpToDate,
    Available,
    FullPackageRequired,
    Installing,
    RestartRequired,
    CheckFailed,
    InstallFailed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AtelierUpdateSnapshot {
    pub phase: AtelierUpdatePhase,
    pub current_version: Option<Version>,
    pub available_version: Option<Version>,
    pub min_bootstrap_generation: Option<u32>,
    pub last_error: Option<String>,
}

impl AtelierUpdateSnapshot {
    #[must_use]
    pub fn disabled(current_version: Version, reason: impl Into<String>) -> Self {
        Self {
            phase: AtelierUpdatePhase::Disabled,
            current_version: Some(current_version),
            last_error: Some(reason.into()),
            ..Self::default()
        }
    }
}

#[derive(Clone)]
pub struct AtelierUpdateChecker {
    inner: Arc<AtelierUpdateCheckerInner>,
}

struct AtelierUpdateCheckerInner {
    source: Arc<dyn UpdateSource>,
    verifier: Arc<dyn ManifestVerifier>,
    platform: UpdatePlatform,
    current_version: Version,
    bootstrap_generation: u32,
    operation: AsyncMutex<()>,
    available: Mutex<Option<AvailableUpdate>>,
    state_sender: watch::Sender<AtelierUpdateSnapshot>,
}

#[derive(Clone)]
struct AvailableUpdate {
    origins: Vec<Url>,
    manifest: UpdateManifest,
}

fn verify_and_parse_candidate(
    verifier: &dyn ManifestVerifier,
    fetched: FetchedManifest,
) -> Result<(Url, UpdateManifest), String> {
    if fetched.signature.is_empty() {
        return Err(format!(
            "{}: {}",
            fetched.origin,
            AtelierUpdateError::EmptySignature
        ));
    }
    verifier
        .verify(&fetched.manifest, &fetched.signature)
        .map_err(|error| {
            format!(
                "{}: signature verification failed: {error:#}",
                fetched.origin
            )
        })?;
    let manifest = UpdateManifest::parse(&fetched.manifest)
        .map_err(|error| format!("{}: {error}", fetched.origin))?;
    Ok((fetched.origin, manifest))
}

impl AtelierUpdateChecker {
    #[must_use]
    pub fn new(
        source: Arc<dyn UpdateSource>,
        verifier: Arc<dyn ManifestVerifier>,
        platform: UpdatePlatform,
        current_version: Version,
        bootstrap_generation: u32,
    ) -> Self {
        let (state_sender, _) = watch::channel(AtelierUpdateSnapshot {
            phase: AtelierUpdatePhase::Idle,
            current_version: Some(current_version.clone()),
            ..AtelierUpdateSnapshot::default()
        });
        Self {
            inner: Arc::new(AtelierUpdateCheckerInner {
                source,
                verifier,
                platform,
                current_version,
                bootstrap_generation,
                operation: AsyncMutex::new(()),
                available: Mutex::new(None),
                state_sender,
            }),
        }
    }

    /// Schedules network work on Tokio and returns immediately. Returns false
    /// when a check is already in progress.
    pub fn check_in_background(&self) -> bool {
        if matches!(
            self.snapshot().phase,
            AtelierUpdatePhase::Disabled
                | AtelierUpdatePhase::Checking
                | AtelierUpdatePhase::Installing
                | AtelierUpdatePhase::RestartRequired
        ) {
            return false;
        }
        *self.inner.available.lock().expect("available update lock") = None;
        self.publish(AtelierUpdateSnapshot {
            phase: AtelierUpdatePhase::Checking,
            current_version: Some(self.inner.current_version.clone()),
            ..AtelierUpdateSnapshot::default()
        });
        let checker = self.clone();
        tokio::spawn(async move {
            checker.perform_check().await;
        });
        true
    }

    async fn perform_check(&self) {
        let _operation = self.inner.operation.lock().await;
        let candidates = self.inner.source.fetch_manifests().await;
        let mut failures = Vec::new();
        let mut selected: Option<AvailableUpdate> = None;
        for candidate in candidates {
            let fetched = match candidate {
                ManifestCandidate::Fetched(fetched) => fetched,
                ManifestCandidate::Failed { origin, error } => {
                    failures.push(format!("{origin}: {error}"));
                    continue;
                }
            };
            let candidate = verify_and_parse_candidate(self.inner.verifier.as_ref(), fetched);
            let (origin, manifest) = match candidate {
                Ok(candidate) => candidate,
                Err(error) => {
                    failures.push(error);
                    continue;
                }
            };
            match &mut selected {
                None => {
                    selected = Some(AvailableUpdate {
                        origins: vec![origin],
                        manifest,
                    });
                }
                Some(current) if manifest.version > current.manifest.version => {
                    *current = AvailableUpdate {
                        origins: vec![origin],
                        manifest,
                    };
                }
                Some(current)
                    if manifest.version == current.manifest.version
                        && manifest == current.manifest =>
                {
                    if !current.origins.contains(&origin) {
                        current.origins.push(origin);
                    }
                }
                Some(current) if manifest.version == current.manifest.version => {
                    failures.push(format!(
                        "{origin}: manifest conflicts with another valid manifest for {}",
                        manifest.version
                    ));
                }
                Some(_) => {}
            }
        }
        let Some(selected) = selected else {
            let summary = if failures.is_empty() {
                "no update source returned a manifest candidate".to_owned()
            } else {
                failures.join("; ")
            };
            self.fail_check(AtelierUpdateError::AllSourcesFailed(summary));
            return;
        };
        if !failures.is_empty() {
            tracing::warn!(errors = %failures.join("; "), "some Atelier update sources were ignored");
        }
        let manifest = &selected.manifest;
        if manifest.version <= self.inner.current_version {
            self.publish(AtelierUpdateSnapshot {
                phase: AtelierUpdatePhase::UpToDate,
                current_version: Some(self.inner.current_version.clone()),
                ..AtelierUpdateSnapshot::default()
            });
            return;
        }

        let phase = if manifest.min_bootstrap_generation > self.inner.bootstrap_generation {
            AtelierUpdatePhase::FullPackageRequired
        } else {
            AtelierUpdatePhase::Available
        };
        let available_version = manifest.version.clone();
        let min_bootstrap_generation = manifest.min_bootstrap_generation;
        *self.inner.available.lock().expect("available update lock") = Some(selected);
        self.publish(AtelierUpdateSnapshot {
            phase,
            current_version: Some(self.inner.current_version.clone()),
            available_version: Some(available_version),
            min_bootstrap_generation: Some(min_bootstrap_generation),
            last_error: None,
        });
    }

    /// Downloads and stages the selected Runtime. Network I/O is asynchronous;
    /// ZIP verification and filesystem work run on Tokio's blocking pool.
    pub async fn stage_available(&self, runtime_dir: &Path) -> anyhow::Result<StagedRuntime> {
        let _operation = self
            .inner
            .operation
            .try_lock()
            .map_err(|_| AtelierUpdateError::OperationInProgress)?;
        let snapshot = self.snapshot();
        if !matches!(
            snapshot.phase,
            AtelierUpdatePhase::Available | AtelierUpdatePhase::InstallFailed
        ) {
            return Err(AtelierUpdateError::NoAvailableUpdate.into());
        }
        let available = self
            .inner
            .available
            .lock()
            .expect("available update lock")
            .clone()
            .ok_or(AtelierUpdateError::NoAvailableUpdate)?;
        self.publish(AtelierUpdateSnapshot {
            phase: AtelierUpdatePhase::Installing,
            current_version: Some(self.inner.current_version.clone()),
            available_version: Some(available.manifest.version.clone()),
            min_bootstrap_generation: Some(available.manifest.min_bootstrap_generation),
            last_error: None,
        });
        let result = self.stage_selected(available, runtime_dir).await;
        match &result {
            Ok(staged) => self.publish(AtelierUpdateSnapshot {
                phase: AtelierUpdatePhase::Installing,
                current_version: Some(self.inner.current_version.clone()),
                available_version: Some(staged.version.clone()),
                min_bootstrap_generation: snapshot.min_bootstrap_generation,
                last_error: None,
            }),
            Err(error) => self.publish(AtelierUpdateSnapshot {
                phase: AtelierUpdatePhase::InstallFailed,
                current_version: Some(self.inner.current_version.clone()),
                available_version: snapshot.available_version,
                min_bootstrap_generation: snapshot.min_bootstrap_generation,
                last_error: Some(format!("{error:#}")),
            }),
        }
        result
    }

    /// Confirms that the caller persisted `pending.json` for this staged
    /// Runtime. Restart is never advertised before this acknowledgement.
    pub fn mark_restart_required(&self, staged: &StagedRuntime) -> Result<(), AtelierUpdateError> {
        let snapshot = self.snapshot();
        if snapshot.phase != AtelierUpdatePhase::Installing {
            return Err(AtelierUpdateError::InvalidStateTransition(
                "no Runtime is awaiting pending pointer confirmation",
            ));
        }
        if snapshot.available_version.as_ref() != Some(&staged.version) {
            return Err(AtelierUpdateError::InvalidStateTransition(
                "staged Runtime version does not match the available update",
            ));
        }
        self.publish(AtelierUpdateSnapshot {
            phase: AtelierUpdatePhase::RestartRequired,
            last_error: None,
            ..snapshot
        });
        Ok(())
    }

    /// Reports a failure that occurred after extraction, such as an atomic
    /// `pending.json` write failure. The available update remains retryable.
    pub fn mark_install_failed(
        &self,
        error: impl std::fmt::Display,
    ) -> Result<(), AtelierUpdateError> {
        let snapshot = self.snapshot();
        if snapshot.phase != AtelierUpdatePhase::Installing {
            return Err(AtelierUpdateError::InvalidStateTransition(
                "no Runtime installation is awaiting completion",
            ));
        }
        self.publish(AtelierUpdateSnapshot {
            phase: AtelierUpdatePhase::InstallFailed,
            last_error: Some(error.to_string()),
            ..snapshot
        });
        Ok(())
    }

    async fn stage_selected(
        &self,
        available: AvailableUpdate,
        runtime_dir: &Path,
    ) -> anyhow::Result<StagedRuntime> {
        if available.manifest.min_bootstrap_generation > self.inner.bootstrap_generation {
            return Err(AtelierUpdateError::BootstrapTooOld {
                version: available.manifest.version,
                required: available.manifest.min_bootstrap_generation,
                current: self.inner.bootstrap_generation,
            }
            .into());
        }
        let asset = available.manifest.asset_for(self.inner.platform).clone();
        let mut failures = Vec::new();
        for origin in available.origins {
            let archive = match self.inner.source.fetch_portable(&origin, &asset.path).await {
                Ok(archive) => archive,
                Err(error) => {
                    failures.push(format!("{origin}: {error:#}"));
                    continue;
                }
            };
            let manifest = available.manifest.clone();
            let platform = self.inner.platform;
            let runtime_dir = runtime_dir.to_owned();
            let result = tokio::task::spawn_blocking(move || {
                stage_runtime_from_zip_bytes(&archive, &manifest, platform, &runtime_dir)
            })
            .await
            .map_err(anyhow::Error::from)?;
            match result {
                Ok(staged) => return Ok(staged),
                Err(error) => failures.push(format!("{origin}: {error}")),
            }
        }
        Err(
            AtelierUpdateError::AllSourcesFailed(if failures.is_empty() {
                "the selected manifest has no artifact origins".to_owned()
            } else {
                failures.join("; ")
            })
            .into(),
        )
    }

    #[must_use]
    pub fn snapshot(&self) -> AtelierUpdateSnapshot {
        self.inner.state_sender.borrow().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<AtelierUpdateSnapshot> {
        self.inner.state_sender.subscribe()
    }

    fn fail_check(&self, error: impl std::fmt::Display) {
        tracing::warn!(%error, "background Atelier update check failed");
        self.publish(AtelierUpdateSnapshot {
            phase: AtelierUpdatePhase::CheckFailed,
            current_version: Some(self.inner.current_version.clone()),
            last_error: Some(error.to_string()),
            ..AtelierUpdateSnapshot::default()
        });
    }

    fn publish(&self, snapshot: AtelierUpdateSnapshot) {
        self.inner.state_sender.send_replace(snapshot);
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedRuntime {
    pub version: Version,
    pub executable: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct PendingRuntimeEntry {
    pub version: String,
    pub executable: PathBuf,
}

impl StagedRuntime {
    #[must_use]
    pub fn pending_entry(&self) -> PendingRuntimeEntry {
        PendingRuntimeEntry {
            version: self.version.to_string(),
            executable: self.executable.clone(),
        }
    }
}

pub fn stage_runtime_from_zip(
    archive_path: &Path,
    manifest: &UpdateManifest,
    platform: UpdatePlatform,
    runtime_dir: &Path,
) -> Result<StagedRuntime, AtelierUpdateError> {
    let asset = manifest.asset_for(platform);
    let metadata = fs::metadata(archive_path).map_err(|source| AtelierUpdateError::Io {
        action: "inspect",
        path: archive_path.to_owned(),
        source,
    })?;
    if metadata.len() != asset.size {
        return Err(AtelierUpdateError::ArchiveSizeMismatch {
            expected: asset.size,
            actual: metadata.len(),
        });
    }
    let mut archive = File::open(archive_path).map_err(|source| AtelierUpdateError::Io {
        action: "open",
        path: archive_path.to_owned(),
        source,
    })?;
    let actual_hash = sha256_reader(&mut archive).map_err(|source| AtelierUpdateError::Io {
        action: "hash",
        path: archive_path.to_owned(),
        source,
    })?;
    if actual_hash != asset.sha256 {
        return Err(AtelierUpdateError::ArchiveHashMismatch {
            expected: asset.sha256.clone(),
            actual: actual_hash,
        });
    }
    let archive = File::open(archive_path).map_err(|source| AtelierUpdateError::Io {
        action: "open",
        path: archive_path.to_owned(),
        source,
    })?;
    stage_verified_runtime(archive, manifest, platform, runtime_dir)
}

fn stage_runtime_from_zip_bytes(
    archive: &[u8],
    manifest: &UpdateManifest,
    platform: UpdatePlatform,
    runtime_dir: &Path,
) -> Result<StagedRuntime, AtelierUpdateError> {
    let asset = manifest.asset_for(platform);
    if archive.len() as u64 != asset.size {
        return Err(AtelierUpdateError::ArchiveSizeMismatch {
            expected: asset.size,
            actual: archive.len() as u64,
        });
    }
    let actual_hash = sha256_bytes(archive);
    if actual_hash != asset.sha256 {
        return Err(AtelierUpdateError::ArchiveHashMismatch {
            expected: asset.sha256.clone(),
            actual: actual_hash,
        });
    }
    stage_verified_runtime(io::Cursor::new(archive), manifest, platform, runtime_dir)
}

fn stage_verified_runtime<R: Read + io::Seek>(
    mut reader: R,
    manifest: &UpdateManifest,
    platform: UpdatePlatform,
    runtime_dir: &Path,
) -> Result<StagedRuntime, AtelierUpdateError> {
    let runtime_path = &manifest.asset_for(platform).runtime_path;
    let declared_entries = zip_entry_count(&mut reader)?;
    reader
        .seek(io::SeekFrom::Start(0))
        .map_err(|source| AtelierUpdateError::Io {
            action: "rewind",
            path: PathBuf::from("portable archive"),
            source,
        })?;
    let mut archive = zip::ZipArchive::new(reader)?;
    if declared_entries != archive.len() {
        return Err(AtelierUpdateError::DuplicateArchiveEntries);
    }
    let mut runtime_index = None;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name = entry.name().to_owned();
        if entry.enclosed_name().is_none() || !is_safe_zip_path(&name, entry.is_dir()) {
            return Err(AtelierUpdateError::UnsafeArchiveEntry(name));
        }
        if name == *runtime_path {
            if runtime_index.replace(index).is_some() {
                return Err(AtelierUpdateError::DuplicateRuntimeEntry(
                    runtime_path.clone(),
                ));
            }
            if entry.is_dir()
                || entry.encrypted()
                || entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                return Err(AtelierUpdateError::UnsupportedArchiveEntry(name));
            }
            if entry.size() > MAX_RUNTIME_BYTES {
                return Err(AtelierUpdateError::RuntimeTooLarge(entry.size()));
            }
        }
    }
    let runtime_index = runtime_index
        .ok_or_else(|| AtelierUpdateError::RuntimeEntryMissing(runtime_path.clone()))?;

    let version = manifest.version.to_string();
    let relative_executable = PathBuf::from("versions")
        .join(&version)
        .join(platform.runtime_executable_name());
    let destination = runtime_dir.join("versions").join(&version);
    let staging_root = runtime_dir.join("staging");
    fs::create_dir_all(&staging_root).map_err(|source| AtelierUpdateError::Io {
        action: "create directory",
        path: staging_root.clone(),
        source,
    })?;
    fs::create_dir_all(runtime_dir.join("versions")).map_err(|source| AtelierUpdateError::Io {
        action: "create directory",
        path: runtime_dir.join("versions"),
        source,
    })?;
    let staging = staging_root.join(format!("{version}-{}", fresh_staging_nonce()));
    fs::create_dir(&staging).map_err(|source| AtelierUpdateError::Io {
        action: "create directory",
        path: staging.clone(),
        source,
    })?;

    let result = (|| {
        let executable = staging.join(platform.runtime_executable_name());
        let mut entry = archive.by_index(runtime_index)?;
        let expected_size = entry.size();
        let mut output =
            File::create_new(&executable).map_err(|source| AtelierUpdateError::Io {
                action: "create",
                path: executable.clone(),
                source,
            })?;
        let actual_size = io::copy(&mut entry.by_ref().take(MAX_RUNTIME_BYTES + 1), &mut output)
            .map_err(|source| AtelierUpdateError::Io {
                action: "extract",
                path: executable.clone(),
                source,
            })?;
        if actual_size > MAX_RUNTIME_BYTES {
            return Err(AtelierUpdateError::RuntimeTooLarge(actual_size));
        }
        if actual_size != expected_size {
            return Err(AtelierUpdateError::RuntimeSizeMismatch {
                expected: expected_size,
                actual: actual_size,
            });
        }
        set_executable(&executable)?;
        output.sync_all().map_err(|source| AtelierUpdateError::Io {
            action: "sync",
            path: executable.clone(),
            source,
        })?;
        drop(output);
        drop(entry);
        if destination.exists() {
            reuse_identical_destination(
                &staging,
                &destination,
                platform.runtime_executable_name(),
            )?;
            return Ok(StagedRuntime {
                version: manifest.version.clone(),
                executable: relative_executable,
            });
        }
        if let Err(source) = fs::rename(&staging, &destination) {
            if destination.exists() {
                reuse_identical_destination(
                    &staging,
                    &destination,
                    platform.runtime_executable_name(),
                )?;
            } else {
                return Err(AtelierUpdateError::Io {
                    action: "activate",
                    path: destination.clone(),
                    source,
                });
            }
        }
        Ok(StagedRuntime {
            version: manifest.version.clone(),
            executable: relative_executable,
        })
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result
}

fn reuse_identical_destination(
    staging: &Path,
    destination: &Path,
    executable_name: &str,
) -> Result<(), AtelierUpdateError> {
    let mut entries = fs::read_dir(destination).map_err(|source| AtelierUpdateError::Io {
        action: "inspect",
        path: destination.to_owned(),
        source,
    })?;
    let Some(entry) = entries
        .next()
        .transpose()
        .map_err(|source| AtelierUpdateError::Io {
            action: "inspect",
            path: destination.to_owned(),
            source,
        })?
    else {
        return Err(AtelierUpdateError::VersionAlreadyInstalled(
            destination.to_owned(),
        ));
    };
    let has_additional_entry = entries
        .next()
        .transpose()
        .map_err(|source| AtelierUpdateError::Io {
            action: "inspect",
            path: destination.to_owned(),
            source,
        })?
        .is_some();
    if entry.file_name() != executable_name
        || has_additional_entry
        || !entry
            .file_type()
            .map_err(|source| AtelierUpdateError::Io {
                action: "inspect",
                path: entry.path(),
                source,
            })?
            .is_file()
        || !files_have_same_sha256(&entry.path(), &staging.join(executable_name))?
    {
        return Err(AtelierUpdateError::VersionAlreadyInstalled(
            destination.to_owned(),
        ));
    }
    fs::remove_dir_all(staging).map_err(|source| AtelierUpdateError::Io {
        action: "remove directory",
        path: staging.to_owned(),
        source,
    })
}

fn files_have_same_sha256(left: &Path, right: &Path) -> Result<bool, AtelierUpdateError> {
    let hash = |path: &Path| {
        let mut file = File::open(path).map_err(|source| AtelierUpdateError::Io {
            action: "open",
            path: path.to_owned(),
            source,
        })?;
        sha256_reader(&mut file).map_err(|source| AtelierUpdateError::Io {
            action: "hash",
            path: path.to_owned(),
            source,
        })
    };
    Ok(hash(left)? == hash(right)?)
}

fn zip_entry_count(reader: &mut (impl Read + io::Seek)) -> Result<usize, AtelierUpdateError> {
    let length = reader
        .seek(io::SeekFrom::End(0))
        .map_err(|source| AtelierUpdateError::Io {
            action: "inspect",
            path: PathBuf::from("portable archive"),
            source,
        })?;
    let tail_length = length.min(65_557) as usize;
    reader
        .seek(io::SeekFrom::End(-(tail_length as i64)))
        .map_err(|source| AtelierUpdateError::Io {
            action: "inspect",
            path: PathBuf::from("portable archive"),
            source,
        })?;
    let mut tail = vec![0_u8; tail_length];
    reader
        .read_exact(&mut tail)
        .map_err(|source| AtelierUpdateError::Io {
            action: "inspect",
            path: PathBuf::from("portable archive"),
            source,
        })?;
    for index in (0..tail.len().saturating_sub(21)).rev() {
        if tail[index..].starts_with(b"PK\x05\x06") {
            let comment_length = u16::from_le_bytes([tail[index + 20], tail[index + 21]]) as usize;
            if index + 22 + comment_length != tail.len() {
                continue;
            }
            let disk = u16::from_le_bytes([tail[index + 4], tail[index + 5]]);
            let central_disk = u16::from_le_bytes([tail[index + 6], tail[index + 7]]);
            let disk_entries = u16::from_le_bytes([tail[index + 8], tail[index + 9]]);
            let total_entries = u16::from_le_bytes([tail[index + 10], tail[index + 11]]);
            if disk != 0
                || central_disk != 0
                || disk_entries != total_entries
                || total_entries == u16::MAX
            {
                return Err(AtelierUpdateError::UnsupportedArchiveEntry(
                    "multi-disk or ZIP64 central directory".to_owned(),
                ));
            }
            return Ok(total_entries as usize);
        }
    }
    Err(zip::result::ZipError::InvalidArchive("missing end of central directory".into()).into())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), AtelierUpdateError> {
    use std::os::unix::fs::PermissionsExt;

    let mut permissions = fs::metadata(path)
        .map_err(|source| AtelierUpdateError::Io {
            action: "inspect",
            path: path.to_owned(),
            source,
        })?
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).map_err(|source| AtelierUpdateError::Io {
        action: "set permissions on",
        path: path.to_owned(),
        source,
    })
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), AtelierUpdateError> {
    Ok(())
}

fn sha256_reader(reader: &mut impl Read) -> io::Result<String> {
    let mut digest = Sha256::new();
    io::copy(reader, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn fresh_staging_nonce() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:08x}-{timestamp:032x}", std::process::id())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        io::{Cursor, Write},
        sync::atomic::{AtomicUsize, Ordering},
    };

    use ed25519_dalek::{Signer, SigningKey};
    use tokio::sync::Notify;
    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::*;

    const WINDOWS_RUNTIME_PATH: &str = "DSH-Atelier/dsh-atelier-runtime.exe";
    const MACOS_RUNTIME_PATH: &str =
        "DSH Atelier Portable/DSH Atelier.app/Contents/MacOS/dsh-atelier-runtime";

    #[test]
    fn parses_a_strict_stable_manifest() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let manifest = parse_manifest(&archive, WINDOWS_RUNTIME_PATH);

        assert_eq!(manifest.version, Version::parse("0.2.0").unwrap());
        assert_eq!(manifest.asset_for(UpdatePlatform::WindowsX64).format, "zip");
        assert_eq!(
            manifest.asset_for(UpdatePlatform::WindowsX64).runtime_path,
            WINDOWS_RUNTIME_PATH
        );
    }

    #[test]
    fn strict_manifest_rejects_unknown_fields_and_non_stable_versions() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let mut value: serde_json::Value =
            serde_json::from_slice(&manifest_bytes(&archive, WINDOWS_RUNTIME_PATH)).unwrap();
        value["unexpected"] = serde_json::json!(true);
        assert!(matches!(
            UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()),
            Err(AtelierUpdateError::InvalidManifestJson(_))
        ));

        value.as_object_mut().unwrap().remove("unexpected");
        value["version"] = serde_json::json!("0.2.0-rc.1");
        assert!(matches!(
            UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()),
            Err(AtelierUpdateError::UnstableVersion(_))
        ));
    }

    #[test]
    fn manifest_requires_the_canonical_version_tag() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let mut value: serde_json::Value =
            serde_json::from_slice(&manifest_bytes(&archive, WINDOWS_RUNTIME_PATH)).unwrap();
        value["tag"] = serde_json::json!("v0.2.0/../../unexpected");

        assert!(matches!(
            UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()),
            Err(AtelierUpdateError::InvalidReleaseTag { .. })
        ));
    }

    #[test]
    fn manifest_rejects_unsupported_archive_formats() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let mut value: serde_json::Value =
            serde_json::from_slice(&manifest_bytes(&archive, WINDOWS_RUNTIME_PATH)).unwrap();
        value["assets"]["macos-universal"]["format"] = serde_json::json!("dmg");

        assert!(matches!(
            UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()),
            Err(AtelierUpdateError::InvalidAsset {
                field: "format",
                ..
            })
        ));
    }

    #[test]
    fn manifest_rejects_unsafe_paths_and_noncanonical_hashes() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let mut value: serde_json::Value =
            serde_json::from_slice(&manifest_bytes(&archive, WINDOWS_RUNTIME_PATH)).unwrap();
        value["assets"]["windows-x64"]["runtime_path"] =
            serde_json::json!("../dsh-atelier-runtime.exe");
        assert!(matches!(
            UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()),
            Err(AtelierUpdateError::InvalidAsset {
                field: "runtime_path",
                ..
            })
        ));

        value["assets"]["windows-x64"]["runtime_path"] = serde_json::json!(WINDOWS_RUNTIME_PATH);
        value["assets"]["windows-x64"]["sha256"] = serde_json::json!("A".repeat(64));
        assert!(matches!(
            UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()),
            Err(AtelierUpdateError::InvalidAsset {
                field: "sha256",
                ..
            })
        ));

        value["assets"]["windows-x64"]["sha256"] = serde_json::json!(sha256_bytes(&archive));
        value["assets"]["windows-x64"]["size"] = serde_json::json!(MAX_PORTABLE_BYTES as u64 + 1);
        assert!(matches!(
            UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()),
            Err(AtelierUpdateError::InvalidAsset { field: "size", .. })
        ));
    }

    #[test]
    fn ed25519_verifier_accepts_only_the_signed_manifest() {
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let public_key = BASE64_STANDARD.encode(signing_key.verifying_key().as_bytes());
        let verifier = Ed25519ManifestVerifier::from_base64(&public_key).unwrap();
        let manifest = br#"{"schema":1}"#;
        let signature = BASE64_STANDARD.encode(signing_key.sign(manifest).to_bytes()) + "\n";

        verifier.verify(manifest, signature.as_bytes()).unwrap();
        assert!(verifier.verify(b"tampered", signature.as_bytes()).is_err());

        let mut tampered_signature = signing_key.sign(manifest).to_bytes();
        tampered_signature[0] ^= 1;
        assert!(
            verifier
                .verify(
                    manifest,
                    BASE64_STANDARD.encode(tampered_signature).as_bytes()
                )
                .is_err()
        );
    }

    #[test]
    fn ed25519_verifier_rejects_invalid_key_and_signature_encodings() {
        assert!(Ed25519ManifestVerifier::from_base64("not base64").is_err());
        assert!(Ed25519ManifestVerifier::from_base64(&BASE64_STANDARD.encode([0_u8; 31])).is_err());

        let signing_key = SigningKey::from_bytes(&[9_u8; 32]);
        let verifier = Ed25519ManifestVerifier::from_base64(
            &BASE64_STANDARD.encode(signing_key.verifying_key().as_bytes()),
        )
        .unwrap();
        assert!(verifier.verify(b"manifest", b"not base64").is_err());
        assert!(
            verifier
                .verify(b"manifest", BASE64_STANDARD.encode([0_u8; 63]).as_bytes())
                .is_err()
        );
    }

    #[test]
    fn extracts_only_the_allowlisted_runtime_and_returns_a_pending_entry() {
        let archive = make_zip(&[
            ("README.txt", b"ignored"),
            (WINDOWS_RUNTIME_PATH, b"runtime payload"),
        ]);
        let manifest = parse_manifest(&archive, WINDOWS_RUNTIME_PATH);
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("portable.zip");
        fs::write(&archive_path, &archive).unwrap();

        let staged = stage_runtime_from_zip(
            &archive_path,
            &manifest,
            UpdatePlatform::WindowsX64,
            &temporary.path().join("runtime"),
        )
        .unwrap();

        assert_eq!(
            fs::read(temporary.path().join("runtime").join(&staged.executable)).unwrap(),
            b"runtime payload"
        );
        assert!(
            !temporary
                .path()
                .join("runtime/versions/0.2.0/README.txt")
                .exists()
        );
        assert_eq!(
            staged.pending_entry(),
            PendingRuntimeEntry {
                version: "0.2.0".to_owned(),
                executable: PathBuf::from("versions/0.2.0/dsh-atelier-runtime.exe"),
            }
        );
    }

    #[test]
    fn rejects_archive_size_and_hash_mismatches_before_extracting() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let manifest = parse_manifest(&archive, WINDOWS_RUNTIME_PATH);
        let temporary = tempfile::tempdir().unwrap();
        let archive_path = temporary.path().join("portable.zip");
        fs::write(&archive_path, &archive[..archive.len() - 1]).unwrap();
        assert!(matches!(
            stage_runtime_from_zip(
                &archive_path,
                &manifest,
                UpdatePlatform::WindowsX64,
                &temporary.path().join("runtime")
            ),
            Err(AtelierUpdateError::ArchiveSizeMismatch { .. })
        ));

        fs::write(&archive_path, &archive).unwrap();
        let mut value: serde_json::Value =
            serde_json::from_slice(&manifest_bytes(&archive, WINDOWS_RUNTIME_PATH)).unwrap();
        value["assets"]["windows-x64"]["sha256"] = serde_json::json!("0".repeat(64));
        let bad_hash = UpdateManifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(matches!(
            stage_runtime_from_zip(
                &archive_path,
                &bad_hash,
                UpdatePlatform::WindowsX64,
                &temporary.path().join("runtime")
            ),
            Err(AtelierUpdateError::ArchiveHashMismatch { .. })
        ));
    }

    #[test]
    fn restaging_is_idempotent_only_for_the_identical_runtime() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let manifest = parse_manifest(&archive, WINDOWS_RUNTIME_PATH);
        let temporary = tempfile::tempdir().unwrap();
        let runtime_dir = temporary.path().join("runtime");

        let first = stage_runtime_from_zip_bytes(
            &archive,
            &manifest,
            UpdatePlatform::WindowsX64,
            &runtime_dir,
        )
        .unwrap();
        let second = stage_runtime_from_zip_bytes(
            &archive,
            &manifest,
            UpdatePlatform::WindowsX64,
            &runtime_dir,
        )
        .unwrap();
        assert_eq!(first, second);

        fs::write(runtime_dir.join(&first.executable), b"tampered").unwrap();
        assert!(matches!(
            stage_runtime_from_zip_bytes(
                &archive,
                &manifest,
                UpdatePlatform::WindowsX64,
                &runtime_dir,
            ),
            Err(AtelierUpdateError::VersionAlreadyInstalled(_))
        ));
    }

    #[test]
    fn rejects_traversal_and_duplicate_runtime_entries() {
        let traversal_archive = make_zip(&[
            ("../outside", b"malicious"),
            (WINDOWS_RUNTIME_PATH, b"runtime"),
        ]);
        let traversal_manifest = parse_manifest(&traversal_archive, WINDOWS_RUNTIME_PATH);
        let temporary = tempfile::tempdir().unwrap();
        assert!(matches!(
            stage_runtime_from_zip_bytes(
                &traversal_archive,
                &traversal_manifest,
                UpdatePlatform::WindowsX64,
                &temporary.path().join("runtime")
            ),
            Err(AtelierUpdateError::UnsafeArchiveEntry(_))
        ));
        assert!(!temporary.path().join("outside").exists());

        let alias = "DSH-Atelier/dsh-atelier-runtime.exf";
        let mut duplicate_archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"one"), (alias, b"two")]);
        replace_all_exact(
            &mut duplicate_archive,
            alias.as_bytes(),
            WINDOWS_RUNTIME_PATH.as_bytes(),
        );
        let duplicate_manifest = parse_manifest(&duplicate_archive, WINDOWS_RUNTIME_PATH);
        let duplicate_result = stage_runtime_from_zip_bytes(
            &duplicate_archive,
            &duplicate_manifest,
            UpdatePlatform::WindowsX64,
            &temporary.path().join("runtime"),
        );
        assert!(
            matches!(
                duplicate_result,
                Err(AtelierUpdateError::DuplicateArchiveEntries)
            ),
            "unexpected duplicate result: {duplicate_result:?}"
        );
    }

    #[tokio::test]
    async fn background_check_does_not_wait_for_the_update_source() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let release = Arc::new(Notify::new());
        let source = Arc::new(FixtureSource {
            fetched: fixture_fetched_manifest(&archive, WINDOWS_RUNTIME_PATH),
            release: Arc::clone(&release),
            fetches: AtomicUsize::new(0),
            archive,
        });
        let checker = AtelierUpdateChecker::new(
            source.clone(),
            Arc::new(FixtureVerifier { accepts: true }),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );

        assert!(checker.check_in_background());
        assert_eq!(checker.snapshot().phase, AtelierUpdatePhase::Checking);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while source.fetches.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(checker.snapshot().phase, AtelierUpdatePhase::Checking);
        release.notify_one();
        wait_for_phase(&checker, AtelierUpdatePhase::Available).await;
    }

    #[tokio::test]
    async fn invalid_signature_never_exposes_an_update() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let release = Arc::new(Notify::new());
        let source = Arc::new(FixtureSource {
            fetched: fixture_fetched_manifest(&archive, WINDOWS_RUNTIME_PATH),
            release: Arc::clone(&release),
            fetches: AtomicUsize::new(0),
            archive,
        });
        let checker = AtelierUpdateChecker::new(
            source,
            Arc::new(FixtureVerifier { accepts: false }),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );
        checker.check_in_background();
        release.notify_one();

        wait_for_phase(&checker, AtelierUpdatePhase::CheckFailed).await;

        assert!(checker.snapshot().available_version.is_none());
        assert!(checker.stage_available(Path::new("unused")).await.is_err());
    }

    #[tokio::test]
    async fn invalid_signature_falls_back_to_the_next_manifest_source() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let first = Url::parse("https://first.example/").unwrap();
        let second = Url::parse("https://second.example/").unwrap();
        let source = Arc::new(MultiFixtureSource::new(vec![
            ManifestCandidate::Fetched(fetched_manifest_at(
                &archive,
                WINDOWS_RUNTIME_PATH,
                "0.2.0",
                first,
                b"invalid",
            )),
            ManifestCandidate::Fetched(fetched_manifest_at(
                &archive,
                WINDOWS_RUNTIME_PATH,
                "0.2.0",
                second.clone(),
                b"valid",
            )),
        ]));
        let checker = AtelierUpdateChecker::new(
            source,
            Arc::new(ExactSignatureVerifier),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );

        checker.check_in_background();
        wait_for_phase(&checker, AtelierUpdatePhase::Available).await;

        let available = checker.inner.available.lock().unwrap().clone().unwrap();
        assert_eq!(available.origins, vec![second]);
    }

    #[tokio::test]
    async fn a_newer_mirror_manifest_wins_over_an_older_primary() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let first = Url::parse("https://first.example/").unwrap();
        let second = Url::parse("https://second.example/").unwrap();
        let source = Arc::new(MultiFixtureSource::new(vec![
            ManifestCandidate::Fetched(fetched_manifest_at(
                &archive,
                WINDOWS_RUNTIME_PATH,
                "0.2.0",
                first,
                b"valid",
            )),
            ManifestCandidate::Fetched(fetched_manifest_at(
                &archive,
                WINDOWS_RUNTIME_PATH,
                "0.3.0",
                second.clone(),
                b"valid",
            )),
        ]));
        let checker = AtelierUpdateChecker::new(
            source,
            Arc::new(ExactSignatureVerifier),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );

        checker.check_in_background();
        wait_for_phase(&checker, AtelierUpdatePhase::Available).await;

        assert_eq!(
            checker.snapshot().available_version.unwrap().to_string(),
            "0.3.0"
        );
        let available = checker.inner.available.lock().unwrap().clone().unwrap();
        assert_eq!(available.origins, vec![second]);
    }

    #[tokio::test]
    async fn manifest_failures_are_aggregated_when_no_source_is_valid() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let first = Url::parse("https://first.example/").unwrap();
        let second = Url::parse("https://second.example/").unwrap();
        let source = Arc::new(MultiFixtureSource::new(vec![
            ManifestCandidate::Failed {
                origin: first.clone(),
                error: "fixture timeout".to_owned(),
            },
            ManifestCandidate::Fetched(fetched_manifest_at(
                &archive,
                WINDOWS_RUNTIME_PATH,
                "0.2.0",
                second.clone(),
                b"invalid",
            )),
        ]));
        let checker = AtelierUpdateChecker::new(
            source,
            Arc::new(ExactSignatureVerifier),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );

        checker.check_in_background();
        wait_for_phase(&checker, AtelierUpdatePhase::CheckFailed).await;

        let error = checker.snapshot().last_error.unwrap();
        assert!(error.contains(first.as_str()));
        assert!(error.contains("fixture timeout"));
        assert!(error.contains(second.as_str()));
        assert!(error.contains("signature verification failed"));
    }

    #[tokio::test]
    async fn portable_download_falls_back_to_the_next_valid_manifest_origin() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let first = Url::parse("https://first.example/").unwrap();
        let second = Url::parse("https://second.example/").unwrap();
        let source = Arc::new(MultiFixtureSource::new(vec![
            ManifestCandidate::Fetched(fetched_manifest_at(
                &archive,
                WINDOWS_RUNTIME_PATH,
                "0.2.0",
                first.clone(),
                b"valid",
            )),
            ManifestCandidate::Fetched(fetched_manifest_at(
                &archive,
                WINDOWS_RUNTIME_PATH,
                "0.2.0",
                second.clone(),
                b"valid",
            )),
        ]));
        source.set_portable(first.clone(), PortableFixture::Failure("404".to_owned()));
        source.set_portable(second.clone(), PortableFixture::Bytes(archive));
        let checker = AtelierUpdateChecker::new(
            source.clone(),
            Arc::new(ExactSignatureVerifier),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );
        checker.check_in_background();
        wait_for_phase(&checker, AtelierUpdatePhase::Available).await;

        let temporary = tempfile::tempdir().unwrap();
        let staged = checker
            .stage_available(&temporary.path().join("runtime"))
            .await
            .unwrap();

        assert!(
            temporary
                .path()
                .join("runtime")
                .join(staged.executable)
                .is_file()
        );
        assert_eq!(
            source.portable_requests.lock().unwrap().as_slice(),
            [first, second]
        );
    }

    #[tokio::test]
    async fn bootstrap_generation_can_require_a_full_package() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let release = Arc::new(Notify::new());
        let mut fetched = fixture_fetched_manifest(&archive, WINDOWS_RUNTIME_PATH);
        let mut value: serde_json::Value = serde_json::from_slice(&fetched.manifest).unwrap();
        value["min_bootstrap_generation"] = serde_json::json!(2);
        fetched.manifest = serde_json::to_vec(&value).unwrap();
        let checker = AtelierUpdateChecker::new(
            Arc::new(FixtureSource {
                fetched,
                release: Arc::clone(&release),
                fetches: AtomicUsize::new(0),
                archive,
            }),
            Arc::new(FixtureVerifier { accepts: true }),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );
        checker.check_in_background();
        release.notify_one();

        wait_for_phase(&checker, AtelierUpdatePhase::FullPackageRequired).await;

        assert!(checker.stage_available(Path::new("unused")).await.is_err());
    }

    #[tokio::test]
    async fn install_runs_in_background_pool_and_rejects_double_clicks() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let portable_started = Arc::new(Notify::new());
        let portable_release = Arc::new(Notify::new());
        let checker = AtelierUpdateChecker::new(
            Arc::new(InstallFixtureSource {
                fetched: fixture_fetched_manifest(&archive, WINDOWS_RUNTIME_PATH),
                archive,
                portable_started: Arc::clone(&portable_started),
                portable_release: Arc::clone(&portable_release),
                fail_portable: false,
            }),
            Arc::new(FixtureVerifier { accepts: true }),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );
        checker.check_in_background();
        wait_for_phase(&checker, AtelierUpdatePhase::Available).await;
        let temporary = tempfile::tempdir().unwrap();
        let runtime_dir = temporary.path().join("runtime");
        let installing_checker = checker.clone();
        let installing_runtime_dir = runtime_dir.clone();
        let install = tokio::spawn(async move {
            installing_checker
                .stage_available(&installing_runtime_dir)
                .await
        });
        portable_started.notified().await;

        assert_eq!(checker.snapshot().phase, AtelierUpdatePhase::Installing);
        assert!(!checker.check_in_background());
        assert!(matches!(
            checker
                .stage_available(&runtime_dir)
                .await
                .unwrap_err()
                .downcast_ref::<AtelierUpdateError>(),
            Some(AtelierUpdateError::OperationInProgress)
        ));

        portable_release.notify_one();
        let staged = install.await.unwrap().unwrap();
        assert!(runtime_dir.join(&staged.executable).is_file());
        assert_eq!(checker.snapshot().phase, AtelierUpdatePhase::Installing);
        assert!(!checker.check_in_background());
        checker.mark_restart_required(&staged).unwrap();
        assert_eq!(
            checker.snapshot().phase,
            AtelierUpdatePhase::RestartRequired
        );
        assert!(!checker.check_in_background());
    }

    #[tokio::test]
    async fn failed_install_retains_the_update_for_an_explicit_retry() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let portable_started = Arc::new(Notify::new());
        let portable_release = Arc::new(Notify::new());
        let checker = AtelierUpdateChecker::new(
            Arc::new(InstallFixtureSource {
                fetched: fixture_fetched_manifest(&archive, WINDOWS_RUNTIME_PATH),
                archive,
                portable_started,
                portable_release,
                fail_portable: true,
            }),
            Arc::new(FixtureVerifier { accepts: true }),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );
        checker.check_in_background();
        wait_for_phase(&checker, AtelierUpdatePhase::Available).await;

        assert!(
            checker
                .stage_available(tempfile::tempdir().unwrap().path())
                .await
                .is_err()
        );

        let snapshot = checker.snapshot();
        assert_eq!(snapshot.phase, AtelierUpdatePhase::InstallFailed);
        assert_eq!(snapshot.available_version.unwrap().to_string(), "0.2.0");
        assert!(snapshot.last_error.is_some());
        assert!(
            checker
                .stage_available(tempfile::tempdir().unwrap().path())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn pointer_failure_after_staging_is_reported_without_restart_required() {
        let archive = make_zip(&[(WINDOWS_RUNTIME_PATH, b"runtime")]);
        let portable_started = Arc::new(Notify::new());
        let portable_release = Arc::new(Notify::new());
        let checker = AtelierUpdateChecker::new(
            Arc::new(InstallFixtureSource {
                fetched: fixture_fetched_manifest(&archive, WINDOWS_RUNTIME_PATH),
                archive,
                portable_started: Arc::clone(&portable_started),
                portable_release: Arc::clone(&portable_release),
                fail_portable: false,
            }),
            Arc::new(FixtureVerifier { accepts: true }),
            UpdatePlatform::WindowsX64,
            Version::parse("0.1.0").unwrap(),
            1,
        );
        checker.check_in_background();
        wait_for_phase(&checker, AtelierUpdatePhase::Available).await;
        let temporary = tempfile::tempdir().unwrap();
        let installing_checker = checker.clone();
        let runtime_dir = temporary.path().join("runtime");
        let install =
            tokio::spawn(async move { installing_checker.stage_available(&runtime_dir).await });
        portable_started.notified().await;
        portable_release.notify_one();
        let staged = install.await.unwrap().unwrap();

        assert_eq!(checker.snapshot().phase, AtelierUpdatePhase::Installing);
        checker
            .mark_install_failed("could not persist pending.json")
            .unwrap();
        assert_eq!(checker.snapshot().phase, AtelierUpdatePhase::InstallFailed);
        assert!(checker.mark_restart_required(&staged).is_err());
    }

    #[test]
    fn disabled_snapshot_is_explicit_and_does_not_look_like_a_failure() {
        let snapshot = AtelierUpdateSnapshot::disabled(
            Version::parse("0.1.0").unwrap(),
            "public key is not configured",
        );
        assert_eq!(snapshot.phase, AtelierUpdatePhase::Disabled);
        assert_eq!(snapshot.current_version.unwrap().to_string(), "0.1.0");
        assert_eq!(
            snapshot.last_error.as_deref(),
            Some("public key is not configured")
        );
    }

    #[test]
    fn http_sources_require_credential_free_https_base_urls() {
        let valid = HttpUpdateFeed {
            manifest_url: Url::parse("https://updates.example.com/stable/latest.json").unwrap(),
            signature_url: Url::parse("https://updates.example.com/stable/latest.json.sig")
                .unwrap(),
            asset_base_url: Url::parse("https://updates.example.com/stable/").unwrap(),
        };
        assert!(HttpUpdateSource::new(vec![valid.clone()]).is_ok());
        for invalid in [
            "http://updates.example.com/",
            "https://user:secret@updates.example.com/",
            "https://updates.example.com/feed",
            "https://updates.example.com/?mirror=one",
        ] {
            let mut feed = valid.clone();
            feed.asset_base_url = Url::parse(invalid).unwrap();
            assert!(HttpUpdateSource::new(vec![feed]).is_err());
        }
    }

    #[test]
    fn http_feed_derives_manifest_and_assets_from_a_github_release_base() {
        let feed = HttpUpdateFeed::from_base_url(
            Url::parse("https://github.com/mwbimh/dsh-atelier/releases/latest/download/").unwrap(),
        )
        .unwrap();

        assert_eq!(
            feed.manifest_url.as_str(),
            "https://github.com/mwbimh/dsh-atelier/releases/latest/download/update-manifest.json"
        );
        assert_eq!(
            feed.signature_url.as_str(),
            "https://github.com/mwbimh/dsh-atelier/releases/latest/download/update-manifest.json.sig"
        );
        assert_eq!(
            feed.asset_base_url.as_str(),
            "https://github.com/mwbimh/dsh-atelier/releases/latest/download/"
        );
        assert!(
            HttpUpdateFeed::from_base_url(
                Url::parse("https://github.com/mwbimh/dsh-atelier/releases/latest/download")
                    .unwrap()
            )
            .is_err()
        );
    }

    struct FixtureVerifier {
        accepts: bool,
    }

    struct ExactSignatureVerifier;

    impl ManifestVerifier for ExactSignatureVerifier {
        fn verify(&self, _manifest: &[u8], signature: &[u8]) -> anyhow::Result<()> {
            if signature == b"valid" {
                Ok(())
            } else {
                anyhow::bail!("fixture signature rejected")
            }
        }
    }

    #[derive(Clone)]
    enum PortableFixture {
        Bytes(Vec<u8>),
        Failure(String),
    }

    struct MultiFixtureSource {
        candidates: Vec<ManifestCandidate>,
        portables: Mutex<HashMap<String, PortableFixture>>,
        portable_requests: Mutex<Vec<Url>>,
    }

    impl MultiFixtureSource {
        fn new(candidates: Vec<ManifestCandidate>) -> Self {
            Self {
                candidates,
                portables: Mutex::new(HashMap::new()),
                portable_requests: Mutex::new(Vec::new()),
            }
        }

        fn set_portable(&self, origin: Url, fixture: PortableFixture) {
            self.portables
                .lock()
                .unwrap()
                .insert(origin.to_string(), fixture);
        }
    }

    #[async_trait]
    impl UpdateSource for MultiFixtureSource {
        async fn fetch_manifests(&self) -> Vec<ManifestCandidate> {
            self.candidates.clone()
        }

        async fn fetch_portable(&self, origin: &Url, _path: &str) -> anyhow::Result<Vec<u8>> {
            self.portable_requests.lock().unwrap().push(origin.clone());
            match self.portables.lock().unwrap().get(origin.as_str()).cloned() {
                Some(PortableFixture::Bytes(bytes)) => Ok(bytes),
                Some(PortableFixture::Failure(error)) => anyhow::bail!(error),
                None => anyhow::bail!("no fixture portable for {origin}"),
            }
        }
    }

    impl ManifestVerifier for FixtureVerifier {
        fn verify(&self, _manifest: &[u8], _signature: &[u8]) -> anyhow::Result<()> {
            if self.accepts {
                Ok(())
            } else {
                anyhow::bail!("fixture signature rejected")
            }
        }
    }

    struct FixtureSource {
        fetched: FetchedManifest,
        release: Arc<Notify>,
        fetches: AtomicUsize,
        archive: Vec<u8>,
    }

    struct InstallFixtureSource {
        fetched: FetchedManifest,
        archive: Vec<u8>,
        portable_started: Arc<Notify>,
        portable_release: Arc<Notify>,
        fail_portable: bool,
    }

    #[async_trait]
    impl UpdateSource for InstallFixtureSource {
        async fn fetch_manifests(&self) -> Vec<ManifestCandidate> {
            vec![ManifestCandidate::Fetched(self.fetched.clone())]
        }

        async fn fetch_portable(&self, _origin: &Url, _path: &str) -> anyhow::Result<Vec<u8>> {
            if self.fail_portable {
                anyhow::bail!("fixture download failed");
            }
            self.portable_started.notify_waiters();
            self.portable_release.notified().await;
            Ok(self.archive.clone())
        }
    }

    #[async_trait]
    impl UpdateSource for FixtureSource {
        async fn fetch_manifests(&self) -> Vec<ManifestCandidate> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            self.release.notified().await;
            vec![ManifestCandidate::Fetched(self.fetched.clone())]
        }

        async fn fetch_portable(&self, _origin: &Url, _path: &str) -> anyhow::Result<Vec<u8>> {
            Ok(self.archive.clone())
        }
    }

    async fn wait_for_phase(checker: &AtelierUpdateChecker, expected: AtelierUpdatePhase) {
        let mut updates = checker.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while updates.borrow().phase != expected {
                updates.changed().await.unwrap();
            }
        })
        .await
        .expect("update phase should change");
    }

    fn fixture_fetched_manifest(archive: &[u8], runtime_path: &str) -> FetchedManifest {
        fetched_manifest_at(
            archive,
            runtime_path,
            "0.2.0",
            Url::parse("https://updates.example.com/").unwrap(),
            b"fixture signature",
        )
    }

    fn fetched_manifest_at(
        archive: &[u8],
        runtime_path: &str,
        version: &str,
        origin: Url,
        signature: &[u8],
    ) -> FetchedManifest {
        FetchedManifest {
            origin,
            manifest: manifest_bytes_for_version(archive, runtime_path, version),
            signature: signature.to_vec(),
        }
    }

    fn parse_manifest(archive: &[u8], runtime_path: &str) -> UpdateManifest {
        UpdateManifest::parse(&manifest_bytes(archive, runtime_path)).unwrap()
    }

    fn manifest_bytes(archive: &[u8], windows_runtime_path: &str) -> Vec<u8> {
        manifest_bytes_for_version(archive, windows_runtime_path, "0.2.0")
    }

    fn manifest_bytes_for_version(
        archive: &[u8],
        windows_runtime_path: &str,
        version: &str,
    ) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "channel": "stable",
            "version": version,
            "tag": format!("v{version}"),
            "min_bootstrap_generation": 1,
            "assets": {
                "windows-x64": {
                    "path": format!("dsh-atelier-v{version}-windows-x64.zip"),
                    "format": "zip",
                    "runtime_path": windows_runtime_path,
                    "size": archive.len(),
                    "sha256": sha256_bytes(archive),
                },
                "macos-universal": {
                    "path": format!("dsh-atelier-v{version}-macos-universal-portable.zip"),
                    "format": "zip",
                    "runtime_path": MACOS_RUNTIME_PATH,
                    "size": archive.len(),
                    "sha256": sha256_bytes(archive),
                }
            }
        }))
        .unwrap()
    }

    fn make_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (path, bytes) in entries {
            writer
                .start_file(*path, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn replace_all_exact(bytes: &mut [u8], from: &[u8], to: &[u8]) {
        assert_eq!(from.len(), to.len());
        let mut replacements = 0;
        for index in 0..=bytes.len() - from.len() {
            if &bytes[index..index + from.len()] == from {
                bytes[index..index + to.len()].copy_from_slice(to);
                replacements += 1;
            }
        }
        assert_eq!(replacements, 2, "local and central ZIP names should change");
    }
}
