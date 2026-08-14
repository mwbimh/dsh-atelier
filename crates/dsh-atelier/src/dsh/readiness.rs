use std::fmt;

use thiserror::Error;
use url::Url;

const READINESS_PREFIX: &str = "dsh web: ";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopbackUrl {
    url: Url,
    port: u16,
}

impl LoopbackUrl {
    pub fn parse(value: &str) -> Result<Self, ReadinessError> {
        let port_text = value
            .strip_prefix("http://127.0.0.1:")
            .ok_or_else(|| ReadinessError::UnsafeUrl(value.to_owned()))?;
        let port = port_text
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .ok_or_else(|| ReadinessError::UnsafeUrl(value.to_owned()))?;
        if port.to_string() != port_text {
            return Err(ReadinessError::UnsafeUrl(value.to_owned()));
        }

        let url = Url::parse(value).map_err(ReadinessError::InvalidUrl)?;

        let is_valid = url.scheme() == "http"
            && url.host_str() == Some("127.0.0.1")
            && url.username().is_empty()
            && url.password().is_none()
            && url.path() == "/"
            && url.query().is_none()
            && url.fragment().is_none();

        if !is_valid {
            return Err(ReadinessError::UnsafeUrl(value.to_owned()));
        }

        Ok(Self { url, port })
    }

    pub fn as_url(&self) -> &Url {
        &self.url
    }

    pub fn as_str(&self) -> &str {
        self.url.as_str()
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl fmt::Display for LoopbackUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.url.fmt(formatter)
    }
}

pub fn parse_readiness_line(line: &str) -> Result<Option<LoopbackUrl>, ReadinessError> {
    let Some(value) = line.strip_prefix(READINESS_PREFIX) else {
        return Ok(None);
    };

    LoopbackUrl::parse(value).map(Some)
}

#[derive(Debug, Error)]
pub enum ReadinessError {
    #[error("readiness URL is malformed: {0}")]
    InvalidUrl(url::ParseError),
    #[error("readiness URL must be exactly http://127.0.0.1:<non-zero-port>: {0}")]
    UnsafeUrl(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_exact_dsh_loopback_readiness_line() {
        let url =
            parse_readiness_line("dsh web: http://127.0.0.1:43127").expect("valid readiness line");
        let url = url.expect("readiness signal");

        assert_eq!(url.as_str(), "http://127.0.0.1:43127/");
        assert_eq!(url.port(), 43127);
    }

    #[test]
    fn retains_an_explicit_default_http_port() {
        let url = parse_readiness_line("dsh web: http://127.0.0.1:80")
            .expect("valid readiness line")
            .expect("readiness signal");

        assert_eq!(url.port(), 80);
    }

    #[test]
    fn ignores_unrelated_stdout() {
        assert_eq!(parse_readiness_line("loader settled").unwrap(), None);
    }

    #[test]
    fn rejects_non_loopback_or_ambiguous_urls() {
        for line in [
            "dsh web: https://127.0.0.1:43127",
            "dsh web: http://localhost:43127",
            "dsh web: http://0.0.0.0:43127",
            "dsh web: http://example.com:43127",
            "dsh web: http://user@127.0.0.1:43127",
            "dsh web: http://127.0.0.1:43127/?token=value",
            "dsh web: http://127.0.0.1:43127/#fragment",
            "dsh web: http://127.0.0.1:43127/other",
            "dsh web: http://127.0.0.1:0",
            "dsh web: http://127.0.0.1",
        ] {
            assert!(
                parse_readiness_line(line).is_err(),
                "unexpectedly accepted {line}"
            );
        }
    }
}
