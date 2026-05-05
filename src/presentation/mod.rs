//! Frame-first presentation types and engine.
//!
//! This module provides:
//! - [`Frame`]: immutable frame type for submission
//! - [`TransitionFn`] / [`default_transition`]: blanking between frames
//! - `PresentationEngine`: core frame lifecycle manager (internal)
//! - [`FrameSession`] / [`FrameSessionConfig`]: public frame-mode API

mod engine;
mod session;

pub use session::FrameSessionMetrics;
pub use session::{FrameSession, FrameSessionConfig};

// Re-export internal types for tests (they live in sub-modules but tests use `super::*`)
#[cfg(test)]
pub(crate) use engine::ColorDelayLine;
#[cfg(all(test, not(feature = "testutils")))]
pub(crate) use engine::PresentationEngine;

// Re-export PresentationEngine for benchmarks behind testutils feature
#[cfg(feature = "testutils")]
pub use engine::PresentationEngine;

use crate::point::LaserPoint;
use std::sync::Arc;

// =============================================================================
// OutputFilter
// =============================================================================

/// Why an [`OutputFilter`] was reset.
///
/// Resets happen at output continuity boundaries where downstream processing
/// should discard history and treat the next presented slice as a fresh stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputResetReason {
    /// The session scheduler started.
    SessionStart,
    /// The backend reconnected and presentation state was replayed.
    Reconnect,
    /// Output was armed and startup blanking is about to resume visible content.
    Arm,
    /// Output was disarmed and presented output becomes forced-blanked.
    Disarm,
}

/// The delivery mode of the presented slice passed to an [`OutputFilter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentedSliceKind {
    /// A FIFO chunk materialized by the frame session.
    FifoChunk,
    /// A complete frame-swap hardware frame.
    FrameSwapFrame,
}

/// Metadata describing the final presented slice seen by an [`OutputFilter`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputFilterContext {
    /// Output rate for the presented slice.
    pub pps: u32,
    /// Whether the slice is a FIFO chunk or a frame-swap frame.
    pub kind: PresentedSliceKind,
    /// Whether the slice should be interpreted as cyclic.
    pub is_cyclic: bool,
}

/// Hook for advanced output-space processing in frame mode.
///
/// The filter runs on the final point sequence immediately before backend
/// write, after transition composition, blanking, and color delay.
///
/// `WouldBlock` retries reuse the already-filtered buffer verbatim. The filter
/// is only called again when a new presented slice is materialized.
pub trait OutputFilter: Send + 'static {
    /// Reset internal continuity state after a stream break.
    fn reset(&mut self, _reason: OutputResetReason) {}

    /// Transform the final presented output in place.
    fn filter(&mut self, points: &mut [LaserPoint], ctx: &OutputFilterContext);
}

// =============================================================================
// Frame
// =============================================================================

/// A complete frame of laser points authored by the application.
///
/// This is the unit of submission for frame-mode output. Frames are immutable
/// once created and cheaply cloneable via `Arc`.
///
/// # Example
///
/// ```
/// use laser_dac::presentation::Frame;
/// use laser_dac::LaserPoint;
///
/// let frame = Frame::new(vec![
///     LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
///     LaserPoint::new(1.0, 0.0, 0, 65535, 0, 65535),
/// ]);
/// assert_eq!(frame.len(), 2);
/// ```
#[derive(Clone, Debug)]
pub struct Frame {
    points: Arc<Vec<LaserPoint>>,
}

impl Frame {
    /// Create a new frame from a vector of points.
    pub fn new(points: Vec<LaserPoint>) -> Self {
        Self {
            points: Arc::new(points),
        }
    }

    /// Returns a reference to the frame's points.
    pub fn points(&self) -> &[LaserPoint] {
        &self.points
    }

    /// Returns the first point, or `None` if the frame is empty.
    pub fn first_point(&self) -> Option<&LaserPoint> {
        self.points.first()
    }

    /// Returns the last point, or `None` if the frame is empty.
    pub fn last_point(&self) -> Option<&LaserPoint> {
        self.points.last()
    }

    /// Returns the number of points in the frame.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    /// Returns true if the frame contains no points.
    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }
}

impl From<Vec<LaserPoint>> for Frame {
    fn from(points: Vec<LaserPoint>) -> Self {
        Self::new(points)
    }
}

// =============================================================================
// TransitionFn
// =============================================================================

/// Describes how to handle the seam between two adjacent frame endpoints.
///
/// Returned by [`TransitionFn`] to tell the engine what to do at each seam —
/// including self-loops (A→A) and frame changes (A→B).
#[derive(Clone, Debug)]
pub enum TransitionPlan {
    /// Keep both seam endpoints and insert these points between them.
    /// An empty vec keeps both endpoints with nothing in between.
    Transition(Vec<LaserPoint>),
    /// The two seam endpoints are the same logical point — coalesce them
    /// so only one copy appears in the output.
    Coalesce,
}

/// Callback that generates a transition plan between frames.
///
/// Called with the last point of the outgoing frame and the first point of
/// the incoming frame. Returns a [`TransitionPlan`] describing how to handle
/// the seam.
///
/// Self-loops (A→A) also run through this callback, so transition planning
/// is consistent regardless of whether the frame changed.
pub type TransitionFn = Box<dyn Fn(&LaserPoint, &LaserPoint) -> TransitionPlan + Send>;

/// Default blanking transition settings (microseconds).
///
/// Converted to point counts via `round(µs × pps / 1_000_000)`.
const END_DWELL_US: f64 = 100.0;
const START_DWELL_US: f64 = 400.0;

/// Create the default transition function for the given PPS.
///
/// Produces a 3-phase blanking sequence between frames:
///
/// 1. **End dwell** — repeat `from` with laser OFF. Lets the galvo settle
///    at the endpoint before moving.
/// 2. **Transit** — quintic ease-in-out interpolation from→to with laser OFF.
///    Point count scales with L∞ distance (0–64 points).
/// 3. **Start dwell** — repeat `to` with laser OFF. Lets the galvo settle
///    at the new position before the next frame lights up.
///
/// All points are blanked. The on-beam dwell phases (post-on, pre-on) from
/// the full 5-phase sequence are omitted — those are the frame's responsibility.
pub fn default_transition(pps: u32) -> TransitionFn {
    let end_dwell = (END_DWELL_US * pps as f64 / 1_000_000.0).round() as usize;
    let start_dwell = (START_DWELL_US * pps as f64 / 1_000_000.0).round() as usize;

    Box::new(move |from: &LaserPoint, to: &LaserPoint| {
        let dx = to.x - from.x;
        let dy = to.y - from.y;

        // L-infinity distance (correct for independent galvo axes)
        let d_inf = dx.abs().max(dy.abs());
        let transit = (32.0 * d_inf).ceil().clamp(0.0, 64.0) as usize;

        let total = end_dwell + transit + start_dwell;
        let mut points = Vec::with_capacity(total);

        // Phase 1: end dwell — blanked at source
        for _ in 0..end_dwell {
            points.push(LaserPoint::blanked(from.x, from.y));
        }

        // Phase 2: transit — quintic ease-in-out from→to, blanked
        for i in 0..transit {
            let t = (i as f32 + 1.0) / (transit as f32 + 1.0);
            let t = quintic_ease_in_out(t);
            points.push(LaserPoint::blanked(from.x + dx * t, from.y + dy * t));
        }

        // Phase 3: start dwell — blanked at destination
        for _ in 0..start_dwell {
            points.push(LaserPoint::blanked(to.x, to.y));
        }

        TransitionPlan::Transition(points)
    })
}

/// Quintic ease-in-out: smooth acceleration/deceleration for galvo transit.
///
/// `t` in [0, 1] → output in [0, 1].
/// First half:  `16t⁵`
/// Second half: `0.5(2t−2)⁵ + 1`
fn quintic_ease_in_out(t: f32) -> f32 {
    if t < 0.5 {
        16.0 * t * t * t * t * t
    } else {
        let u = 2.0 * t - 2.0;
        0.5 * u * u * u * u * u + 1.0
    }
}

#[cfg(test)]
mod tests;
