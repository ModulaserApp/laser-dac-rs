//! Error types for LaserCube WiFi DAC communication.

use std::error::Error;
use std::fmt;
use std::io;

/// Errors that may occur during communication with a LaserCube DAC.
#[derive(Debug)]
pub enum CommunicationError {
    /// An I/O error occurred (network, timeout, etc.).
    Io(io::Error),
    /// Connection was not properly initialized.
    NotInitialized,
}

impl Error for CommunicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CommunicationError::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl fmt::Display for CommunicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommunicationError::Io(err) => write!(f, "I/O error: {}", err),
            CommunicationError::NotInitialized => write!(f, "connection not initialized"),
        }
    }
}

impl From<io::Error> for CommunicationError {
    fn from(err: io::Error) -> Self {
        CommunicationError::Io(err)
    }
}
