//! High-level abstractions around a LaserCube/LaserDock USB DAC.

pub mod stream;

pub use self::stream::Stream;

use std::fmt;

/// Device status.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DeviceStatus {
    /// Device is not initialized.
    #[default]
    Unknown,
    /// Device is initialized and ready.
    Initialized,
}

/// Firmware version information.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FirmwareVersion {
    /// Major version number.
    pub major: u32,
    /// Minor version number.
    pub minor: u32,
}

impl fmt::Display for FirmwareVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// Device capabilities and status.
#[derive(Clone, Debug, Default)]
pub struct DeviceInfo {
    /// Firmware version.
    pub firmware_version: FirmwareVersion,
    /// Maximum DAC sample rate.
    pub max_dac_rate: u32,
    /// Minimum DAC value.
    pub min_dac_value: u32,
    /// Maximum DAC value.
    pub max_dac_value: u32,
    /// Number of sample elements.
    pub sample_element_count: u32,
    /// Bulk packet sample count.
    pub bulk_packet_sample_count: u32,
    /// Current DAC rate.
    pub current_rate: u32,
    /// Ring buffer capacity.
    pub ringbuffer_capacity: u32,
    /// Current free space in ring buffer.
    pub ringbuffer_free_space: u32,
    /// Whether output is enabled.
    pub output_enabled: bool,
}

impl fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "LaserCube USB (firmware: {}, max rate: {} Hz, buffer: {}/{})",
            self.firmware_version,
            self.max_dac_rate,
            self.ringbuffer_free_space,
            self.ringbuffer_capacity
        )
    }
}
