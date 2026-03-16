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

// =============================================================================
// PresentationEngine
// =============================================================================

/// Core frame lifecycle manager.
///
/// Manages the current and pending frames, cursor position, and transition
/// point insertion. Provides two delivery modes:
///
/// - [`fill_chunk`](Self::fill_chunk): FIFO delivery for queue-based DACs.
///   Traverses the drawable, inserting transition points at frame boundaries.
/// - [`compose_hardware_frame`](Self::compose_hardware_frame): Frame-swap
///   delivery. Returns a complete composed frame with transition points.
pub(crate) struct PresentationEngine {
    /// The currently playing frame.
    current_base: Option<Arc<AuthoredFrame>>,
    /// The next frame to promote (latest-wins).
    pending_base: Option<Arc<AuthoredFrame>>,
    /// Composed drawable: base points + transition points for current state.
    drawable: Vec<LaserPoint>,
    /// Whether the drawable needs to be recomposed.
    drawable_dirty: bool,
    /// Current read cursor within `drawable`.
    cursor: usize,
    /// Transition function for generating blanking between frames.
    transition_fn: TransitionFn,
}

impl PresentationEngine {
    /// Create a new engine with the given transition function.
    pub fn new(transition_fn: TransitionFn) -> Self {
        Self {
            current_base: None,
            pending_base: None,
            drawable: Vec::new(),
            drawable_dirty: true,
            cursor: 0,
            transition_fn,
        }
    }

    /// Submit a new frame. Latest-wins: multiple calls before consumption
    /// keep only the most recent frame.
    ///
    /// If no current frame exists, the pending is immediately promoted.
    pub fn set_pending(&mut self, frame: Arc<AuthoredFrame>) {
        if self.current_base.is_none() {
            self.current_base = Some(frame);
            self.drawable_dirty = true;
            self.cursor = 0;
        } else {
            self.pending_base = Some(frame);
            // Don't mark dirty yet — we compose on promotion
        }
    }

    /// FIFO delivery: fill `buffer[..max_points]` from the drawable.
    ///
    /// Traverses the current frame cyclically, inserting transition points
    /// at wrap boundaries. When the frame completes and a pending frame
    /// exists, promotes it and continues filling.
    ///
    /// Returns the number of points written (always `max_points` if a
    /// frame is available, 0 if no frame has been submitted).
    pub fn fill_chunk(&mut self, buffer: &mut [LaserPoint], max_points: usize) -> usize {
        let max_points = max_points.min(buffer.len());

        // Before first frame: output blanked at origin
        if self.current_base.is_none() {
            for p in &mut buffer[..max_points] {
                *p = LaserPoint::blanked(0.0, 0.0);
            }
            return max_points;
        }

        // Rebuild drawable if dirty
        if self.drawable_dirty {
            self.refresh_drawable();
        }

        if self.drawable.is_empty() {
            for p in &mut buffer[..max_points] {
                *p = LaserPoint::blanked(0.0, 0.0);
            }
            return max_points;
        }

        let mut written = 0;
        while written < max_points {
            // Output current point
            buffer[written] = self.drawable[self.cursor];
            written += 1;
            self.cursor += 1;

            // Check if we've completed the drawable
            if self.cursor >= self.drawable.len() {
                // Promote pending if available
                if let Some(pending) = self.pending_base.take() {
                    self.current_base = Some(pending);
                    self.drawable_dirty = true;
                    self.refresh_drawable();
                    self.cursor = 0;
                } else {
                    // Self-loop: reset cursor, recompose for self-transition
                    self.drawable_dirty = true;
                    self.refresh_drawable();
                    self.cursor = 0;
                }
            }
        }

        written
    }

    /// Frame-swap delivery: compose and return a complete hardware frame.
    ///
    /// Returns the composed frame (base + transition) for the current state.
    /// If a pending frame exists, it is promoted first.
    pub fn compose_hardware_frame(&mut self) -> &[LaserPoint] {
        // Promote pending if available
        if let Some(pending) = self.pending_base.take() {
            self.current_base = Some(pending);
            self.drawable_dirty = true;
        }

        if self.drawable_dirty {
            self.refresh_drawable();
        }

        &self.drawable
    }

    /// Rebuild the drawable from the current base frame + transition.
    fn refresh_drawable(&mut self) {
        self.drawable.clear();
        self.drawable_dirty = false;

        let Some(current) = &self.current_base else {
            return;
        };

        if current.is_empty() {
            return;
        }

        let points = current.points();

        // Add transition from last point to first point (self-loop or inter-frame)
        let last = points.last().unwrap();
        let first = points.first().unwrap();
        let transition = (self.transition_fn)(last, first);
        self.drawable.extend_from_slice(&transition);

        // Add the base frame points
        self.drawable.extend_from_slice(points);
    }
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

    // =========================================================================
    // PresentationEngine tests
    // =========================================================================

    /// Create an engine with a transition that produces 2 blanked interpolated points.
    fn make_engine() -> PresentationEngine {
        PresentationEngine::new(Box::new(|from: &LaserPoint, to: &LaserPoint| {
            vec![
                LaserPoint::blanked(from.x * 0.5 + to.x * 0.5, from.y * 0.5 + to.y * 0.5),
                LaserPoint::blanked(to.x, to.y),
            ]
        }))
    }

    /// Create an engine with zero transition points.
    fn make_engine_no_transition() -> PresentationEngine {
        PresentationEngine::new(Box::new(|_: &LaserPoint, _: &LaserPoint| vec![]))
    }

    fn make_frame(points: Vec<LaserPoint>) -> Arc<AuthoredFrame> {
        Arc::new(AuthoredFrame::new(points))
    }

    #[test]
    fn test_engine_before_first_frame_blanks_at_origin() {
        let mut engine = make_engine();
        let mut buffer = vec![LaserPoint::default(); 10];
        let n = engine.fill_chunk(&mut buffer, 10);
        assert_eq!(n, 10);
        for p in &buffer {
            assert_eq!(p.x, 0.0);
            assert_eq!(p.y, 0.0);
            assert_eq!(p.intensity, 0);
        }
    }

    #[test]
    fn test_engine_set_pending_promotes_when_no_current() {
        let mut engine = make_engine_no_transition();
        let frame = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
        engine.set_pending(frame);

        // Should have promoted to current
        assert!(engine.current_base.is_some());
        assert!(engine.pending_base.is_none());
    }

    #[test]
    fn test_engine_set_pending_overwrites_existing_pending() {
        let mut engine = make_engine_no_transition();
        let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
        let frame_b = make_frame(vec![make_point(2.0, 0.0)]);
        let frame_c = make_frame(vec![make_point(3.0, 0.0)]);

        engine.set_pending(frame_a); // promotes to current
        engine.set_pending(frame_b); // pending
        engine.set_pending(frame_c); // overwrites pending

        assert!(engine.pending_base.is_some());
        assert_eq!(engine.pending_base.as_ref().unwrap().points()[0].x, 3.0);
    }

    #[test]
    fn test_engine_fill_chunk_cycles_frame() {
        let mut engine = make_engine_no_transition();
        let frame = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
        engine.set_pending(frame);

        let mut buffer = vec![LaserPoint::default(); 6];
        let n = engine.fill_chunk(&mut buffer, 6);
        assert_eq!(n, 6);
        // Frame is [1.0, 2.0], cycles: 1.0, 2.0, 1.0, 2.0, 1.0, 2.0
        assert_eq!(buffer[0].x, 1.0);
        assert_eq!(buffer[1].x, 2.0);
        assert_eq!(buffer[2].x, 1.0);
        assert_eq!(buffer[3].x, 2.0);
    }

    #[test]
    fn test_engine_fill_chunk_self_loop_calls_transition() {
        let mut engine = make_engine(); // 2-point transition
        let frame = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);
        engine.set_pending(frame);

        // Drawable = [transition(last→first)] + [frame points]
        // = 2 transition pts + 2 frame pts = 4 total per cycle
        let mut buffer = vec![LaserPoint::default(); 8];
        let n = engine.fill_chunk(&mut buffer, 8);
        assert_eq!(n, 8);

        // First cycle: [trans0, trans1, 0.0, 1.0]
        // trans0 is blanked (midpoint of last(1.0) and first(0.0) = 0.5)
        assert_eq!(buffer[0].intensity, 0); // transition point
        assert_eq!(buffer[2].x, 0.0); // frame point
        assert_eq!(buffer[3].x, 1.0); // frame point
    }

    #[test]
    fn test_engine_fill_chunk_promotes_pending_at_frame_end() {
        let mut engine = make_engine_no_transition();
        let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
        let frame_b = make_frame(vec![make_point(2.0, 0.0)]);

        engine.set_pending(frame_a);
        engine.set_pending(frame_b); // pending

        // First point is frame_a, then frame_b takes over
        let mut buffer = vec![LaserPoint::default(); 4];
        let n = engine.fill_chunk(&mut buffer, 4);
        assert_eq!(n, 4);
        assert_eq!(buffer[0].x, 1.0); // frame_a
        assert_eq!(buffer[1].x, 2.0); // frame_b promoted
        assert_eq!(buffer[2].x, 2.0); // frame_b cycles
    }

    #[test]
    fn test_engine_fill_chunk_frame_skip() {
        let mut engine = make_engine_no_transition();
        let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
        let frame_b = make_frame(vec![make_point(2.0, 0.0)]);
        let frame_c = make_frame(vec![make_point(3.0, 0.0)]);

        engine.set_pending(frame_a);
        // B is pending, then overwritten by C
        engine.set_pending(frame_b);
        engine.set_pending(frame_c);

        let mut buffer = vec![LaserPoint::default(); 4];
        engine.fill_chunk(&mut buffer, 4);
        // frame_a plays, then C (B was skipped)
        assert_eq!(buffer[0].x, 1.0);
        assert_eq!(buffer[1].x, 3.0);
    }

    #[test]
    fn test_engine_fill_chunk_cursor_continuity() {
        let mut engine = make_engine_no_transition();
        let frame = make_frame(vec![
            make_point(0.0, 0.0),
            make_point(1.0, 0.0),
            make_point(2.0, 0.0),
        ]);
        engine.set_pending(frame);

        // Fill 2 points
        let mut buf1 = vec![LaserPoint::default(); 2];
        engine.fill_chunk(&mut buf1, 2);
        assert_eq!(buf1[0].x, 0.0);
        assert_eq!(buf1[1].x, 1.0);

        // Fill 2 more — should continue from where we left off
        let mut buf2 = vec![LaserPoint::default(); 2];
        engine.fill_chunk(&mut buf2, 2);
        assert_eq!(buf2[0].x, 2.0);
        // Next is wrap back to 0.0
        assert_eq!(buf2[1].x, 0.0);
    }

    #[test]
    fn test_engine_compose_hardware_frame_self_loop() {
        let mut engine = make_engine(); // 2-point transition
        let frame = make_frame(vec![make_point(0.0, 0.0), make_point(1.0, 0.0)]);
        engine.set_pending(frame);

        let composed = engine.compose_hardware_frame();
        // Should be: 2 transition + 2 frame = 4 points
        assert_eq!(composed.len(), 4);
        // Transition points are blanked
        assert_eq!(composed[0].intensity, 0);
        assert_eq!(composed[1].intensity, 0);
        // Frame points are full intensity
        assert_eq!(composed[2].x, 0.0);
        assert_eq!(composed[3].x, 1.0);
    }

    #[test]
    fn test_engine_compose_hardware_frame_promotes_pending() {
        let mut engine = make_engine_no_transition();
        let frame_a = make_frame(vec![make_point(1.0, 0.0)]);
        let frame_b = make_frame(vec![make_point(2.0, 0.0)]);

        engine.set_pending(frame_a);
        engine.set_pending(frame_b);

        let composed = engine.compose_hardware_frame();
        // Should be frame_b (promoted)
        assert_eq!(composed.len(), 1);
        assert_eq!(composed[0].x, 2.0);
    }

    #[test]
    fn test_engine_compose_hardware_frame_empty_before_first_frame() {
        let mut engine = make_engine();
        let composed = engine.compose_hardware_frame();
        assert!(composed.is_empty());
    }

    #[test]
    fn test_engine_fill_chunk_multiple_wraps_in_single_call() {
        let mut engine = make_engine_no_transition();
        let frame = make_frame(vec![make_point(5.0, 0.0)]);
        engine.set_pending(frame);

        // Fill 10 points from a 1-point frame — should wrap 10 times
        let mut buffer = vec![LaserPoint::default(); 10];
        let n = engine.fill_chunk(&mut buffer, 10);
        assert_eq!(n, 10);
        for p in &buffer {
            assert_eq!(p.x, 5.0);
        }
    }

    #[test]
    fn test_engine_empty_transition_result() {
        let mut engine = make_engine_no_transition();
        let frame = make_frame(vec![make_point(1.0, 0.0), make_point(2.0, 0.0)]);
        engine.set_pending(frame);

        let mut buffer = vec![LaserPoint::default(); 4];
        engine.fill_chunk(&mut buffer, 4);
        // No transition points, just frame cycling: 1, 2, 1, 2
        assert_eq!(buffer[0].x, 1.0);
        assert_eq!(buffer[1].x, 2.0);
        assert_eq!(buffer[2].x, 1.0);
        assert_eq!(buffer[3].x, 2.0);
    }
}
