//! Frame adapters for converting frame-based sources to streams.
//!
//! Frames remain supported as a convenience, but they feed a stream rather than
//! being written to DACs directly. The `FrameAdapter` converts frames to chunks
//! matching each `ChunkRequest`.

use std::sync::{Arc, Mutex};

use crate::types::{ChunkRequest, LaserPoint, StreamInstant};

/// A frame of laser points with a conceptual frame rate.
#[derive(Clone, Debug)]
pub struct Frame {
    /// The points in this frame.
    pub points: Vec<LaserPoint>,
    /// Conceptual frame rate for authoring sources (frames per second).
    pub fps: f32,
}

impl Frame {
    /// Creates a new frame with the given points and frame rate.
    pub fn new(points: Vec<LaserPoint>, fps: f32) -> Self {
        Self { points, fps }
    }

    /// Creates a blank frame (no points) at the given frame rate.
    pub fn blank(fps: f32) -> Self {
        Self {
            points: Vec::new(),
            fps,
        }
    }
}

/// Trait for sources that produce frames on demand.
///
/// Implement this trait for pull-based frame generation where the adapter
/// requests frames as needed.
pub trait FrameSource: Send {
    /// Produces the next frame for the given stream time and PPS.
    ///
    /// The `t` parameter indicates the stream time (in points since start),
    /// and `pps` is the current points-per-second rate.
    fn next_frame(&mut self, t: StreamInstant, pps: u32) -> Frame;
}

/// Converts frames to chunks matching each `ChunkRequest`.
///
/// The adapter supports two modes:
///
/// - **Latest frame mode**: You push frames in (from your UI/render loop),
///   and the stream pulls chunks out. Use `FrameAdapter::latest()` and
///   `update_frame()`.
///
/// - **Pull-based mode**: The adapter pulls frames from a `FrameSource`.
///   Use `FrameAdapter::from_source()`.
///
/// # Example (latest frame mode)
///
/// ```ignore
/// let mut adapter = FrameAdapter::latest(30.0);
///
/// // In your render loop, push new frames:
/// adapter.update_frame(Frame::new(points, 30.0));
///
/// // In the stream loop, pull chunks:
/// let req = stream.next_request()?;
/// let points = adapter.next_chunk(&req);
/// stream.write(&req, &points)?;
/// ```
pub struct FrameAdapter {
    inner: FrameAdapterInner,
}

enum FrameAdapterInner {
    /// Latest-frame mode: uses the most recently provided frame.
    Latest(LatestFrameAdapter),
    /// Pull-based mode: pulls frames from a source.
    Source(SourceFrameAdapter),
}

struct LatestFrameAdapter {
    /// The current frame being output.
    current_frame: Frame,
    /// Pending frame to switch to at next chunk boundary.
    pending_frame: Option<Frame>,
    /// Cursor position within the current frame (0.0 to 1.0).
    cursor: f64,
    /// Target FPS for timing calculations (reserved for future frame-rate-aware switching).
    #[allow(dead_code)]
    fps: f32,
}

struct SourceFrameAdapter {
    /// The frame source.
    source: Box<dyn FrameSource>,
    /// The current frame being output.
    current_frame: Option<Frame>,
    /// Cursor position within the current frame (0.0 to 1.0).
    cursor: f64,
    /// Last stream instant we fetched a frame for.
    last_frame_time: Option<StreamInstant>,
}

impl FrameAdapter {
    /// Creates a new adapter in "latest frame" mode.
    ///
    /// In this mode, you push frames via `update_frame()` and the adapter
    /// uses the most recently provided frame until a new one arrives.
    ///
    /// # Arguments
    ///
    /// * `fps` - The target frame rate. This is used to determine when to
    ///   switch to a new frame (at frame boundaries).
    pub fn latest(fps: f32) -> Self {
        Self {
            inner: FrameAdapterInner::Latest(LatestFrameAdapter {
                current_frame: Frame::blank(fps),
                pending_frame: None,
                cursor: 0.0,
                fps,
            }),
        }
    }

    /// Creates a new adapter in pull-based mode.
    ///
    /// The adapter will call `source.next_frame()` to get frames as needed.
    pub fn from_source(source: Box<dyn FrameSource>) -> Self {
        Self {
            inner: FrameAdapterInner::Source(SourceFrameAdapter {
                source,
                current_frame: None,
                cursor: 0.0,
                last_frame_time: None,
            }),
        }
    }

    /// Updates the current frame (latest-frame mode only).
    ///
    /// The new frame will be used starting at the next chunk boundary.
    ///
    /// # Panics
    ///
    /// Panics if called on a source-based adapter.
    pub fn update_frame(&mut self, frame: Frame) {
        match &mut self.inner {
            FrameAdapterInner::Latest(adapter) => {
                adapter.pending_frame = Some(frame);
            }
            FrameAdapterInner::Source(_) => {
                panic!("update_frame() called on source-based adapter");
            }
        }
    }

    /// Produces exactly `req.n_points` points for the given chunk request.
    ///
    /// This method maintains a cursor through the current frame and cycles
    /// when reaching the end. When a new frame arrives (via `update_frame()`
    /// or from the source), it switches at the next chunk boundary.
    pub fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint> {
        match &mut self.inner {
            FrameAdapterInner::Latest(adapter) => adapter.next_chunk(req),
            FrameAdapterInner::Source(adapter) => adapter.next_chunk(req),
        }
    }

    /// Returns a thread-safe handle for updating frames from another thread.
    ///
    /// This is useful when you want to update frames from a render thread
    /// while the stream runs on another thread.
    ///
    /// # Panics
    ///
    /// Panics if called on a source-based adapter.
    pub fn shared(self) -> SharedFrameAdapter {
        match self.inner {
            FrameAdapterInner::Latest(adapter) => SharedFrameAdapter {
                inner: Arc::new(Mutex::new(adapter)),
            },
            FrameAdapterInner::Source(_) => {
                panic!("shared() called on source-based adapter");
            }
        }
    }
}

impl LatestFrameAdapter {
    fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint> {
        // Check if we should switch to pending frame at chunk boundary
        if let Some(pending) = self.pending_frame.take() {
            self.current_frame = pending;
            self.cursor = 0.0;
        }

        self.generate_points(req.n_points)
    }

    fn generate_points(&mut self, n_points: usize) -> Vec<LaserPoint> {
        let frame_points = &self.current_frame.points;

        // Handle empty frame - return blanked points
        if frame_points.is_empty() {
            return vec![LaserPoint::blanked(0.0, 0.0); n_points];
        }

        let mut output = Vec::with_capacity(n_points);
        let frame_len = frame_points.len() as f64;

        for _ in 0..n_points {
            // Get the point at the current cursor position
            let index = (self.cursor * frame_len) as usize;
            let index = index.min(frame_points.len() - 1);
            output.push(frame_points[index]);

            // Advance cursor and wrap around
            self.cursor += 1.0 / frame_len;
            if self.cursor >= 1.0 {
                self.cursor -= 1.0;
            }
        }

        output
    }
}

impl SourceFrameAdapter {
    fn next_chunk(&mut self, req: &ChunkRequest) -> Vec<LaserPoint> {
        // Determine if we need a new frame based on time
        let need_new_frame = match self.last_frame_time {
            None => true,
            Some(last_time) => {
                // Get a new frame if we've advanced past it
                // For simplicity, get a new frame each chunk (can be optimized later)
                let frame_duration = self
                    .current_frame
                    .as_ref()
                    .map(|f| (req.pps as f32 / f.fps) as u64)
                    .unwrap_or(0);

                req.start.0 >= last_time.0 + frame_duration
            }
        };

        if need_new_frame {
            let frame = self.source.next_frame(req.start, req.pps);
            self.current_frame = Some(frame);
            self.last_frame_time = Some(req.start);
            self.cursor = 0.0;
        }

        self.generate_points(req.n_points)
    }

    fn generate_points(&mut self, n_points: usize) -> Vec<LaserPoint> {
        let Some(ref frame) = self.current_frame else {
            return vec![LaserPoint::blanked(0.0, 0.0); n_points];
        };

        let frame_points = &frame.points;

        // Handle empty frame - return blanked points
        if frame_points.is_empty() {
            return vec![LaserPoint::blanked(0.0, 0.0); n_points];
        }

        let mut output = Vec::with_capacity(n_points);
        let frame_len = frame_points.len() as f64;

        for _ in 0..n_points {
            // Get the point at the current cursor position
            let index = (self.cursor * frame_len) as usize;
            let index = index.min(frame_points.len() - 1);
            output.push(frame_points[index]);

            // Advance cursor and wrap around
            self.cursor += 1.0 / frame_len;
            if self.cursor >= 1.0 {
                self.cursor -= 1.0;
            }
        }

        output
    }
}

/// Thread-safe handle for updating frames from another thread.
///
/// Use `FrameAdapter::shared()` to create this from a latest-frame adapter.
#[derive(Clone)]
pub struct SharedFrameAdapter {
    inner: Arc<Mutex<LatestFrameAdapter>>,
}

impl SharedFrameAdapter {
    /// Updates the current frame.
    ///
    /// The new frame will be used starting at the next chunk boundary.
    pub fn update_frame(&self, frame: Frame) {
        let mut adapter = self.inner.lock().unwrap();
        adapter.pending_frame = Some(frame);
    }

    /// Produces exactly `req.n_points` points for the given chunk request.
    pub fn next_chunk(&self, req: &ChunkRequest) -> Vec<LaserPoint> {
        let mut adapter = self.inner.lock().unwrap();
        adapter.next_chunk(req)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_latest_frame_empty() {
        let mut adapter = FrameAdapter::latest(30.0);
        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 100,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        let points = adapter.next_chunk(&req);
        assert_eq!(points.len(), 100);
        // All points should be blanked
        assert!(points.iter().all(|p| p.intensity == 0));
    }

    #[test]
    fn test_latest_frame_cycles() {
        let mut adapter = FrameAdapter::latest(30.0);

        // Create a simple frame with 10 points
        let frame_points: Vec<LaserPoint> = (0..10)
            .map(|i| LaserPoint::new(i as f32 / 10.0, 0.0, 65535, 0, 0, 65535))
            .collect();
        adapter.update_frame(Frame::new(frame_points, 30.0));

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 25,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        let points = adapter.next_chunk(&req);
        assert_eq!(points.len(), 25);
    }

    #[test]
    fn test_frame_switch_at_boundary() {
        let mut adapter = FrameAdapter::latest(30.0);

        // First frame
        let frame1 = Frame::new(
            vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)],
            30.0,
        );
        adapter.update_frame(frame1);

        let req = ChunkRequest {
            start: StreamInstant(0),
            pps: 30000,
            n_points: 10,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        let points1 = adapter.next_chunk(&req);
        assert_eq!(points1[0].x, 0.0);

        // Update with new frame
        let frame2 = Frame::new(
            vec![LaserPoint::new(1.0, 1.0, 0, 65535, 0, 65535)],
            30.0,
        );
        adapter.update_frame(frame2);

        // Next chunk should use new frame
        let points2 = adapter.next_chunk(&req);
        assert_eq!(points2[0].x, 1.0);
    }
}
