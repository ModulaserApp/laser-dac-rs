//! ContentSource — the seam where points enter the [`OutputModelAdapter`].
//!
//! Two flavours: [`FifoContentSource`] produces variable-sized chunks for
//! `NetworkFifo` / `UdpTimed` adapters; [`FrameContentSource`] produces whole
//! frames for `UsbFrameSwap`. [`SlicePipeline`](super::slice_pipeline::SlicePipeline)
//! implements both today; [`ChunkProducer`](#) (phase 4) will implement
//! `FifoContentSource` only.
//!
//! ## Cache contract
//!
//! Both traits expose a single-slot cache. After `produce_*` returns a non-empty
//! slice, the source caches it; [`cached_slice`] returns it back to the adapter
//! (used for retain-on-`WouldBlock`). On a successful write, the adapter MUST
//! call [`commit_written`] exactly once. Cache disposal on the explicit reset
//! path is owned by [`on_reconnect`], which resets the source (and with it the
//! cache) as part of re-priming the last frame — the driver does not call
//! [`discard_cached`] separately. [`discard_cached`] is provided as a
//! decoupled disposal hook (drop the cache without recording a write) for future
//! callers, but has no call site today. Neither disposal path is a synonym for
//! [`commit_written`]: implementations with per-write derived state
//! (`ChunkProducer`'s `RepeatLast`, `current_instant`, stats) advance only in
//! [`commit_written`].
//!
//! [`cached_slice`]: FifoContentSource::cached_slice
//! [`commit_written`]: FifoContentSource::commit_written
//! [`discard_cached`]: FifoContentSource::discard_cached

use crate::device::DacInfo;
use crate::error::Error;
use crate::point::LaserPoint;

use super::{Frame, OutputResetReason};

/// Variable-chunk source. Implemented by `SlicePipeline` and (phase 4)
/// `ChunkProducer`. Used by `NetworkFifoAdapter` and `UdpTimedAdapter`.
pub(crate) trait FifoContentSource: Send {
    /// Produce up to `target_points` points for the next chunk. Returns `&[]`
    /// when no work is currently available. On a non-empty return the slice is
    /// cached and `cached_slice` returns the same bytes until `commit_written`
    /// or `discard_cached` is called.
    fn produce_chunk(&mut self, target_points: usize, pps: u32, is_armed: bool) -> &[LaserPoint];

    /// The previously-produced slice if not yet retired.
    fn cached_slice(&self) -> Option<&[LaserPoint]>;

    /// Record a successful write of `n` points from the cached slice and clear
    /// the cache. MUST be called exactly once after a successful `try_write`.
    fn commit_written(&mut self, n: usize, is_armed: bool);

    /// Drop the cache without recording a write. NOT a synonym for
    /// `commit_written`. Currently unused: cache disposal on the reset path is
    /// owned by [`on_reconnect`](Self::on_reconnect) (which resets the source),
    /// so this remains a decoupled disposal hook for future callers.
    #[allow(dead_code)]
    fn discard_cached(&mut self);

    /// Reserve working-buffer capacity for `n` points.
    fn reserve_buf(&mut self, n: usize);

    /// Reset derived state on reconnect. Implementations that have a logical
    /// frame to replay re-prime it here.
    fn on_reconnect(&mut self, info: &DacInfo);

    /// Producer requested graceful shutdown; the driver should drain and exit.
    /// Wired up by the unified driver in phase 4.
    #[allow(dead_code)]
    fn is_ended(&self) -> bool;

    /// Submit a frame for replay. No-op for sources that don't compose frames
    /// (e.g. `ChunkProducer`).
    #[allow(dead_code)]
    fn submit_frame(&mut self, _frame: Frame) {}

    /// Re-arm the startup-blank window using the current pps. No-op for
    /// sources that don't track startup blanking.
    #[allow(dead_code)]
    fn arm_startup_blank(&mut self, _pps: u32) {}

    /// Disarm-edge hook; sources that hold per-write state (color delay
    /// line) clear it here.
    #[allow(dead_code)]
    fn on_disarm(&mut self) {}

    /// Forward an `OutputResetReason` to any installed output filter. No-op
    /// for sources without a filter.
    #[allow(dead_code)]
    fn reset_output_filter(&mut self, _reason: OutputResetReason) {}

    /// Resize the color-delay line for the current pps. Sources that read
    /// the delay via an atomic on each chunk (`ChunkProducer`) ignore this.
    #[allow(dead_code)]
    fn resize_color_delay_micros(&mut self, _micros: u64, _pps: u32) {}

    /// If the source ended because of `IdlePolicy::Stop`, return the
    /// matching error so the driver can propagate it as `Err(Error::Stopped)`
    /// rather than a graceful `Ok(RunExit::ProducerEnded)`.
    #[allow(dead_code)]
    fn take_stop_error(&mut self) -> Option<Error> {
        None
    }
}

/// Whole-frame source. Implemented by `SlicePipeline`. Used by
/// `UsbFrameSwapAdapter`.
pub(crate) trait FrameContentSource: Send {
    /// Produce a complete frame for atomic upload. Returns `&[]` when no
    /// frame is currently available.
    fn produce_frame(&mut self, pps: u32, is_armed: bool) -> &[LaserPoint];

    /// The previously-produced frame if not yet retired.
    fn cached_slice(&self) -> Option<&[LaserPoint]>;

    /// Record a successful write of `n` points and clear the cache.
    fn commit_written(&mut self, n: usize, is_armed: bool);

    /// Drop the cache without recording a write. Currently unused: cache
    /// disposal on the reset path is owned by
    /// [`on_reconnect`](Self::on_reconnect); this is a decoupled disposal hook
    /// for future callers.
    #[allow(dead_code)]
    fn discard_cached(&mut self);

    /// Reset derived state on reconnect.
    fn on_reconnect(&mut self, info: &DacInfo);

    /// Update the frame capacity hint after reconnect. Default no-op for
    /// sources that don't compose hardware frames.
    #[allow(dead_code)]
    fn set_frame_capacity(&mut self, _cap: Option<usize>) {}

    /// Submit a frame for replay.
    #[allow(dead_code)]
    fn submit_frame(&mut self, _frame: Frame) {}

    /// Re-arm the startup-blank window.
    #[allow(dead_code)]
    fn arm_startup_blank(&mut self, _pps: u32) {}

    /// Disarm-edge hook.
    #[allow(dead_code)]
    fn on_disarm(&mut self) {}

    /// Forward an `OutputResetReason` to any installed output filter.
    #[allow(dead_code)]
    fn reset_output_filter(&mut self, _reason: OutputResetReason) {}

    /// Resize the color-delay line for the current pps.
    #[allow(dead_code)]
    fn resize_color_delay_micros(&mut self, _micros: u64, _pps: u32) {}
}

/// Erased borrow of a source for the per-iteration loop context. Adapters
/// destructure to the variant they expect.
pub(crate) enum ContentSourceKind<'a> {
    Fifo(&'a mut dyn FifoContentSource),
    Frame(&'a mut dyn FrameContentSource),
}
