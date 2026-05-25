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
