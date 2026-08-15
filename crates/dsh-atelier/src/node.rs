use std::{
    fs::{self, File},
    io::{self, BufReader, Write},
    path::{Component, Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use flate2::read::GzDecoder;
use semver::Version;
use sha2::{Digest, Sha256};
use thiserror::Error;
use url::Url;

use crate::process::{CommandOutput, CommandSpec, ProcessError, run_once};

pub const DEFAULT_NODE_VERSION: &str = "24.19.0";
pub const MINIMUM_NODE_MAJOR: u64 = 24;
pub const NODE_DIST_BASE_URL: &str = "https://nodejs.org/dist/";
pub const NODE_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub const MAX_NODE_ARCHIVE_BYTES: usize = 128 * 1024 * 1024;
pub const MAX_SHASUMS_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodePlatform {
    Windows,
    MacOs,
}

impl NodePlatform {
    pub fn current() -> Result<Self, NodeError> {
        match std::env::consts::OS {
            "windows" => Ok(Self::Windows),
            "macos" => Ok(Self::MacOs),
            other => Err(NodeError::UnsupportedPlatform(other.to_owned())),
        }
    }

    const fn artifact_segment(self) -> &'static str {
        match self {
            Self::Windows => "win",
            Self::MacOs => "darwin",
        }
    }

    const fn archive_format(self) -> NodeArchiveFormat {
        match self {
            Self::Windows => NodeArchiveFormat::Zip,
            Self::MacOs => NodeArchiveFormat::TarGz,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeArchitecture {
    X64,
    Arm64,
}

impl NodeArchitecture {
    pub fn current() -> Result<Self, NodeError> {
        match std::env::consts::ARCH {
            "x86_64" => Ok(Self::X64),
            "aarch64" => Ok(Self::Arm64),
            other => Err(NodeError::UnsupportedArchitecture(other.to_owned())),
        }
    }

    const fn artifact_segment(self) -> &'static str {
        match self {
            Self::X64 => "x64",
            Self::Arm64 => "arm64",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeArchiveFormat {
    Zip,
    TarGz,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeDistribution {
    pub version: Version,
    pub platform: NodePlatform,
    pub architecture: NodeArchitecture,
    pub archive_format: NodeArchiveFormat,
    pub directory_name: String,
    pub artifact_name: String,
    pub artifact_url: Url,
    pub shasums_url: Url,
}

impl NodeDistribution {
    pub fn new(
        version: &Version,
        platform: NodePlatform,
        architecture: NodeArchitecture,
    ) -> Result<Self, NodeError> {
        let directory_name = format!(
            "node-v{version}-{}-{}",
            platform.artifact_segment(),
            architecture.artifact_segment()
        );
        let archive_format = platform.archive_format();
        let extension = match archive_format {
            NodeArchiveFormat::Zip => "zip",
            NodeArchiveFormat::TarGz => "tar.gz",
        };
        let artifact_name = format!("{directory_name}.{extension}");
        let version_base = Url::parse(NODE_DIST_BASE_URL)
            .expect("the hard-coded Node distribution URL is valid")
            .join(&format!("v{version}/"))
            .map_err(NodeError::InvalidDistributionUrl)?;
        let artifact_url = version_base
            .join(&artifact_name)
            .map_err(NodeError::InvalidDistributionUrl)?;
        let shasums_url = version_base
            .join("SHASUMS256.txt")
            .map_err(NodeError::InvalidDistributionUrl)?;

        Ok(Self {
            version: version.clone(),
            platform,
            architecture,
            archive_format,
            directory_name,
            artifact_name,
            artifact_url,
            shasums_url,
        })
    }

    pub fn default_for_current_platform() -> Result<Self, NodeError> {
        let version = Version::parse(DEFAULT_NODE_VERSION)
            .expect("the hard-coded default Node version is valid");
        Self::new(
            &version,
            NodePlatform::current()?,
            NodeArchitecture::current()?,
        )
    }

    #[must_use]
    pub fn node_relative_path(&self) -> PathBuf {
        match self.platform {
            NodePlatform::Windows => PathBuf::from("node.exe"),
            NodePlatform::MacOs => PathBuf::from("bin/node"),
        }
    }

    #[must_use]
    pub fn npm_cli_relative_path(&self) -> PathBuf {
        match self.platform {
            NodePlatform::Windows => PathBuf::from("node_modules/npm/bin/npm-cli.js"),
            NodePlatform::MacOs => PathBuf::from("lib/node_modules/npm/bin/npm-cli.js"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProbedNode {
    pub program: PathBuf,
    pub version: Version,
}

#[derive(Debug, Error)]
pub enum NodeError {
    #[error("unsupported operating system for managed Node: {0}")]
    UnsupportedPlatform(String),
    #[error("unsupported CPU architecture for managed Node: {0}")]
    UnsupportedArchitecture(String),
    #[error("invalid Node distribution URL: {0}")]
    InvalidDistributionUrl(url::ParseError),
    #[error("Node version output is empty")]
    EmptyVersion,
    #[error("Node version output contains multiple lines")]
    MultilineVersion,
    #[error("invalid Node semantic version: {0}")]
    InvalidVersion(String),
    #[error("failed to execute Node: {0}")]
    Probe(#[from] ProcessError),
    #[error("Node --version exited with status {status}: {stderr}")]
    ProbeExit { status: i32, stderr: String },
    #[error("invalid SHASUMS256.txt line {line}: {reason}")]
    InvalidShasumLine { line: usize, reason: String },
    #[error("SHASUMS256.txt has no entry for {0}")]
    MissingShasum(String),
    #[error("SHASUMS256.txt has duplicate entries for {0}")]
    DuplicateShasum(String),
    #[error("SHA-256 mismatch for {path}")]
    ChecksumMismatch { path: PathBuf },
    #[error("download failed: {0}")]
    Download(#[from] reqwest::Error),
    #[error("download from {url} exceeded the {limit}-byte limit")]
    DownloadTooLarge { url: Url, limit: usize },
    #[error("download destination already exists: {0}")]
    DownloadDestinationExists(PathBuf),
    #[error("archive destination already exists: {0}")]
    ExtractDestinationExists(PathBuf),
    #[error("installed Node destination already exists: {0}")]
    InstallDestinationExists(PathBuf),
    #[error("unsafe or unexpected archive entry: {0}")]
    UnsafeArchiveEntry(PathBuf),
    #[error("unsupported archive entry type for {0}")]
    UnsupportedArchiveEntry(PathBuf),
    #[error("Node archive is missing required file: {0}")]
    MissingInstalledFile(PathBuf),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid zip archive: {0}")]
    Zip(#[from] zip::result::ZipError),
}

#[must_use]
pub fn node_version_is_compatible(version: &Version) -> bool {
    version.major >= MINIMUM_NODE_MAJOR && version.pre.is_empty()
}

pub fn parse_node_version(output: &str) -> Result<Version, NodeError> {
    let output = output.trim();
    if output.is_empty() {
        return Err(NodeError::EmptyVersion);
    }
    if output.contains(['\r', '\n']) {
        return Err(NodeError::MultilineVersion);
    }

    let exact = output.strip_prefix('v').unwrap_or(output);
    Version::parse(exact).map_err(|_| NodeError::InvalidVersion(output.to_owned()))
}

#[async_trait]
pub trait NodeVersionProbe: Send + Sync {
    async fn run(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput, ProcessError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessNodeVersionProbe;

#[async_trait]
impl NodeVersionProbe for ProcessNodeVersionProbe {
    async fn run(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput, ProcessError> {
        run_once(spec, timeout).await
    }
}

pub async fn probe_node(program: &Path) -> Result<ProbedNode, NodeError> {
    probe_node_with(&ProcessNodeVersionProbe, program, NODE_PROBE_TIMEOUT).await
}

pub async fn probe_node_with<P: NodeVersionProbe + ?Sized>(
    probe: &P,
    program: &Path,
    timeout: Duration,
) -> Result<ProbedNode, NodeError> {
    let spec = CommandSpec {
        program: program.to_owned(),
        args: vec!["--version".to_owned()],
        current_dir: None,
        env: Default::default(),
    };
    let output = probe.run(&spec, timeout).await?;
    if output.status != 0 {
        return Err(NodeError::ProbeExit {
            status: output.status,
            stderr: output.stderr,
        });
    }

    Ok(ProbedNode {
        program: program.to_owned(),
        version: parse_node_version(&output.stdout)?,
    })
}

pub fn expected_sha256(shasums: &str, artifact_name: &str) -> Result<String, NodeError> {
    let mut found = None;

    for (index, raw_line) in shasums.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let hash = fields.next().unwrap_or_default();
        let file_name = fields.next().unwrap_or_default().trim_start_matches('*');
        if fields.next().is_some()
            || hash.len() != 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !is_plain_file_name(file_name)
        {
            return Err(NodeError::InvalidShasumLine {
                line: index + 1,
                reason: "expected a 64-digit SHA-256 and a plain file name".to_owned(),
            });
        }

        if file_name == artifact_name {
            if found.is_some() {
                return Err(NodeError::DuplicateShasum(artifact_name.to_owned()));
            }
            found = Some(hash.to_ascii_lowercase());
        }
    }

    found.ok_or_else(|| NodeError::MissingShasum(artifact_name.to_owned()))
}

pub fn verify_sha256(path: &Path, expected: &str) -> Result<(), NodeError> {
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(NodeError::InvalidShasumLine {
            line: 0,
            reason: "expected checksum is not a 64-digit SHA-256".to_owned(),
        });
    }

    let mut reader = BufReader::new(File::open(path)?);
    let mut hasher = Sha256::new();
    io::copy(&mut reader, &mut hasher)?;
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(NodeError::ChecksumMismatch {
            path: path.to_owned(),
        });
    }
    Ok(())
}

pub async fn download_to(
    client: &reqwest::Client,
    url: &Url,
    destination: &Path,
    limit: usize,
) -> Result<(), NodeError> {
    if destination.exists() {
        return Err(NodeError::DownloadDestinationExists(destination.to_owned()));
    }
    let response = client.get(url.clone()).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(NodeError::DownloadTooLarge {
            url: url.clone(),
            limit,
        });
    }
    let bytes = response.bytes().await?;
    if bytes.len() > limit {
        return Err(NodeError::DownloadTooLarge {
            url: url.clone(),
            limit,
        });
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut output = File::create_new(destination)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    Ok(())
}

pub fn extract_node_archive(
    archive_path: &Path,
    distribution: &NodeDistribution,
    destination: &Path,
) -> Result<(), NodeError> {
    if destination.exists() {
        return Err(NodeError::ExtractDestinationExists(destination.to_owned()));
    }
    fs::create_dir_all(destination)?;

    match distribution.archive_format {
        NodeArchiveFormat::Zip => extract_zip(archive_path, distribution, destination),
        NodeArchiveFormat::TarGz => extract_tar_gz(archive_path, distribution, destination),
    }
}

pub fn stage_downloaded_node(
    archive_path: &Path,
    shasums: &str,
    distribution: &NodeDistribution,
    staging: &Path,
) -> Result<(), NodeError> {
    let checksum = expected_sha256(shasums, &distribution.artifact_name)?;
    verify_sha256(archive_path, &checksum)?;
    extract_node_archive(archive_path, distribution, staging)?;
    validate_extracted_node(distribution, staging)
}

pub fn validate_extracted_node(
    distribution: &NodeDistribution,
    root: &Path,
) -> Result<(), NodeError> {
    for relative in [
        distribution.node_relative_path(),
        distribution.npm_cli_relative_path(),
    ] {
        let path = root.join(relative);
        if !path.is_file() {
            return Err(NodeError::MissingInstalledFile(path));
        }
    }
    Ok(())
}

pub fn promote_staged_node(staging: &Path, destination: &Path) -> Result<(), NodeError> {
    if destination.exists() {
        return Err(NodeError::InstallDestinationExists(destination.to_owned()));
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::rename(staging, destination)?;
    Ok(())
}

fn extract_zip(
    archive_path: &Path,
    distribution: &NodeDistribution,
    destination: &Path,
) -> Result<(), NodeError> {
    let mut archive = zip::ZipArchive::new(File::open(archive_path)?)?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| NodeError::UnsafeArchiveEntry(PathBuf::from(entry.name())))?;
        let relative = stripped_archive_path(&enclosed, &distribution.directory_name)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let output = destination.join(&relative);
        if entry.is_dir() {
            fs::create_dir_all(&output)?;
        } else {
            if entry
                .unix_mode()
                .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                return Err(NodeError::UnsupportedArchiveEntry(enclosed));
            }
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = File::create_new(&output)?;
            io::copy(&mut entry, &mut file)?;
        }
    }
    Ok(())
}

fn extract_tar_gz(
    archive_path: &Path,
    distribution: &NodeDistribution,
    destination: &Path,
) -> Result<(), NodeError> {
    let archive = File::open(archive_path)?;
    let mut archive = tar::Archive::new(GzDecoder::new(archive));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let archived_path = entry.path()?.into_owned();
        let relative = stripped_archive_path(&archived_path, &distribution.directory_name)?;
        if relative.as_os_str().is_empty() {
            continue;
        }
        let output = destination.join(&relative);
        let entry_type = entry.header().entry_type();
        if entry_type.is_dir() {
            fs::create_dir_all(&output)?;
        } else if entry_type.is_file() {
            if let Some(parent) = output.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut file = File::create_new(&output)?;
            io::copy(&mut entry, &mut file)?;
            set_unix_mode(&file, entry.header().mode()?)?;
        } else if entry_type.is_symlink() {
            let target = entry
                .link_name()?
                .ok_or_else(|| NodeError::UnsupportedArchiveEntry(archived_path.clone()))?;
            validate_symlink_target(&relative, &target)
                .ok_or_else(|| NodeError::UnsafeArchiveEntry(archived_path.clone()))?;
            create_symlink(&target, &output, &archived_path)?;
        } else {
            return Err(NodeError::UnsupportedArchiveEntry(archived_path));
        }
    }
    Ok(())
}

fn stripped_archive_path(path: &Path, expected_root: &str) -> Result<PathBuf, NodeError> {
    if path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(NodeError::UnsafeArchiveEntry(path.to_owned()));
    }
    path.strip_prefix(expected_root)
        .map(Path::to_owned)
        .map_err(|_| NodeError::UnsafeArchiveEntry(path.to_owned()))
}

fn is_plain_file_name(value: &str) -> bool {
    !value.is_empty()
        && Path::new(value)
            .file_name()
            .is_some_and(|name| name == value)
        && !value.contains(['/', '\\'])
}

fn validate_symlink_target(link_path: &Path, target: &Path) -> Option<PathBuf> {
    if target.is_absolute() {
        return None;
    }
    let mut resolved = link_path.parent()?.to_path_buf();
    for component in target.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => resolved.push(part),
            Component::ParentDir => {
                if !resolved.pop() {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(resolved)
}

#[cfg(unix)]
fn create_symlink(target: &Path, output: &Path, _archived_path: &Path) -> Result<(), NodeError> {
    use std::os::unix::fs::symlink;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    symlink(target, output)?;
    Ok(())
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _output: &Path, archived_path: &Path) -> Result<(), NodeError> {
    Err(NodeError::UnsupportedArchiveEntry(archived_path.to_owned()))
}

#[cfg(unix)]
fn set_unix_mode(file: &File, mode: u32) -> Result<(), NodeError> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_unix_mode(_file: &File, _mode: u32) -> Result<(), NodeError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, io::Cursor, sync::Mutex};

    use flate2::{Compression, write::GzEncoder};
    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::*;

    #[test]
    fn default_version_and_compatibility_use_node_24_baseline() {
        assert_eq!(DEFAULT_NODE_VERSION, "24.19.0");
        assert!(!node_version_is_compatible(
            &Version::parse("23.99.0").unwrap()
        ));
        assert!(node_version_is_compatible(
            &Version::parse("24.0.0").unwrap()
        ));
        assert!(!node_version_is_compatible(
            &Version::parse("25.0.0-rc.1").unwrap()
        ));
    }

    #[test]
    fn maps_all_supported_platform_architecture_artifacts() {
        let version = Version::parse("24.19.0").unwrap();
        let cases = [
            (
                NodePlatform::Windows,
                NodeArchitecture::X64,
                "node-v24.19.0-win-x64.zip",
            ),
            (
                NodePlatform::Windows,
                NodeArchitecture::Arm64,
                "node-v24.19.0-win-arm64.zip",
            ),
            (
                NodePlatform::MacOs,
                NodeArchitecture::X64,
                "node-v24.19.0-darwin-x64.tar.gz",
            ),
            (
                NodePlatform::MacOs,
                NodeArchitecture::Arm64,
                "node-v24.19.0-darwin-arm64.tar.gz",
            ),
        ];

        for (platform, architecture, artifact) in cases {
            let distribution = NodeDistribution::new(&version, platform, architecture).unwrap();
            assert_eq!(distribution.artifact_name, artifact);
            assert_eq!(
                distribution.artifact_url.as_str(),
                format!("https://nodejs.org/dist/v24.19.0/{artifact}")
            );
            assert_eq!(
                distribution.shasums_url.as_str(),
                "https://nodejs.org/dist/v24.19.0/SHASUMS256.txt"
            );
        }
    }

    #[test]
    fn parses_only_exact_node_version_output() {
        assert_eq!(
            parse_node_version(" v24.19.0\r\n").unwrap(),
            Version::parse("24.19.0").unwrap()
        );
        assert!(matches!(
            parse_node_version("node v24.19.0"),
            Err(NodeError::InvalidVersion(_))
        ));
        assert!(matches!(
            parse_node_version("v24.19.0\nwarning"),
            Err(NodeError::MultilineVersion)
        ));
    }

    #[test]
    fn selects_exact_artifact_hash_and_rejects_duplicates() {
        let hash = "A".repeat(64);
        let other = "b".repeat(64);
        let artifact = "node-v24.19.0-win-x64.zip";
        let fixture = format!("{other}  other.zip\n{hash}  {artifact}\n");
        assert_eq!(expected_sha256(&fixture, artifact).unwrap(), "a".repeat(64));

        let duplicate = format!("{hash}  {artifact}\n{other}  {artifact}\n");
        assert!(matches!(
            expected_sha256(&duplicate, artifact),
            Err(NodeError::DuplicateShasum(_))
        ));
    }

    #[test]
    fn verifies_downloaded_archive_checksum() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("archive");
        fs::write(&path, b"node archive").unwrap();
        let expected = format!("{:x}", Sha256::digest(b"node archive"));
        verify_sha256(&path, &expected).unwrap();
        assert!(matches!(
            verify_sha256(&path, &"0".repeat(64)),
            Err(NodeError::ChecksumMismatch { .. })
        ));
    }

    struct FakeProbe {
        output: CommandOutput,
        calls: Mutex<Vec<CommandSpec>>,
    }

    #[async_trait]
    impl NodeVersionProbe for FakeProbe {
        async fn run(
            &self,
            spec: &CommandSpec,
            _timeout: Duration,
        ) -> Result<CommandOutput, ProcessError> {
            self.calls.lock().unwrap().push(spec.clone());
            Ok(self.output.clone())
        }
    }

    #[tokio::test]
    async fn probes_injected_node_without_shell_or_environment_changes() {
        let probe = FakeProbe {
            output: CommandOutput {
                status: 0,
                stdout: "v24.19.0\n".to_owned(),
                stderr: String::new(),
            },
            calls: Mutex::new(Vec::new()),
        };
        let program = PathBuf::from("injected/node.exe");
        let found = probe_node_with(&probe, &program, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(found.version, Version::parse("24.19.0").unwrap());
        assert_eq!(
            probe.calls.lock().unwrap().as_slice(),
            [CommandSpec {
                program,
                args: vec!["--version".to_owned()],
                current_dir: None,
                env: BTreeMap::new(),
            }]
        );
    }

    #[test]
    fn safely_extracts_windows_zip_without_the_distribution_wrapper() {
        let distribution = distribution(NodePlatform::Windows);
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("node.zip");
        let file = File::create(&archive_path).unwrap();
        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        archive
            .start_file(format!("{}/node.exe", distribution.directory_name), options)
            .unwrap();
        archive.write_all(b"node").unwrap();
        archive
            .start_file(
                format!(
                    "{}/node_modules/npm/bin/npm-cli.js",
                    distribution.directory_name
                ),
                options,
            )
            .unwrap();
        archive.write_all(b"npm").unwrap();
        archive.finish().unwrap();

        let extracted = temp.path().join("staging");
        let checksum = format!("{:x}", Sha256::digest(fs::read(&archive_path).unwrap()));
        let shasums = format!("{checksum}  {}\n", distribution.artifact_name);
        stage_downloaded_node(&archive_path, &shasums, &distribution, &extracted).unwrap();
        assert_eq!(fs::read(extracted.join("node.exe")).unwrap(), b"node");
    }

    #[test]
    fn safely_extracts_macos_tar_gz_without_the_distribution_wrapper() {
        let distribution = distribution(NodePlatform::MacOs);
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("node.tar.gz");
        let encoder = GzEncoder::new(File::create(&archive_path).unwrap(), Compression::default());
        let mut archive = tar::Builder::new(encoder);
        append_tar_file(
            &mut archive,
            &format!("{}/bin/node", distribution.directory_name),
            b"node",
            0o755,
        );
        append_tar_file(
            &mut archive,
            &format!(
                "{}/lib/node_modules/npm/bin/npm-cli.js",
                distribution.directory_name
            ),
            b"npm",
            0o644,
        );
        archive.into_inner().unwrap().finish().unwrap();

        let extracted = temp.path().join("staging");
        extract_node_archive(&archive_path, &distribution, &extracted).unwrap();
        validate_extracted_node(&distribution, &extracted).unwrap();
        assert_eq!(fs::read(extracted.join("bin/node")).unwrap(), b"node");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = fs::metadata(extracted.join("bin/node"))
                .unwrap()
                .permissions()
                .mode();
            assert_ne!(mode & 0o111, 0, "extracted Node must remain executable");
        }
    }

    #[test]
    fn rejects_entries_outside_expected_distribution_root() {
        let distribution = distribution(NodePlatform::Windows);
        let temp = tempfile::tempdir().unwrap();
        let archive_path = temp.path().join("wrong-root.zip");
        let mut archive = ZipWriter::new(File::create(&archive_path).unwrap());
        archive
            .start_file("another-root/node.exe", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"bad").unwrap();
        archive.finish().unwrap();

        let error =
            extract_node_archive(&archive_path, &distribution, &temp.path().join("staging"))
                .unwrap_err();
        assert!(matches!(error, NodeError::UnsafeArchiveEntry(_)));
    }

    fn distribution(platform: NodePlatform) -> NodeDistribution {
        NodeDistribution::new(
            &Version::parse("24.19.0").unwrap(),
            platform,
            NodeArchitecture::X64,
        )
        .unwrap()
    }

    fn append_tar_file(
        archive: &mut tar::Builder<GzEncoder<File>>,
        path: &str,
        data: &[u8],
        mode: u32,
    ) {
        let mut header = tar::Header::new_gnu();
        header.set_path(path).unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        archive.append(&header, Cursor::new(data)).unwrap();
    }
}
