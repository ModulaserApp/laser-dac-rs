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

    /// The stream was explicitly stopped via `StreamControl::stop()`.
    Stopped,

    /// The device disconnected or became unreachable.
    Disconnected(String),

    /// The OS denied access to the device (e.g. a USB permission failure).
    ///
    /// Distinct from [`Error::Backend`] so consumers can detect a fixable
    /// setup problem — on Linux a USB laser DAC needs a udev rule granting the
    /// user access to the device node — and guide the user rather than surface
    /// a generic error. Retriable: once access is granted (udev rule installed
    /// and the device replugged/re-triggered) a later connect attempt succeeds.
    PermissionDenied(String),

    /// Invalid configuration or API misuse.
    InvalidConfig(String),

    /// Backend/protocol error (wrapped).
    Backend(Box<dyn StdError + Send + Sync>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::WouldBlock => write!(f, "would block: device cannot accept more data"),
            Error::Stopped => write!(f, "stopped: stream was explicitly stopped"),
            Error::Disconnected(msg) => write!(f, "disconnected: {}", msg),
            Error::PermissionDenied(msg) => write!(f, "permission denied: {}", msg),
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

    /// Create a permission-denied error with a message.
    pub fn permission_denied(msg: impl Into<String>) -> Self {
        Error::PermissionDenied(msg.into())
    }

    /// Create a permission-denied error for a USB access failure, appending a
    /// platform-appropriate hint. On Linux, USB laser DACs are inaccessible to
    /// non-root users until a udev rule grants access to the device node, so the
    /// message points at that fix; other platforms get the bare context.
    pub fn usb_permission_denied(context: impl std::fmt::Display) -> Self {
        #[cfg(target_os = "linux")]
        let msg = format!(
            "{context}: USB access denied — this laser DAC needs a udev rule \
             granting the user access to the device node (then replug the device)"
        );
        #[cfg(not(target_os = "linux"))]
        let msg = format!("{context}: USB access denied");
        Error::PermissionDenied(msg)
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

    /// Returns true if this is a PermissionDenied error.
    ///
    /// Consumers can branch on this to guide the user through granting device
    /// access (e.g. installing a udev rule on Linux) instead of showing a
    /// generic failure.
    pub fn is_permission_denied(&self) -> bool {
        matches!(self, Error::PermissionDenied(_))
    }

    /// Returns true if this is a Stopped error.
    pub fn is_stopped(&self) -> bool {
        matches!(self, Error::Stopped)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_denied_is_detected_and_distinct_from_backend() {
        let err = Error::permission_denied("no access");
        assert!(err.is_permission_denied());
        assert!(!err.is_disconnected());
        assert!(!matches!(err, Error::Backend(_)));
    }

    #[test]
    fn usb_permission_denied_carries_context() {
        let err = Error::usb_permission_denied("helios open");
        assert!(err.is_permission_denied());
        let msg = err.to_string();
        assert!(
            msg.contains("helios open"),
            "message keeps the context: {msg}"
        );
        // On Linux the message must point at the udev-rule fix so a consumer can
        // relay actionable guidance; elsewhere it is just the bare access note.
        #[cfg(target_os = "linux")]
        assert!(
            msg.contains("udev"),
            "linux message names the udev fix: {msg}"
        );
    }
}
