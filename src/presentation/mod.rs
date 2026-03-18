//! Frame-first presentation types and engine.
//!
//! This module provides:
//! - [`Frame`]: immutable frame type for submission
//! - [`TransitionFn`] / [`default_transition`]: blanking between frames
//! - `PresentationEngine`: core frame lifecycle manager (internal)
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

/// Default transition: blank at source → linear travel → blank at destination.
///
/// Always inserts blanking, even for nearby points. The number of travel
/// steps scales with the distance between `from` and `to` (minimum 1).
pub fn default_transition(from: &LaserPoint, to: &LaserPoint) -> TransitionPlan {
    let dx = to.x - from.x;
    let dy = to.y - from.y;
    let distance = (dx * dx + dy * dy).sqrt(); // 0.0 to ~2.83 (diagonal)

    let travel = (distance * 30.0).ceil().max(1.0) as usize; // 1..~85

    let mut points = Vec::with_capacity(travel + 2);

    // Blank at source
    points.push(LaserPoint::blanked(from.x, from.y));

    // Linear travel
    for i in 0..travel {
        let t = (i + 1) as f32 / (travel + 1) as f32;
        points.push(LaserPoint::blanked(from.x + dx * t, from.y + dy * t));
    }

    // Blank at destination
    points.push(LaserPoint::blanked(to.x, to.y));

    TransitionPlan::Transition(points)
}

#[cfg(test)]
mod tests;
