//! Error types for the laser-dac crate.

use std::error::Error as StdError;
use std::fmt;

// =============================================================================
// Streaming Error
// =============================================================================

/// Streaming-specific error type.
///
/// This error type is designed for the streaming API and includes variants
/// that enforce the uniform backpressure contract across all backends.
#[derive(Debug)]
pub enum Error {
    /// The device/library cannot accept more data right now.
    WouldBlock,

    /// The device disconnected or became unreachable.
    Disconnected(String),

    /// Invalid configuration or API misuse.
    InvalidConfig(String),

    /// Backend/protocol error (wrapped).
    Backend(Box<dyn StdError + Send + Sync>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::WouldBlock => write!(f, "would block: device cannot accept more data"),
            Error::Disconnected(msg) => write!(f, "disconnected: {}", msg),
            Error::InvalidConfig(msg) => write!(f, "invalid configuration: {}", msg),
            Error::Backend(e) => write!(f, "backend error: {}", e),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            Error::Backend(e) => Some(e.as_ref()),
            _ => None,
        }
    }
}

impl Error {
    /// Create a disconnected error with a message.
    pub fn disconnected(msg: impl Into<String>) -> Self {
        Error::Disconnected(msg.into())
    }

    /// Create an invalid config error with a message.
    pub fn invalid_config(msg: impl Into<String>) -> Self {
        Error::InvalidConfig(msg.into())
    }

    /// Create a backend error from any error type.
    pub fn backend(err: impl StdError + Send + Sync + 'static) -> Self {
        Error::Backend(Box::new(err))
    }

    /// Returns true if this is a WouldBlock error.
    pub fn is_would_block(&self) -> bool {
        matches!(self, Error::WouldBlock)
    }

    /// Returns true if this is a Disconnected error.
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Error::Disconnected(_))
    }
}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        if err.kind() == std::io::ErrorKind::WouldBlock {
            Error::WouldBlock
        } else {
            Error::Backend(Box::new(err))
        }
    }
}

/// Result type for streaming operations.
pub type Result<T> = std::result::Result<T, Error>;
