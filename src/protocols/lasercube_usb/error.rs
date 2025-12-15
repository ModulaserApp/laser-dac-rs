//! Error types for LaserCube USB DAC communication.

use thiserror::Error;

/// Errors that can occur during LaserCube USB DAC operations.
#[derive(Error, Debug)]
pub enum Error {
    /// USB communication error.
    #[error("USB error: {0}")]
    Usb(#[from] rusb::Error),

    /// Device is not in the open state.
    #[error("device is not opened")]
    DeviceNotOpened,

    /// Device returned invalid or unexpected data.
    #[error("invalid device response")]
    InvalidResponse,

    /// Failed to initialize the device.
    #[error("device initialization failed")]
    InitializationFailed,

    /// Operation timed out.
    #[error("operation timed out")]
    Timeout,
}

/// Result type alias for LaserCube USB DAC operations.
pub type Result<T> = std::result::Result<T, Error>;
