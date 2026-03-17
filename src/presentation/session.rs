//! FrameSession and FrameSessionConfig — public frame-mode API.

use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::backend::{BackendKind, WriteOutcome};
use crate::error::{Error, Result};
use crate::stream::StreamControl;
use crate::types::{LaserPoint, RunExit};

use super::engine::{ColorDelayLine, PresentationEngine};
use super::{Frame, TransitionFn, default_transition};

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
    /// Delays color channels relative to XY coordinates, compensating for
    /// the difference in galvo mirror and laser modulation response times.
    /// Applied at composition time. Set to 0 to disable.
    pub color_delay_points: usize,
}

impl FrameSessionConfig {
    const DEFAULT_COLOR_DELAY: std::time::Duration = std::time::Duration::from_micros(150);

    /// Create a new config with the given PPS and default transition.
    pub fn new(pps: u32) -> Self {
        let color_delay_points =
            (Self::DEFAULT_COLOR_DELAY.as_secs_f64() * pps as f64).ceil() as usize;
        Self {
            pps,
            transition_fn: Box::new(default_transition),
            startup_blank: std::time::Duration::from_millis(1),
            color_delay_points,
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

/// A frame-mode streaming session.
///
/// Owns a scheduler thread that reads frames from a channel and writes them
/// to the DAC backend using the appropriate strategy (FIFO or frame-swap).
///
/// # Example
///
/// ```ignore
/// use laser_dac::{open_device, FrameSessionConfig, Frame, LaserPoint};
///
/// let device = open_device("my-device")?;
/// let config = FrameSessionConfig::new(30_000);
/// let session = device.start_frame_session(config)?;
///
/// session.control().arm()?;
/// session.send_frame(Frame::new(vec![
///     LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
/// ]));
/// ```
pub struct FrameSession {
    control: StreamControl,
    thread: Option<JoinHandle<Result<RunExit>>>,
    frame_tx: mpsc::Sender<Frame>,
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
        let (frame_tx, frame_rx) = mpsc::channel::<Frame>();

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
    pub fn send_frame(&self, frame: Frame) {
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
        backend: BackendKind,
        config: FrameSessionConfig,
        control: StreamControl,
        control_rx: mpsc::Receiver<crate::stream::ControlMsg>,
        frame_rx: mpsc::Receiver<Frame>,
    ) -> Result<RunExit> {
        let is_udp_timed = backend.caps().output_model == crate::types::OutputModel::UdpTimed;

        if is_udp_timed {
            Self::run_udp_timed_loop(backend, config, control, control_rx, frame_rx)
        } else {
            Self::run_fifo_estimation_loop(backend, config, control, control_rx, frame_rx)
        }
    }

    /// Fixed-rate loop for UdpTimed backends (LaserCube WiFi).
    ///
    /// Sends a fixed chunk every cycle at precise intervals. No buffer
    /// estimation — just metronomic pacing. This avoids all the overflow
    /// and estimation issues that plague UDP-based laser DACs.
    fn run_udp_timed_loop(
        mut backend: BackendKind,
        config: FrameSessionConfig,
        control: StreamControl,
        control_rx: mpsc::Receiver<crate::stream::ControlMsg>,
        frame_rx: mpsc::Receiver<Frame>,
    ) -> Result<RunExit> {
        use std::time::{Duration, Instant};

        let pps = config.pps;
        let pps_f64 = pps as f64;
        // Fixed chunk: ~10ms worth of points (small enough to avoid overflow,
        // large enough for efficient packets). At 30kpps = 300 points = ~2 packets.
        let chunk_points = ((pps_f64 * 0.010).ceil() as usize).min(backend.caps().max_points_per_chunk);
        let chunk_duration = Duration::from_secs_f64(chunk_points as f64 / pps_f64);

        let mut engine = PresentationEngine::new(config.transition_fn);
        let mut chunk_buffer = vec![LaserPoint::default(); chunk_points];
        let mut color_delay = ColorDelayLine::new(config.color_delay_points);
        let mut shutter_open = false;
        let mut startup_blank_remaining: usize = 0;
        let startup_blank_points = if config.startup_blank.is_zero() {
            0
        } else {
            (config.startup_blank.as_secs_f64() * pps_f64).ceil() as usize
        };
        let mut last_armed = false;
        let mut next_send = Instant::now();

        loop {
            // 1. High-precision wait until next send time
            Self::sleep_until_precise(&control, &control_rx, next_send, &mut shutter_open, &mut backend)?;
            next_send += chunk_duration;

            // Catch up if we fell behind (don't accumulate debt)
            let now = Instant::now();
            if next_send < now {
                next_send = now;
            }

            // 2. Check stop
            if control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            // 3. Drain frame channel
            while let Ok(frame) = frame_rx.try_recv() {
                engine.set_pending(Arc::new(frame));
            }

            // 4. Check connection
            if !backend.is_connected() {
                return Ok(RunExit::Disconnected);
            }

            // 5. Process control messages
            if Self::process_control_messages(&control_rx, &mut shutter_open, &mut backend) {
                return Ok(RunExit::Stopped);
            }

            // 6. Handle arm/disarm transitions
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

            // 7. Fill chunk from engine
            let n = engine.fill_chunk(&mut chunk_buffer, chunk_points);
            if n == 0 {
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

            // Apply color delay (stateful — carries across chunks)
            color_delay.apply(&mut chunk_buffer[..n]);

            // 8. Write (no retry — if device is busy, skip this cycle
            // and catch up next time. Retrying stalls the fixed-rate loop.)
            match backend.try_write(pps, &chunk_buffer[..n]) {
                Ok(WriteOutcome::Written) => {}
                Ok(WriteOutcome::WouldBlock) => {
                    // Device busy — we'll catch up next cycle
                }
                Err(e) if e.is_disconnected() => {
                    return Ok(RunExit::Disconnected);
                }
                Err(_) => {}
            }
        }
    }

    /// Buffer-estimation loop for standard FIFO backends (Ether Dream, IDN, etc).
    fn run_fifo_estimation_loop(
        mut backend: BackendKind,
        config: FrameSessionConfig,
        control: StreamControl,
        control_rx: mpsc::Receiver<crate::stream::ControlMsg>,
        frame_rx: mpsc::Receiver<Frame>,
    ) -> Result<RunExit> {
        use std::time::{Duration, Instant};

        let pps = config.pps as f64;
        let max_points = backend.caps().max_points_per_chunk;
        let target_buffer_secs = 0.020_f64;
        let target_buffer_points = (target_buffer_secs * pps) as u64;

        let mut engine = PresentationEngine::new(config.transition_fn);
        let mut scheduled_ahead: u64 = 0;
        let mut fractional_consumed: f64 = 0.0;
        let mut last_iteration = Instant::now();
        let mut chunk_buffer = vec![LaserPoint::default(); max_points];
        let mut color_delay = ColorDelayLine::new(config.color_delay_points);
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

            // Apply color delay (stateful — carries across chunks)
            color_delay.apply(&mut chunk_buffer[..n]);

            // 9. Write to backend with retry on WouldBlock
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
                        break;
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
        frame_rx: mpsc::Receiver<Frame>,
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

            // 8. Compose frame and copy to mutable buffer
            let composed = engine.compose_hardware_frame();
            if composed.is_empty() {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }
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

            // Note: no color delay for frame-swap. The frame loops on hardware,
            // so a per-frame delay would create artifacts at the loop point.
            // Frame-swap DACs (Helios) handle modulation timing internally.

            // 9. Write frame
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

    /// High-precision sleep for UdpTimed backends.
    ///
    /// Uses coarse sleeps first, then busy-waits near the deadline to
    /// minimize wake-up jitter. Matches `Stream::sleep_until_with_control_check`.
    fn sleep_until_precise(
        control: &StreamControl,
        control_rx: &mpsc::Receiver<crate::stream::ControlMsg>,
        deadline: std::time::Instant,
        shutter_open: &mut bool,
        backend: &mut BackendKind,
    ) -> Result<()> {
        const BUSY_WAIT_THRESHOLD: std::time::Duration = std::time::Duration::from_micros(500);
        const SLEEP_SLICE: std::time::Duration = std::time::Duration::from_millis(1);

        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(());
            }

            let remaining = deadline.duration_since(now);
            if remaining > BUSY_WAIT_THRESHOLD {
                let slice = remaining.saturating_sub(BUSY_WAIT_THRESHOLD).min(SLEEP_SLICE);
                std::thread::sleep(slice);
            } else {
                std::thread::yield_now();
            }

            if control.is_stop_requested() {
                return Err(Error::Stopped);
            }
            Self::process_control_messages(control_rx, shutter_open, backend);
        }
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
