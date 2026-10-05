//! DAC backend traits and implementations for the streaming API.
//!
//! This module provides the backend trait hierarchy that all DAC backends must
//! implement:
//!
//! - [`DacBackend`] — common device lifecycle (connect, disconnect, shutter, stop)
//! - [`FifoBackend`] — FIFO/queue-based DACs (Ether Dream, IDN, LaserCube, AVB)
//! - [`FrameSwapBackend`] — double-buffered frame DACs (Helios)
//!
//! [`BackendKind`] type-erases either backend kind for use in the stream scheduler.

use crate::buffer_estimate::BufferEstimator;
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
    ///
    /// [`DacCapabilities::output_model`] is a construction-time contract and
    /// must remain stable for the lifetime of the backend. Other negotiated
    /// limits may be refined during [`connect`](Self::connect).
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
/// Implementations: Ether Dream, IDN, LaserCube Network, LaserCube USB, AVB.
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

    /// The protocol-owned [`BufferEstimator`] strategy.
    ///
    /// Read-only: backends mutate their concrete strategy internally through
    /// protocol-specific event hooks. Adapters (and any other observers) only
    /// query estimated fullness via this getter.
    fn estimator(&self) -> &dyn BufferEstimator;

    /// The most points the adapter should ever try to keep buffered in the
    /// device. `None` means no device-imposed limit.
    ///
    /// The network FIFO adapter clamps its target buffer
    /// (`target_buffer * pps`) to this, so a target longer than the device ring
    /// at high point rates does not leave a permanent deficit that keeps
    /// asking for chunks that cannot fit. Other adapters ignore it. Called
    /// every adapter step, so it may change across reconnects.
    fn target_buffer_ceiling(&self) -> Option<usize> {
        None
    }

    /// Clear the device-side queue (drop all buffered-but-unplayed points) and
    /// reset queue-depth bookkeeping.
    ///
    /// Called by the scheduler when re-arming an
    /// [`OutputModel::BlockingFifo`](crate::device::OutputModel::BlockingFifo)
    /// device whose hardware ring does not drain while output is disabled
    /// (e.g. LaserCube USB), so stale points do not replay on re-arm. The
    /// default is a no-op: FIFO devices that keep draining while disarmed empty
    /// their queue on their own and have nothing to clear.
    fn reset_device_buffer(&mut self) -> Result<()> {
        Ok(())
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

    /// Write one frame after [`is_ready_for_frame`](Self::is_ready_for_frame)
    /// returned `true`.
    ///
    /// This is the second transition in the frame-write state machine. A
    /// successful readiness probe grants permission for exactly one call to
    /// this method; another readiness probe or frame write invalidates that
    /// permission. Implementations may still return [`WriteOutcome::WouldBlock`]
    /// if device state changes between the probe and the write.
    ///
    /// The default implementation is conservative and calls [`write_frame`](Self::write_frame),
    /// which may perform another readiness probe. Backends whose probe has an
    /// observable cost can override this method to consume the prior probe
    /// without repeating it.
    fn write_frame_after_ready(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        self.write_frame(pps, points)
    }
}

// =============================================================================
// BackendKind — type-erased wrapper
// =============================================================================

/// Type-erased backend wrapper for use in the stream scheduler.
///
/// Fallible construction rejects a runtime kind that initially disagrees with
/// [`DacCapabilities::output_model`], and connection revalidates the invariant
/// after backend initialization. Runtime dispatch is always derived from the
/// private variant; capabilities remain discovery metadata and select the
/// concrete FIFO pacing adapter.
pub struct BackendKind {
    inner: BackendVariant,
}

enum BackendVariant {
    Fifo(Box<dyn FifoBackend>),
    FrameSwap(Box<dyn FrameSwapBackend>),
}

impl BackendKind {
    /// Wrap a FIFO backend, rejecting frame-swap capability metadata.
    pub fn fifo(backend: Box<dyn FifoBackend>) -> Result<Self> {
        let backend = Self {
            inner: BackendVariant::Fifo(backend),
        };
        backend.validate_output_model()?;
        Ok(backend)
    }

    /// Wrap a frame-swap backend, requiring frame-swap capability metadata.
    pub fn frame_swap(backend: Box<dyn FrameSwapBackend>) -> Result<Self> {
        let backend = Self {
            inner: BackendVariant::FrameSwap(backend),
        };
        backend.validate_output_model()?;
        Ok(backend)
    }

    // Keep crate-internal tests concise while production callers use the new
    // fallible constructors. These are not part of the public API.
    #[cfg(test)]
    #[allow(non_snake_case)]
    pub(crate) fn Fifo(backend: Box<dyn FifoBackend>) -> Self {
        Self::fifo(backend).expect("test FIFO backend must advertise a FIFO output model")
    }

    #[cfg(test)]
    #[allow(non_snake_case)]
    pub(crate) fn FrameSwap(backend: Box<dyn FrameSwapBackend>) -> Self {
        Self::frame_swap(backend).expect("test frame-swap backend must advertise UsbFrameSwap")
    }

    /// Verify that capability metadata agrees with the runtime backend kind.
    pub fn validate_output_model(&self) -> Result<()> {
        use crate::device::OutputModel;

        let caps_frame_swap = matches!(self.caps().output_model, OutputModel::UsbFrameSwap);
        if caps_frame_swap == self.is_frame_swap() {
            return Ok(());
        }

        Err(Error::invalid_config(format!(
            "backend kind {} disagrees with output model {:?}",
            if self.is_frame_swap() {
                "FrameSwap"
            } else {
                "Fifo"
            },
            self.caps().output_model
        )))
    }

    // =========================================================================
    // DacBackend delegation
    // =========================================================================

    /// Returns the DAC type.
    pub fn dac_type(&self) -> DacType {
        match &self.inner {
            BackendVariant::Fifo(b) => b.dac_type(),
            BackendVariant::FrameSwap(b) => b.dac_type(),
        }
    }

    /// Returns the device capabilities.
    pub fn caps(&self) -> &DacCapabilities {
        match &self.inner {
            BackendVariant::Fifo(b) => b.caps(),
            BackendVariant::FrameSwap(b) => b.caps(),
        }
    }

    /// Connect to the device and revalidate metadata that may have changed
    /// while the concrete backend initialized itself.
    pub fn connect(&mut self) -> Result<()> {
        self.validate_output_model()?;
        match &mut self.inner {
            BackendVariant::Fifo(b) => b.connect()?,
            BackendVariant::FrameSwap(b) => b.connect()?,
        }
        if let Err(invariant_err) = self.validate_output_model() {
            let cleanup = match &mut self.inner {
                BackendVariant::Fifo(b) => b.disconnect(),
                BackendVariant::FrameSwap(b) => b.disconnect(),
            };
            return match cleanup {
                Ok(()) => Err(invariant_err),
                Err(cleanup_err) => Err(Error::invalid_config(format!(
                    "{invariant_err}; disconnect after invariant violation also failed: {cleanup_err}"
                ))),
            };
        }
        Ok(())
    }

    /// Disconnect from the device.
    pub fn disconnect(&mut self) -> Result<()> {
        match &mut self.inner {
            BackendVariant::Fifo(b) => b.disconnect(),
            BackendVariant::FrameSwap(b) => b.disconnect(),
        }
    }

    /// Returns whether the device is connected.
    pub fn is_connected(&self) -> bool {
        match &self.inner {
            BackendVariant::Fifo(b) => b.is_connected(),
            BackendVariant::FrameSwap(b) => b.is_connected(),
        }
    }

    /// Stop output.
    pub fn stop(&mut self) -> Result<()> {
        match &mut self.inner {
            BackendVariant::Fifo(b) => b.stop(),
            BackendVariant::FrameSwap(b) => b.stop(),
        }
    }

    /// Open/close the shutter.
    pub fn set_shutter(&mut self, open: bool) -> Result<()> {
        match &mut self.inner {
            BackendVariant::Fifo(b) => b.set_shutter(open),
            BackendVariant::FrameSwap(b) => b.set_shutter(open),
        }
    }

    /// Best-effort safe teardown for an owned backend that cannot be returned
    /// to its caller. Both commands are attempted even when connection state is
    /// unknown or shutter closure fails.
    pub(crate) fn close_and_disconnect(&mut self) {
        let _ = self.set_shutter(false);
        let _ = self.disconnect();
    }

    // =========================================================================
    // Write dispatch
    // =========================================================================

    /// Safely attempt a write.
    ///
    /// FIFO backends call [`FifoBackend::try_write_points`]. Frame-swap
    /// backends call the conservative [`FrameSwapBackend::write_frame`], which
    /// owns any required readiness probe.
    pub fn try_write(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        match &mut self.inner {
            BackendVariant::Fifo(b) => b.try_write_points(pps, points),
            BackendVariant::FrameSwap(b) => b.write_frame(pps, points),
        }
    }

    /// Perform the write transition after a successful frame-readiness probe.
    ///
    /// The concrete backend decides whether its implementation can consume the
    /// prior probe directly or must conservatively probe again. This operation
    /// is frame-swap-only by construction.
    pub(crate) fn write_frame_after_ready(
        &mut self,
        pps: u32,
        points: &[LaserPoint],
    ) -> Result<WriteOutcome> {
        match &mut self.inner {
            BackendVariant::FrameSwap(b) => b.write_frame_after_ready(pps, points),
            BackendVariant::Fifo(_) => Err(Error::invalid_config(
                "write_frame_after_ready called on a FIFO backend",
            )),
        }
    }

    // =========================================================================
    // Query helpers
    // =========================================================================

    /// The protocol-owned [`BufferEstimator`] for FIFO backends.
    ///
    /// Frame-swap backends never queue points, so they return `None`.
    pub fn estimator(&self) -> Option<&dyn BufferEstimator> {
        match &self.inner {
            BackendVariant::Fifo(b) => Some(b.estimator()),
            BackendVariant::FrameSwap(_) => None,
        }
    }

    /// The FIFO backend's [`FifoBackend::target_buffer_ceiling`].
    ///
    /// Frame-swap backends never queue points, so they return `None`.
    pub fn target_buffer_ceiling(&self) -> Option<usize> {
        match &self.inner {
            BackendVariant::Fifo(b) => b.target_buffer_ceiling(),
            BackendVariant::FrameSwap(_) => None,
        }
    }

    /// Clear the device-side queue and reset queue-depth bookkeeping.
    pub fn reset_device_buffer(&mut self) -> Result<()> {
        match &mut self.inner {
            BackendVariant::Fifo(b) => b.reset_device_buffer(),
            BackendVariant::FrameSwap(_) => Ok(()),
        }
    }

    /// Returns `true` when the runtime object is a frame-swap backend.
    pub fn is_frame_swap(&self) -> bool {
        matches!(self.inner, BackendVariant::FrameSwap(_))
    }

    /// Perform the readiness transition for a frame-swap backend.
    pub(crate) fn is_ready_for_frame(&mut self) -> Result<bool> {
        match &mut self.inner {
            BackendVariant::FrameSwap(b) => Ok(b.is_ready_for_frame()),
            BackendVariant::Fifo(_) => Err(Error::invalid_config(
                "frame readiness queried on a FIFO backend",
            )),
        }
    }

    /// Returns the frame capacity for frame-swap backends, or `None` for FIFO.
    pub fn frame_capacity(&self) -> Option<usize> {
        match &self.inner {
            BackendVariant::Fifo(_) => None,
            BackendVariant::FrameSwap(b) => Some(b.frame_capacity()),
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

#[cfg(feature = "lasercube-network")]
pub use crate::protocols::lasercube_network::LaserCubeNetworkBackend;

#[cfg(feature = "lasercube-usb")]
pub use crate::protocols::lasercube_usb::LaserCubeUsbBackend;

#[cfg(feature = "oscilloscope")]
pub use crate::protocols::oscilloscope::OscilloscopeBackend;

#[cfg(feature = "avb")]
pub use crate::protocols::avb::AvbBackend;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::{DacCapabilities, DacType, OutputModel};

    /// Stub FIFO backend with a configurable `OutputModel`.
    struct StubFifo {
        caps: DacCapabilities,
        estimator: crate::buffer_estimate::SoftwareDecayEstimator,
    }
    impl DacBackend for StubFifo {
        fn dac_type(&self) -> DacType {
            DacType::Custom("stub".into())
        }
        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }
        fn connect(&mut self) -> Result<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            Ok(())
        }
    }
    impl FifoBackend for StubFifo {
        fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Written)
        }

        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }
    }

    struct StubFrameSwap {
        caps: DacCapabilities,
    }
    impl DacBackend for StubFrameSwap {
        fn dac_type(&self) -> DacType {
            DacType::Custom("stub-fs".into())
        }
        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }
        fn connect(&mut self) -> Result<()> {
            Ok(())
        }
        fn disconnect(&mut self) -> Result<()> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            Ok(())
        }
    }
    impl FrameSwapBackend for StubFrameSwap {
        fn frame_capacity(&self) -> usize {
            4096
        }
        fn is_ready_for_frame(&mut self) -> bool {
            true
        }
        fn write_frame(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Written)
        }
    }

    fn stub_fifo(model: OutputModel) -> Box<dyn FifoBackend> {
        Box::new(StubFifo {
            caps: DacCapabilities {
                output_model: model,
                ..DacCapabilities::default()
            },
            estimator: crate::buffer_estimate::SoftwareDecayEstimator::new(),
        })
    }

    fn stub_frame_swap(model: OutputModel) -> Box<dyn FrameSwapBackend> {
        Box::new(StubFrameSwap {
            caps: DacCapabilities {
                output_model: model,
                ..DacCapabilities::default()
            },
        })
    }

    #[test]
    fn runtime_kind_is_derived_from_private_variant() {
        for model in [
            OutputModel::NetworkFifo,
            OutputModel::UdpTimed,
            OutputModel::BlockingFifo,
        ] {
            let backend = BackendKind::fifo(stub_fifo(model)).unwrap();
            assert!(!backend.is_frame_swap());
        }

        let backend = BackendKind::frame_swap(stub_frame_swap(OutputModel::UsbFrameSwap)).unwrap();
        assert!(backend.is_frame_swap());
    }

    struct MutatingFifo {
        caps: DacCapabilities,
        connected: bool,
        disconnected: std::sync::Arc<std::sync::atomic::AtomicBool>,
        fail_disconnect: bool,
        estimator: crate::buffer_estimate::SoftwareDecayEstimator,
    }

    impl DacBackend for MutatingFifo {
        fn dac_type(&self) -> DacType {
            DacType::Custom("mutating-fifo".into())
        }

        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }

        fn connect(&mut self) -> Result<()> {
            self.connected = true;
            self.caps.output_model = OutputModel::UsbFrameSwap;
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            self.disconnected
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if self.fail_disconnect {
                Err(Error::backend(std::io::Error::other("cleanup failed")))
            } else {
                Ok(())
            }
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            Ok(())
        }
    }

    impl FifoBackend for MutatingFifo {
        fn try_write_points(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Written)
        }

        fn estimator(&self) -> &dyn BufferEstimator {
            &self.estimator
        }
    }

    #[test]
    fn constructors_reject_kind_output_model_disagreement() {
        let err = match BackendKind::fifo(stub_fifo(OutputModel::UsbFrameSwap)) {
            Ok(_) => panic!("FIFO wrapper must reject UsbFrameSwap metadata"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("Fifo"));
        assert!(err.to_string().contains("UsbFrameSwap"));

        let err = match BackendKind::frame_swap(stub_frame_swap(OutputModel::NetworkFifo)) {
            Ok(_) => panic!("frame-swap wrapper must reject FIFO metadata"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("FrameSwap"));
        assert!(err.to_string().contains("NetworkFifo"));
    }

    #[test]
    fn connect_revalidates_output_model_and_disconnects_on_disagreement() {
        use std::sync::atomic::Ordering;

        let disconnected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut backend = BackendKind::fifo(Box::new(MutatingFifo {
            caps: DacCapabilities {
                output_model: OutputModel::NetworkFifo,
                ..DacCapabilities::default()
            },
            connected: false,
            disconnected: disconnected.clone(),
            fail_disconnect: false,
            estimator: crate::buffer_estimate::SoftwareDecayEstimator::new(),
        }))
        .unwrap();

        let err = backend.connect().unwrap_err();
        assert!(err.to_string().contains("disagrees"));
        assert!(!backend.is_connected());
        assert!(disconnected.load(Ordering::SeqCst));
    }

    #[test]
    fn connect_reports_cleanup_failure_after_invariant_violation() {
        let mut backend = BackendKind::fifo(Box::new(MutatingFifo {
            caps: DacCapabilities {
                output_model: OutputModel::NetworkFifo,
                ..DacCapabilities::default()
            },
            connected: false,
            disconnected: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            fail_disconnect: true,
            estimator: crate::buffer_estimate::SoftwareDecayEstimator::new(),
        }))
        .unwrap();

        let err = backend.connect().unwrap_err();
        assert!(err.to_string().contains("invariant violation"));
        assert!(err.to_string().contains("cleanup failed"));
    }
}
