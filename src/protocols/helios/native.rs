//! Helios USB communication.

use std::time::Duration;

use super::{DeviceStatus, Frame};
use rusb::{Context, Device, UsbContext};
use thiserror::Error;

type Result<T> = std::result::Result<T, HeliosDacError>;

const SDK_VERSION: u8 = 6;

const HELIOS_VID: u16 = 0x1209;
const HELIOS_PID: u16 = 0xE500;

// USB endpoints
const ENDPOINT_BULK_OUT: u8 = 0x02;
const ENDPOINT_INT_OUT: u8 = 0x06;
const ENDPOINT_INT_IN: u8 = 0x83;

// Interrupt control bytes
const CONTROL_STOP: u8 = 0x01;
const CONTROL_SET_SHUTTER: u8 = 0x02;
const CONTROL_GET_STATUS: u8 = 0x03;
const CONTROL_GET_FIRMWARE_VERSION: u8 = 0x04;
const CONTROL_GET_NAME: u8 = 0x05;
const CONTROL_SEND_SDK_VERSION: u8 = 0x07;

/// Controller for discovering and managing Helios DACs.
pub struct HeliosDacController {
    context: rusb::Context,
}

impl HeliosDacController {
    /// Create a new Helios DAC controller.
    pub fn new() -> Result<Self> {
        Ok(HeliosDacController {
            context: rusb::Context::new()?,
        })
    }

    /// List all connected Helios DAC devices.
    pub fn list_devices(&self) -> Result<Vec<HeliosDac>> {
        let dacs = self
            .context
            .devices()?
            .iter()
            .filter_map(|device| {
                let descriptor = device.device_descriptor().ok()?;
                (descriptor.vendor_id() == HELIOS_VID && descriptor.product_id() == HELIOS_PID)
                    .then(|| device.into())
            })
            .collect();
        Ok(dacs)
    }
}

/// Mock USB device state for testing without real hardware.
#[cfg(test)]
pub(crate) struct MockUsbState {
    /// When `false`, all USB operations fail with `disconnect_error`.
    pub connected: bool,
    /// The `rusb::Error` variant returned when the mock device is disconnected.
    pub disconnect_error: fn() -> rusb::Error,
}

#[cfg(test)]
impl MockUsbState {
    pub fn new() -> Self {
        Self {
            connected: true,
            disconnect_error: || rusb::Error::NoDevice,
        }
    }

    pub fn with_disconnect_error(mut self, f: fn() -> rusb::Error) -> Self {
        self.disconnect_error = f;
        self
    }
}

/// A Helios DAC device.
///
/// Can be in either Idle (not opened) or Open state.
pub enum HeliosDac {
    /// Device is idle (not opened).
    Idle(rusb::Device<rusb::Context>),
    /// Device is open and ready for communication.
    Open {
        device: rusb::Device<rusb::Context>,
        handle: rusb::DeviceHandle<rusb::Context>,
    },
    /// Test-only variant that simulates USB communication.
    #[cfg(test)]
    #[allow(private_interfaces)]
    MockOpen(std::sync::Arc<std::sync::Mutex<MockUsbState>>),
}

impl HeliosDac {
    /// Open the device for communication.
    pub fn open(self) -> Result<Self> {
        match self {
            HeliosDac::Idle(device) => {
                let handle = device.open()?;
                handle.claim_interface(0)?;
                handle.set_alternate_setting(0, 1)?;
                let device = HeliosDac::Open { device, handle };

                let _ = device.firmware_version()?;
                device.send_sdk_version()?;

                Ok(device)
            }
            open => Ok(open),
        }
    }

    /// Returns a reference to the open USB handle, or an error if the device is not opened.
    fn handle(&self) -> Result<&rusb::DeviceHandle<rusb::Context>> {
        match self {
            HeliosDac::Open { handle, .. } => Ok(handle),
            #[cfg(test)]
            HeliosDac::MockOpen(_) => Err(HeliosDacError::DeviceNotOpened), // never called for mocks
            _ => Err(HeliosDacError::DeviceNotOpened),
        }
    }

    /// Check mock state and return an error if the simulated device is disconnected.
    #[cfg(test)]
    fn mock_check(state: &std::sync::Arc<std::sync::Mutex<MockUsbState>>) -> Result<()> {
        let s = state.lock().unwrap();
        if s.connected {
            Ok(())
        } else {
            Err(HeliosDacError::UsbError((s.disconnect_error)()))
        }
    }

    /// Writes and outputs a frame to the DAC.
    pub fn write_frame(&mut self, frame: Frame) -> Result<()> {
        #[cfg(test)]
        if let HeliosDac::MockOpen(state) = self {
            return Self::mock_check(state);
        }

        let handle = self.handle()?;
        let frame_buffer = encode_frame(frame);
        let timeout = bulk_transfer_timeout(frame_buffer.len());

        handle.write_bulk(ENDPOINT_BULK_OUT, &frame_buffer, timeout)?;
        Ok(())
    }

    /// Write a pre-encoded frame buffer to the DAC.
    ///
    /// Use [`encode_frame_into`] to build the buffer, then call this to
    /// send it. Avoids allocating a new buffer per frame.
    pub fn write_frame_buffer(&mut self, buf: &[u8]) -> Result<()> {
        #[cfg(test)]
        if let HeliosDac::MockOpen(state) = self {
            return Self::mock_check(state);
        }

        let handle = self.handle()?;
        let timeout = bulk_transfer_timeout(buf.len());
        handle.write_bulk(ENDPOINT_BULK_OUT, buf, timeout)?;
        Ok(())
    }

    /// Gets name of DAC.
    pub fn name(&self) -> Result<String> {
        let ctrl_buffer = [CONTROL_GET_NAME, 0];
        let (buffer, _) = self.call_control(&ctrl_buffer)?;

        match buffer {
            [0x85, bytes @ ..] => {
                let null_byte_position = bytes.iter().position(|b| *b == 0u8).unwrap_or(31); // max length is 30 chars
                let (bytes_until_null, _) = bytes.split_at(null_byte_position);
                let name = String::from_utf8(bytes_until_null.to_vec())?;

                Ok(name)
            }
            _ => Err(HeliosDacError::InvalidDeviceResult),
        }
    }

    /// Get firmware version.
    pub fn firmware_version(&self) -> Result<u32> {
        let ctrl_buffer = [CONTROL_GET_FIRMWARE_VERSION, 0];
        let (buffer, size) = self.call_control(&ctrl_buffer)?;

        match &buffer[0..size] {
            [0x84, b0, b1, b2, b3, ..] => Ok(u32::from_le_bytes([*b0, *b1, *b2, *b3])),
            _ => Err(HeliosDacError::InvalidDeviceResult),
        }
    }

    fn send_sdk_version(&self) -> Result<()> {
        let ctrl_buffer = [CONTROL_SEND_SDK_VERSION, SDK_VERSION];
        self.send_control(&ctrl_buffer)
    }

    /// Get device status (ready or not ready).
    pub fn status(&self) -> Result<DeviceStatus> {
        let ctrl_buffer = [CONTROL_GET_STATUS, 0];
        let (buffer, size) = self.call_control(&ctrl_buffer)?;

        match &buffer[0..size] {
            [0x83, 0] => Ok(DeviceStatus::NotReady),
            [0x83, 1] => Ok(DeviceStatus::Ready),
            _ => Err(HeliosDacError::InvalidDeviceResult),
        }
    }

    /// Stops output of DAC.
    pub fn stop(&self) -> Result<()> {
        let ctrl_buffer = [CONTROL_STOP, 0];
        self.send_control(&ctrl_buffer)
    }

    /// Opens or closes the hardware shutter.
    pub fn set_shutter(&self, open: bool) -> Result<()> {
        let ctrl_buffer = [CONTROL_SET_SHUTTER, open as u8];
        self.send_control(&ctrl_buffer)
    }

    fn call_control(&self, buffer: &[u8]) -> Result<([u8; 32], usize)> {
        self.send_control(buffer)?;
        self.read_response()
    }

    fn send_control(&self, buffer: &[u8]) -> Result<()> {
        #[cfg(test)]
        if let HeliosDac::MockOpen(state) = self {
            return Self::mock_check(state);
        }

        let handle = self.handle()?;
        let written_length =
            handle.write_interrupt(ENDPOINT_INT_OUT, buffer, Duration::from_millis(16))?;
        assert_eq!(written_length, buffer.len());
        Ok(())
    }

    fn read_response(&self) -> Result<([u8; 32], usize)> {
        #[cfg(test)]
        if let HeliosDac::MockOpen(state) = self {
            Self::mock_check(state)?;
            // Return a status-ready response (0x83 = status prefix, 1 = ready).
            let mut buf = [0u8; 32];
            buf[0] = 0x83;
            buf[1] = 1;
            return Ok((buf, 2));
        }

        let handle = self.handle()?;
        let mut buffer: [u8; 32] = [0; 32];
        let size =
            handle.read_interrupt(ENDPOINT_INT_IN, &mut buffer, Duration::from_millis(32))?;
        Ok((buffer, size))
    }
}

impl From<rusb::Device<rusb::Context>> for HeliosDac {
    fn from(device: Device<Context>) -> Self {
        HeliosDac::Idle(device)
    }
}

fn encode_frame(frame: Frame) -> Vec<u8> {
    let mut buf = Vec::with_capacity(frame.points.len() * 7 + 5);
    encode_frame_into(frame.pps, &frame.points, frame.flags, &mut buf);
    buf
}

/// Encode a Helios frame into a reusable byte buffer.
///
/// Clears `buf` and writes the wire-format bytes for the given points,
/// PPS, and flags. Use with [`HeliosDac::write_frame_buffer`] to avoid
/// per-frame heap allocation.
pub fn encode_frame_into(
    pps: u32,
    points: &[super::Point],
    flags: super::WriteFrameFlags,
    buf: &mut Vec<u8>,
) {
    let requested_points = points.len();
    let (pps_actual, num_of_points_actual) = adjusted_frame_params(pps, requested_points);

    buf.clear();
    buf.reserve(num_of_points_actual * 7 + 5);
    for point in points.iter().take(num_of_points_actual) {
        buf.extend_from_slice(&[
            (point.coordinate.x >> 4) as u8,
            ((point.coordinate.x & 0x0F) << 4) as u8 | (point.coordinate.y >> 8) as u8,
            (point.coordinate.y & 0xFF) as u8,
            point.color.r,
            point.color.g,
            point.color.b,
            point.intensity,
        ]);
    }
    buf.push((pps_actual & 0xFF) as u8);
    buf.push((pps_actual >> 8) as u8);
    buf.push((num_of_points_actual & 0xFF) as u8);
    buf.push((num_of_points_actual >> 8) as u8);
    buf.push(flags.bits());
}

fn adjusted_frame_params(requested_pps: u32, requested_points: usize) -> (u32, usize) {
    if requested_points >= 45 && (requested_points - 45).is_multiple_of(64) {
        let actual_points = requested_points - 1;
        let pps_actual = (((requested_pps as u64) * (actual_points as u64))
            + (requested_points as u64 / 2))
            / (requested_points as u64);
        log::debug!(
            "helios transfer-size workaround applied: requested_points={}, actual_points={}, requested_pps={}, actual_pps={}",
            requested_points,
            actual_points,
            requested_pps,
            pps_actual
        );
        (pps_actual as u32, actual_points)
    } else {
        (requested_pps, requested_points)
    }
}

/// Compute the USB bulk transfer timeout for a frame buffer.
///
/// Matches the Helios C++ SDK formula: `8 + (bufferSize >> 5)` ms.
/// The constant 8ms base ensures a minimum timeout even for tiny frames.
fn bulk_transfer_timeout(buffer_len: usize) -> Duration {
    Duration::from_millis((8 + (buffer_len >> 5)) as u64)
}

/// Errors that can occur when communicating with a Helios DAC.
#[derive(Error, Debug)]
pub enum HeliosDacError {
    #[error("device is not opened")]
    DeviceNotOpened,
    #[error("usb connection error: {0}")]
    UsbError(#[from] rusb::Error),
    #[error("usb device answered with invalid data")]
    InvalidDeviceResult,
    #[error("could not parse string: {0}")]
    Utf8Error(#[from] std::string::FromUtf8Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::helios::{Color, Coordinate, Point, WriteFrameFlags};

    fn test_point() -> Point {
        Point {
            coordinate: Coordinate { x: 0x123, y: 0x456 },
            color: Color::new(0x11, 0x22, 0x33),
            intensity: 0x44,
        }
    }

    #[test]
    fn test_bulk_transfer_timeout_matches_sdk() {
        // C++ SDK: 8 + (frameBufferSize >> 5) ms
        // Must have an 8ms minimum floor for any buffer size.

        // 1 point = 12 bytes → 8 + 0 = 8ms
        assert_eq!(bulk_transfer_timeout(12), Duration::from_millis(8));

        // 10 points = 75 bytes → 8 + 2 = 10ms
        assert_eq!(bulk_transfer_timeout(75), Duration::from_millis(10));

        // 100 points = 710 bytes → 8 + 22 = 30ms
        assert_eq!(bulk_transfer_timeout(710), Duration::from_millis(30));

        // 4095 points = 28670 bytes → 8 + 895 = 903ms
        assert_eq!(bulk_transfer_timeout(28670), Duration::from_millis(903));

        // Empty buffer → still 8ms minimum
        assert_eq!(bulk_transfer_timeout(0), Duration::from_millis(8));
    }

    #[test]
    fn test_adjusted_frame_params_leaves_small_frames_unchanged() {
        assert_eq!(adjusted_frame_params(30_000, 1), (30_000, 1));
        assert_eq!(adjusted_frame_params(30_000, 44), (30_000, 44));
    }

    #[test]
    fn test_adjusted_frame_params_applies_problem_size_workaround() {
        assert_eq!(adjusted_frame_params(30_000, 45), (29_333, 44));
        assert_eq!(adjusted_frame_params(30_000, 109), (29_725, 108));
    }

    #[test]
    fn test_encode_frame_truncates_problematic_transfer_payload() {
        let points = vec![test_point(); 109];
        let buffer = encode_frame(Frame::new_with_flags(
            30_000,
            points,
            WriteFrameFlags::SINGLE_MODE,
        ));

        assert_eq!(buffer.len(), 108 * 7 + 5);

        let footer = &buffer[buffer.len() - 5..];
        assert_eq!(u16::from_le_bytes([footer[0], footer[1]]), 29_725);
        assert_eq!(u16::from_le_bytes([footer[2], footer[3]]), 108);
        assert_eq!(footer[4], WriteFrameFlags::SINGLE_MODE.bits());
    }
}
