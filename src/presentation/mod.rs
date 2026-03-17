//! Frame-first presentation types and engine.
//!
//! This module provides:
//! - [`Frame`]: immutable frame type for submission
//! - [`TransitionFn`] / [`default_transition`]: blanking between frames
//! - [`PresentationEngine`]: core frame lifecycle manager (internal)
//! - [`FrameSession`] / [`FrameSessionConfig`]: public frame-mode API

mod engine;
mod session;

pub use session::{FrameSession, FrameSessionConfig};

// Re-export internal types for tests (they live in sub-modules but tests use `super::*`)
#[cfg(test)]
pub(crate) use engine::{ColorDelayLine, PresentationEngine};

use crate::types::LaserPoint;
use std::sync::Arc;

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

/// Callback that generates transition (blanking) points between frames.
///
/// Called with the last point of the outgoing frame and the first point of
/// the incoming frame. Returns a vector of blanked/interpolated points to
/// insert between them.
///
/// The returned points are inserted between the two frames in the output
/// stream. Return an empty vec to skip transition points entirely.
pub type TransitionFn = Box<dyn Fn(&LaserPoint, &LaserPoint) -> Vec<LaserPoint> + Send>;

/// Default transition: dwell → travel → dwell, all blanked, scaled by distance.
///
/// Produces three phases of blanked points:
/// 1. **Dwell at source** — stationary at `from`, gives galvos time to decelerate
/// 2. **Linear travel** — interpolated from `from` to `to`
/// 3. **Dwell at destination** — stationary at `to`, lets galvos settle before laser fires
///
/// All point counts scale with the distance between `from` and `to`.
/// At 30kpps, a full-range diagonal jump gets ~5ms total blanking.
/// Nearby points get a minimal ~0.5ms pause.
pub fn default_transition(from: &LaserPoint, to: &LaserPoint) -> Vec<LaserPoint> {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let distance = (dx * dx + dy * dy).sqrt(); // 0.0 to ~2.83 (diagonal)

    // If points are nearly adjacent, no blanking needed — the galvos can
    // handle the tiny step without visible artifacts. This prevents gaps
    // at the loop point of closed shapes (circles, etc.) on frame-swap DACs.
    if distance < 0.02 {
        return vec![];
    }

    let dwell = (3.0 + distance * 12.0).ceil() as usize; // 3..~37 per end
    let travel = (5.0 + distance * 30.0).ceil() as usize; // 5..~90

    let mut points = Vec::with_capacity(dwell * 2 + travel);

    // Phase 1: dwell at source (decelerate, laser already off)
    for _ in 0..dwell {
        points.push(LaserPoint::blanked(from.x, from.y));
    }

    // Phase 2: linear travel
    for i in 0..travel {
        let t = (i + 1) as f32 / (travel + 1) as f32;
        points.push(LaserPoint::blanked(from.x + dx * t, from.y + dy * t));
    }

    // Phase 3: dwell at destination (settle before laser fires)
    for _ in 0..dwell {
        points.push(LaserPoint::blanked(to.x, to.y));
    }

    points
}

#[cfg(test)]
mod tests;
