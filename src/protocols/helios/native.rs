//! Helios USB communication.

use std::time::Duration;

use super::{DeviceStatus, Frame};
use rusb::{Context, Device, UsbContext};
use thiserror::Error;

type Result<T> = std::result::Result<T, HeliosDacError>;

const SDK_VERSION: u8 = 6;

const HELIOS_VID: u16 = 0x1209;
const HELIOS_PID: u16 = 0xE500;

const _HELIOS_MAX_POINTS: u32 = 0x1000;
const _HELIOS_MAX_RATE: u32 = 0xFFFF;
const _HELIOS_MIN_RATE: u32 = 7;

const _FRAME_BUFFER_SIZE: u32 = _HELIOS_MAX_POINTS * 7 + 5;

// Interrupt endpoints
const ENDPOINT_BULK_OUT: u8 = 0x02;
const _ENDPOINT_BULK_IN: u8 = 0x81;
const ENDPOINT_INT_OUT: u8 = 0x06;
const ENDPOINT_INT_IN: u8 = 0x83;

// Interrupt control bytes
const CONTROL_STOP: u8 = 0x01;
const CONTROL_SET_SHUTTER: u8 = 0x02;
const CONTROL_GET_STATUS: u8 = 0x03;
const CONTROL_GET_FIRMWARE_VERSION: u8 = 0x04;
const CONTROL_GET_NAME: u8 = 0x05;
const _CONTROL_SET_NAME: u8 = 0x06;
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

    /// Writes and outputs a frame to the DAC.
    pub fn write_frame(&mut self, frame: Frame) -> Result<()> {
        if let HeliosDac::Open { handle, .. } = self {
            let mut frame_buffer = Vec::with_capacity(frame.points.len() * 7 + 5);

            // this is a bug workaround, the mcu won't correctly receive transfers with these sizes
            let mut pps_actual = frame.pps;
            let mut num_of_points_actual = frame.points.len();
            if (frame.points.len() - 45).is_multiple_of(64) {
                num_of_points_actual -= 1;
                // adjust pps to keep the same frame duration even with one less point
                pps_actual = frame.pps
                    * ((num_of_points_actual as f32) / (frame.points.len() as f32) + 0.5f32) as u32;
            }

            for point in frame.points {
                frame_buffer.push((point.coordinate.x >> 4) as u8);
                frame_buffer.push(
                    ((point.coordinate.x & 0x0F) << 4) as u8 | (point.coordinate.y >> 8) as u8,
                );
                frame_buffer.push((point.coordinate.y & 0xFF) as u8);
                frame_buffer.push(point.color.r);
                frame_buffer.push(point.color.g);
                frame_buffer.push(point.color.b);
                frame_buffer.push(point.intensity);
            }
            frame_buffer.push((pps_actual & 0xFF) as u8);
            frame_buffer.push((pps_actual >> 8) as u8);
            frame_buffer.push((num_of_points_actual & 0xFF) as u8);
            frame_buffer.push((num_of_points_actual >> 8) as u8);
            frame_buffer.push(frame.flags.bits());

            handle.write_bulk(
                ENDPOINT_BULK_OUT,
                &frame_buffer,
                bulk_transfer_timeout(frame_buffer.len()),
            )?;

            Ok(())
        } else {
            Err(HeliosDacError::DeviceNotOpened)
        }
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
        if let HeliosDac::Open { handle, .. } = self {
            let written_length =
                handle.write_interrupt(ENDPOINT_INT_OUT, buffer, Duration::from_millis(16))?;
            assert_eq!(written_length, buffer.len());

            Ok(())
        } else {
            Err(HeliosDacError::DeviceNotOpened)
        }
    }

    fn read_response(&self) -> Result<([u8; 32], usize)> {
        if let HeliosDac::Open { handle, .. } = self {
            let mut buffer: [u8; 32] = [0; 32];
            let size =
                handle.read_interrupt(ENDPOINT_INT_IN, &mut buffer, Duration::from_millis(32))?;

            Ok((buffer, size))
        } else {
            Err(HeliosDacError::DeviceNotOpened)
        }
    }
}

impl From<rusb::Device<rusb::Context>> for HeliosDac {
    fn from(device: Device<Context>) -> Self {
        HeliosDac::Idle(device)
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
}
