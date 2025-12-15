//! LaserCube/LaserDock USB DAC protocol implementation.
//!
//! This module provides support for LaserCube and LaserDock USB laser DACs.
//!
//! # Example
//!
//! ```no_run
//! use laser_dac::protocols::lasercube_usb::{discover_dacs, dac::Stream, protocol::Sample};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Discover LaserCube USB devices
//!     let devices = discover_dacs()?;
//!
//!     if let Some(device) = devices.into_iter().next() {
//!         // Open the device
//!         let mut stream = Stream::open(device)?;
//!
//!         // Enable output
//!         stream.enable_output()?;
//!
//!         // Set sample rate
//!         stream.set_rate(30000)?;
//!
//!         // Send some samples
//!         let samples: Vec<Sample> = (0..100)
//!             .map(|i| Sample::new(i * 40, i * 40, 255, 0, 0))
//!             .collect();
//!         stream.send_samples(&samples)?;
//!
//!         // Stop when done
//!         stream.stop()?;
//!     }
//!
//!     Ok(())
//! }
//! ```

pub mod dac;
pub mod error;
pub mod protocol;

pub use crate::protocols::lasercube_usb::dac::{DeviceInfo, DeviceStatus, FirmwareVersion, Stream};
pub use crate::protocols::lasercube_usb::error::{Error, Result};
pub use crate::protocols::lasercube_usb::protocol::Sample;

// Re-export rusb for consumers that need the Context type
pub use rusb;

use protocol::{LASERDOCK_PID, LASERDOCK_VID};
use rusb::UsbContext;

/// A controller for managing LaserCube/LaserDock USB DAC discovery.
pub struct DacController {
    context: rusb::Context,
}

impl DacController {
    /// Create a new DAC controller.
    ///
    /// This initializes the USB context for device discovery.
    pub fn new() -> Result<Self> {
        Ok(DacController {
            context: rusb::Context::new()?,
        })
    }

    /// List all connected LaserCube/LaserDock USB devices.
    pub fn list_devices(&self) -> Result<Vec<rusb::Device<rusb::Context>>> {
        let devices = self.context.devices()?;
        let mut dacs = Vec::new();

        for device in devices.iter() {
            let descriptor = device.device_descriptor()?;
            if descriptor.vendor_id() == LASERDOCK_VID && descriptor.product_id() == LASERDOCK_PID {
                dacs.push(device);
            }
        }

        Ok(dacs)
    }

    /// Open the first available LaserCube/LaserDock USB device.
    pub fn open_first(&self) -> Result<dac::Stream<rusb::Context>> {
        let devices = self.list_devices()?;
        let device = devices.into_iter().next().ok_or(Error::DeviceNotOpened)?;
        dac::Stream::open(device)
    }
}

/// Discover LaserCube/LaserDock USB devices on the system.
///
/// This is a convenience function that creates a controller and lists devices.
///
/// # Example
///
/// ```no_run
/// use laser_dac::protocols::lasercube_usb::discover_dacs;
///
/// let devices = discover_dacs().expect("failed to discover devices");
/// println!("Found {} device(s)", devices.len());
/// ```
pub fn discover_dacs() -> Result<Vec<rusb::Device<rusb::Context>>> {
    let controller = DacController::new()?;
    controller.list_devices()
}

/// Check if a USB device is a LaserCube/LaserDock.
pub fn is_laserdock_device<T: UsbContext>(device: &rusb::Device<T>) -> bool {
    if let Ok(descriptor) = device.device_descriptor() {
        descriptor.vendor_id() == LASERDOCK_VID && descriptor.product_id() == LASERDOCK_PID
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sample_creation() {
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
        assert_eq!(blank.red(), 0);
        assert_eq!(blank.green(), 0);
        assert_eq!(blank.blue(), 0);
    }
}
