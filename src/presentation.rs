//! Frame-first presentation types and engine.
//!
//! This module provides:
//! - [`AuthoredFrame`]: immutable frame type for submission
//! - [`TransitionFn`] / [`default_transition`]: blanking between frames
//! - [`PresentationEngine`]: core frame lifecycle manager (internal)
//! - [`FrameSession`] / [`FrameSessionConfig`]: public frame-mode API

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

// =============================================================================
// FrameSessionConfig
// =============================================================================

/// Configuration for a frame-mode streaming session.
pub struct FrameSessionConfig {
    /// Points per second output rate.
    pub pps: u32,
    /// Transition function for blanking between frames.
    pub transition_fn: TransitionFn,
    /// Duration of forced blanking after arming (default: 1ms).
    pub startup_blank: std::time::Duration,
    /// Number of points to shift RGB relative to XY (0 = disabled).
    ///
    /// Applied at composition time in the `PresentationEngine`. Frame-swap DACs
    /// receive already-correct data with no cross-frame color bleed.
    pub color_delay_points: usize,
}

impl FrameSessionConfig {
    /// Create a new config with the given PPS and default transition.
    pub fn new(pps: u32) -> Self {
        Self {
            pps,
            transition_fn: Box::new(default_transition),
            startup_blank: std::time::Duration::from_millis(1),
            color_delay_points: 0,
        }
    }

    /// Set the transition function (builder pattern).
    pub fn with_transition_fn(mut self, f: TransitionFn) -> Self {
        self.transition_fn = f;
        self
    }

    /// Set the startup blank duration (builder pattern).
    pub fn with_startup_blank(mut self, duration: std::time::Duration) -> Self {
        self.startup_blank = duration;
        self
    }

    /// Set the color delay in points (builder pattern).
    pub fn with_color_delay_points(mut self, n: usize) -> Self {
        self.color_delay_points = n;
        self
    }
}

// =============================================================================
// FrameSession
// =============================================================================

use std::sync::mpsc;
use std::thread::JoinHandle;

use crate::backend::{BackendKind, WriteOutcome};
use crate::error::{Error, Result};
use crate::stream::StreamControl;
use crate::types::RunExit;

/// A frame-mode streaming session.
///
/// Owns a scheduler thread that reads frames from a channel and writes them
/// to the DAC backend using the appropriate strategy (FIFO or frame-swap).
///
/// # Example
///
/// ```ignore
/// use laser_dac::{open_device, FrameSessionConfig, AuthoredFrame, LaserPoint};
///
/// let device = open_device("my-device")?;
/// let config = FrameSessionConfig::new(30_000);
/// let session = device.start_frame_session(config)?;
///
/// session.control().arm()?;
/// session.send_frame(AuthoredFrame::new(vec![
///     LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
/// ]));
/// ```
pub struct FrameSession {
    control: StreamControl,
    thread: Option<JoinHandle<Result<RunExit>>>,
    frame_tx: mpsc::Sender<AuthoredFrame>,
}

impl FrameSession {
    /// Start a frame session on the given backend.
    pub(crate) fn start(
        mut backend: BackendKind,
        config: FrameSessionConfig,
    ) -> Result<Self> {
        // Connect if needed
        if !backend.is_connected() {
            backend.connect()?;
        }

        let (control_tx, control_rx) = mpsc::channel();
        let control = StreamControl::new(control_tx, std::time::Duration::ZERO);
        let (frame_tx, frame_rx) = mpsc::channel::<AuthoredFrame>();

        let control_clone = control.clone();
        let is_frame_swap = backend.is_frame_swap();

        let thread = std::thread::spawn(move || {
            if is_frame_swap {
                Self::run_frame_swap_loop(backend, config, control_clone, control_rx, frame_rx)
            } else {
                Self::run_fifo_loop(backend, config, control_clone, control_rx, frame_rx)
            }
        });

        Ok(Self {
            control,
            thread: Some(thread),
            frame_tx,
        })
    }

    /// Returns a control handle for arm/disarm/stop.
    pub fn control(&self) -> StreamControl {
        self.control.clone()
    }

    /// Submit a frame for display. Latest-wins if the engine hasn't consumed
    /// the previous pending frame yet.
    pub fn send_frame(&self, frame: AuthoredFrame) {
        let _ = self.frame_tx.send(frame);
    }

    /// Returns true if the scheduler thread has finished.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Wait for the session thread to finish and return the exit reason.
    pub fn join(mut self) -> Result<RunExit> {
        if let Some(handle) = self.thread.take() {
            handle.join().unwrap_or(Err(Error::disconnected("thread panicked")))
        } else {
            Ok(RunExit::Stopped)
        }
    }

    // =========================================================================
    // FIFO scheduler loop
    // =========================================================================

    fn run_fifo_loop(
        mut backend: BackendKind,
        config: FrameSessionConfig,
        control: StreamControl,
        control_rx: mpsc::Receiver<crate::stream::ControlMsg>,
        frame_rx: mpsc::Receiver<AuthoredFrame>,
    ) -> Result<RunExit> {
        use std::time::{Duration, Instant};

        let pps = config.pps as f64;
        let max_points = backend.caps().max_points_per_chunk;
        let target_buffer_secs = 0.020_f64; // 20ms
        let target_buffer_points = (target_buffer_secs * pps) as u64;

        let mut engine = PresentationEngine::new(config.transition_fn);
        let mut scheduled_ahead: u64 = 0;
        let mut fractional_consumed: f64 = 0.0;
        let mut last_iteration = Instant::now();
        let mut chunk_buffer = vec![LaserPoint::default(); max_points];
        let mut shutter_open = false;
        let mut startup_blank_remaining: usize = 0;
        let startup_blank_points = if config.startup_blank.is_zero() {
            0
        } else {
            (config.startup_blank.as_secs_f64() * pps).ceil() as usize
        };
        let mut last_armed = false;

        loop {
            // 1. Check stop
            if control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            // Time-based decay of scheduled_ahead
            let now = Instant::now();
            let elapsed = now.duration_since(last_iteration);
            let consumed_f64 = elapsed.as_secs_f64() * pps + fractional_consumed;
            let points_consumed = consumed_f64 as u64;
            fractional_consumed = consumed_f64 - points_consumed as f64;
            scheduled_ahead = scheduled_ahead.saturating_sub(points_consumed);
            last_iteration = now;

            // 2. Drain frame channel
            while let Ok(frame) = frame_rx.try_recv() {
                engine.set_pending(Arc::new(frame));
            }

            // 3. Estimate buffer
            let buffered = if let Some(hw) = backend.queued_points() {
                hw.min(scheduled_ahead)
            } else {
                scheduled_ahead
            };

            // 4. Sleep if buffer healthy
            if buffered > target_buffer_points {
                let excess = buffered - target_buffer_points;
                let sleep_time = Duration::from_secs_f64(excess as f64 / pps);
                Self::sleep_with_control_check(&control, &control_rx, sleep_time, &mut shutter_open, &mut backend)?;
                continue;
            }

            // 5. Check connection
            if !backend.is_connected() {
                return Ok(RunExit::Disconnected);
            }

            // 6. Process control messages
            if Self::process_control_messages(&control_rx, &mut shutter_open, &mut backend) {
                return Ok(RunExit::Stopped);
            }

            // 7. Handle arm/disarm transitions
            let is_armed = control.is_armed();
            if !last_armed && is_armed {
                startup_blank_remaining = startup_blank_points;
                if !shutter_open {
                    let _ = backend.set_shutter(true);
                    shutter_open = true;
                }
            } else if last_armed && !is_armed {
                if shutter_open {
                    let _ = backend.set_shutter(false);
                    shutter_open = false;
                }
            }
            last_armed = is_armed;

            // 8. Fill chunk from engine
            let deficit = (target_buffer_secs - buffered as f64 / pps).max(0.0);
            let target_points = ((deficit * pps).ceil() as usize).min(max_points);
            if target_points == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }

            let n = engine.fill_chunk(&mut chunk_buffer, target_points);
            if n == 0 {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }

            // Apply blanking when disarmed
            if !is_armed {
                for p in &mut chunk_buffer[..n] {
                    *p = LaserPoint::blanked(p.x, p.y);
                }
            }

            // Apply startup blanking
            if is_armed && startup_blank_remaining > 0 {
                let blank_count = n.min(startup_blank_remaining);
                for p in &mut chunk_buffer[..blank_count] {
                    p.r = 0;
                    p.g = 0;
                    p.b = 0;
                    p.intensity = 0;
                }
                startup_blank_remaining -= blank_count;
            }

            // Apply color delay
            Self::apply_color_delay(&mut chunk_buffer[..n], config.color_delay_points);

            // 9. Write to backend
            match backend.try_write(config.pps, &chunk_buffer[..n]) {
                Ok(WriteOutcome::Written) => {
                    scheduled_ahead += n as u64;
                }
                Ok(WriteOutcome::WouldBlock) => {
                    std::thread::yield_now();
                }
                Err(e) if e.is_disconnected() => {
                    return Ok(RunExit::Disconnected);
                }
                Err(_) => {
                    // Backend error — continue
                }
            }
        }
    }

    // =========================================================================
    // Frame-swap scheduler loop
    // =========================================================================

    fn run_frame_swap_loop(
        mut backend: BackendKind,
        config: FrameSessionConfig,
        control: StreamControl,
        control_rx: mpsc::Receiver<crate::stream::ControlMsg>,
        frame_rx: mpsc::Receiver<AuthoredFrame>,
    ) -> Result<RunExit> {
        use std::time::Duration;

        let mut engine = PresentationEngine::new(config.transition_fn);
        let mut shutter_open = false;
        let mut startup_blank_remaining: usize = 0;
        let pps = config.pps as f64;
        let startup_blank_points = if config.startup_blank.is_zero() {
            0
        } else {
            (config.startup_blank.as_secs_f64() * pps).ceil() as usize
        };
        let mut last_armed = false;

        loop {
            // 1. Check stop
            if control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            // 2. Drain frame channel
            while let Ok(frame) = frame_rx.try_recv() {
                engine.set_pending(Arc::new(frame));
            }

            // 3. Check connection
            if !backend.is_connected() {
                return Ok(RunExit::Disconnected);
            }

            // 4. Process control messages
            if Self::process_control_messages(&control_rx, &mut shutter_open, &mut backend) {
                return Ok(RunExit::Stopped);
            }

            // 5. Handle arm/disarm transitions
            let is_armed = control.is_armed();
            if !last_armed && is_armed {
                startup_blank_remaining = startup_blank_points;
                if !shutter_open {
                    let _ = backend.set_shutter(true);
                    shutter_open = true;
                }
            } else if last_armed && !is_armed {
                if shutter_open {
                    let _ = backend.set_shutter(false);
                    shutter_open = false;
                }
            }
            last_armed = is_armed;

            // 6. Check if device is ready
            let ready = match &mut backend {
                BackendKind::FrameSwap(b) => b.is_ready_for_frame(),
                _ => true,
            };

            if !ready {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }

            // 7. Compose frame
            let composed = engine.compose_hardware_frame();
            if composed.is_empty() {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }

            // Copy to mutable buffer for blanking/color delay
            let mut frame_buf: Vec<LaserPoint> = composed.to_vec();

            // Apply blanking when disarmed
            if !is_armed {
                for p in &mut frame_buf {
                    *p = LaserPoint::blanked(p.x, p.y);
                }
            }

            // Apply startup blanking
            if is_armed && startup_blank_remaining > 0 {
                let blank_count = frame_buf.len().min(startup_blank_remaining);
                for p in &mut frame_buf[..blank_count] {
                    p.r = 0;
                    p.g = 0;
                    p.b = 0;
                    p.intensity = 0;
                }
                startup_blank_remaining -= blank_count;
            }

            // Apply color delay
            Self::apply_color_delay(&mut frame_buf, config.color_delay_points);

            // 8. Write frame
            match backend.try_write(config.pps, &frame_buf) {
                Ok(WriteOutcome::Written) => {}
                Ok(WriteOutcome::WouldBlock) => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) if e.is_disconnected() => {
                    return Ok(RunExit::Disconnected);
                }
                Err(_) => {}
            }
        }
    }

    // =========================================================================
    // Shared helpers
    // =========================================================================

    /// Apply static color delay by shifting RGB+intensity relative to XY.
    fn apply_color_delay(points: &mut [LaserPoint], delay: usize) {
        if delay == 0 || points.len() <= delay {
            return;
        }
        // Collect delayed colors
        let colors: Vec<(u16, u16, u16, u16)> = points
            .iter()
            .map(|p| (p.r, p.g, p.b, p.intensity))
            .collect();
        // Shift: point[i] gets colors from point[i - delay]
        for i in 0..points.len() {
            if i < delay {
                points[i].r = 0;
                points[i].g = 0;
                points[i].b = 0;
                points[i].intensity = 0;
            } else {
                let (r, g, b, intensity) = colors[i - delay];
                points[i].r = r;
                points[i].g = g;
                points[i].b = b;
                points[i].intensity = intensity;
            }
        }
    }

    fn process_control_messages(
        control_rx: &mpsc::Receiver<crate::stream::ControlMsg>,
        shutter_open: &mut bool,
        backend: &mut BackendKind,
    ) -> bool {
        use std::sync::mpsc::TryRecvError;
        loop {
            match control_rx.try_recv() {
                Ok(crate::stream::ControlMsg::Arm) => {
                    if !*shutter_open {
                        let _ = backend.set_shutter(true);
                        *shutter_open = true;
                    }
                }
                Ok(crate::stream::ControlMsg::Disarm) => {
                    if *shutter_open {
                        let _ = backend.set_shutter(false);
                        *shutter_open = false;
                    }
                }
                Ok(crate::stream::ControlMsg::Stop) => {
                    return true;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        false
    }

    fn sleep_with_control_check(
        control: &StreamControl,
        control_rx: &mpsc::Receiver<crate::stream::ControlMsg>,
        duration: std::time::Duration,
        shutter_open: &mut bool,
        backend: &mut BackendKind,
    ) -> Result<()> {
        const SLICE: std::time::Duration = std::time::Duration::from_millis(2);
        let mut remaining = duration;
        while remaining > std::time::Duration::ZERO {
            let slice = remaining.min(SLICE);
            std::thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
            if control.is_stop_requested() {
                return Err(Error::Stopped);
            }
            Self::process_control_messages(control_rx, shutter_open, backend);
        }
        Ok(())
    }
}

impl Drop for FrameSession {
    fn drop(&mut self) {
        let _ = self.control.stop();
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
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

    // =========================================================================
    // FrameSession tests
    // =========================================================================

    use crate::backend::{BackendKind, DacBackend, FifoBackend, FrameSwapBackend};
    use crate::error::{Error as DacError, Result as DacResult};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Minimal FIFO test backend for FrameSession tests.
    struct FifoTestBackend {
        connected: bool,
        write_count: Arc<AtomicUsize>,
        points_written: Arc<AtomicUsize>,
        shutter_open: Arc<AtomicBool>,
    }

    impl FifoTestBackend {
        fn new() -> Self {
            Self {
                connected: false,
                write_count: Arc::new(AtomicUsize::new(0)),
                points_written: Arc::new(AtomicUsize::new(0)),
                shutter_open: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    impl DacBackend for FifoTestBackend {
        fn dac_type(&self) -> crate::types::DacType {
            crate::types::DacType::Custom("FifoTest".into())
        }
        fn caps(&self) -> &crate::types::DacCapabilities {
            static CAPS: crate::types::DacCapabilities = crate::types::DacCapabilities {
                pps_min: 1000,
                pps_max: 100000,
                max_points_per_chunk: 1000,
                output_model: crate::types::OutputModel::NetworkFifo,
            };
            &CAPS
        }
        fn connect(&mut self) -> DacResult<()> {
            self.connected = true;
            Ok(())
        }
        fn disconnect(&mut self) -> DacResult<()> {
            self.connected = false;
            Ok(())
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn stop(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn set_shutter(&mut self, open: bool) -> DacResult<()> {
            self.shutter_open.store(open, Ordering::SeqCst);
            Ok(())
        }
    }

    impl FifoBackend for FifoTestBackend {
        fn try_write_points(
            &mut self,
            _pps: u32,
            points: &[LaserPoint],
        ) -> DacResult<crate::backend::WriteOutcome> {
            self.write_count.fetch_add(1, Ordering::SeqCst);
            self.points_written
                .fetch_add(points.len(), Ordering::SeqCst);
            Ok(crate::backend::WriteOutcome::Written)
        }
    }

    /// Minimal FrameSwap test backend.
    struct FrameSwapTestBackend {
        connected: bool,
        write_count: Arc<AtomicUsize>,
        last_frame_size: Arc<AtomicUsize>,
        shutter_open: Arc<AtomicBool>,
        ready: Arc<AtomicBool>,
    }

    impl FrameSwapTestBackend {
        fn new() -> Self {
            Self {
                connected: false,
                write_count: Arc::new(AtomicUsize::new(0)),
                last_frame_size: Arc::new(AtomicUsize::new(0)),
                shutter_open: Arc::new(AtomicBool::new(false)),
                ready: Arc::new(AtomicBool::new(true)),
            }
        }
    }

    impl DacBackend for FrameSwapTestBackend {
        fn dac_type(&self) -> crate::types::DacType {
            crate::types::DacType::Custom("FrameSwapTest".into())
        }
        fn caps(&self) -> &crate::types::DacCapabilities {
            static CAPS: crate::types::DacCapabilities = crate::types::DacCapabilities {
                pps_min: 1000,
                pps_max: 100000,
                max_points_per_chunk: 4095,
                output_model: crate::types::OutputModel::UsbFrameSwap,
            };
            &CAPS
        }
        fn connect(&mut self) -> DacResult<()> {
            self.connected = true;
            Ok(())
        }
        fn disconnect(&mut self) -> DacResult<()> {
            self.connected = false;
            Ok(())
        }
        fn is_connected(&self) -> bool {
            self.connected
        }
        fn stop(&mut self) -> DacResult<()> {
            Ok(())
        }
        fn set_shutter(&mut self, open: bool) -> DacResult<()> {
            self.shutter_open.store(open, Ordering::SeqCst);
            Ok(())
        }
    }

    impl FrameSwapBackend for FrameSwapTestBackend {
        fn frame_capacity(&self) -> usize {
            4095
        }
        fn is_ready_for_frame(&mut self) -> bool {
            self.ready.load(Ordering::SeqCst)
        }
        fn write_frame(
            &mut self,
            _pps: u32,
            points: &[LaserPoint],
        ) -> DacResult<crate::backend::WriteOutcome> {
            self.write_count.fetch_add(1, Ordering::SeqCst);
            self.last_frame_size
                .store(points.len(), Ordering::SeqCst);
            Ok(crate::backend::WriteOutcome::Written)
        }
    }

    #[test]
    fn test_frame_session_fifo_submit_frame_writes_points() {
        let backend = FifoTestBackend::new();
        let points_written = backend.points_written.clone();
        let backend_kind = BackendKind::Fifo(Box::new(backend));

        let config = FrameSessionConfig::new(30000)
            .with_startup_blank(std::time::Duration::ZERO);
        let session = FrameSession::start(backend_kind, config).unwrap();

        session.control().arm().unwrap();
        session.send_frame(AuthoredFrame::new(vec![
            LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
            LaserPoint::new(1.0, 0.0, 0, 65535, 0, 65535),
        ]));

        // Give the scheduler time to process
        std::thread::sleep(std::time::Duration::from_millis(50));

        assert!(
            points_written.load(Ordering::SeqCst) > 0,
            "Should have written points"
        );

        session.control().stop().unwrap();
        let exit = session.join().unwrap();
        assert_eq!(exit, RunExit::Stopped);
    }

    #[test]
    fn test_frame_session_frame_swap_writes_frames() {
        let backend = FrameSwapTestBackend::new();
        let write_count = backend.write_count.clone();
        let last_frame_size = backend.last_frame_size.clone();
        let backend_kind = BackendKind::FrameSwap(Box::new(backend));

        let config = FrameSessionConfig::new(30000)
            .with_startup_blank(std::time::Duration::ZERO);
        let session = FrameSession::start(backend_kind, config).unwrap();

        session.control().arm().unwrap();
        session.send_frame(AuthoredFrame::new(vec![
            LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
            LaserPoint::new(1.0, 0.0, 0, 65535, 0, 65535),
        ]));

        std::thread::sleep(std::time::Duration::from_millis(50));

        assert!(
            write_count.load(Ordering::SeqCst) > 0,
            "Should have written frames"
        );
        // Frame size = 8 transition + 2 base = 10 (with default transition)
        assert!(
            last_frame_size.load(Ordering::SeqCst) > 0,
            "Frame should have points"
        );

        session.control().stop().unwrap();
        let exit = session.join().unwrap();
        assert_eq!(exit, RunExit::Stopped);
    }

    #[test]
    fn test_frame_session_arm_disarm() {
        let backend = FifoTestBackend::new();
        let shutter = backend.shutter_open.clone();
        let backend_kind = BackendKind::Fifo(Box::new(backend));

        let config = FrameSessionConfig::new(30000);
        let session = FrameSession::start(backend_kind, config).unwrap();

        assert!(!session.control().is_armed());

        session.control().arm().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(shutter.load(Ordering::SeqCst));

        session.control().disarm().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(!shutter.load(Ordering::SeqCst));

        session.control().stop().unwrap();
        session.join().unwrap();
    }

    #[test]
    fn test_frame_session_stop() {
        let backend = FifoTestBackend::new();
        let backend_kind = BackendKind::Fifo(Box::new(backend));

        let config = FrameSessionConfig::new(30000);
        let session = FrameSession::start(backend_kind, config).unwrap();

        session.control().stop().unwrap();
        let exit = session.join().unwrap();
        assert_eq!(exit, RunExit::Stopped);
    }

    #[test]
    fn test_frame_session_color_delay() {
        // Test that color delay shifts RGB relative to XY
        let mut points = vec![
            LaserPoint::new(0.0, 0.0, 100, 200, 300, 400),
            LaserPoint::new(1.0, 0.0, 500, 600, 700, 800),
            LaserPoint::new(2.0, 0.0, 900, 1000, 1100, 1200),
        ];

        FrameSession::apply_color_delay(&mut points, 1);

        // First point should have blanked colors (delay = 1)
        assert_eq!(points[0].r, 0);
        assert_eq!(points[0].intensity, 0);
        // Second point should have first point's original colors
        assert_eq!(points[1].r, 100);
        assert_eq!(points[1].g, 200);
        assert_eq!(points[1].intensity, 400);
        // Third point should have second point's original colors
        assert_eq!(points[2].r, 500);
        assert_eq!(points[2].g, 600);
        // XY should be unchanged
        assert_eq!(points[0].x, 0.0);
        assert_eq!(points[1].x, 1.0);
        assert_eq!(points[2].x, 2.0);
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
