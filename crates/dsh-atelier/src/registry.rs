use std::collections::HashMap;
use std::time::Duration;

use reqwest::StatusCode;
use semver::Version;
use serde::Deserialize;
use thiserror::Error;
use url::Url;

pub const REGISTRY_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
pub const REGISTRY_REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
pub const OFFICIAL_REGISTRY_ATTEMPTS: usize = 3;
pub const OFFICIAL_REGISTRY_ADDITIONAL_RETRIES: usize = 2;
pub const MIRROR_REGISTRY_ATTEMPTS: usize = 1;
pub const OFFICIAL_REGISTRY_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_secs(1), Duration::from_secs(3)];
pub const REGISTRY_RETRY_JITTER_MAX: Duration = Duration::from_millis(250);
pub const DSH_PACKAGE_NAME: &str = "@deepseek-ai/dsh";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NpmRegistry {
    pub base_url: &'static str,
    pub official: bool,
}

pub const NPM_REGISTRIES: [NpmRegistry; 4] = [
    NpmRegistry {
        base_url: "https://registry.npmjs.org/",
        official: true,
    },
    NpmRegistry {
        base_url: "https://registry.npmmirror.com/",
        official: false,
    },
    NpmRegistry {
        base_url: "https://mirrors.cloud.tencent.com/npm/",
        official: false,
    },
    NpmRegistry {
        base_url: "https://repo.huaweicloud.com/repository/npm/",
        official: false,
    },
];

impl NpmRegistry {
    pub fn packument_url(&self, package_name: &str) -> Result<Url, RegistryError> {
        let encoded_package = package_name.replace('/', "%2f");
        Url::parse(self.base_url)
            .and_then(|base_url| base_url.join(&encoded_package))
            .map_err(|error| {
                RegistryError::new(
                    RegistryErrorKind::Permanent,
                    format!("cannot build npm packument URL: {error}"),
                )
            })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryErrorKind {
    Availability,
    AuthoritativeNotFound,
    Security,
    Permanent,
}

impl RegistryErrorKind {
    #[must_use]
    pub const fn should_fallback(self) -> bool {
        matches!(self, Self::Availability)
    }
}

#[derive(Debug, Error)]
#[error("{message}")]
pub struct RegistryError {
    kind: RegistryErrorKind,
    message: String,
}

impl RegistryError {
    fn new(kind: RegistryErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    #[must_use]
    pub const fn kind(&self) -> RegistryErrorKind {
        self.kind
    }

    #[must_use]
    pub const fn should_fallback(&self) -> bool {
        self.kind.should_fallback()
    }
}

#[must_use]
pub fn classify_http_status(status: StatusCode, official_registry: bool) -> RegistryErrorKind {
    if status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
    {
        RegistryErrorKind::Availability
    } else if status == StatusCode::NOT_FOUND {
        if official_registry {
            RegistryErrorKind::AuthoritativeNotFound
        } else {
            RegistryErrorKind::Availability
        }
    } else {
        RegistryErrorKind::Permanent
    }
}

#[must_use]
pub fn classify_reqwest_error(error: &reqwest::Error) -> RegistryErrorKind {
    if error.is_timeout() || error.is_connect() {
        RegistryErrorKind::Availability
    } else {
        RegistryErrorKind::Permanent
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LatestRelease {
    pub version: Version,
    pub integrity: String,
    pub tarball: Url,
    pub engines_node: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Packument {
    #[serde(rename = "dist-tags")]
    dist_tags: DistTags,
    versions: HashMap<String, VersionMetadata>,
}

#[derive(Debug, Deserialize)]
struct DistTags {
    latest: String,
}

#[derive(Debug, Deserialize)]
struct VersionMetadata {
    version: String,
    dist: Distribution,
    #[serde(default)]
    engines: Option<Engines>,
}

#[derive(Debug, Deserialize)]
struct Distribution {
    #[serde(default)]
    integrity: Option<String>,
    tarball: String,
}

#[derive(Debug, Deserialize)]
struct Engines {
    #[serde(default)]
    node: Option<String>,
}

pub fn parse_latest_release(packument_json: &str) -> Result<LatestRelease, RegistryError> {
    let packument: Packument = serde_json::from_str(packument_json).map_err(|error| {
        RegistryError::new(
            RegistryErrorKind::Permanent,
            format!("invalid npm packument JSON: {error}"),
        )
    })?;

    let version = Version::parse(&packument.dist_tags.latest).map_err(|error| {
        RegistryError::new(
            RegistryErrorKind::Permanent,
            format!("npm latest dist-tag is not an exact version: {error}"),
        )
    })?;
    let metadata = packument
        .versions
        .get(&packument.dist_tags.latest)
        .ok_or_else(|| {
            RegistryError::new(
                RegistryErrorKind::Permanent,
                format!(
                    "npm packument has no metadata for latest version {}",
                    packument.dist_tags.latest
                ),
            )
        })?;
    if metadata.version != packument.dist_tags.latest {
        return Err(RegistryError::new(
            RegistryErrorKind::Permanent,
            format!(
                "npm version metadata mismatch: latest is {}, metadata is {}",
                packument.dist_tags.latest, metadata.version
            ),
        ));
    }

    let integrity = metadata.dist.integrity.as_deref().ok_or_else(|| {
        RegistryError::new(
            RegistryErrorKind::Security,
            "npm latest version metadata has no dist.integrity",
        )
    })?;
    if !is_valid_sha512_integrity(integrity) {
        return Err(RegistryError::new(
            RegistryErrorKind::Security,
            "npm latest version has an invalid SHA-512 integrity value",
        ));
    }

    let tarball = Url::parse(&metadata.dist.tarball).map_err(|error| {
        RegistryError::new(
            RegistryErrorKind::Permanent,
            format!("npm latest version has an invalid tarball URL: {error}"),
        )
    })?;
    if tarball.scheme() != "https"
        || tarball.host_str().is_none()
        || !tarball.username().is_empty()
        || tarball.password().is_some()
    {
        return Err(RegistryError::new(
            RegistryErrorKind::Security,
            "npm latest version tarball URL must be credential-free HTTPS",
        ));
    }

    Ok(LatestRelease {
        version,
        integrity: integrity.to_owned(),
        tarball,
        engines_node: metadata
            .engines
            .as_ref()
            .and_then(|engines| engines.node.clone()),
    })
}

fn is_valid_sha512_integrity(integrity: &str) -> bool {
    integrity.split_ascii_whitespace().any(|token| {
        token.strip_prefix("sha512-").is_some_and(|digest| {
            digest.len() == 88
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'='))
                && digest
                    .bytes()
                    .position(|byte| byte == b'=')
                    .is_none_or(|padding_start| {
                        digest.as_bytes()[padding_start..]
                            .iter()
                            .all(|byte| *byte == b'=')
                            && digest.len() - padding_start == 2
                    })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn registries_are_fixed_with_official_first() {
        assert_eq!(NPM_REGISTRIES.len(), 4);
        assert!(NPM_REGISTRIES[0].official);
        assert_eq!(NPM_REGISTRIES[0].base_url, "https://registry.npmjs.org/");
        assert_eq!(
            NPM_REGISTRIES
                .iter()
                .map(|registry| registry.base_url)
                .collect::<Vec<_>>(),
            vec![
                "https://registry.npmjs.org/",
                "https://registry.npmmirror.com/",
                "https://mirrors.cloud.tencent.com/npm/",
                "https://repo.huaweicloud.com/repository/npm/",
            ]
        );
    }

    #[test]
    fn builds_scoped_packument_url_without_shell_or_query_encoding() {
        assert_eq!(DSH_PACKAGE_NAME, "@deepseek-ai/dsh");
        assert_eq!(
            NPM_REGISTRIES[0]
                .packument_url(DSH_PACKAGE_NAME)
                .unwrap()
                .as_str(),
            "https://registry.npmjs.org/@deepseek-ai%2fdsh"
        );
    }

    #[test]
    fn only_availability_failures_allow_registry_fallback() {
        assert!(RegistryErrorKind::Availability.should_fallback());
        assert!(!RegistryErrorKind::AuthoritativeNotFound.should_fallback());
        assert!(!RegistryErrorKind::Security.should_fallback());
        assert!(!RegistryErrorKind::Permanent.should_fallback());
    }

    #[test]
    fn classifies_retryable_http_failures_as_availability() {
        for status in [
            StatusCode::REQUEST_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
        ] {
            assert_eq!(
                classify_http_status(status, true),
                RegistryErrorKind::Availability
            );
        }
    }

    #[test]
    fn official_not_found_is_authoritative_and_other_client_errors_are_permanent() {
        assert_eq!(
            classify_http_status(StatusCode::NOT_FOUND, true),
            RegistryErrorKind::AuthoritativeNotFound
        );
        assert_eq!(
            classify_http_status(StatusCode::NOT_FOUND, false),
            RegistryErrorKind::Availability
        );
        assert_eq!(
            classify_http_status(StatusCode::UNAUTHORIZED, true),
            RegistryErrorKind::Permanent
        );
        assert_eq!(
            classify_http_status(StatusCode::FORBIDDEN, false),
            RegistryErrorKind::Permanent
        );
    }

    #[test]
    fn parses_latest_exact_version_and_integrity_from_packument() {
        let fixture = r#"
        {
          "dist-tags": { "latest": "0.1.0-rc.6", "next": "0.1.0-rc.6" },
          "versions": {
            "0.1.0-rc.5": {
              "version": "0.1.0-rc.5",
              "dist": { "integrity": "sha512-b2xk", "tarball": "https://example.test/old.tgz" }
            },
            "0.1.0-rc.6": {
              "version": "0.1.0-rc.6",
              "dist": {
                "integrity": "sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg==",
                "tarball": "https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-0.1.0-rc.6.tgz"
              }
            }
          }
        }
        "#;

        let release = parse_latest_release(fixture).unwrap();

        assert_eq!(release.version.to_string(), "0.1.0-rc.6");
        assert_eq!(
            release.integrity,
            "sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg=="
        );
        assert_eq!(release.engines_node, None);
        assert_eq!(
            release.tarball.as_str(),
            "https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-0.1.0-rc.6.tgz"
        );
    }

    #[test]
    fn parses_optional_node_engine_requirement() {
        let fixture = r#"
        {
          "dist-tags": { "latest": "2.0.0" },
          "versions": {
            "2.0.0": {
              "version": "2.0.0",
              "engines": { "node": "^22.19.0 || >=24.0.0" },
              "dist": {
                "integrity": "sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg==",
                "tarball": "https://registry.npmjs.org/@deepseek-ai/dsh/-/dsh-2.0.0.tgz"
              }
            }
          }
        }
        "#;

        assert_eq!(
            parse_latest_release(fixture).unwrap().engines_node,
            Some("^22.19.0 || >=24.0.0".to_owned())
        );
    }

    #[test]
    fn timeout_and_retry_defaults_are_bounded() {
        assert_eq!(REGISTRY_CONNECT_TIMEOUT, Duration::from_secs(5));
        assert_eq!(REGISTRY_REQUEST_TIMEOUT, Duration::from_secs(20));
        assert_eq!(OFFICIAL_REGISTRY_ATTEMPTS, 3);
        assert_eq!(OFFICIAL_REGISTRY_ADDITIONAL_RETRIES, 2);
        assert_eq!(MIRROR_REGISTRY_ATTEMPTS, 1);
        assert_eq!(
            OFFICIAL_REGISTRY_RETRY_DELAYS,
            [Duration::from_secs(1), Duration::from_secs(3)]
        );
        assert!(REGISTRY_RETRY_JITTER_MAX <= Duration::from_millis(500));
    }

    #[test]
    fn rejects_non_https_tarballs_as_security_failures() {
        let fixture = r#"
        {
          "dist-tags": { "latest": "1.2.3" },
          "versions": {
            "1.2.3": {
              "version": "1.2.3",
              "dist": {
                "integrity": "sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg==",
                "tarball": "http://registry.example.test/dsh.tgz"
              }
            }
          }
        }
        "#;

        assert_eq!(
            parse_latest_release(fixture).unwrap_err().kind(),
            RegistryErrorKind::Security
        );
    }

    #[test]
    fn rejects_tarball_urls_containing_credentials() {
        let fixture = r#"
        {
          "dist-tags": { "latest": "1.2.3" },
          "versions": {
            "1.2.3": {
              "version": "1.2.3",
              "dist": {
                "integrity": "sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg==",
                "tarball": "https://token@example.test/dsh.tgz"
              }
            }
          }
        }
        "#;

        assert_eq!(
            parse_latest_release(fixture).unwrap_err().kind(),
            RegistryErrorKind::Security
        );
    }

    #[test]
    fn rejects_latest_tag_without_matching_version_metadata() {
        let fixture = r#"
        {
          "dist-tags": { "latest": "1.2.3" },
          "versions": {
            "1.2.2": {
              "version": "1.2.2",
              "dist": { "integrity": "sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg==", "tarball": "https://example.test/dsh.tgz" }
            }
          }
        }
        "#;

        let error = parse_latest_release(fixture).unwrap_err();
        assert_eq!(error.kind(), RegistryErrorKind::Permanent);
    }

    #[test]
    fn rejects_mismatched_or_non_exact_versions() {
        let mismatched = r#"
        {
          "dist-tags": { "latest": "1.2.3" },
          "versions": {
            "1.2.3": {
              "version": "1.2.4",
              "dist": { "integrity": "sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg==", "tarball": "https://example.test/dsh.tgz" }
            }
          }
        }
        "#;
        let non_exact = r#"
        {
          "dist-tags": { "latest": "^1.2.3" },
          "versions": {}
        }
        "#;

        assert_eq!(
            parse_latest_release(mismatched).unwrap_err().kind(),
            RegistryErrorKind::Permanent
        );
        assert_eq!(
            parse_latest_release(non_exact).unwrap_err().kind(),
            RegistryErrorKind::Permanent
        );
    }

    #[test]
    fn missing_or_malformed_integrity_is_a_security_failure() {
        for integrity in ["", "sha1-YWJjZA==", "sha512-not valid"] {
            let fixture = format!(
                r#"
                {{
                  "dist-tags": {{ "latest": "1.2.3" }},
                  "versions": {{
                    "1.2.3": {{
                      "version": "1.2.3",
                      "dist": {{
                        "integrity": "{integrity}",
                        "tarball": "https://example.test/dsh.tgz"
                      }}
                    }}
                  }}
                }}
                "#
            );

            assert_eq!(
                parse_latest_release(&fixture).unwrap_err().kind(),
                RegistryErrorKind::Security
            );
        }
    }

    #[test]
    fn accepts_sha512_from_a_multi_algorithm_sri_value() {
        let fixture = r#"
        {
          "dist-tags": { "latest": "1.2.3" },
          "versions": {
            "1.2.3": {
              "version": "1.2.3",
              "dist": {
                "integrity": "sha256-YWJjZA== sha512-brpZfED7ieRa2PQ5tUxMhHrM1pb2CmKFVM/f6yMULBDMicahk+Z2OsHgTwTDnoiZm23Ftu9rQz0NN4pflaoJcg==",
                "tarball": "https://example.test/dsh.tgz"
              }
            }
          }
        }
        "#;

        assert!(parse_latest_release(fixture).is_ok());
    }

    #[test]
    fn missing_integrity_is_a_security_failure() {
        let fixture = r#"
        {
          "dist-tags": { "latest": "1.2.3" },
          "versions": {
            "1.2.3": {
              "version": "1.2.3",
              "dist": { "tarball": "https://example.test/dsh.tgz" }
            }
          }
        }
        "#;

        assert_eq!(
            parse_latest_release(fixture).unwrap_err().kind(),
            RegistryErrorKind::Security
        );
    }
}
