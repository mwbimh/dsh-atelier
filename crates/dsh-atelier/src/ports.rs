use thiserror::Error;

use crate::dsh::readiness::LoopbackUrl;

pub trait Browser: Send + Sync {
    fn open(&self, url: &LoopbackUrl) -> Result<(), PlatformError>;
}

pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str) -> Result<(), PlatformError>;
}

pub trait Autostart: Send + Sync {
    fn is_enabled(&self) -> Result<bool, PlatformError>;
    fn set_enabled(&self, enabled: bool) -> Result<(), PlatformError>;
}

#[derive(Debug, Error)]
#[error("platform integration failed: {message}")]
pub struct PlatformError {
    message: String,
}

impl PlatformError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}
