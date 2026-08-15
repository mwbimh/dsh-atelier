use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use async_trait::async_trait;
use reqwest::{Client, redirect::Policy};
use semver::{Version, VersionReq};
use thiserror::Error;
use tokio::time::sleep;
use url::Url;

use crate::{
    paths::AtelierPaths,
    process::{CommandOutput, CommandSpec, ProcessError, run_once},
    registry::{
        LatestRelease, NPM_REGISTRIES, RegistryErrorKind, classify_http_status,
        classify_reqwest_error, parse_latest_release,
    },
};

pub const DEFAULT_DSH_VERSION: &str = "0.1.0-rc.6";
pub const DEFAULT_NODE_VERSION: &str = "24.19.0";
pub const CURRENT_DSH_NODE_REQUIREMENT: &str = "^22.19.0 || >=24.0.0";
pub const TOOL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
pub const NPM_INSTALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Error)]
pub enum InstallError {
    #[error("{tool} path is not a file: {path}")]
    InvalidToolPath { tool: &'static str, path: PathBuf },
    #[error("{tool} returned an invalid version: {output:?}")]
    InvalidToolVersion { tool: &'static str, output: String },
    #[error("{tool} exited with status {status}: {stderr}")]
    ToolExit {
        tool: &'static str,
        status: i32,
        stderr: String,
    },
    #[error("cannot locate npm-cli.js for Node executable {0}")]
    NpmCliNotFound(PathBuf),
    #[error("invalid Node version requirement: {0}")]
    InvalidNodeRequirement(String),
    #[error("invalid npm registry URL: {0}")]
    InvalidRegistry(String),
    #[error("installation destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("npm install failed with status {status}: {stderr}")]
    NpmInstallFailed { status: i32, stderr: String },
    #[error("no npm registry endpoints are configured")]
    NoRegistryEndpoints,
    #[error(transparent)]
    Process(#[from] ProcessError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn parse_tool_version(tool: &'static str, output: &str) -> Result<Version, InstallError> {
    let trimmed = output.trim();
    if trimmed.is_empty() || trimmed.contains(['\r', '\n']) {
        return Err(InstallError::InvalidToolVersion {
            tool,
            output: output.to_owned(),
        });
    }
    Version::parse(trimmed.strip_prefix('v').unwrap_or(trimmed)).map_err(|_| {
        InstallError::InvalidToolVersion {
            tool,
            output: output.to_owned(),
        }
    })
}

pub fn node_satisfies_requirement(
    version: &Version,
    requirement: &str,
) -> Result<bool, InstallError> {
    let mut saw_requirement = false;
    for alternative in requirement.split("||") {
        let alternative = alternative.trim();
        if alternative.is_empty() {
            return Err(InstallError::InvalidNodeRequirement(requirement.to_owned()));
        }
        saw_requirement = true;
        let parsed = VersionReq::parse(alternative)
            .map_err(|_| InstallError::InvalidNodeRequirement(requirement.to_owned()))?;
        if parsed.matches(version) {
            return Ok(true);
        }
    }
    if !saw_requirement {
        return Err(InstallError::InvalidNodeRequirement(requirement.to_owned()));
    }
    Ok(false)
}

#[must_use]
pub fn node_supports_current_dsh(version: &Version) -> bool {
    node_satisfies_requirement(version, CURRENT_DSH_NODE_REQUIREMENT).unwrap_or(false)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeToolchain {
    pub node_executable: PathBuf,
    pub npm_cli: PathBuf,
    pub node_version: Version,
    pub npm_version: Version,
}

#[async_trait]
pub trait CommandRunner: Send + Sync {
    async fn run(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput, ProcessError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCommandRunner;

#[async_trait]
impl CommandRunner for ProcessCommandRunner {
    async fn run(
        &self,
        spec: &CommandSpec,
        timeout: Duration,
    ) -> Result<CommandOutput, ProcessError> {
        run_once(spec, timeout).await
    }
}

pub fn resolve_npm_cli(node_executable: &Path) -> Result<PathBuf, InstallError> {
    validate_file("node", node_executable)?;
    let bin_dir = node_executable
        .parent()
        .ok_or_else(|| InstallError::NpmCliNotFound(node_executable.to_owned()))?;
    let mut candidates = vec![bin_dir.join("node_modules/npm/bin/npm-cli.js")];
    if let Some(root) = bin_dir.parent() {
        candidates.push(root.join("lib/node_modules/npm/bin/npm-cli.js"));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| InstallError::NpmCliNotFound(node_executable.to_owned()))
}

pub async fn probe_node_toolchain(node_executable: &Path) -> Result<NodeToolchain, InstallError> {
    let npm_cli = resolve_npm_cli(node_executable)?;
    probe_node_toolchain_with(
        &ProcessCommandRunner,
        node_executable,
        &npm_cli,
        TOOL_PROBE_TIMEOUT,
    )
    .await
}

pub async fn probe_node_toolchain_with<R: CommandRunner + ?Sized>(
    runner: &R,
    node_executable: &Path,
    npm_cli: &Path,
    timeout: Duration,
) -> Result<NodeToolchain, InstallError> {
    validate_file("node", node_executable)?;
    validate_file("npm-cli.js", npm_cli)?;

    let node_output = runner
        .run(
            &CommandSpec {
                program: node_executable.to_owned(),
                args: vec!["--version".to_owned()],
                current_dir: None,
                env: BTreeMap::new(),
            },
            timeout,
        )
        .await?;
    ensure_success("node", &node_output)?;

    let npm_output = runner
        .run(
            &CommandSpec {
                program: node_executable.to_owned(),
                args: vec![
                    npm_cli.to_string_lossy().into_owned(),
                    "--version".to_owned(),
                ],
                current_dir: None,
                env: BTreeMap::new(),
            },
            timeout,
        )
        .await?;
    ensure_success("npm", &npm_output)?;

    Ok(NodeToolchain {
        node_executable: node_executable.to_owned(),
        npm_cli: npm_cli.to_owned(),
        node_version: parse_tool_version("node", &node_output.stdout)?,
        npm_version: parse_tool_version("npm", &npm_output.stdout)?,
    })
}

fn validate_file(tool: &'static str, path: &Path) -> Result<(), InstallError> {
    if path.is_file() {
        Ok(())
    } else {
        Err(InstallError::InvalidToolPath {
            tool,
            path: path.to_owned(),
        })
    }
}

fn ensure_success(tool: &'static str, output: &CommandOutput) -> Result<(), InstallError> {
    if output.status == 0 {
        Ok(())
    } else {
        Err(InstallError::ToolExit {
            tool,
            status: output.status,
            stderr: output.stderr.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NpmIsolation {
    pub cache_dir: PathBuf,
    pub user_config_file: PathBuf,
    pub global_config_file: PathBuf,
    pub temp_dir: PathBuf,
}

impl NpmIsolation {
    #[must_use]
    pub fn under(npm_root: &Path) -> Self {
        Self {
            cache_dir: npm_root.join("cache"),
            user_config_file: npm_root.join("userconfig").join("npmrc"),
            global_config_file: npm_root.join("globalconfig").join("npmrc"),
            temp_dir: npm_root.join("tmp"),
        }
    }

    pub fn prepare(&self) -> Result<(), InstallError> {
        fs::create_dir_all(&self.cache_dir)?;
        fs::create_dir_all(&self.temp_dir)?;
        prepare_empty_config(&self.user_config_file)?;
        prepare_empty_config(&self.global_config_file)?;
        Ok(())
    }
}

fn prepare_empty_config(path: &Path) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if !path.exists() {
        fs::write(path, [])?;
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NpmCli {
    pub node_executable: PathBuf,
    pub npm_cli: PathBuf,
    pub isolation: NpmIsolation,
}

impl NpmCli {
    #[must_use]
    pub fn new(node_executable: PathBuf, npm_cli: PathBuf, isolation: NpmIsolation) -> Self {
        Self {
            node_executable,
            npm_cli,
            isolation,
        }
    }

    pub fn install_exact_spec(
        &self,
        prefix: &Path,
        package: &str,
        version: &Version,
        registry: &Url,
    ) -> Result<CommandSpec, InstallError> {
        validate_registry_url(registry)?;
        let exact_package = format!("{package}@{version}");
        let args = vec![
            self.npm_cli.to_string_lossy().into_owned(),
            "install".to_owned(),
            "--global".to_owned(),
            "--ignore-scripts".to_owned(),
            "--no-audit".to_owned(),
            "--no-fund".to_owned(),
            format!("--registry={registry}"),
            format!("--prefix={}", prefix.to_string_lossy()),
            exact_package,
        ];
        let mut env = BTreeMap::new();
        env.insert(
            "NPM_CONFIG_CACHE".to_owned(),
            self.isolation.cache_dir.to_string_lossy().into_owned(),
        );
        env.insert(
            "NPM_CONFIG_USERCONFIG".to_owned(),
            self.isolation
                .user_config_file
                .to_string_lossy()
                .into_owned(),
        );
        env.insert(
            "NPM_CONFIG_GLOBALCONFIG".to_owned(),
            self.isolation
                .global_config_file
                .to_string_lossy()
                .into_owned(),
        );
        env.insert(
            "NPM_CONFIG_PREFIX".to_owned(),
            prefix.to_string_lossy().into_owned(),
        );
        env.insert("NPM_CONFIG_IGNORE_SCRIPTS".to_owned(), "true".to_owned());
        for key in ["TMP", "TEMP", "TMPDIR"] {
            env.insert(
                key.to_owned(),
                self.isolation.temp_dir.to_string_lossy().into_owned(),
            );
        }
        Ok(CommandSpec {
            program: self.node_executable.clone(),
            args,
            current_dir: Some(prefix.to_owned()),
            env,
        })
    }

    pub async fn install_exact_with<R: CommandRunner + ?Sized>(
        &self,
        runner: &R,
        prefix: &Path,
        package: &str,
        version: &Version,
        registry: &Url,
        timeout: Duration,
    ) -> Result<(), InstallError> {
        self.isolation.prepare()?;
        fs::create_dir_all(prefix)?;
        let output = runner
            .run(
                &self.install_exact_spec(prefix, package, version, registry)?,
                timeout,
            )
            .await?;
        if output.status != 0 {
            return Err(InstallError::NpmInstallFailed {
                status: output.status,
                stderr: output.stderr,
            });
        }
        Ok(())
    }
}

pub async fn install_exact_with_registry_fallback<R: CommandRunner + ?Sized>(
    npm: &NpmCli,
    runner: &R,
    prefix: &Path,
    package: &str,
    version: &Version,
    registries: &[RegistryEndpoint],
    timeout: Duration,
) -> Result<RegistryEndpoint, InstallError> {
    let mut last_error = None;
    for registry in registries {
        match npm
            .install_exact_with(
                runner,
                prefix,
                package,
                version,
                &registry.base_url,
                timeout,
            )
            .await
        {
            Ok(()) => return Ok(registry.clone()),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or(InstallError::NoRegistryEndpoints))
}

fn validate_registry_url(url: &Url) -> Result<(), InstallError> {
    if url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
    {
        Ok(())
    } else {
        Err(InstallError::InvalidRegistry(url.to_string()))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedDshLayout {
    pub versions_dir: PathBuf,
    pub staging_dir: PathBuf,
    pub npm: NpmIsolation,
}

impl ManagedDshLayout {
    #[must_use]
    pub fn from_paths(paths: &AtelierPaths) -> Self {
        Self {
            versions_dir: paths.dsh_installations_dir.join("versions"),
            staging_dir: paths.dsh_installations_dir.join("staging"),
            npm: NpmIsolation::under(&paths.npm_dir),
        }
    }

    #[must_use]
    pub fn version_dir(&self, version: &Version) -> PathBuf {
        self.versions_dir.join(version.to_string())
    }

    pub fn begin(&self, version: &Version) -> Result<StagedDshInstallation, InstallError> {
        self.npm.prepare()?;
        fs::create_dir_all(&self.versions_dir)?;
        fs::create_dir_all(&self.staging_dir)?;
        let root = self.staging_dir.join(version.to_string());
        fs::create_dir_all(&root)?;
        Ok(StagedDshInstallation {
            root,
            final_root: self.version_dir(version),
            version: version.clone(),
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedDshInstallation {
    pub root: PathBuf,
    pub final_root: PathBuf,
    pub version: Version,
}

impl StagedDshInstallation {
    #[must_use]
    pub fn managed_program(&self) -> PathBuf {
        managed_dsh_program(&self.root)
    }

    pub fn promote(self) -> Result<ManagedDshInstallation, InstallError> {
        if self.final_root.exists() {
            return Err(InstallError::DestinationExists(self.final_root));
        }
        fs::rename(&self.root, &self.final_root)?;
        Ok(ManagedDshInstallation {
            root: self.final_root,
            version: self.version,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedDshInstallation {
    pub root: PathBuf,
    pub version: Version,
}

impl ManagedDshInstallation {
    #[must_use]
    pub fn managed_program(&self) -> PathBuf {
        managed_dsh_program(&self.root)
    }
}

fn managed_dsh_program(prefix: &Path) -> PathBuf {
    if cfg!(windows) {
        prefix.join("dsh.cmd")
    } else {
        prefix.join("bin").join("dsh")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryEndpoint {
    pub base_url: Url,
    pub official: bool,
}

impl RegistryEndpoint {
    pub fn parse(value: &str, official: bool) -> Result<Self, InstallError> {
        let mut base_url =
            Url::parse(value).map_err(|_| InstallError::InvalidRegistry(value.to_owned()))?;
        validate_registry_url(&base_url)?;
        if !base_url.path().ends_with('/') {
            let path = format!("{}/", base_url.path());
            base_url.set_path(&path);
        }
        Ok(Self { base_url, official })
    }

    fn packument_url(&self, package: &str) -> Result<Url, InstallError> {
        self.base_url
            .join(&package.replace('/', "%2f"))
            .map_err(|_| InstallError::InvalidRegistry(self.base_url.to_string()))
    }
}

#[must_use]
pub fn default_registry_endpoints() -> Vec<RegistryEndpoint> {
    NPM_REGISTRIES
        .iter()
        .map(|registry| {
            RegistryEndpoint::parse(registry.base_url, registry.official)
                .expect("built-in npm registry URL must be valid")
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct RegistryRetryPolicy {
    pub official_attempts: usize,
    pub mirror_attempts: usize,
    pub retry_delays: Vec<Duration>,
}

impl Default for RegistryRetryPolicy {
    fn default() -> Self {
        Self {
            official_attempts: 3,
            mirror_attempts: 1,
            retry_delays: vec![Duration::from_secs(1), Duration::from_secs(3)],
        }
    }
}

impl RegistryRetryPolicy {
    #[cfg(test)]
    fn without_delays() -> Self {
        Self {
            retry_delays: vec![Duration::ZERO, Duration::ZERO],
            ..Self::default()
        }
    }
}

#[derive(Clone, Debug, Error)]
#[error("{message}")]
pub struct PackumentFetchError {
    pub kind: RegistryErrorKind,
    message: String,
}

impl PackumentFetchError {
    pub fn new(kind: RegistryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    pub fn availability(message: impl Into<String>) -> Self {
        Self::new(RegistryErrorKind::Availability, message)
    }
}

#[async_trait]
pub trait PackumentSource: Send + Sync {
    async fn fetch(&self, url: &Url, official: bool) -> Result<String, PackumentFetchError>;
}

#[derive(Clone, Debug)]
pub struct ReqwestPackumentSource {
    client: Client,
}

impl ReqwestPackumentSource {
    pub fn new(
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Result<Self, reqwest::Error> {
        Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .redirect(Policy::none())
            .build()
            .map(|client| Self { client })
    }
}

#[async_trait]
impl PackumentSource for ReqwestPackumentSource {
    async fn fetch(&self, url: &Url, official: bool) -> Result<String, PackumentFetchError> {
        let response = self.client.get(url.clone()).send().await.map_err(|error| {
            PackumentFetchError::new(classify_reqwest_error(&error), error.to_string())
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(PackumentFetchError::new(
                classify_http_status(status, official),
                format!("npm registry returned HTTP {status}"),
            ));
        }
        response.text().await.map_err(|error| {
            PackumentFetchError::new(classify_reqwest_error(&error), error.to_string())
        })
    }
}

#[derive(Clone, Debug)]
pub struct LocatedRelease {
    pub release: LatestRelease,
    pub registry: RegistryEndpoint,
}

#[derive(Clone, Debug, Error)]
#[error("{message}")]
pub struct ReleaseLookupError {
    pub kind: RegistryErrorKind,
    message: String,
}

pub async fn lookup_latest_release_with<S: PackumentSource + ?Sized>(
    source: &S,
    registries: &[RegistryEndpoint],
    package: &str,
    policy: &RegistryRetryPolicy,
) -> Result<LocatedRelease, ReleaseLookupError> {
    let mut last_availability_error = None;
    for registry in registries {
        let attempts = if registry.official {
            policy.official_attempts
        } else {
            policy.mirror_attempts
        };
        for attempt in 0..attempts.max(1) {
            let url = registry
                .packument_url(package)
                .map_err(|error| ReleaseLookupError {
                    kind: RegistryErrorKind::Permanent,
                    message: error.to_string(),
                })?;
            match source.fetch(&url, registry.official).await {
                Ok(json) => {
                    let release =
                        parse_latest_release(&json).map_err(|error| ReleaseLookupError {
                            kind: error.kind(),
                            message: error.to_string(),
                        })?;
                    return Ok(LocatedRelease {
                        release,
                        registry: registry.clone(),
                    });
                }
                Err(error) if error.kind == RegistryErrorKind::Availability => {
                    last_availability_error = Some(error.message);
                    if attempt + 1 < attempts {
                        let delay = policy
                            .retry_delays
                            .get(attempt)
                            .copied()
                            .unwrap_or_default();
                        sleep(delay).await;
                    }
                }
                Err(error) => {
                    return Err(ReleaseLookupError {
                        kind: error.kind,
                        message: error.message,
                    });
                }
            }
        }
    }
    Err(ReleaseLookupError {
        kind: RegistryErrorKind::Availability,
        message: last_availability_error
            .unwrap_or_else(|| "no npm registry endpoints are configured".to_owned()),
    })
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, VecDeque},
        fs,
        path::{Path, PathBuf},
        sync::Mutex,
        time::Duration,
    };

    use async_trait::async_trait;
    use semver::Version;
    use url::Url;

    use super::*;
    use crate::{
        paths::AtelierPaths,
        process::{CommandOutput, CommandSpec, ProcessError},
        registry::RegistryErrorKind,
    };

    #[test]
    fn parses_node_and_npm_versions_as_exact_semver() {
        assert_eq!(
            parse_tool_version("node", " v24.19.0\r\n").unwrap(),
            Version::parse("24.19.0").unwrap()
        );
        assert_eq!(
            parse_tool_version("npm", "11.17.0\n").unwrap(),
            Version::parse("11.17.0").unwrap()
        );
        assert!(parse_tool_version("node", "Node 24.19.0").is_err());
        assert!(parse_tool_version("npm", "11.17.0\nwarning").is_err());
    }

    #[test]
    fn node_compatibility_matches_the_current_dsh_engine_contract() {
        assert!(!node_supports_current_dsh(
            &Version::parse("22.18.0").unwrap()
        ));
        assert!(node_supports_current_dsh(
            &Version::parse("22.19.0").unwrap()
        ));
        assert!(!node_supports_current_dsh(
            &Version::parse("23.11.1").unwrap()
        ));
        assert!(node_supports_current_dsh(
            &Version::parse("24.0.0").unwrap()
        ));
    }

    struct FakeRunner {
        outputs: Mutex<VecDeque<CommandOutput>>,
        calls: Mutex<Vec<CommandSpec>>,
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            spec: &CommandSpec,
            _timeout: Duration,
        ) -> Result<CommandOutput, ProcessError> {
            self.calls.lock().unwrap().push(spec.clone());
            Ok(self.outputs.lock().unwrap().pop_front().unwrap())
        }
    }

    #[tokio::test]
    async fn validates_node_and_npm_cli_through_node_without_a_shell() {
        let temp = tempfile::tempdir().unwrap();
        let node = fixture_file(&temp.path().join(node_file_name()));
        let npm_cli = fixture_file(&temp.path().join("node_modules/npm/bin/npm-cli.js"));
        let runner = FakeRunner {
            outputs: Mutex::new(VecDeque::from([
                output(0, "v24.19.0\n", ""),
                output(0, "11.17.0\n", ""),
            ])),
            calls: Mutex::new(Vec::new()),
        };

        let tools = probe_node_toolchain_with(&runner, &node, &npm_cli, Duration::from_secs(2))
            .await
            .unwrap();

        assert_eq!(tools.node_version, Version::parse("24.19.0").unwrap());
        assert_eq!(tools.npm_version, Version::parse("11.17.0").unwrap());
        assert_eq!(
            runner.calls.lock().unwrap().as_slice(),
            [
                CommandSpec {
                    program: node.clone(),
                    args: vec!["--version".into()],
                    current_dir: None,
                    env: BTreeMap::new(),
                },
                CommandSpec {
                    program: node,
                    args: vec![npm_cli.to_string_lossy().into_owned(), "--version".into()],
                    current_dir: None,
                    env: BTreeMap::new(),
                },
            ]
        );
    }

    #[test]
    fn resolves_npm_cli_next_to_windows_and_unix_node_layouts() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        let node = fixture_file(&bin.join(node_file_name()));
        let expected = if cfg!(windows) {
            fixture_file(&bin.join("node_modules/npm/bin/npm-cli.js"))
        } else {
            fixture_file(&temp.path().join("lib/node_modules/npm/bin/npm-cli.js"))
        };

        assert_eq!(resolve_npm_cli(&node).unwrap(), expected);
    }

    #[test]
    fn npm_install_is_exact_global_and_fully_isolated() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let node = root.join(node_file_name());
        let npm_cli = root.join("npm-cli.js");
        let isolation = NpmIsolation::under(&root.join("npm"));
        let npm = NpmCli::new(node.clone(), npm_cli.clone(), isolation.clone());
        let prefix = root.join("staging/0.1.0-rc.6");
        let registry = Url::parse("https://registry.npmjs.org/").unwrap();

        let spec = npm
            .install_exact_spec(
                &prefix,
                "@deepseek-ai/dsh",
                &Version::parse("0.1.0-rc.6").unwrap(),
                &registry,
            )
            .unwrap();

        assert_eq!(spec.program, node);
        assert_eq!(spec.current_dir, Some(prefix.clone()));
        assert_eq!(spec.args[0], npm_cli.to_string_lossy());
        assert!(spec.args.contains(&"install".to_owned()));
        assert!(spec.args.contains(&"--global".to_owned()));
        assert!(spec.args.contains(&"--ignore-scripts".to_owned()));
        assert!(spec.args.contains(&"--no-audit".to_owned()));
        assert!(spec.args.contains(&"--no-fund".to_owned()));
        assert!(
            spec.args
                .contains(&"@deepseek-ai/dsh@0.1.0-rc.6".to_owned())
        );
        assert_eq!(
            spec.env.get("NPM_CONFIG_CACHE").unwrap(),
            &isolation.cache_dir.to_string_lossy()
        );
        assert_eq!(
            spec.env.get("NPM_CONFIG_USERCONFIG").unwrap(),
            &isolation.user_config_file.to_string_lossy()
        );
        assert_eq!(
            spec.env.get("NPM_CONFIG_PREFIX").unwrap(),
            &prefix.to_string_lossy()
        );
        assert_eq!(spec.env.get("NPM_CONFIG_IGNORE_SCRIPTS").unwrap(), "true");
        assert!(!spec.env.contains_key("DSH_HOME"));
    }

    #[test]
    fn staging_is_out_of_place_and_promotes_without_overwriting() {
        let temp = tempfile::tempdir().unwrap();
        let paths = AtelierPaths::from_root(temp.path());
        let layout = ManagedDshLayout::from_paths(&paths);
        let version = Version::parse("0.1.0-rc.6").unwrap();

        let staged = layout.begin(&version).unwrap();
        assert_eq!(staged.root, layout.staging_dir.join("0.1.0-rc.6"));
        assert!(staged.root.is_dir());
        assert!(layout.npm.cache_dir.is_dir());
        assert!(layout.npm.user_config_file.is_file());
        assert!(!layout.version_dir(&version).exists());

        fixture_file(&staged.managed_program());
        let installed = staged.promote().unwrap();
        assert_eq!(installed.root, layout.version_dir(&version));
        assert!(installed.managed_program().is_file());
        assert!(!layout.staging_dir.join("0.1.0-rc.6").exists());

        let error = layout.begin(&version).unwrap().promote().unwrap_err();
        assert!(matches!(error, InstallError::DestinationExists(_)));
    }

    struct FakePackumentSource {
        responses: Mutex<VecDeque<Result<String, PackumentFetchError>>>,
        calls: Mutex<Vec<Url>>,
    }

    #[async_trait]
    impl PackumentSource for FakePackumentSource {
        async fn fetch(&self, url: &Url, _official: bool) -> Result<String, PackumentFetchError> {
            self.calls.lock().unwrap().push(url.clone());
            self.responses.lock().unwrap().pop_front().unwrap()
        }
    }

    #[tokio::test]
    async fn registry_lookup_retries_official_then_falls_back_and_freezes_latest() {
        let source = FakePackumentSource {
            responses: Mutex::new(VecDeque::from([
                Err(PackumentFetchError::availability("timeout")),
                Err(PackumentFetchError::availability("timeout")),
                Err(PackumentFetchError::availability("timeout")),
                Ok(packument_fixture()),
            ])),
            calls: Mutex::new(Vec::new()),
        };
        let registries = vec![
            RegistryEndpoint::parse("https://registry.npmjs.org/", true).unwrap(),
            RegistryEndpoint::parse("https://registry.npmmirror.com/", false).unwrap(),
        ];
        let policy = RegistryRetryPolicy {
            official_attempts: 3,
            mirror_attempts: 1,
            retry_delays: vec![Duration::ZERO, Duration::ZERO],
        };

        let found = lookup_latest_release_with(&source, &registries, "@deepseek-ai/dsh", &policy)
            .await
            .unwrap();

        assert_eq!(found.release.version.to_string(), "0.1.0-rc.6");
        assert_eq!(
            found.registry.base_url.as_str(),
            "https://registry.npmmirror.com/"
        );
        assert_eq!(source.calls.lock().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn npm_install_falls_back_to_the_next_registry() {
        let temp = tempfile::tempdir().unwrap();
        let node = fixture_file(&temp.path().join(node_file_name()));
        let npm_cli = fixture_file(&temp.path().join("npm-cli.js"));
        let npm = NpmCli::new(node, npm_cli, NpmIsolation::under(&temp.path().join("npm")));
        let runner = FakeRunner {
            outputs: Mutex::new(VecDeque::from([
                output(1, "", "official unavailable"),
                output(0, "installed", ""),
            ])),
            calls: Mutex::new(Vec::new()),
        };
        let registries = vec![
            RegistryEndpoint::parse("https://registry.npmjs.org/", true).unwrap(),
            RegistryEndpoint::parse("https://registry.npmmirror.com/", false).unwrap(),
        ];

        let used = install_exact_with_registry_fallback(
            &npm,
            &runner,
            &temp.path().join("prefix"),
            "@deepseek-ai/dsh",
            &Version::parse("0.1.0-rc.6").unwrap(),
            &registries,
            Duration::from_secs(30),
        )
        .await
        .expect("mirror install succeeds");

        assert_eq!(used, registries[1]);
        let calls = runner.calls.lock().unwrap();
        assert!(
            calls[0]
                .args
                .contains(&"--registry=https://registry.npmjs.org/".to_owned())
        );
        assert!(
            calls[1]
                .args
                .contains(&"--registry=https://registry.npmmirror.com/".to_owned())
        );
    }

    #[tokio::test]
    async fn registry_lookup_never_falls_back_after_security_or_authoritative_errors() {
        for kind in [
            RegistryErrorKind::Security,
            RegistryErrorKind::AuthoritativeNotFound,
        ] {
            let source = FakePackumentSource {
                responses: Mutex::new(VecDeque::from([Err(PackumentFetchError::new(
                    kind, "terminal",
                ))])),
                calls: Mutex::new(Vec::new()),
            };
            let registries = default_registry_endpoints();

            let error = lookup_latest_release_with(
                &source,
                &registries,
                "@deepseek-ai/dsh",
                &RegistryRetryPolicy::without_delays(),
            )
            .await
            .unwrap_err();

            assert_eq!(error.kind, kind);
            assert_eq!(source.calls.lock().unwrap().len(), 1);
        }
    }

    fn fixture_file(path: &Path) -> PathBuf {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture").unwrap();
        path.to_owned()
    }

    fn node_file_name() -> &'static str {
        if cfg!(windows) { "node.exe" } else { "node" }
    }

    fn output(status: i32, stdout: &str, stderr: &str) -> CommandOutput {
        CommandOutput {
            status,
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
        }
    }

    fn packument_fixture() -> String {
        r#"{
          "dist-tags": { "latest": "0.1.0-rc.6" },
          "versions": {
            "0.1.0-rc.6": {
              "version": "0.1.0-rc.6",
              "engines": { "node": "^22.19.0 || >=24.0.0" },
              "dist": {
                "integrity": "sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg==",
                "tarball": "https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-0.1.0-rc.6.tgz"
              }
            }
          }
        }"#
            .to_owned()
    }
}
