//! FrameSession and FrameSessionConfig — public frame-mode API.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::backend::{BackendKind, WriteOutcome};
use crate::error::{Error, Result};
use crate::reconnect::ReconnectPolicy;
use crate::stream::StreamControl;
use crate::types::{DacInfo, LaserPoint, RunExit};

use super::engine::{ColorDelayLine, PresentationEngine};
use super::{default_transition, Frame, TransitionFn};

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
    /// Reconnection configuration (default: disabled).
    ///
    /// Set via [`with_reconnect`](Self::with_reconnect) to enable automatic
    /// reconnection when the device disconnects.
    pub reconnect: Option<crate::types::ReconnectConfig>,
}

impl FrameSessionConfig {
    const DEFAULT_COLOR_DELAY: std::time::Duration = std::time::Duration::from_micros(150);

    /// Create a new config with the given PPS and default transition.
    pub fn new(pps: u32) -> Self {
        let color_delay_points =
            (Self::DEFAULT_COLOR_DELAY.as_secs_f64() * pps as f64).ceil() as usize;
        Self {
            pps,
            transition_fn: default_transition(pps),
            startup_blank: std::time::Duration::from_millis(1),
            color_delay_points,
            reconnect: None,
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

    /// Enable automatic reconnection (builder pattern).
    ///
    /// Requires the device to have been opened via [`open_device`](crate::open_device).
    pub fn with_reconnect(mut self, config: crate::types::ReconnectConfig) -> Self {
        self.reconnect = Some(config);
        self
    }

    /// Compute the number of startup blank points for this config.
    fn startup_blank_points(&self) -> usize {
        if self.startup_blank.is_zero() {
            0
        } else {
            (self.startup_blank.as_secs_f64() * self.pps as f64).ceil() as usize
        }
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
/// let (session, _info) = device.start_frame_session(config)?;
///
/// session.control().arm()?;
/// session.send_frame(Frame::new(vec![
///     LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
/// ]));
/// ```
pub struct FrameSession {
    control: StreamControl,
    thread: Option<JoinHandle<Result<RunExit>>>,
    frame_slot: Arc<Mutex<Option<Frame>>>,
}

impl FrameSession {
    /// Start a frame session on the given backend.
    pub(crate) fn start(
        mut backend: BackendKind,
        config: FrameSessionConfig,
        reconnect_policy: Option<ReconnectPolicy>,
    ) -> Result<Self> {
        // Connect if needed
        if !backend.is_connected() {
            backend.connect()?;
        }

        let (control_tx, control_rx) = mpsc::channel();
        let control = StreamControl::new(control_tx, std::time::Duration::ZERO);
        let frame_slot: Arc<Mutex<Option<Frame>>> = Arc::new(Mutex::new(None));

        let control_clone = control.clone();
        let is_frame_swap = backend.is_frame_swap();
        let slot_clone = frame_slot.clone();

        let thread = std::thread::spawn(move || {
            if is_frame_swap {
                Self::run_frame_swap_loop(
                    backend,
                    config,
                    control_clone,
                    control_rx,
                    slot_clone,
                    reconnect_policy,
                )
            } else {
                Self::run_fifo_loop(
                    backend,
                    config,
                    control_clone,
                    control_rx,
                    slot_clone,
                    reconnect_policy,
                )
            }
        });

        Ok(Self {
            control,
            thread: Some(thread),
            frame_slot,
        })
    }

    /// Returns a control handle for arm/disarm/stop.
    pub fn control(&self) -> StreamControl {
        self.control.clone()
    }

    /// Submit a frame for display. Latest-wins: overwrites any unconsumed
    /// pending frame immediately, with no buffering or memory growth.
    pub fn send_frame(&self, frame: Frame) {
        *self.frame_slot.lock().unwrap() = Some(frame);
    }

    /// Returns true if the scheduler thread has finished.
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_some_and(|h| h.is_finished())
    }

    /// Wait for the session thread to finish and return the exit reason.
    pub fn join(mut self) -> Result<RunExit> {
        if let Some(handle) = self.thread.take() {
            handle
                .join()
                .unwrap_or(Err(Error::disconnected("thread panicked")))
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
        frame_slot: Arc<Mutex<Option<Frame>>>,
        reconnect_policy: Option<ReconnectPolicy>,
    ) -> Result<RunExit> {
        let is_udp_timed = backend.caps().output_model == crate::types::OutputModel::UdpTimed;

        if is_udp_timed {
            Self::run_udp_timed_loop(
                backend,
                config,
                control,
                control_rx,
                frame_slot,
                reconnect_policy,
            )
        } else {
            Self::run_fifo_estimation_loop(
                backend,
                config,
                control,
                control_rx,
                frame_slot,
                reconnect_policy,
            )
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
        frame_slot: Arc<Mutex<Option<Frame>>>,
        reconnect_policy: Option<ReconnectPolicy>,
    ) -> Result<RunExit> {
        use std::time::{Duration, Instant};

        let pps = config.pps;
        let pps_f64 = pps as f64;
        let startup_blank_points = config.startup_blank_points();
        // Fixed chunk: ~10ms worth of points (small enough to avoid overflow,
        // large enough for efficient packets). At 30kpps = 300 points = ~2 packets.
        let mut chunk_points =
            ((pps_f64 * 0.010).ceil() as usize).min(backend.caps().max_points_per_chunk);
        let mut chunk_duration = Duration::from_secs_f64(chunk_points as f64 / pps_f64);

        let mut engine = PresentationEngine::new(config.transition_fn);
        let mut chunk_buffer = vec![LaserPoint::default(); chunk_points];
        let mut color_delay = ColorDelayLine::new(config.color_delay_points);
        let mut shutter_open = false;
        let mut startup_blank_remaining: usize = 0;
        let mut last_armed = false;
        let mut next_send = Instant::now();
        let mut last_frame: Option<Frame> = None;
        let mut retry_points: Option<usize> = None;

        loop {
            // 1. High-precision wait until next send time
            Self::sleep_until_precise(
                &control,
                &control_rx,
                next_send,
                &mut shutter_open,
                &mut backend,
            )?;
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

            // 3. Check for new frame (latest-wins slot)
            if let Some(frame) = frame_slot.lock().unwrap().take() {
                last_frame = Some(frame.clone());
                engine.set_pending(frame);
            }

            // 4. Check connection
            if !backend.is_connected() {
                if let Some(ref policy) = reconnect_policy {
                    match Self::try_reconnect(
                        &mut backend,
                        policy,
                        &control,
                        &mut shutter_open,
                        &mut last_armed,
                        &mut engine,
                        &last_frame,
                        pps,
                        false,
                    ) {
                        Ok(_info) => {
                            next_send = Instant::now();
                            chunk_points = ((pps_f64 * 0.010).ceil() as usize)
                                .min(backend.caps().max_points_per_chunk);
                            chunk_duration = Duration::from_secs_f64(chunk_points as f64 / pps_f64);
                            chunk_buffer.resize(chunk_points, LaserPoint::default());
                            color_delay.reset();
                            retry_points = None;
                            continue;
                        }
                        Err(exit) => return Ok(exit),
                    }
                }
                return Ok(RunExit::Disconnected);
            }

            // 5. Process control messages
            if Self::process_control_messages(&control_rx, &mut shutter_open, &mut backend) {
                return Ok(RunExit::Stopped);
            }

            // 6. Handle arm/disarm transitions
            let is_armed = control.is_armed();
            Self::handle_shutter_transition(
                is_armed,
                &mut last_armed,
                &mut shutter_open,
                &mut startup_blank_remaining,
                startup_blank_points,
                &mut backend,
            );

            // 7. Fill chunk from engine
            let n = if let Some(n) = retry_points {
                n
            } else {
                let n = engine.fill_chunk(&mut chunk_buffer, chunk_points);
                if n == 0 {
                    continue;
                }

                // Apply blanking modifications
                Self::apply_blanking(
                    is_armed,
                    &mut startup_blank_remaining,
                    &mut chunk_buffer[..n],
                );

                // Apply color delay (stateful — carries across chunks)
                color_delay.apply(&mut chunk_buffer[..n]);
                retry_points = Some(n);
                n
            };

            // 8. Write. On backpressure, retry the exact same transmit
            // buffer next cycle instead of advancing frame state.
            match backend.try_write(pps, &chunk_buffer[..n]) {
                Ok(WriteOutcome::Written) => {
                    retry_points = None;
                }
                Ok(WriteOutcome::WouldBlock) => {
                    // Device busy — keep retry_points set and retry next cycle
                }
                Err(e) if e.is_disconnected() => {
                    if let Some(ref policy) = reconnect_policy {
                        match Self::try_reconnect(
                            &mut backend,
                            policy,
                            &control,
                            &mut shutter_open,
                            &mut last_armed,
                            &mut engine,
                            &last_frame,
                            pps,
                            false,
                        ) {
                            Ok(_info) => {
                                next_send = Instant::now();
                                chunk_points = ((pps_f64 * 0.010).ceil() as usize)
                                    .min(backend.caps().max_points_per_chunk);
                                chunk_duration =
                                    Duration::from_secs_f64(chunk_points as f64 / pps_f64);
                                chunk_buffer.resize(chunk_points, LaserPoint::default());
                                color_delay.reset();
                                retry_points = None;
                                continue;
                            }
                            Err(exit) => return Ok(exit),
                        }
                    }
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
        frame_slot: Arc<Mutex<Option<Frame>>>,
        reconnect_policy: Option<ReconnectPolicy>,
    ) -> Result<RunExit> {
        use std::time::{Duration, Instant};

        let pps = config.pps as f64;
        let startup_blank_points = config.startup_blank_points();
        let mut max_points = backend.caps().max_points_per_chunk;
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
        let mut last_armed = false;
        let mut last_frame: Option<Frame> = None;

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

            // 2. Check for new frame (latest-wins slot)
            if let Some(frame) = frame_slot.lock().unwrap().take() {
                last_frame = Some(frame.clone());
                engine.set_pending(frame);
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
                Self::sleep_with_control_check(
                    &control,
                    &control_rx,
                    sleep_time,
                    &mut shutter_open,
                    &mut backend,
                )?;
                continue;
            }

            // 5. Check connection
            if !backend.is_connected() {
                if let Some(ref policy) = reconnect_policy {
                    match Self::try_reconnect(
                        &mut backend,
                        policy,
                        &control,
                        &mut shutter_open,
                        &mut last_armed,
                        &mut engine,
                        &last_frame,
                        config.pps,
                        false,
                    ) {
                        Ok(info) => {
                            scheduled_ahead = 0;
                            fractional_consumed = 0.0;
                            last_iteration = Instant::now();
                            max_points = info.caps.max_points_per_chunk;
                            chunk_buffer.resize(max_points, LaserPoint::default());
                            color_delay.reset();
                            continue;
                        }
                        Err(exit) => return Ok(exit),
                    }
                }
                return Ok(RunExit::Disconnected);
            }

            // 6. Process control messages
            if Self::process_control_messages(&control_rx, &mut shutter_open, &mut backend) {
                return Ok(RunExit::Stopped);
            }

            // 7. Handle arm/disarm transitions
            let is_armed = control.is_armed();
            Self::handle_shutter_transition(
                is_armed,
                &mut last_armed,
                &mut shutter_open,
                &mut startup_blank_remaining,
                startup_blank_points,
                &mut backend,
            );

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

            // Apply blanking modifications
            Self::apply_blanking(
                is_armed,
                &mut startup_blank_remaining,
                &mut chunk_buffer[..n],
            );

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
                        if let Some(ref policy) = reconnect_policy {
                            match Self::try_reconnect(
                                &mut backend,
                                policy,
                                &control,
                                &mut shutter_open,
                                &mut last_armed,
                                &mut engine,
                                &last_frame,
                                config.pps,
                                false,
                            ) {
                                Ok(info) => {
                                    scheduled_ahead = 0;
                                    fractional_consumed = 0.0;
                                    last_iteration = Instant::now();
                                    max_points = info.caps.max_points_per_chunk;
                                    chunk_buffer.resize(max_points, LaserPoint::default());
                                    color_delay.reset();
                                    break;
                                }
                                Err(exit) => return Ok(exit),
                            }
                        }
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
        frame_slot: Arc<Mutex<Option<Frame>>>,
        reconnect_policy: Option<ReconnectPolicy>,
    ) -> Result<RunExit> {
        use std::time::Duration;

        let startup_blank_points = config.startup_blank_points();
        let mut engine = PresentationEngine::new(config.transition_fn);
        engine.set_frame_capacity(backend.frame_capacity());
        let mut shutter_open = false;
        let mut startup_blank_remaining: usize = 0;
        let mut last_armed = false;
        let mut last_frame: Option<Frame> = None;
        let mut frame_buf: Vec<LaserPoint> = Vec::new();
        let mut color_delay = ColorDelayLine::new(config.color_delay_points);
        let mut write_pending = false;

        loop {
            // 1. Check stop
            if control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            // 2. Check for new frame (latest-wins slot)
            if let Some(frame) = frame_slot.lock().unwrap().take() {
                last_frame = Some(frame.clone());
                engine.set_pending(frame);
            }

            // 3. Check connection
            if !backend.is_connected() {
                if let Some(ref policy) = reconnect_policy {
                    match Self::try_reconnect(
                        &mut backend,
                        policy,
                        &control,
                        &mut shutter_open,
                        &mut last_armed,
                        &mut engine,
                        &last_frame,
                        config.pps,
                        true,
                    ) {
                        Ok(_info) => {
                            engine.set_frame_capacity(backend.frame_capacity());
                            color_delay.reset();
                            write_pending = false;
                            continue;
                        }
                        Err(exit) => return Ok(exit),
                    }
                }
                return Ok(RunExit::Disconnected);
            }

            // 4. Process control messages
            if Self::process_control_messages(&control_rx, &mut shutter_open, &mut backend) {
                return Ok(RunExit::Stopped);
            }

            // 5. Handle arm/disarm transitions
            let is_armed = control.is_armed();
            Self::handle_shutter_transition(
                is_armed,
                &mut last_armed,
                &mut shutter_open,
                &mut startup_blank_remaining,
                startup_blank_points,
                &mut backend,
            );

            // 6. Check if device is ready
            if !write_pending && !backend.is_ready_for_frame() {
                std::thread::sleep(Duration::from_millis(1));
                continue;
            }

            // 7. Compose frame and copy to reusable buffer
            if !write_pending {
                let composed = engine.compose_hardware_frame();
                if composed.is_empty() {
                    std::thread::sleep(Duration::from_millis(1));
                    continue;
                }
                frame_buf.clear();
                frame_buf.extend_from_slice(composed);

                // Apply blanking modifications
                Self::apply_blanking(is_armed, &mut startup_blank_remaining, &mut frame_buf);

                color_delay.apply(&mut frame_buf);
            }

            // 8. Write frame
            match backend.try_write(config.pps, &frame_buf) {
                Ok(WriteOutcome::Written) => {
                    write_pending = false;
                }
                Ok(WriteOutcome::WouldBlock) => {
                    write_pending = true;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(e) if e.is_disconnected() => {
                    if let Some(ref policy) = reconnect_policy {
                        match Self::try_reconnect(
                            &mut backend,
                            policy,
                            &control,
                            &mut shutter_open,
                            &mut last_armed,
                            &mut engine,
                            &last_frame,
                            config.pps,
                            true,
                        ) {
                            Ok(_info) => {
                                engine.set_frame_capacity(backend.frame_capacity());
                                color_delay.reset();
                                write_pending = false;
                                continue;
                            }
                            Err(exit) => return Ok(exit),
                        }
                    }
                    return Ok(RunExit::Disconnected);
                }
                Err(_) => {}
            }
        }
    }

    // =========================================================================
    // Shared helpers
    // =========================================================================

    /// Attempt to reconnect the backend using the reconnection policy.
    ///
    /// On success, replaces `backend` and resets scheduler state.
    /// Returns the new `DacInfo` on success.
    #[allow(clippy::too_many_arguments)]
    fn try_reconnect(
        backend: &mut BackendKind,
        policy: &ReconnectPolicy,
        control: &StreamControl,
        shutter_open: &mut bool,
        last_armed: &mut bool,
        engine: &mut PresentationEngine,
        last_frame: &Option<Frame>,
        config_pps: u32,
        expected_frame_swap: bool,
    ) -> std::result::Result<DacInfo, RunExit> {
        // Fire on_disconnect
        if let Some(cb) = policy.on_disconnect.lock().unwrap().as_mut() {
            cb(&Error::disconnected("backend disconnected"));
        }

        let mut retries = 0u32;
        loop {
            if control.is_stop_requested() {
                return Err(RunExit::Stopped);
            }
            if let Some(max) = policy.max_retries {
                if retries >= max {
                    return Err(RunExit::Disconnected);
                }
            }

            // Backoff
            if ReconnectPolicy::sleep_with_stop(policy.backoff, || control.is_stop_requested()) {
                return Err(RunExit::Stopped);
            }

            log::info!(
                "'{}' reconnect attempt {} ...",
                policy.target.device_id,
                retries + 1
            );

            let device = match policy.target.open_device() {
                Ok(d) => d,
                Err(err) => {
                    if !ReconnectPolicy::is_retriable(&err) {
                        return Err(RunExit::Disconnected);
                    }
                    log::warn!("'{}' open_device failed: {}", policy.target.device_id, err);
                    retries = retries.saturating_add(1);
                    continue;
                }
            };

            let info = device.info().clone();
            let mut new_backend = match device.into_backend() {
                Some(b) => b,
                None => {
                    retries = retries.saturating_add(1);
                    continue;
                }
            };

            // Validate PPS against new device
            if config_pps < info.caps.pps_min || config_pps > info.caps.pps_max {
                log::error!(
                    "'{}' PPS {} outside new device range [{}, {}]",
                    policy.target.device_id,
                    config_pps,
                    info.caps.pps_min,
                    info.caps.pps_max
                );
                return Err(RunExit::Disconnected);
            }

            // Verify backend type matches the scheduler loop
            if new_backend.is_frame_swap() != expected_frame_swap {
                log::error!(
                    "'{}' reconnected device has incompatible backend type",
                    policy.target.device_id
                );
                return Err(RunExit::Disconnected);
            }

            if !new_backend.is_connected() {
                if let Err(err) = new_backend.connect() {
                    if !ReconnectPolicy::is_retriable(&err) {
                        return Err(RunExit::Disconnected);
                    }
                    log::warn!("'{}' connect failed: {}", policy.target.device_id, err);
                    retries = retries.saturating_add(1);
                    continue;
                }
            }

            log::info!("'{}' reconnected successfully", policy.target.device_id);

            // Swap backend
            *backend = new_backend;
            *shutter_open = false;
            *last_armed = false;

            // Reset engine and replay last frame
            engine.reset();
            if let Some(frame) = last_frame {
                engine.set_pending(frame.clone());
            }

            // Fire on_reconnect
            if let Some(cb) = policy.on_reconnect.lock().unwrap().as_mut() {
                cb(&info);
            }

            return Ok(info);
        }
    }

    /// Handle arm/disarm shutter transitions.
    fn handle_shutter_transition(
        is_armed: bool,
        last_armed: &mut bool,
        shutter_open: &mut bool,
        startup_blank_remaining: &mut usize,
        startup_blank_points: usize,
        backend: &mut BackendKind,
    ) {
        if !*last_armed && is_armed {
            *startup_blank_remaining = startup_blank_points;
            if !*shutter_open {
                let _ = backend.set_shutter(true);
                *shutter_open = true;
            }
        } else if *last_armed && !is_armed && *shutter_open {
            let _ = backend.set_shutter(false);
            *shutter_open = false;
        }
        *last_armed = is_armed;
    }

    /// Apply disarm blanking and startup blanking to a buffer.
    fn apply_blanking(
        is_armed: bool,
        startup_blank_remaining: &mut usize,
        buffer: &mut [LaserPoint],
    ) {
        if !is_armed {
            for p in buffer.iter_mut() {
                *p = LaserPoint::blanked(p.x, p.y);
            }
        } else if *startup_blank_remaining > 0 {
            let blank_count = buffer.len().min(*startup_blank_remaining);
            for p in &mut buffer[..blank_count] {
                p.r = 0;
                p.g = 0;
                p.b = 0;
                p.intensity = 0;
            }
            *startup_blank_remaining -= blank_count;
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
                let slice = remaining
                    .saturating_sub(BUSY_WAIT_THRESHOLD)
                    .min(SLEEP_SLICE);
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
