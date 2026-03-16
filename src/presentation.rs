//! Frame-first presentation types and engine.
//!
//! This module provides the [`AuthoredFrame`] type for submitting complete frames,
//! [`TransitionFn`] for blanking/transition callbacks between frames, and
//! [`default_transition`] which produces 8 linearly-interpolated blanked points.

use crate::types::LaserPoint;
use std::sync::Arc;

// =============================================================================
// AuthoredFrame
// =============================================================================

/// A complete frame of laser points authored by the application.
///
/// This is the unit of submission for frame-mode output. Frames are immutable
/// once created and cheaply cloneable via `Arc`.
///
/// # Example
///
/// ```
/// use laser_dac::presentation::AuthoredFrame;
/// use laser_dac::LaserPoint;
///
/// let frame = AuthoredFrame::new(vec![
///     LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
///     LaserPoint::new(1.0, 0.0, 0, 65535, 0, 65535),
/// ]);
/// assert_eq!(frame.len(), 2);
/// ```
#[derive(Clone, Debug)]
pub struct AuthoredFrame {
    points: Arc<Vec<LaserPoint>>,
}

impl AuthoredFrame {
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

impl From<Vec<LaserPoint>> for AuthoredFrame {
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

/// Default transition: 8 linearly-interpolated blanked points.
///
/// Produces 8 points with XY linearly interpolated from `from` to `to`,
/// all with colors and intensity set to zero. This gives the galvo mirrors
/// time to travel between frames without visible artifacts.
///
/// Always produces exactly 8 points, even when `from` and `to` are identical.
pub fn default_transition(from: &LaserPoint, to: &LaserPoint) -> Vec<LaserPoint> {
    const N: usize = 8;
    (0..N)
        .map(|i| {
            let t = (i + 1) as f32 / (N + 1) as f32;
            LaserPoint::blanked(
                from.x + (to.x - from.x) * t,
                from.y + (to.y - from.y) * t,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_point(x: f32, y: f32) -> LaserPoint {
        LaserPoint::new(x, y, 65535, 0, 0, 65535)
    }

    // =========================================================================
    // AuthoredFrame tests
    // =========================================================================

    #[test]
    fn test_authored_frame_new_and_points() {
        let pts = vec![make_point(0.0, 0.0), make_point(1.0, 1.0)];
        let frame = AuthoredFrame::new(pts.clone());
        assert_eq!(frame.points().len(), 2);
        assert_eq!(frame.points()[0].x, 0.0);
        assert_eq!(frame.points()[1].x, 1.0);
    }

    #[test]
    fn test_authored_frame_first_last_point() {
        let frame = AuthoredFrame::new(vec![
            make_point(-1.0, -1.0),
            make_point(0.0, 0.0),
            make_point(1.0, 1.0),
        ]);
        assert_eq!(frame.first_point().unwrap().x, -1.0);
        assert_eq!(frame.last_point().unwrap().x, 1.0);
    }

    #[test]
    fn test_authored_frame_empty() {
        let frame = AuthoredFrame::new(vec![]);
        assert!(frame.is_empty());
        assert_eq!(frame.len(), 0);
        assert!(frame.first_point().is_none());
        assert!(frame.last_point().is_none());
    }

    #[test]
    fn test_authored_frame_len() {
        let frame = AuthoredFrame::new(vec![make_point(0.0, 0.0); 42]);
        assert_eq!(frame.len(), 42);
        assert!(!frame.is_empty());
    }

    #[test]
    fn test_authored_frame_from_vec() {
        let pts = vec![make_point(0.5, 0.5)];
        let frame: AuthoredFrame = pts.into();
        assert_eq!(frame.len(), 1);
        assert_eq!(frame.points()[0].x, 0.5);
    }

    #[test]
    fn test_authored_frame_clone_shares_data() {
        let frame = AuthoredFrame::new(vec![make_point(0.0, 0.0)]);
        let clone = frame.clone();
        // Both should point to the same Arc data
        assert_eq!(frame.len(), clone.len());
        assert_eq!(frame.points()[0].x, clone.points()[0].x);
    }

    // =========================================================================
    // default_transition tests
    // =========================================================================

    #[test]
    fn test_default_transition_produces_8_points() {
        let from = make_point(0.0, 0.0);
        let to = make_point(1.0, 1.0);
        let result = default_transition(&from, &to);
        assert_eq!(result.len(), 8);
    }

    #[test]
    fn test_default_transition_all_blanked() {
        let from = make_point(0.0, 0.0);
        let to = make_point(1.0, 1.0);
        let result = default_transition(&from, &to);
        for p in &result {
            assert_eq!(p.r, 0, "r should be 0");
            assert_eq!(p.g, 0, "g should be 0");
            assert_eq!(p.b, 0, "b should be 0");
            assert_eq!(p.intensity, 0, "intensity should be 0");
        }
    }

    #[test]
    fn test_default_transition_interpolates_xy() {
        let from = make_point(0.0, 0.0);
        let to = make_point(9.0, 9.0);
        let result = default_transition(&from, &to);

        // Points should be between from and to, strictly increasing
        for i in 0..result.len() {
            let p = &result[i];
            assert!(p.x > 0.0, "point {} x={} should be > 0", i, p.x);
            assert!(p.x < 9.0, "point {} x={} should be < 9", i, p.x);
            if i > 0 {
                assert!(
                    result[i].x > result[i - 1].x,
                    "x should be increasing: {} vs {}",
                    result[i - 1].x,
                    result[i].x
                );
            }
        }
    }

    #[test]
    fn test_default_transition_same_point_still_produces_8() {
        let p = make_point(0.5, -0.3);
        let result = default_transition(&p, &p);
        assert_eq!(result.len(), 8);
        // All points should be at the same position
        for point in &result {
            assert!((point.x - 0.5).abs() < 1e-6);
            assert!((point.y - (-0.3)).abs() < 1e-6);
            assert_eq!(point.r, 0);
        }
    }

    #[test]
    fn test_default_transition_endpoints_not_included() {
        // The transition points should NOT include the exact from/to positions
        let from = make_point(0.0, 0.0);
        let to = make_point(1.0, 0.0);
        let result = default_transition(&from, &to);
        assert!(result[0].x > 0.0, "first point should not be at from.x");
        assert!(
            result[7].x < 1.0,
            "last point should not be at to.x, got {}",
            result[7].x
        );
    }
}
