//! Buffer fullness estimation for FIFO DAC backends.
//!
//! Each FIFO [`FifoBackend`](crate::backend::FifoBackend) owns a concrete
//! [`BufferEstimator`] strategy that tracks how many points are still queued
//! in the device. The trait is read-only — backends drive estimator state
//! internally through protocol-specific event hooks on the concrete type
//! (`record_send`, `record_status`, `record_ack`, …).
//!
//! Four strategies cover today's protocol mix:
//!
//! - [`SoftwareDecayEstimator`] — pure software bookkeeping; used when no
//!   telemetry is available (IDN, LaserCube USB).
//! - [`StatusDecayEstimator`] — periodic authoritative status reports decay
//!   between updates (Ether Dream).
//! - [`DualTrackAckEstimator`] — UDP send-track + ACK-track, conservative
//!   maximum. Available for UDP-acked transports but not currently wired to a
//!   backend: the LaserCube Network transport tracks device fullness inside its
//!   own worker (ACK-sequence correlation plus rate-based decay, exposed via the
//!   transport's `BufferEstimator` impl) rather than owning this strategy.
//! - [`RuntimeAuthorityEstimator`] — delegated to an external runtime that
//!   already tracks queue depth (AVB, Oscilloscope).

use std::time::Instant;

mod dual_track_ack;
mod runtime_authority;
mod software_decay;
mod status_decay;

pub use dual_track_ack::{DualTrackAckEstimator, LATENCY_POINT_ADJUSTMENT};
pub use runtime_authority::{QueueDepthSource, RuntimeAuthorityEstimator};
pub use software_decay::SoftwareDecayEstimator;
pub use status_decay::StatusDecayEstimator;

/// Read-only estimate of how many points are still queued in a device.
///
/// Implementations are owned by FIFO backends and mutated internally through
/// protocol-specific event hooks on the concrete strategy type. Callers (the
/// adapter and downstream policy code) never mutate.
pub trait BufferEstimator: Send {
    /// Best estimate of the device's queued points at `now`, given the current
    /// playback rate. Strategies that already track depth in pps-points (e.g.
    /// [`SoftwareDecayEstimator`]) ignore `pps`; strategies that hold depth in
    /// another unit (e.g. [`RuntimeAuthorityEstimator`], which tracks device
    /// output samples) use it to convert into comparable pps-points.
    fn estimated_fullness(&self, now: Instant, pps: u32) -> u64;

    /// Whether [`estimated_fullness`](Self::estimated_fullness) actually
    /// consults `now`. Defaults to `true`; estimators that ignore the
    /// timestamp (e.g. [`RuntimeAuthorityEstimator`]) override to `false` so
    /// the caller can skip the `Instant::now()` query on hot paths.
    fn needs_clock(&self) -> bool {
        true
    }
}
