//! DAC backend traits and implementations for the streaming API.
//!
//! This module provides the backend trait hierarchy that all DAC backends must
//! implement:
//!
//! - [`DacBackend`] — common device lifecycle (connect, disconnect, shutter, stop)
//! - [`FifoBackend`] — FIFO/queue-based DACs (Ether Dream, IDN, LaserCube, AVB)
//! - [`FrameSwapBackend`] — double-buffered frame DACs (Helios)
//!
//! The [`BackendKind`] enum wraps either variant for use in the stream scheduler.

use crate::device::{DacCapabilities, DacType};
use crate::point::LaserPoint;

// Re-export error types for backwards compatibility
pub use crate::error::{Error, Result};

// =============================================================================
// Write Outcome
// =============================================================================

/// Write result from a backend point/frame submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteOutcome {
    /// The data was accepted and written.
    Written,
    /// The device cannot accept more data right now.
    WouldBlock,
}

// =============================================================================
// DacBackend Trait — common device lifecycle
// =============================================================================

/// Common backend trait for all DAC device types.
///
/// Provides device lifecycle management (connect, disconnect, stop, shutter)
/// and capability/type queries. All specific backend traits extend this.
pub trait DacBackend: Send + 'static {
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

    /// Stop output (if supported by the device).
    fn stop(&mut self) -> Result<()>;

    /// Open/close the shutter (if supported by the device).
    fn set_shutter(&mut self, open: bool) -> Result<()>;
}

// =============================================================================
// FifoBackend Trait — queue/FIFO based DACs
// =============================================================================

/// Backend trait for FIFO/queue-based DACs.
///
/// These DACs accept arbitrary-sized chunks of points into a queue or buffer.
/// The stream scheduler tops up the buffer to maintain a target level.
///
/// Implementations: Ether Dream, IDN, LaserCube USB/WiFi, AVB.
pub trait FifoBackend: DacBackend {
    /// Attempt to write points at the given PPS.
    ///
    /// # Contract
    ///
    /// This is the core backpressure mechanism. Implementations must:
    ///
    /// 1. Return `WriteOutcome::WouldBlock` when the device cannot accept more data
    ///    (buffer full, not ready, etc.).
    /// 2. Return `WriteOutcome::Written` when the points were accepted.
    /// 3. Return `Err(...)` only for actual errors (disconnection, protocol errors).
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome>;

    /// Best-effort estimate of points currently queued in the device.
    ///
    /// Not all devices can report this. Return `None` if unavailable.
    fn queued_points(&self) -> Option<u64> {
        None
    }
}

// =============================================================================
// FrameSwapBackend Trait — double-buffered frame DACs
// =============================================================================

/// Backend trait for double-buffered frame-swap DACs.
///
/// These DACs accept complete frames that replace the previous frame atomically.
/// The device holds at most one pending frame at a time.
///
/// Implementations: Helios.
pub trait FrameSwapBackend: DacBackend {
    /// Maximum number of points the device can accept in a single frame.
    fn frame_capacity(&self) -> usize;

    /// Returns true if the device is ready to accept a new frame.
    ///
    /// For Helios, this queries the USB device status.
    fn is_ready_for_frame(&mut self) -> bool;

    /// Write a complete frame at the given PPS.
    ///
    /// The caller should check `is_ready_for_frame()` first, but implementations
    /// may still return `WouldBlock` for race conditions.
    fn write_frame(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome>;
}

// =============================================================================
// BackendKind — type-erased wrapper
// =============================================================================

/// Type-erased backend wrapper for use in the stream scheduler.
///
/// Wraps either a [`FifoBackend`] or a [`FrameSwapBackend`], providing
/// delegation for common [`DacBackend`] methods and a unified write path.
pub enum BackendKind {
    /// A FIFO/queue-based backend.
    Fifo(Box<dyn FifoBackend>),
    /// A double-buffered frame-swap backend.
    FrameSwap(Box<dyn FrameSwapBackend>),
}

impl BackendKind {
    // =========================================================================
    // DacBackend delegation
    // =========================================================================

    /// Returns the DAC type.
    pub fn dac_type(&self) -> DacType {
        match self {
            BackendKind::Fifo(b) => b.dac_type(),
            BackendKind::FrameSwap(b) => b.dac_type(),
        }
    }

    /// Returns the device capabilities.
    pub fn caps(&self) -> &DacCapabilities {
        match self {
            BackendKind::Fifo(b) => b.caps(),
            BackendKind::FrameSwap(b) => b.caps(),
        }
    }

    /// Connect to the device.
    pub fn connect(&mut self) -> Result<()> {
        match self {
            BackendKind::Fifo(b) => b.connect(),
            BackendKind::FrameSwap(b) => b.connect(),
        }
    }

    /// Disconnect from the device.
    pub fn disconnect(&mut self) -> Result<()> {
        match self {
            BackendKind::Fifo(b) => b.disconnect(),
            BackendKind::FrameSwap(b) => b.disconnect(),
        }
    }

    /// Returns whether the device is connected.
    pub fn is_connected(&self) -> bool {
        match self {
            BackendKind::Fifo(b) => b.is_connected(),
            BackendKind::FrameSwap(b) => b.is_connected(),
        }
    }

    /// Stop output.
    pub fn stop(&mut self) -> Result<()> {
        match self {
            BackendKind::Fifo(b) => b.stop(),
            BackendKind::FrameSwap(b) => b.stop(),
        }
    }

    /// Open/close the shutter.
    pub fn set_shutter(&mut self, open: bool) -> Result<()> {
        match self {
            BackendKind::Fifo(b) => b.set_shutter(open),
            BackendKind::FrameSwap(b) => b.set_shutter(open),
        }
    }

    // =========================================================================
    // Write dispatch
    // =========================================================================

    /// Write points to the backend (dispatches to the appropriate method).
    ///
    /// - For FIFO backends: calls `try_write_points()`
    /// - For frame-swap backends: calls `write_frame()`
    pub fn try_write(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        match self {
            BackendKind::Fifo(b) => b.try_write_points(pps, points),
            BackendKind::FrameSwap(b) => b.write_frame(pps, points),
        }
    }

    // =========================================================================
    // Query helpers
    // =========================================================================

    /// Best-effort estimate of points currently queued in the device.
    ///
    /// Only FIFO backends can report this; frame-swap backends return `None`.
    pub fn queued_points(&self) -> Option<u64> {
        match self {
            BackendKind::Fifo(b) => b.queued_points(),
            BackendKind::FrameSwap(_) => None,
        }
    }

    /// Returns `true` if this is a frame-swap backend.
    pub fn is_frame_swap(&self) -> bool {
        matches!(self, BackendKind::FrameSwap(_))
    }

    /// Returns true if the device is ready to accept a new frame.
    ///
    /// For FIFO backends, always returns `true` (they handle backpressure via `try_write`).
    /// For frame-swap backends, queries the device readiness.
    pub fn is_ready_for_frame(&mut self) -> bool {
        match self {
            BackendKind::Fifo(_) => true,
            BackendKind::FrameSwap(b) => b.is_ready_for_frame(),
        }
    }

    /// Returns the frame capacity for frame-swap backends, or `None` for FIFO.
    pub fn frame_capacity(&self) -> Option<usize> {
        match self {
            BackendKind::Fifo(_) => None,
            BackendKind::FrameSwap(b) => Some(b.frame_capacity()),
        }
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

#[cfg(feature = "oscilloscope")]
pub use crate::protocols::oscilloscope::OscilloscopeBackend;

#[cfg(feature = "avb")]
pub use crate::protocols::avb::AvbBackend;
