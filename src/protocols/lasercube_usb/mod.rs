//! LaserCube/LaserDock USB DAC protocol implementation.
//!
//! This module provides support for LaserCube and LaserDock USB laser DACs.
//!
//! # Example
//!
//! ```no_run
//! use laser_dac::protocols::lasercube_usb::{DacController, dac::Stream, protocol::Sample};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Discover LaserCube USB devices
//!     let controller = DacController::new()?;
//!     let devices = controller.list_devices()?;
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

pub mod backend;
pub mod dac;
pub mod error;
pub mod protocol;

pub use crate::protocols::lasercube_usb::dac::{DeviceInfo, DeviceStatus, FirmwareVersion, Stream};
pub use crate::protocols::lasercube_usb::error::{Error, Result};
pub use crate::protocols::lasercube_usb::protocol::Sample;
pub use backend::LasercubeUsbBackend;

// Re-export rusb for consumers that need the Context type
pub use rusb;

use crate::types::{DacCapabilities, OutputModel};

/// Returns the default capabilities for LaserCube USB DACs.
pub fn default_capabilities() -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max: 35_000,
        max_points_per_chunk: 4096,
        output_model: OutputModel::NetworkFifo,
    }
}

use protocol::{CONTROL_TIMEOUT, LASERDOCK_PID, LASERDOCK_VID};
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

}

/// Read the serial number from a LaserCube USB device.
///
/// Returns `None` if the device cannot be opened or has no serial number.
pub fn get_serial_number<T: UsbContext>(device: &rusb::Device<T>) -> Option<String> {
    let descriptor = device.device_descriptor().ok()?;
    // Check if device has a serial number string
    descriptor.serial_number_string_index()?;
    let handle = device.open().ok()?;
    let languages = handle.read_languages(CONTROL_TIMEOUT).ok()?;
    let lang = languages.first()?;
    handle
        .read_serial_number_string(*lang, &descriptor, CONTROL_TIMEOUT)
        .ok()
}

