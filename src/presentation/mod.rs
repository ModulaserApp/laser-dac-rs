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
    /// Current frame's points (no transition — just the raw frame).
    drawable: Vec<LaserPoint>,
    /// Whether the drawable needs to be rebuilt from current_base.
    drawable_dirty: bool,
    /// Current read cursor within `drawable`.
    cursor: usize,
    /// Transition function for generating blanking between frames.
    transition_fn: TransitionFn,
    /// Buffer for transition points injected between frames.
    transition_buf: Vec<LaserPoint>,
    /// Read cursor within transition_buf.
    transition_cursor: usize,
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
            transition_buf: Vec::new(),
            transition_cursor: 0,
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

    /// FIFO delivery: fill `buffer[..max_points]` from the current frame.
    ///
    /// Traverses the current frame cyclically. On self-loop, the cursor
    /// wraps without inserting transition points. When a pending frame is
    /// promoted, transition points are injected between the old frame's
    /// last point and the new frame's first point.
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
            // Drain any pending transition points first
            if self.transition_cursor < self.transition_buf.len() {
                buffer[written] = self.transition_buf[self.transition_cursor];
                self.transition_cursor += 1;
                written += 1;
                continue;
            }

            // Output current frame point
            buffer[written] = self.drawable[self.cursor];
            written += 1;
            self.cursor += 1;

            // Check if we've completed the frame
            if self.cursor >= self.drawable.len() {
                if let Some(pending) = self.pending_base.take() {
                    // Frame change: inject transition, then promote
                    let last = self.drawable.last().unwrap();
                    let new_frame = pending;
                    if let Some(first) = new_frame.first_point() {
                        self.transition_buf = (self.transition_fn)(last, first);
                        self.transition_cursor = 0;
                    }
                    self.current_base = Some(new_frame);
                    self.refresh_drawable();
                    self.cursor = 0;
                } else {
                    // Self-loop: just wrap the cursor, no transition
                    self.cursor = 0;
                }
            }
        }

        written
    }

    /// Frame-swap delivery: compose and return a complete hardware frame.
    ///
    /// Returns the frame points for the current state. If a pending frame
    /// exists, it is promoted first. For frame-swap DACs, the self-loop
    /// transition (last→first of same frame) is prepended so the device
    /// receives one atomic frame with clean blanking at the loop point.
    pub fn compose_hardware_frame(&mut self) -> &[LaserPoint] {
        // Promote pending if available
        if let Some(pending) = self.pending_base.take() {
            self.current_base = Some(pending);
            self.drawable_dirty = true;
        }

        if self.drawable_dirty {
            self.refresh_drawable_for_frame_swap();
        }

        &self.drawable
    }

    /// Rebuild drawable for frame-swap: includes self-loop transition.
    ///
    /// Frame-swap DACs send the entire drawable as one atomic frame, so the
    /// transition from last→first point must be included for clean looping.
    fn refresh_drawable_for_frame_swap(&mut self) {
        self.drawable.clear();
        self.drawable_dirty = false;

        let Some(current) = &self.current_base else {
            return;
        };

        if current.is_empty() {
            return;
        }

        let points = current.points();
        let last = points.last().unwrap();
        let first = points.first().unwrap();
        let transition = (self.transition_fn)(last, first);
        self.drawable.extend_from_slice(&transition);
        self.drawable.extend_from_slice(points);
    }

    /// Rebuild the drawable from the current base frame.
    fn refresh_drawable(&mut self) {
        self.drawable.clear();
        self.drawable_dirty = false;

        let Some(current) = &self.current_base else {
            return;
        };

        self.drawable.extend_from_slice(current.points());
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

            // 9. Write to backend with retry on WouldBlock
            //
            // The engine cursor has already advanced past these points.
            // If we don't retry, they're lost and the output glitches.
            loop {
                match backend.try_write(config.pps, &chunk_buffer[..n]) {
                    Ok(WriteOutcome::Written) => {
                        scheduled_ahead += n as u64;
                        break;
                    }
                    Ok(WriteOutcome::WouldBlock) => {
                        std::thread::yield_now();
                        if control.is_stop_requested() {
                            return Ok(RunExit::Stopped);
                        }
                        std::thread::sleep(Duration::from_micros(100));
                    }
                    Err(e) if e.is_disconnected() => {
                        return Ok(RunExit::Disconnected);
                    }
                    Err(_) => {
                        break; // Backend error — drop chunk, continue
                    }
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
mod tests;
