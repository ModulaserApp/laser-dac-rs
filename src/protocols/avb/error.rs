//! Error types for AVB DAC operations.

use thiserror::Error;

/// Errors that can occur during AVB DAC operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Audio device could not be found.
    #[error("audio device not found: {0}")]
    DeviceNotFound(String),

    /// No compatible output stream configuration is available.
    #[error("no compatible output config found for AVB device")]
    UnsupportedOutputConfig,

    /// Stream startup failed.
    #[error("failed to start AVB output stream")]
    StreamStartFailed,
}
