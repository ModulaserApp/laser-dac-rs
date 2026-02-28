//! DAC backend trait and implementations for the streaming API.
//!
//! This module provides the [`StreamBackend`] trait that all DAC backends must
//! implement, as well as implementations for all supported DAC types.

use crate::types::{DacCapabilities, DacType, LaserPoint};

// Re-export error types for backwards compatibility
pub use crate::error::{Error, Result};

// =============================================================================
// StreamBackend Trait
// =============================================================================

/// Write result from a backend chunk submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The chunk was accepted and written.
    Written,
    /// The device cannot accept more data right now.
    WouldBlock,
}

/// Backend trait for streaming DAC output.
///
/// All backends must implement this trait to support the streaming API.
/// The key contract is uniform backpressure: `try_write_chunk` must return
/// `WriteOutcome::WouldBlock` when the device cannot accept more data,
/// enabling the stream scheduler to pace output correctly.
pub trait StreamBackend: Send + 'static {
    /// Returns the DAC type for this backend.
    fn dac_type(&self) -> DacType;

    /// Returns the device capabilities.
    fn caps(&self) -> &DacCapabilities;

    /// Connect to the device.
    fn connect(&mut self) -> Result<()>;

    /// Disconnect from the device.
    fn disconnect(&mut self) -> Result<()>;

    /// Returns whether the device is connected.
    fn is_connected(&self) -> bool;

    /// Attempt to write a chunk of points at the given PPS.
    ///
    /// # Contract
    ///
    /// This is the core backpressure mechanism. Implementations must:
    ///
    /// 1. Return `WriteOutcome::WouldBlock` when the device cannot accept more data
    ///    (buffer full, not ready, etc.).
    /// 2. Return `WriteOutcome::Written` when the chunk was accepted.
    /// 3. Return `Err(...)` only for actual errors (disconnection, protocol errors).
    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome>;

    /// Stop output (if supported by the device).
    fn stop(&mut self) -> Result<()>;

    /// Open/close the shutter (if supported by the device).
    fn set_shutter(&mut self, open: bool) -> Result<()>;

    /// Best-effort estimate of points currently queued in the device.
    ///
    /// Not all devices can report this. Return `None` if unavailable.
    fn queued_points(&self) -> Option<u64> {
        None
    }
}

// =============================================================================
// Re-exports from protocol-specific backends
// =============================================================================

#[cfg(feature = "helios")]
pub use crate::protocols::helios::HeliosBackend;

#[cfg(feature = "ether-dream")]
pub use crate::protocols::ether_dream::EtherDreamBackend;

#[cfg(feature = "idn")]
pub use crate::protocols::idn::IdnBackend;

#[cfg(feature = "lasercube-wifi")]
pub use crate::protocols::lasercube_wifi::LasercubeWifiBackend;

#[cfg(feature = "lasercube-usb")]
pub use crate::protocols::lasercube_usb::LasercubeUsbBackend;

#[cfg(feature = "avb")]
pub use crate::protocols::avb::AvbBackend;
