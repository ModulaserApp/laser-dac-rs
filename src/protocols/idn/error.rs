//! Error types for the IDN DAC protocol.
//!
//! Uses a 3-layer error model similar to ether-dream:
//! - IO errors from network operations
//! - Protocol errors from invalid data
//! - Response errors from DAC responses

use std::error::Error;
use std::fmt;
use std::io;

// -------------------------------------------------------------------------------------------------
//  Communication Error (Top Level)
// -------------------------------------------------------------------------------------------------

/// Top-level communication error that encompasses all error types.
#[derive(Debug)]
pub enum CommunicationError {
    /// An I/O error occurred (network, socket, etc.)
    Io(io::Error),
    /// A protocol-level error occurred (invalid data format)
    Protocol(ProtocolError),
    /// An error in the DAC's response
    Response(ResponseError),
}

impl fmt::Display for CommunicationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommunicationError::Io(e) => write!(f, "I/O error: {}", e),
            CommunicationError::Protocol(e) => write!(f, "protocol error: {}", e),
            CommunicationError::Response(e) => write!(f, "response error: {}", e),
        }
    }
}

impl Error for CommunicationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            CommunicationError::Io(e) => Some(e),
            CommunicationError::Protocol(e) => Some(e),
            CommunicationError::Response(e) => Some(e),
        }
    }
}

impl From<io::Error> for CommunicationError {
    fn from(e: io::Error) -> Self {
        CommunicationError::Io(e)
    }
}

impl From<ProtocolError> for CommunicationError {
    fn from(e: ProtocolError) -> Self {
        CommunicationError::Protocol(e)
    }
}

impl From<ResponseError> for CommunicationError {
    fn from(e: ResponseError) -> Self {
        CommunicationError::Response(e)
    }
}

// -------------------------------------------------------------------------------------------------
//  Protocol Error
// -------------------------------------------------------------------------------------------------

/// Protocol-level errors for invalid or malformed data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// Invalid packet header
    InvalidPacketHeader,
    /// Unknown command byte
    UnknownCommand(u8),
    /// Unknown service type
    UnknownServiceType(u8),
    /// Packet exceeds maximum UDP payload size
    PacketTooLarge,
    /// Invalid scan response
    InvalidScanResponse,
    /// Invalid service map response
    InvalidServiceMapResponse,
    /// Buffer too small for operation
    BufferTooSmall,
    /// Invalid point format
    InvalidPointFormat,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::InvalidPacketHeader => write!(f, "invalid packet header"),
            ProtocolError::UnknownCommand(cmd) => write!(f, "unknown command: 0x{:02x}", cmd),
            ProtocolError::UnknownServiceType(t) => write!(f, "unknown service type: 0x{:02x}", t),
            ProtocolError::PacketTooLarge => write!(f, "packet exceeds maximum UDP payload size"),
            ProtocolError::InvalidScanResponse => write!(f, "invalid scan response"),
            ProtocolError::InvalidServiceMapResponse => write!(f, "invalid service map response"),
            ProtocolError::BufferTooSmall => write!(f, "buffer too small for operation"),
            ProtocolError::InvalidPointFormat => write!(f, "invalid point format"),
        }
    }
}

impl Error for ProtocolError {}

// -------------------------------------------------------------------------------------------------
//  Response Error
// -------------------------------------------------------------------------------------------------

/// Errors from DAC responses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResponseError {
    /// Timeout waiting for response
    Timeout,
    /// Unexpected response received
    UnexpectedResponse,
    /// Not connected to a service
    NotConnected,
    /// All sessions on the DAC are occupied
    Occupied,
    /// Client group is excluded from streaming
    Excluded,
    /// Invalid payload in request
    InvalidPayload,
    /// Generic processing error
    GenericError,
    /// Unknown acknowledgment code
    UnknownAckCode(i8),
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResponseError::Timeout => write!(f, "timeout waiting for response"),
            ResponseError::UnexpectedResponse => write!(f, "unexpected response"),
            ResponseError::NotConnected => write!(f, "not connected to a service"),
            ResponseError::Occupied => write!(f, "all sessions are occupied"),
            ResponseError::Excluded => write!(f, "client group is excluded from streaming"),
            ResponseError::InvalidPayload => write!(f, "invalid payload in request"),
            ResponseError::GenericError => write!(f, "generic processing error"),
            ResponseError::UnknownAckCode(code) => {
                write!(f, "unknown acknowledgment code: {}", code)
            }
        }
    }
}

impl Error for ResponseError {}

impl ResponseError {
    /// Create a ResponseError from an IDN acknowledgment result code.
    pub fn from_ack_code(code: i8) -> Option<Self> {
        // Negative codes indicate errors
        if code >= 0 {
            return None; // Success
        }

        // IDN acknowledgment error codes (from idn-hello.h)
        const IDNVAL_RTACK_ERR_NOT_CONNECTED: i8 = -21; // 0xEB as i8
        const IDNVAL_RTACK_ERR_OCCUPIED: i8 = -20; // 0xEC as i8
        const IDNVAL_RTACK_ERR_EXCLUDED: i8 = -19; // 0xED as i8
        const IDNVAL_RTACK_ERR_PAYLOAD: i8 = -18; // 0xEE as i8
        const IDNVAL_RTACK_ERR_GENERIC: i8 = -17; // 0xEF as i8

        Some(match code {
            IDNVAL_RTACK_ERR_NOT_CONNECTED => ResponseError::NotConnected,
            IDNVAL_RTACK_ERR_OCCUPIED => ResponseError::Occupied,
            IDNVAL_RTACK_ERR_EXCLUDED => ResponseError::Excluded,
            IDNVAL_RTACK_ERR_PAYLOAD => ResponseError::InvalidPayload,
            IDNVAL_RTACK_ERR_GENERIC => ResponseError::GenericError,
            _ => ResponseError::UnknownAckCode(code),
        })
    }
}

// -------------------------------------------------------------------------------------------------
//  Result Type Alias
// -------------------------------------------------------------------------------------------------

/// Result type alias for IDN operations.
pub type Result<T> = std::result::Result<T, CommunicationError>;
