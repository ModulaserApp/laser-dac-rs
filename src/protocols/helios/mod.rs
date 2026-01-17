//! Helios laser DAC protocol implementation.
//!
//! Helios is a USB-based laser DAC that uses libusb for communication.
//!
//! # Example
//!
//! ```no_run
//! use laser_dac::protocols::helios::{HeliosDacController, Frame, Point, Coordinate, Color};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let controller = HeliosDacController::new()?;
//!     let dacs = controller.list_devices()?;
//!
//!     if let Some(dac) = dacs.into_iter().next() {
//!         let mut dac = dac.open()?;
//!
//!         let points = vec![
//!             Point {
//!                 coordinate: Coordinate { x: 0, y: 0 },
//!                 color: Color::new(255, 0, 0),
//!                 intensity: 255,
//!             },
//!         ];
//!
//!         let frame = Frame::new(30000, points);
//!         dac.write_frame(frame)?;
//!     }
//!
//!     Ok(())
//! }
//! ```

pub mod backend;
mod frame;
mod native;

pub use backend::HeliosBackend;
pub use frame::*;
pub use native::*;

/// Device status returned by the Helios DAC.
#[derive(Debug, Clone, Copy)]
pub enum DeviceStatus {
    /// Device is ready to receive frame
    Ready = 1,
    /// Device is not ready to receive frame
    NotReady = 0,
}
