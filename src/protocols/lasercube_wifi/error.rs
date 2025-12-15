//! Error types for LaserCube WiFi DAC communication.

use std::error::Error;
use std::fmt;
use std::io;

/// Errors that may occur during communication with a LaserCube DAC.
#[derive(Debug)]
pub enum CommunicationError {
    /// An I/O error occurred (network, timeout, etc.).
    Io(io::Error),
    /// Failed to parse a protocol message.
    Protocol(ProtocolError),
    /// No response received from the device.
    NoResponse,
    /// Device buffer is full.
    BufferFull,
    /// Connection was not properly initialized.
    NotInitialized,
}

/// Errors that may occur when parsing protocol messages.
#[derive(Debug)]
pub enum ProtocolError {
    /// The response buffer was too short.
    ResponseTooShort { expected: usize, actual: usize },
    /// Unexpected command byte in response.
    UnexpectedCommand { expected: u8, actual: u8 },
    /// Invalid data in response.
    InvalidData(String),
}

impl Error for CommunicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CommunicationError::Io(err) => Some(err),
            CommunicationError::Protocol(err) => Some(err),
            _ => None,
        }
    }
}

impl Error for ProtocolError {}

impl fmt::Display for CommunicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommunicationError::Io(err) => write!(f, "I/O error: {}", err),
            CommunicationError::Protocol(err) => write!(f, "protocol error: {}", err),
            CommunicationError::NoResponse => write!(f, "no response from device"),
            CommunicationError::BufferFull => write!(f, "device buffer is full"),
            CommunicationError::NotInitialized => write!(f, "connection not initialized"),
        }
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::ResponseTooShort { expected, actual } => {
                write!(
                    f,
                    "response too short: expected {} bytes, got {}",
                    expected, actual
                )
            }
            ProtocolError::UnexpectedCommand { expected, actual } => {
                write!(
                    f,
                    "unexpected command: expected 0x{:02X}, got 0x{:02X}",
                    expected, actual
                )
            }
            ProtocolError::InvalidData(msg) => write!(f, "invalid data: {}", msg),
        }
    }
}

impl From<io::Error> for CommunicationError {
    fn from(err: io::Error) -> Self {
        CommunicationError::Io(err)
    }
}

impl From<ProtocolError> for CommunicationError {
    fn from(err: ProtocolError) -> Self {
        CommunicationError::Protocol(err)
    }
}
