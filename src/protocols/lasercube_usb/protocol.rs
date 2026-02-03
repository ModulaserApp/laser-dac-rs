//! Low-level USB protocol types and constants for LaserCube/LaserDock DAC communication.

use crate::types::LaserPoint;

/// USB Vendor ID for LaserDock/LaserCube USB devices.
pub const LASERDOCK_VID: u16 = 0x1fc9;

/// USB Product ID for LaserDock/LaserCube USB devices.
pub const LASERDOCK_PID: u16 = 0x04d8;

/// Control interface number.
pub const CONTROL_INTERFACE: u8 = 0;

/// Data interface number.
pub const DATA_INTERFACE: u8 = 1;

/// Alternate setting for data interface.
pub const DATA_ALT_SETTING: u8 = 1;

/// Control endpoint (bulk out).
pub const ENDPOINT_CONTROL_OUT: u8 = 0x01;

/// Control endpoint (bulk in).
pub const ENDPOINT_CONTROL_IN: u8 = 0x81;

/// Data endpoint (bulk out).
pub const ENDPOINT_DATA_OUT: u8 = 0x03;

/// Control packet size.
pub const CONTROL_PACKET_SIZE: usize = 64;

// Command bytes
/// Enable or disable laser output.
pub const CMD_SET_OUTPUT: u8 = 0x80;
/// Get output enable status.
pub const CMD_GET_OUTPUT: u8 = 0x81;
/// Set DAC sample rate.
pub const CMD_SET_DAC_RATE: u8 = 0x82;
/// Get DAC sample rate.
pub const CMD_GET_DAC_RATE: u8 = 0x83;
/// Get maximum DAC sample rate.
pub const CMD_GET_MAX_DAC_RATE: u8 = 0x84;
/// Get sample element count.
pub const CMD_GET_SAMPLE_ELEMENT_COUNT: u8 = 0x85;
/// Get ISO packet sample count.
pub const CMD_GET_ISO_PACKET_SAMPLE_COUNT: u8 = 0x86;
/// Get minimum DAC value.
pub const CMD_GET_MIN_DAC_VALUE: u8 = 0x87;
/// Get maximum DAC value.
pub const CMD_GET_MAX_DAC_VALUE: u8 = 0x88;
/// Get ring buffer sample count.
pub const CMD_GET_RINGBUFFER_SAMPLE_COUNT: u8 = 0x89;
/// Get ring buffer empty sample count.
pub const CMD_GET_RINGBUFFER_EMPTY_SAMPLE_COUNT: u8 = 0x8A;
/// Get firmware major version.
pub const CMD_GET_VERSION_MAJOR: u8 = 0x8B;
/// Get firmware minor version.
pub const CMD_GET_VERSION_MINOR: u8 = 0x8C;
/// Clear ring buffer.
pub const CMD_CLEAR_RINGBUFFER: u8 = 0x8D;
/// Get bulk packet sample count.
pub const CMD_GET_BULK_PACKET_SAMPLE_COUNT: u8 = 0x8E;

/// Runner mode command.
pub const CMD_RUNNER_MODE: u8 = 0xC0;
/// Runner mode sub-command: enable.
pub const RUNNER_MODE_SUB_ENABLE: u8 = 0x01;
/// Runner mode sub-command: run.
pub const RUNNER_MODE_SUB_RUN: u8 = 0x09;

/// Maximum coordinate value (12-bit).
pub const MAX_COORDINATE_VALUE: u16 = 4095;

/// Size of a single sample in bytes.
pub const SAMPLE_SIZE_BYTES: usize = 8;

/// A laser sample with position and color.
///
/// The LaserDock uses 12-bit coordinates (0-4095) where:
/// - X: 0 = left edge, 2047 = center, 4095 = right edge
/// - Y: 0 = bottom edge, 2047 = center, 4095 = top edge
///
/// Colors are 8-bit values (0-255).
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// Red (low byte) and Green (high byte) combined.
    pub rg: u16,
    /// Blue (low byte), high byte unused.
    pub b: u16,
    /// X coordinate (12-bit, 0-4095).
    pub x: u16,
    /// Y coordinate (12-bit, 0-4095).
    pub y: u16,
}

impl Sample {
    /// Create a new sample from individual components.
    ///
    /// # Arguments
    /// * `x` - X coordinate (0-4095)
    /// * `y` - Y coordinate (0-4095)
    /// * `r` - Red intensity (0-255)
    /// * `g` - Green intensity (0-255)
    /// * `b` - Blue intensity (0-255)
    pub fn new(x: u16, y: u16, r: u8, g: u8, b: u8) -> Self {
        Self {
            rg: (r as u16) | ((g as u16) << 8),
            b: b as u16,
            x: x.min(MAX_COORDINATE_VALUE),
            y: y.min(MAX_COORDINATE_VALUE),
        }
    }

    /// Create a blank sample (laser off) at the center position.
    pub fn blank() -> Self {
        Self::new(2048, 2048, 0, 0, 0)
    }

    /// Create a blank sample at a specific position.
    pub fn blank_at(x: u16, y: u16) -> Self {
        Self::new(x, y, 0, 0, 0)
    }

    /// Get the red component.
    pub fn red(&self) -> u8 {
        (self.rg & 0xFF) as u8
    }

    /// Get the green component.
    pub fn green(&self) -> u8 {
        ((self.rg >> 8) & 0xFF) as u8
    }

    /// Get the blue component.
    pub fn blue(&self) -> u8 {
        (self.b & 0xFF) as u8
    }

    /// Create a sample from signed coordinates (-32768 to 32767).
    ///
    /// This maps the signed range to the 0-4095 range used by the device.
    pub fn from_signed(x: i16, y: i16, r: u8, g: u8, b: u8) -> Self {
        // Map from [-32768, 32767] to [0, 4095]
        let x_mapped = (((x as i32) + 32768) * 4095 / 65535) as u16;
        let y_mapped = (((y as i32) + 32768) * 4095 / 65535) as u16;
        Self::new(x_mapped, y_mapped, r, g, b)
    }

    /// Convert coordinates to signed values (-32768 to 32767).
    pub fn to_signed(&self) -> (i16, i16) {
        // Map from [0, 4095] to [-32768, 32767]
        let x = ((self.x as i32) * 65535 / 4095 - 32768) as i16;
        let y = ((self.y as i32) * 65535 / 4095 - 32768) as i16;
        (x, y)
    }

    /// Flip the X coordinate.
    pub fn flip_x(&mut self) {
        self.x = MAX_COORDINATE_VALUE - self.x;
    }

    /// Flip the Y coordinate.
    pub fn flip_y(&mut self) {
        self.y = MAX_COORDINATE_VALUE - self.y;
    }

    /// Convert the sample to raw bytes for USB transmission.
    pub fn to_bytes(&self) -> [u8; SAMPLE_SIZE_BYTES] {
        let mut bytes = [0u8; SAMPLE_SIZE_BYTES];
        bytes[0..2].copy_from_slice(&self.rg.to_le_bytes());
        bytes[2..4].copy_from_slice(&self.b.to_le_bytes());
        bytes[4..6].copy_from_slice(&self.x.to_le_bytes());
        bytes[6..8].copy_from_slice(&self.y.to_le_bytes());
        bytes
    }
}

impl From<&LaserPoint> for Sample {
    /// Convert a LaserPoint to a LaserCube USB Sample.
    ///
    /// LaserPoint uses f32 coordinates (-1.0 to 1.0) and u16 colors (0-65535).
    /// LaserCube USB uses 12-bit unsigned coordinates (0-4095) and u8 colors.
    fn from(p: &LaserPoint) -> Self {
        let x = (((p.x.clamp(-1.0, 1.0) + 1.0) / 2.0) * 4095.0) as u16;
        let y = (((p.y.clamp(-1.0, 1.0) + 1.0) / 2.0) * 4095.0) as u16;
        Sample::new(x, y, (p.r >> 8) as u8, (p.g >> 8) as u8, (p.b >> 8) as u8)
    }
}

/// Convert a float in range [-1.0, 1.0] to a LaserDock coordinate (0-4095).
pub fn float_to_coordinate(value: f32) -> u16 {
    ((MAX_COORDINATE_VALUE as f32) * (value + 1.0) / 2.0) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sample_new() {
        let sample = Sample::new(2048, 2048, 255, 128, 64);
        assert_eq!(sample.x, 2048);
        assert_eq!(sample.y, 2048);
        assert_eq!(sample.red(), 255);
        assert_eq!(sample.green(), 128);
        assert_eq!(sample.blue(), 64);
    }

    #[test]
    fn test_sample_blank() {
        let blank = Sample::blank();
        assert_eq!(blank.x, 2048);
        assert_eq!(blank.y, 2048);
        assert_eq!(blank.red(), 0);
        assert_eq!(blank.green(), 0);
        assert_eq!(blank.blue(), 0);
    }

    #[test]
    fn test_sample_clamps_coordinates() {
        let sample = Sample::new(5000, 6000, 0, 0, 0);
        assert_eq!(sample.x, MAX_COORDINATE_VALUE);
        assert_eq!(sample.y, MAX_COORDINATE_VALUE);
    }

    #[test]
    fn test_sample_flip() {
        let mut sample = Sample::new(1000, 3000, 0, 0, 0);
        sample.flip_x();
        sample.flip_y();
        assert_eq!(sample.x, 4095 - 1000);
        assert_eq!(sample.y, 4095 - 3000);
    }

    #[test]
    fn test_sample_size() {
        assert_eq!(std::mem::size_of::<Sample>(), SAMPLE_SIZE_BYTES);
    }

    #[test]
    fn test_float_to_coordinate() {
        assert_eq!(float_to_coordinate(-1.0), 0);
        assert_eq!(float_to_coordinate(0.0), 2047);
        assert_eq!(float_to_coordinate(1.0), 4095);
    }

    #[test]
    fn test_sample_signed_roundtrip() {
        let sample = Sample::from_signed(0, 0, 100, 150, 200);
        let (x, y) = sample.to_signed();
        // Allow some rounding error
        assert!((x as i32).abs() < 100);
        assert!((y as i32).abs() < 100);
    }
}
