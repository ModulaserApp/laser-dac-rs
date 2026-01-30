//! Frame adapter for converting point buffers to continuous streams.
//!
//! This module provides [`FrameAdapter`] which converts a point buffer (frame)
//! into a continuous stream of points for the DAC. The adapter cycles through
//! the frame's points, producing chunks on demand.
//!
//! # Update Semantics
//!
//! - **Latest-wins**: `update()` sets the pending frame. Multiple calls before
//!   a swap keep only the most recent.
//! - **Immediate swap on frame end**: When the current frame completes (all
//!   points output), any pending frame becomes current immediately—even
//!   mid-chunk. This ensures clean frame-to-frame transitions.
//!
//! The adapter does not insert blanking on frame swaps. If the new frame's first
//! point is far from the previous frame's last point, content should include
//! lead-in blanking to avoid visible travel lines.
//!
//! # Example
//!
//! ```ignore
//! let mut adapter = FrameAdapter::new();
//! adapter.update(Frame::new(points));
//!
//! loop {
//!     let req = stream.next_request()?;
//!     let points = adapter.next_chunk(&req);
//!     stream.write(&req, &points)?;
//! }
//! ```
//!
//! For time-varying animation, use the streaming API directly with a point
//! generator (see the `manual` or `callback` examples with `orbiting-circle`).

use std::sync::{Arc, Mutex};

use crate::types::{FillRequest, FillResult, LaserPoint};

/// A point buffer to be cycled by the adapter.
#[derive(Clone, Debug)]
pub struct Frame {
    pub points: Vec<LaserPoint>,
}

impl Frame {
    /// Creates a new frame from a vector of points.
    pub fn new(points: Vec<LaserPoint>) -> Self {
        Self { points }
    }

    /// Creates an empty frame (outputs blanked points at last position).
    pub fn empty() -> Self {
        Self { points: Vec::new() }
    }
}

impl From<Vec<LaserPoint>> for Frame {
    fn from(points: Vec<LaserPoint>) -> Self {
        Self::new(points)
    }
}

/// Converts a point buffer (frame) into a continuous stream.
///
/// The adapter cycles through the frame's points, producing exactly
/// `req.n_points` on each `next_chunk()` call.
///
/// # Update semantics
///
/// - `update()` sets the pending frame (latest-wins if called multiple times)
/// - When the current frame ends, any pending frame becomes current immediately
///   (even mid-chunk), ensuring clean frame-to-frame transitions
///
/// # Example
///
/// ```ignore
/// let mut adapter = FrameAdapter::new();
/// adapter.update(Frame::new(circle_points));
///
/// loop {
///     let req = stream.next_request()?;
///     let points = adapter.next_chunk(&req);
///     stream.write(&req, &points)?;
/// }
/// ```
pub struct FrameAdapter {
    current: Frame,
    pending: Option<Frame>,
    point_index: usize,
    last_position: (f32, f32),
}

impl FrameAdapter {
    /// Creates a new adapter with an empty frame.
    ///
    /// Call `update()` to set the initial frame before streaming.
    pub fn new() -> Self {
        Self {
            current: Frame::empty(),
            pending: None,
            point_index: 0,
            last_position: (0.0, 0.0),
        }
    }

    /// Sets the pending frame.
    ///
    /// The frame becomes current when the current frame ends (all points
    /// output). If called multiple times before a swap, only the most recent
    /// frame is kept (latest-wins).
    pub fn update(&mut self, frame: Frame) {
        self.pending = Some(frame);
    }

    /// Fill the provided buffer with points from the current frame.
    ///
    /// This is the new zero-allocation API that fills a library-owned buffer
    /// instead of allocating a new Vec on each call.
    ///
    /// Cycles through the current frame. When the frame ends and a pending
    /// frame is available, switches immediately (even mid-chunk).
    ///
    /// # Returns
    ///
    /// - `FillResult::Filled(n)` - Wrote `n` points (always `req.target_points`)
    /// - `FillResult::Starved` - Never returned (adapter always has points to output)
    /// - `FillResult::End` - Never returned (adapter cycles indefinitely)
    pub fn fill_chunk(&mut self, req: &FillRequest, buffer: &mut [LaserPoint]) -> FillResult {
        let n_points = req.target_points.min(buffer.len());
        self.fill_points(buffer, n_points);
        FillResult::Filled(n_points)
    }

    /// Fill buffer with n_points from the frame (zero-allocation).
    #[allow(clippy::needless_range_loop)] // Index used with continue
    fn fill_points(&mut self, buffer: &mut [LaserPoint], n_points: usize) {
        for i in 0..n_points {
            // Handle empty frame: try to swap, else output blanked
            if self.current.points.is_empty() {
                if let Some(pending) = self.pending.take() {
                    self.current = pending;
                    self.point_index = 0;
                }

                if self.current.points.is_empty() {
                    let (x, y) = self.last_position;
                    buffer[i] = LaserPoint::blanked(x, y);
                    continue;
                }
            }

            let point = self.current.points[self.point_index];
            buffer[i] = point;
            self.last_position = (point.x, point.y);

            self.point_index += 1;
            if self.point_index >= self.current.points.len() {
                self.point_index = 0;
                // Immediately swap to pending frame if available
                if let Some(pending) = self.pending.take() {
                    self.current = pending;
                    self.point_index = 0;
                }
            }
        }
    }

    /// Returns a thread-safe handle for updating frames from another thread.
    pub fn shared(self) -> SharedFrameAdapter {
        SharedFrameAdapter {
            inner: Arc::new(Mutex::new(self)),
        }
    }
}

impl Default for FrameAdapter {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe handle for updating frames from another thread.
#[derive(Clone)]
pub struct SharedFrameAdapter {
    inner: Arc<Mutex<FrameAdapter>>,
}

impl SharedFrameAdapter {
    /// Sets the pending frame. Takes effect when the current frame ends.
    pub fn update(&self, frame: Frame) {
        let mut adapter = self.inner.lock().unwrap();
        adapter.update(frame);
    }

    /// Fill the provided buffer with points from the current frame.
    ///
    /// Thread-safe version of `FrameAdapter::fill_chunk()`.
    /// See that method for full documentation.
    pub fn fill_chunk(&self, req: &FillRequest, buffer: &mut [LaserPoint]) -> FillResult {
        let mut adapter = self.inner.lock().unwrap();
        adapter.fill_chunk(req, buffer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::StreamInstant;
    use std::time::Duration;

    fn make_fill_request(target_points: usize) -> FillRequest {
        FillRequest {
            start: StreamInstant(0),
            pps: 30000,
            min_points: target_points,
            target_points,
            buffered_points: 0,
            buffered: Duration::ZERO,
            device_queued_points: None,
        }
    }

    #[test]
    fn test_empty_frame() {
        let mut adapter = FrameAdapter::new();
        let req = make_fill_request(100);
        let mut buffer = vec![LaserPoint::default(); 100];

        let result = adapter.fill_chunk(&req, &mut buffer);
        assert!(matches!(result, FillResult::Filled(100)));
        assert!(buffer.iter().all(|p| p.intensity == 0));
    }

    #[test]
    fn test_frame_cycles() {
        let mut adapter = FrameAdapter::new();
        let frame_points: Vec<LaserPoint> = (0..10)
            .map(|i| LaserPoint::new(i as f32 / 10.0, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame_points));

        let req = make_fill_request(25);
        let mut buffer = vec![LaserPoint::default(); 25];

        let result = adapter.fill_chunk(&req, &mut buffer);
        assert!(matches!(result, FillResult::Filled(25)));
    }

    #[test]
    fn test_single_point_swaps_immediately() {
        let mut adapter = FrameAdapter::new();
        adapter.update(Frame::new(vec![LaserPoint::new(
            0.0, 0.0, 65535, 0, 0, 65535,
        )]));

        let req = make_fill_request(10);
        let mut buffer = vec![LaserPoint::default(); 10];

        adapter.fill_chunk(&req, &mut buffer);
        assert_eq!(buffer[0].x, 0.0);

        adapter.update(Frame::new(vec![LaserPoint::new(
            1.0, 1.0, 0, 65535, 0, 65535,
        )]));

        // Single-point frame wraps every point; swap happens immediately after
        // each point, so first point is old frame, rest is new frame
        adapter.fill_chunk(&req, &mut buffer);
        assert_eq!(buffer[0].x, 0.0, "First point finishes old frame");
        assert_eq!(buffer[1].x, 1.0, "Second point starts new frame");
        assert!(buffer[2..].iter().all(|p| p.x == 1.0), "Rest is new frame");
    }

    #[test]
    fn test_swap_waits_for_frame_end() {
        let mut adapter = FrameAdapter::new();
        let frame1: Vec<LaserPoint> = (0..100)
            .map(|i| LaserPoint::new(i as f32 / 100.0, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame1));

        let req = make_fill_request(10);
        let mut buffer = vec![LaserPoint::default(); 10];

        adapter.fill_chunk(&req, &mut buffer);
        assert_eq!(buffer[0].x, 0.0);

        // Update mid-cycle with different frame
        let frame2: Vec<LaserPoint> = (0..100)
            .map(|_| LaserPoint::new(9.0, 9.0, 0, 65535, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame2));

        // Should still use frame1 (not finished yet)
        adapter.fill_chunk(&req, &mut buffer);
        assert!(
            (buffer[0].x - 0.1).abs() < 1e-4,
            "Expected ~0.1, got {}",
            buffer[0].x
        );

        // Output remaining 80 points to complete frame1
        for _ in 0..8 {
            adapter.fill_chunk(&req, &mut buffer);
        }

        // Now frame1 finished, uses frame2
        adapter.fill_chunk(&req, &mut buffer);
        assert_eq!(buffer[0].x, 9.0);
    }

    #[test]
    fn test_mid_chunk_stitching() {
        // Frame with 95 points, chunk size 10: wrap happens mid-chunk
        let mut adapter = FrameAdapter::new();
        let frame1: Vec<LaserPoint> = (0..95)
            .map(|i| LaserPoint::new(i as f32, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame1));

        let req = make_fill_request(10);
        let mut buffer = vec![LaserPoint::default(); 10];

        // Output 90 points (9 chunks)
        for _ in 0..9 {
            adapter.fill_chunk(&req, &mut buffer);
        }

        // Now at index 90, update with new frame
        let frame2: Vec<LaserPoint> = (0..95)
            .map(|_| LaserPoint::new(999.0, 999.0, 0, 65535, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame2));

        // Next chunk: points 90-94 from frame1, then points 0-4 from frame2
        adapter.fill_chunk(&req, &mut buffer);

        // First 5 points are frame1 (90, 91, 92, 93, 94)
        for (i, p) in buffer[0..5].iter().enumerate() {
            assert_eq!(p.x, (90 + i) as f32, "Point {} should be from frame1", i);
        }

        // Last 5 points are frame2 (all 999.0)
        for (i, p) in buffer[5..10].iter().enumerate() {
            assert_eq!(p.x, 999.0, "Point {} should be from frame2", i + 5);
        }
    }

    #[test]
    fn test_empty_holds_last_position() {
        let mut adapter = FrameAdapter::new();
        adapter.update(Frame::new(vec![LaserPoint::new(
            0.5, -0.3, 65535, 0, 0, 65535,
        )]));

        let req = make_fill_request(5);
        let mut buffer = vec![LaserPoint::default(); 5];

        adapter.fill_chunk(&req, &mut buffer);
        adapter.update(Frame::empty());

        // First point finishes the single-point frame, then swap to empty
        adapter.fill_chunk(&req, &mut buffer);
        assert_eq!(buffer[0].intensity, 65535, "First point finishes old frame");
        assert!(
            buffer[1..].iter().all(|p| p.intensity == 0),
            "Rest is blanked"
        );
        // Empty frame holds the last known position
        assert_eq!(buffer[1].x, 0.5);
        assert_eq!(buffer[1].y, -0.3);
    }

    #[test]
    fn test_integer_index_deterministic() {
        let mut adapter = FrameAdapter::new();
        let frame: Vec<LaserPoint> = (0..7)
            .map(|i| LaserPoint::new(i as f32, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update(Frame::new(frame));

        let req = make_fill_request(7);
        let mut buffer = vec![LaserPoint::default(); 7];

        // No drift over 1000 cycles
        for cycle in 0..1000 {
            adapter.fill_chunk(&req, &mut buffer);
            for (i, p) in buffer.iter().enumerate() {
                assert_eq!(p.x, i as f32, "Cycle {}: drift detected", cycle);
            }
        }
    }

    #[test]
    fn test_from_vec() {
        let points: Vec<LaserPoint> = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];
        let frame: Frame = points.into();
        assert_eq!(frame.points.len(), 1);
    }
}
