//! LaserCube network protocol errors.

use std::fmt;

#[derive(Debug)]
pub enum CommunicationError {
    Io(std::io::Error),
    Protocol(String),
    WorkerStopped,
    QueueFull,
}

impl fmt::Display for CommunicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::Protocol(msg) => write!(f, "protocol error: {}", msg),
            Self::WorkerStopped => write!(f, "worker stopped"),
            Self::QueueFull => write!(f, "host queue full"),
        }
    }
}

impl std::error::Error for CommunicationError {}

impl From<std::io::Error> for CommunicationError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;
    use std::io;

    #[test]
    fn display_matches_variant() {
        assert_eq!(
            CommunicationError::WorkerStopped.to_string(),
            "worker stopped"
        );
        assert_eq!(CommunicationError::QueueFull.to_string(), "host queue full");
        assert_eq!(
            CommunicationError::Protocol("boom".to_string()).to_string(),
            "protocol error: boom"
        );
        let io_err = CommunicationError::from(io::Error::other("nope"));
        assert!(io_err.to_string().starts_with("I/O error:"));
    }

    #[test]
    fn from_io_error_wraps_io_variant() {
        let err = CommunicationError::from(io::Error::new(io::ErrorKind::BrokenPipe, "pipe"));
        assert!(matches!(err, CommunicationError::Io(_)));
        // Implements std::error::Error (no source set).
        assert!(err.source().is_none());
        assert!(format!("{err:?}").contains("Io"));
    }
}
