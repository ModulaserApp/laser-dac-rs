//! Stream and Dac types for point output.
//!
//! This module provides the `Stream` type for streaming point chunks to a DAC,
//! `StreamControl` for out-of-band control (arm/disarm/stop), and `Dac` for
//! connected devices that can start streaming sessions.
//!
//! # Armed/Disarmed Model
//!
//! Streams use a binary armed/disarmed safety model:
//!
//! - **Armed**: Content passes through to device, shutter opened (best-effort).
//! - **Disarmed** (default): Shutter closed, intensity/RGB forced to 0.
//!
//! All streams start disarmed. Call `arm()` to enable output.
//! `arm()` and `disarm()` are the only safety controls — there is no separate
//! shutter API. This keeps the mental model simple: armed = laser may emit.
//!
//! # Hardware Shutter Support
//!
//! Shutter control is best-effort and varies by backend:
//! - **LaserCube USB/WiFi**: Actual hardware control
//! - **Helios**: Hardware shutter control via USB interrupt
//! - **Ether Dream, IDN**: No-op (safety relies on software blanking)
//!
//! # Disconnect Behavior
//!
//! By default, streams do not reconnect — on disconnect, `run()` returns
//! `RunExit::Disconnected`. Configure reconnection via
//! [`StreamConfig::with_reconnect`](crate::StreamConfig::with_reconnect) or
//! [`FrameSessionConfig::with_reconnect`](crate::FrameSessionConfig::with_reconnect).
//! New streams always start disarmed for safety.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{BackendKind, Error, Result, WriteOutcome};
use crate::discovery::DacDiscovery;
use crate::reconnect::{reconnect_backend_with_retry, ReconnectPolicy, ReconnectTarget};
use crate::scheduler;
use crate::types::{
    ChunkRequest, ChunkResult, DacCapabilities, DacInfo, DacType, LaserPoint, OutputModel, RunExit,
    StreamConfig, StreamInstant, StreamStats, StreamStatus, UnderrunPolicy,
};

// =============================================================================
// Stream Control
// =============================================================================

/// Control messages sent from StreamControl to Stream.
///
/// These messages allow out-of-band control actions to take effect immediately,
/// even when the stream is waiting (pacing, backpressure, etc.).
#[derive(Debug, Clone, Copy)]
pub(crate) enum ControlMsg {
    /// Arm the output (opens hardware shutter).
    Arm,
    /// Disarm the output (closes hardware shutter).
    Disarm,
    /// Request the stream to stop.
    Stop,
}

/// Thread-safe control handle for safety-critical actions.
///
/// This allows out-of-band control of the stream (arm/disarm/stop) from
/// a different thread, e.g., for E-stop functionality.
///
/// Control actions take effect as soon as possible - the stream processes
/// control messages at every opportunity (during waits, between retries, etc.).
#[derive(Clone)]
pub struct StreamControl {
    inner: Arc<StreamControlInner>,
}

struct StreamControlInner {
    /// Whether output is armed (laser can fire).
    armed: AtomicBool,
    /// Whether a stop has been requested.
    stop_requested: AtomicBool,
    /// Channel for sending control messages to the stream loop.
    /// Wrapped in Mutex because Sender is Send but not Sync.
    control_tx: Mutex<Sender<ControlMsg>>,
    /// Color delay in microseconds (readable per-chunk without locking).
    color_delay_micros: AtomicU64,
}

impl StreamControl {
    pub(crate) fn new(control_tx: Sender<ControlMsg>, color_delay: Duration) -> Self {
        Self {
            inner: Arc::new(StreamControlInner {
                armed: AtomicBool::new(false),
                stop_requested: AtomicBool::new(false),
                control_tx: Mutex::new(control_tx),
                color_delay_micros: AtomicU64::new(color_delay.as_micros() as u64),
            }),
        }
    }

    /// Arm the output (allow laser to fire).
    ///
    /// When armed, content from the producer passes through unmodified
    /// and the hardware shutter is opened (best-effort).
    pub fn arm(&self) -> Result<()> {
        self.inner.armed.store(true, Ordering::SeqCst);
        // Send message to stream for immediate shutter control
        if let Ok(tx) = self.inner.control_tx.lock() {
            let _ = tx.send(ControlMsg::Arm);
        }
        Ok(())
    }

    /// Disarm the output (force laser off). Designed for E-stop use.
    ///
    /// Immediately sets an atomic flag (works even if stream loop is blocked),
    /// then sends a message to close the hardware shutter. All future points
    /// are blanked in software. The stream stays alive outputting blanks -
    /// use `stop()` to terminate entirely.
    ///
    /// **Latency**: Points already in the device buffer will still play out.
    /// `target_buffer` bounds this latency.
    ///
    /// **Hardware shutter**: Best-effort. LaserCube and Helios have actual hardware
    /// control; Ether Dream, IDN are no-ops (safety relies on software blanking).
    pub fn disarm(&self) -> Result<()> {
        self.inner.armed.store(false, Ordering::SeqCst);
        // Send message to stream for immediate shutter control
        if let Ok(tx) = self.inner.control_tx.lock() {
            let _ = tx.send(ControlMsg::Disarm);
        }
        Ok(())
    }

    /// Check if the output is armed.
    pub fn is_armed(&self) -> bool {
        self.inner.armed.load(Ordering::SeqCst)
    }

    /// Set the color delay for scanner sync compensation.
    ///
    /// Takes effect within one chunk period. The delay is quantized to
    /// whole points: `ceil(delay * pps)`.
    pub fn set_color_delay(&self, delay: Duration) {
        self.inner
            .color_delay_micros
            .store(delay.as_micros() as u64, Ordering::SeqCst);
    }

    /// Get the current color delay.
    pub fn color_delay(&self) -> Duration {
        Duration::from_micros(self.inner.color_delay_micros.load(Ordering::SeqCst))
    }

    /// Request the stream to stop.
    ///
    /// Signals termination; `run()` returns `RunExit::Stopped`.
    /// For clean shutdown with shutter close, prefer `Stream::stop()`.
    pub fn stop(&self) -> Result<()> {
        self.inner.stop_requested.store(true, Ordering::SeqCst);
        // Send message to stream for immediate stop
        if let Ok(tx) = self.inner.control_tx.lock() {
            let _ = tx.send(ControlMsg::Stop);
        }
        Ok(())
    }

    /// Check if a stop has been requested.
    pub fn is_stop_requested(&self) -> bool {
        self.inner.stop_requested.load(Ordering::SeqCst)
    }
}

// =============================================================================
// Stream State
// =============================================================================

struct StreamState {
    /// Current position in stream time (points since start).
    current_instant: StreamInstant,
    /// Points scheduled ahead of current_instant.
    scheduled_ahead: u64,
    /// Fractional points consumed but not yet subtracted from scheduled_ahead.
    /// Accumulates sub-point remainders to prevent stalls when per-iteration
    /// elapsed time is too small to yield a whole point (the u64 truncation bug).
    fractional_consumed: f64,

    // Pre-allocated buffers (no per-chunk allocation in hot path)
    /// Buffer for callback to fill points into.
    chunk_buffer: Vec<LaserPoint>,
    /// Last chunk for RepeatLast underrun policy.
    last_chunk: Vec<LaserPoint>,
    /// Number of valid points in last_chunk.
    last_chunk_len: usize,

    /// FIFO for color delay (r, g, b, intensity per point).
    color_delay_line: VecDeque<(u16, u16, u16, u16)>,

    /// Points remaining in the startup blank window (decremented as points are written).
    startup_blank_remaining: usize,
    /// Total startup blank points (computed once from config).
    startup_blank_points: usize,

    // Cached config-derived values (avoid recomputing per-iteration float math)
    /// target_buffer as seconds (cached from config).
    target_buffer_secs: f64,
    /// min_buffer as seconds (cached from config).
    min_buffer_secs: f64,
    /// target_buffer as points (cached from config + pps).
    target_buffer_points: u64,

    /// Statistics.
    stats: StreamStats,
    /// Track the last armed state to detect transitions.
    last_armed: bool,
    /// Whether the hardware shutter is currently open.
    shutter_open: bool,
}

impl StreamState {
    /// Create new stream state with pre-allocated buffers.
    ///
    /// Buffers are sized to `max_points_per_chunk` from DAC capabilities,
    /// ensuring we can handle any catch-up scenario without reallocation.
    fn new(
        max_points_per_chunk: usize,
        startup_blank_points: usize,
        config: &StreamConfig,
    ) -> Self {
        let pps = config.pps as f64;
        let target_buffer_secs = config.target_buffer.as_secs_f64();
        let min_buffer_secs = config.min_buffer.as_secs_f64();
        Self {
            current_instant: StreamInstant::new(0),
            scheduled_ahead: 0,
            fractional_consumed: 0.0,
            chunk_buffer: vec![LaserPoint::default(); max_points_per_chunk],
            last_chunk: vec![LaserPoint::default(); max_points_per_chunk],
            last_chunk_len: 0,
            color_delay_line: VecDeque::new(),
            startup_blank_remaining: 0,
            startup_blank_points,
            target_buffer_secs,
            min_buffer_secs,
            target_buffer_points: (target_buffer_secs * pps) as u64,
            stats: StreamStats::default(),
            last_armed: false,
            shutter_open: false,
        }
    }
}

// =============================================================================
// Stream
// =============================================================================

/// A streaming session for outputting points to a DAC.
///
/// Use [`run()`](Self::run) to stream with buffer-driven timing.
/// The callback is invoked when the buffer needs filling, providing automatic
/// backpressure handling and zero allocations in the hot path.
///
/// The stream owns pacing, backpressure, and the timebase (`StreamInstant`).
pub struct Stream {
    /// Device info for this stream.
    info: DacInfo,
    /// The backend.
    backend: Option<BackendKind>,
    /// Stream configuration.
    config: StreamConfig,
    /// Thread-safe control handle.
    control: StreamControl,
    /// Receiver for control messages from StreamControl.
    control_rx: Receiver<ControlMsg>,
    /// Stream state.
    state: StreamState,
    /// Reconnection policy (None = no reconnection).
    pub(crate) reconnect_policy: Option<ReconnectPolicy>,
    /// Reopen identity, preserved for `into_dac()` even without reconnect enabled.
    pub(crate) reconnect_target: Option<ReconnectTarget>,
}

impl Stream {
    /// Convert a duration in microseconds to a point count at the given PPS, rounding up.
    fn duration_micros_to_points(micros: u64, pps: u32) -> usize {
        if micros == 0 {
            0
        } else {
            (micros as f64 * pps as f64 / 1_000_000.0).ceil() as usize
        }
    }

    fn udp_timed_sleep_slice(remaining: Duration) -> Option<Duration> {
        const UDP_TIMED_SLEEP_SLICE: Duration = Duration::from_millis(1);
        const UDP_TIMED_BUSY_WAIT_THRESHOLD: Duration = Duration::from_micros(500);

        if remaining <= UDP_TIMED_BUSY_WAIT_THRESHOLD {
            None
        } else {
            Some(
                remaining
                    .saturating_sub(UDP_TIMED_BUSY_WAIT_THRESHOLD)
                    .min(UDP_TIMED_SLEEP_SLICE),
            )
        }
    }

    /// Create a new stream with a backend.
    pub(crate) fn with_backend(info: DacInfo, backend: BackendKind, config: StreamConfig) -> Self {
        let (control_tx, control_rx) = mpsc::channel();
        let max_points = info.caps.max_points_per_chunk;
        let startup_blank_points =
            Self::duration_micros_to_points(config.startup_blank.as_micros() as u64, config.pps);
        let color_delay = config.color_delay;
        let state = StreamState::new(max_points, startup_blank_points, &config);
        Self {
            info,
            backend: Some(backend),
            config,
            control: StreamControl::new(control_tx, color_delay),
            control_rx,
            state,
            reconnect_policy: None,
            reconnect_target: None,
        }
    }

    /// Compute the software buffer target for scheduler pacing.
    ///
    /// UdpTimed backends use `max_points_per_chunk` as the target to keep
    /// the device ringbuffer continuously topped up. A lower target creates
    /// long idle gaps between bursts, causing glitches over WiFi.
    fn scheduler_target_buffer_points(&self) -> u64 {
        if self.info.caps.output_model == OutputModel::UdpTimed {
            self.info.caps.max_points_per_chunk as u64
        } else {
            self.state.target_buffer_points
        }
    }

    /// Returns the device info.
    pub fn info(&self) -> &DacInfo {
        &self.info
    }

    /// Returns the stream configuration.
    pub fn config(&self) -> &StreamConfig {
        &self.config
    }

    /// Returns a thread-safe control handle.
    pub fn control(&self) -> StreamControl {
        self.control.clone()
    }

    /// Returns the current stream status.
    pub fn status(&self) -> Result<StreamStatus> {
        let device_queued_points = self.backend.as_ref().and_then(|b| b.queued_points());

        Ok(StreamStatus {
            connected: self.backend.as_ref().is_some_and(|b| b.is_connected()),
            scheduled_ahead_points: self.state.scheduled_ahead,
            device_queued_points,
            stats: Some(self.state.stats.clone()),
        })
    }

    /// Handle hardware shutter transitions based on arm state changes.
    fn handle_shutter_transition(&mut self, is_armed: bool) {
        let was_armed = self.state.last_armed;
        self.state.last_armed = is_armed;

        if was_armed && !is_armed {
            // Disarmed: close the shutter for safety (best-effort)
            self.state.color_delay_line.clear();
            if self.state.shutter_open {
                if let Some(backend) = &mut self.backend {
                    let _ = backend.set_shutter(false); // Best-effort, ignore errors
                }
                self.state.shutter_open = false;
            }
        } else if !was_armed && is_armed {
            // Armed: open the shutter (best-effort)
            // Pre-fill color delay line with blanked colors so early points
            // come out dark while galvos settle.
            let delay_micros = self.control.inner.color_delay_micros.load(Ordering::SeqCst);
            let delay_points = Self::duration_micros_to_points(delay_micros, self.config.pps);
            self.state.color_delay_line.clear();
            for _ in 0..delay_points {
                self.state.color_delay_line.push_back((0, 0, 0, 0));
            }

            self.state.startup_blank_remaining = self.state.startup_blank_points;

            if !self.state.shutter_open {
                if let Some(backend) = &mut self.backend {
                    let _ = backend.set_shutter(true); // Best-effort, ignore errors
                }
                self.state.shutter_open = true;
            }
        }
    }

    /// Stop the stream and terminate output.
    ///
    /// Disarms the output (software blanking + hardware shutter) before stopping
    /// the backend to prevent the "freeze on last bright point" hazard.
    /// Use `disarm()` instead if you want to keep the stream alive but safe.
    pub fn stop(&mut self) -> Result<()> {
        // Disarm: sets armed flag for software blanking
        self.control.disarm()?;

        self.control.stop()?;

        // Directly close shutter, stop, and disconnect backend (defense-in-depth).
        // Disconnect is needed for protocols like IDN where stop() only blanks
        // but the DAC keeps replaying its buffer until the session is closed.
        if let Some(b) = &mut self.backend {
            let _ = b.set_shutter(false);
            let _ = b.stop();
            b.disconnect()?;
        }

        Ok(())
    }

    /// Consume the stream and recover the device for reuse.
    ///
    /// This method disarms and stops the stream (software blanking + hardware shutter),
    /// then returns the underlying `Dac` along with the final `StreamStats`.
    /// The device can then be used to start a new stream with different configuration.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use laser_dac::StreamConfig;
    ///
    /// // device: Dac, config: StreamConfig (from prior setup)
    /// let (stream, info) = device.start_stream(config)?;
    /// // ... stream for a while ...
    /// let (device, stats) = stream.into_dac();
    /// println!("Streamed {} points", stats.points_written);
    ///
    /// // Restart with different config
    /// let new_config = StreamConfig::new(60_000);
    /// let (stream2, _) = device.start_stream(new_config)?;
    /// ```
    pub fn into_dac(mut self) -> (Dac, StreamStats) {
        // Disarm (software blanking) and close shutter before stopping
        let _ = self.control.disarm();
        let _ = self.control.stop();
        if let Some(backend) = &mut self.backend {
            let _ = backend.set_shutter(false);
            let _ = backend.stop();
        }

        // Take the backend (leaves None, so Drop won't try to stop again)
        let backend = self.backend.take();
        let stats = self.state.stats.clone();
        let reconnect_target = self
            .reconnect_target
            .take()
            .or_else(|| self.reconnect_policy.take().map(|p| p.target));

        let dac = Dac {
            info: self.info.clone(),
            backend,
            reconnect_target,
        };

        (dac, stats)
    }

    /// Attempt to reconnect the backend using the reconnection policy.
    ///
    /// Opens a new device, connects, and swaps the backend. Resets timing
    /// state for the new connection. Returns `Err` on non-retriable errors
    /// or if stop is requested.
    fn handle_reconnect(
        &mut self,
        last_iteration: &mut std::time::Instant,
    ) -> std::result::Result<(), RunExit> {
        let policy = self.reconnect_policy.as_ref().unwrap();

        let (info, new_backend) = reconnect_backend_with_retry(
            policy,
            || self.control.is_stop_requested(),
            |info, new_backend| {
                if new_backend.is_frame_swap() {
                    log::error!(
                        "'{}' reconnected device is frame-swap, incompatible with streaming",
                        policy.target.device_id
                    );
                    return Err(RunExit::Disconnected);
                }

                if Dac::validate_pps(&info.caps, self.config.pps).is_err() {
                    log::error!(
                        "'{}' config invalid for new device",
                        policy.target.device_id
                    );
                    return Err(RunExit::Disconnected);
                }

                Ok(())
            },
            || {},
        )?;

        // Swap the backend
        self.backend = Some(new_backend);
        self.info = info;

        // Reset all runtime state for the new connection
        self.reset_state_for_reconnect(last_iteration);

        // Fire on_reconnect callback
        let policy = self.reconnect_policy.as_ref().unwrap();
        if let Some(cb) = policy.on_reconnect.lock().unwrap().as_mut() {
            cb(&self.info);
        }

        Ok(())
    }

    /// Reset all runtime state for a reconnected device.
    fn reset_state_for_reconnect(&mut self, last_iteration: &mut std::time::Instant) {
        let max_points = self.info.caps.max_points_per_chunk;
        self.state
            .chunk_buffer
            .resize(max_points, LaserPoint::default());
        self.state
            .last_chunk
            .resize(max_points, LaserPoint::default());
        self.state.last_chunk_len = 0;
        self.state.scheduled_ahead = 0;
        self.state.fractional_consumed = 0.0;
        self.state.shutter_open = false;
        self.state.last_armed = false;
        self.state.color_delay_line.clear();
        self.state.startup_blank_remaining = 0;
        self.state.stats.reconnect_count += 1;
        *last_iteration = std::time::Instant::now();
    }

    /// Run the stream with the zero-allocation callback API.
    ///
    /// This method uses **pure buffer-driven timing**:
    /// - Callback is invoked when `buffered < target_buffer`
    /// - Points requested varies based on buffer headroom (`min_points`, `target_points`)
    /// - Callback fills a library-owned buffer (zero allocations in hot path)
    ///
    /// # Callback Contract
    ///
    /// The callback receives a `ChunkRequest` describing buffer state and requirements,
    /// and a mutable slice to fill with points. It returns:
    ///
    /// - `ChunkResult::Filled(n)`: Wrote `n` points to the buffer
    /// - `ChunkResult::Starved`: No data available (underrun policy applies)
    /// - `ChunkResult::End`: Stream should end gracefully
    ///
    /// # Exit Conditions
    ///
    /// - **`RunExit::Stopped`**: Stop requested via `StreamControl::stop()`.
    /// - **`RunExit::ProducerEnded`**: Callback returned `ChunkResult::End`.
    /// - **`RunExit::Disconnected`**: Device disconnected.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use laser_dac::{ChunkRequest, ChunkResult, LaserPoint};
    ///
    /// stream.run(
    ///     |req: &ChunkRequest, buffer: &mut [LaserPoint]| {
    ///         let n = req.target_points;
    ///         for i in 0..n {
    ///             let t = req.start.as_secs_f64(req.pps) + (i as f64 / req.pps as f64);
    ///             let angle = (t * std::f64::consts::TAU) as f32;
    ///             buffer[i] = LaserPoint::new(angle.cos(), angle.sin(), 65535, 0, 0, 65535);
    ///         }
    ///         ChunkResult::Filled(n)
    ///     },
    ///     |err| eprintln!("Error: {}", err),
    /// )?;
    /// ```
    pub fn run<F, E>(mut self, mut producer: F, mut on_error: E) -> Result<RunExit>
    where
        F: FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        use std::time::Instant;

        let pps = self.config.pps as f64;
        let mut max_points = self.info.caps.max_points_per_chunk;

        // Track time between iterations to decrement scheduled_ahead for backends
        // that don't report queued_points(). This prevents stalls when buffered
        // equals target_points exactly.
        let mut last_iteration = Instant::now();
        let mut last_stats_log = Instant::now();

        loop {
            // 1. Check for stop request
            if self.control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            // Decrement scheduled_ahead based on elapsed time since last iteration.
            // This is critical for backends without queued_points() - without this,
            // scheduled_ahead would never decrease and target_points would stay 0.
            // Use fractional accumulator to avoid the truncation-to-zero bug: when
            // loop iterations are faster than 1/pps, (elapsed * pps) as u64 rounds
            // to 0 and scheduled_ahead never decrements, causing a permanent stall.
            let now = Instant::now();
            scheduler::advance_scheduled_ahead(
                &mut self.state.scheduled_ahead,
                &mut self.state.fractional_consumed,
                &mut last_iteration,
                now,
                pps,
            );

            // 2. Estimate buffer state
            let buffered = self.estimate_buffer_points();
            let target_points = self.scheduler_target_buffer_points();

            // Log scheduler state periodically (~2Hz)
            if now.duration_since(last_stats_log) >= Duration::from_millis(500) {
                let lead_ms = self.state.scheduled_ahead as f64 / pps * 1000.0;
                let target_ms = target_points as f64 / pps * 1000.0;
                log::debug!(
                    "scheduler: lead={:.1}ms target={:.1}ms ahead={} buffered={} target_pts={}",
                    lead_ms,
                    target_ms,
                    self.state.scheduled_ahead,
                    buffered,
                    target_points,
                );
                last_stats_log = now;
            }

            // 3. If buffer is above target, sleep until it drains to target
            // Note: use > not >= so we call producer when exactly at target
            if buffered > target_points {
                let excess_points = buffered - target_points;
                let sleep_time = Duration::from_secs_f64(excess_points as f64 / pps);
                let stop = if self.info.caps.output_model == OutputModel::UdpTimed {
                    self.sleep_until_with_control_check(Instant::now() + sleep_time)?
                } else {
                    self.sleep_with_control_check(sleep_time)?
                };
                if stop {
                    return Ok(RunExit::Stopped);
                }
                continue; // Re-check buffer after sleep
            }

            // 4. Check backend connection
            let disconnected = match &self.backend {
                Some(b) => !b.is_connected(),
                None => true,
            };
            if disconnected {
                if self.reconnect_policy.is_some() {
                    match self.handle_reconnect(&mut last_iteration) {
                        Ok(()) => {
                            max_points = self.info.caps.max_points_per_chunk;
                            continue;
                        }
                        Err(exit) => return Ok(exit),
                    }
                }
                log::warn!("backend disconnected, exiting");
                on_error(Error::disconnected("backend disconnected"));
                return Ok(RunExit::Disconnected);
            }

            // 5. Process control messages before calling producer
            if self.process_control_messages() {
                return Ok(RunExit::Stopped);
            }

            // 6. Build fill request with buffer state (reuse cached estimate)
            let req = self.build_fill_request(max_points, buffered);

            // 7. Call producer with pre-allocated buffer
            let buffer = &mut self.state.chunk_buffer[..max_points];
            let result = producer(&req, buffer);

            // 8. Handle result
            match result {
                ChunkResult::Filled(n) => {
                    // Validate n doesn't exceed buffer
                    let n = n.min(max_points);

                    // Treat Filled(0) with target_points > 0 as Starved
                    if n == 0 && req.target_points > 0 {
                        self.handle_underrun(&req)?;
                        continue;
                    }

                    // Write to backend if we have points
                    if n > 0 {
                        match self.write_fill_points(n, &mut on_error) {
                            Ok(()) => {}
                            Err(e) if e.is_disconnected() && self.reconnect_policy.is_some() => {
                                match self.handle_reconnect(&mut last_iteration) {
                                    Ok(()) => {
                                        max_points = self.info.caps.max_points_per_chunk;
                                        continue;
                                    }
                                    Err(exit) => return Ok(exit),
                                }
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                ChunkResult::Starved => {
                    self.handle_underrun(&req)?;
                }
                ChunkResult::End => {
                    // Graceful shutdown: let queued points drain, then blank/park
                    self.drain_and_blank();
                    return Ok(RunExit::ProducerEnded);
                }
            }
        }
    }

    /// Sleep for the given duration while checking for control messages.
    ///
    /// Returns `true` if stop was requested, `false` otherwise.
    fn sleep_with_control_check(&mut self, duration: Duration) -> Result<bool> {
        const SLEEP_SLICE: Duration = Duration::from_millis(2);
        let mut remaining = duration;

        while remaining > Duration::ZERO {
            let slice = remaining.min(SLEEP_SLICE);
            std::thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);

            if self.process_control_messages() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Sleep until a deadline with finer granularity for timed-UDP pacing.
    ///
    /// Uses coarse sleeps first, then yields near the deadline to reduce wake-up jitter.
    fn sleep_until_with_control_check(&mut self, deadline: std::time::Instant) -> Result<bool> {
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(false);
            }

            let remaining = deadline.duration_since(now);
            if let Some(slice) = Self::udp_timed_sleep_slice(remaining) {
                std::thread::sleep(slice);
            } else {
                std::thread::yield_now();
            }

            if self.process_control_messages() {
                return Ok(true);
            }
        }
    }

    /// Write points from chunk_buffer to the backend.
    ///
    /// Called by `run` after the producer fills the buffer.
    fn write_fill_points<E>(&mut self, n: usize, on_error: &mut E) -> Result<()>
    where
        E: FnMut(Error),
    {
        let is_armed = self.control.is_armed();
        let pps = self.config.pps;

        // Handle shutter transitions
        self.handle_shutter_transition(is_armed);

        // Blank points in-place when disarmed
        if !is_armed {
            for p in &mut self.state.chunk_buffer[..n] {
                *p = LaserPoint::blanked(p.x, p.y);
            }
        }

        // Apply startup blanking: force first N points after arming to blank
        if is_armed && self.state.startup_blank_remaining > 0 {
            let blank_count = n.min(self.state.startup_blank_remaining);
            for p in &mut self.state.chunk_buffer[..blank_count] {
                p.r = 0;
                p.g = 0;
                p.b = 0;
                p.intensity = 0;
            }
            self.state.startup_blank_remaining -= blank_count;
        }

        // Apply color delay: read current setting, resize deque, shift colors
        let delay_micros = self.control.inner.color_delay_micros.load(Ordering::SeqCst);
        let color_delay_points = Self::duration_micros_to_points(delay_micros, self.config.pps);

        if color_delay_points > 0 {
            // Resize deque to match current delay (handles dynamic changes)
            self.state
                .color_delay_line
                .resize(color_delay_points, (0, 0, 0, 0));
            for p in &mut self.state.chunk_buffer[..n] {
                self.state
                    .color_delay_line
                    .push_back((p.r, p.g, p.b, p.intensity));
                let (r, g, b, i) = self.state.color_delay_line.pop_front().unwrap();
                p.r = r;
                p.g = g;
                p.b = b;
                p.intensity = i;
            }
        } else if !self.state.color_delay_line.is_empty() {
            // Delay was disabled at runtime — flush the line
            self.state.color_delay_line.clear();
        }

        // Try to write with backpressure handling
        loop {
            // Check backend exists
            let backend = match self.backend.as_mut() {
                Some(b) => b,
                None => return Err(Error::disconnected("no backend")),
            };

            match backend.try_write(pps, &self.state.chunk_buffer[..n]) {
                Ok(WriteOutcome::Written) => {
                    self.record_write(n, is_armed);
                    return Ok(());
                }
                Ok(WriteOutcome::WouldBlock) => {
                    // Backend buffer full - yield and retry
                    // Borrow of backend is dropped here, so we can call process_control_messages
                }
                Err(e) if e.is_stopped() => {
                    return Err(Error::Stopped);
                }
                Err(e) if e.is_disconnected() => {
                    log::warn!("write got Disconnected error, exiting stream: {e}");
                    on_error(Error::disconnected("backend disconnected"));
                    return Err(e);
                }
                Err(e) => {
                    log::warn!("write error, disconnecting backend: {e}");
                    let _ = backend.disconnect();
                    on_error(e);
                    return Ok(());
                }
            }

            // Handle WouldBlock: yield and process control messages
            std::thread::yield_now();
            if self.process_control_messages() {
                return Err(Error::Stopped);
            }
            std::thread::sleep(Duration::from_micros(100));
        }
    }

    /// Handle underrun for the fill API by applying the underrun policy.
    fn handle_underrun(&mut self, req: &ChunkRequest) -> Result<()> {
        self.state.stats.underrun_count += 1;

        let is_armed = self.control.is_armed();
        self.handle_shutter_transition(is_armed);

        // Calculate how many points we need (use target_points as the fill amount)
        let n_points = req.target_points.max(1);

        // Fill chunk_buffer with underrun content
        let fill_start = if !is_armed {
            // When disarmed, always output blanked points
            for i in 0..n_points {
                self.state.chunk_buffer[i] = LaserPoint::blanked(0.0, 0.0);
            }
            n_points
        } else {
            match &self.config.underrun {
                UnderrunPolicy::RepeatLast => {
                    if self.state.last_chunk_len > 0 {
                        // Repeat last chunk cyclically
                        for i in 0..n_points {
                            self.state.chunk_buffer[i] =
                                self.state.last_chunk[i % self.state.last_chunk_len];
                        }
                        n_points
                    } else {
                        // No last chunk, fall back to blank
                        for i in 0..n_points {
                            self.state.chunk_buffer[i] = LaserPoint::blanked(0.0, 0.0);
                        }
                        n_points
                    }
                }
                UnderrunPolicy::Blank => {
                    for i in 0..n_points {
                        self.state.chunk_buffer[i] = LaserPoint::blanked(0.0, 0.0);
                    }
                    n_points
                }
                UnderrunPolicy::Park { x, y } => {
                    for i in 0..n_points {
                        self.state.chunk_buffer[i] = LaserPoint::blanked(*x, *y);
                    }
                    n_points
                }
                UnderrunPolicy::Stop => {
                    self.control.stop()?;
                    return Err(Error::Stopped);
                }
            }
        };

        // Write the fill points
        if let Some(backend) = &mut self.backend {
            match backend.try_write(self.config.pps, &self.state.chunk_buffer[..fill_start]) {
                Ok(WriteOutcome::Written) => {
                    self.record_write(fill_start, is_armed);
                }
                Ok(WriteOutcome::WouldBlock) => {
                    // Backend is full - expected during underrun
                }
                Err(_) => {
                    // Backend error during underrun handling - ignore
                }
            }
        }

        Ok(())
    }

    /// Record a successful write: update last_chunk, timebase, and stats.
    fn record_write(&mut self, n: usize, is_armed: bool) {
        if is_armed {
            debug_assert!(
                n <= self.state.last_chunk.len(),
                "n ({}) exceeds last_chunk capacity ({})",
                n,
                self.state.last_chunk.len()
            );
            self.state.last_chunk[..n].copy_from_slice(&self.state.chunk_buffer[..n]);
            self.state.last_chunk_len = n;
        }
        self.state.current_instant += n as u64;
        self.state.scheduled_ahead += n as u64;
        self.state.stats.chunks_written += 1;
        self.state.stats.points_written += n as u64;
    }

    // =========================================================================
    // Internal helpers
    // =========================================================================

    /// Process any pending control messages from StreamControl.
    ///
    /// This method drains the control message queue and takes immediate action:
    /// - `Arm`: Opens the shutter (best-effort)
    /// - `Disarm`: Closes the shutter immediately
    /// - `Stop`: Returns `true` to signal the caller to stop
    ///
    /// Returns `true` if stop was requested, `false` otherwise.
    fn process_control_messages(&mut self) -> bool {
        loop {
            match self.control_rx.try_recv() {
                Ok(ControlMsg::Arm) => {
                    if !self.state.shutter_open {
                        if let Some(backend) = &mut self.backend {
                            let _ = backend.set_shutter(true);
                        }
                        self.state.shutter_open = true;
                    }
                }
                Ok(ControlMsg::Disarm) => {
                    if self.state.shutter_open {
                        if let Some(backend) = &mut self.backend {
                            let _ = backend.set_shutter(false);
                        }
                        self.state.shutter_open = false;
                    }
                }
                Ok(ControlMsg::Stop) => {
                    return true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        false
    }

    /// Estimate the current buffer level in points using conservative estimation.
    ///
    /// Uses `min(hardware, software)` to prevent underruns:
    /// - If hardware reports fewer points than software estimates, hardware is truth
    /// - Using `max` would overestimate buffer → underrequest points → underrun
    /// - Conservative (lower) estimate is safer: we might overfill slightly, but won't underrun
    ///
    /// # Returns
    ///
    /// The estimated number of points currently buffered (points sent but not yet played).
    fn estimate_buffer_points(&self) -> u64 {
        scheduler::conservative_buffered_points(
            self.state.scheduled_ahead,
            self.backend.as_ref().and_then(|b| b.queued_points()),
        )
    }

    /// Build a ChunkRequest with calculated buffer state and point requirements.
    ///
    /// Calculates:
    /// - `buffered_points`: Conservative estimate of points in buffer
    /// - `buffered`: Buffer level as Duration
    /// - `start`: Estimated playback time (playhead + buffered)
    /// - `min_points`: Minimum points to avoid underrun (ceiling rounded)
    /// - `target_points`: Ideal points to reach target buffer (clamped to max)
    ///
    /// # Arguments
    ///
    /// * `max_points` - Maximum points the callback can write (buffer length)
    /// * `buffered_points` - Pre-computed buffer estimate from `estimate_buffer_points()`
    fn build_fill_request(&self, max_points: usize, buffered_points: u64) -> ChunkRequest {
        let pps = self.config.pps;
        let pps_f64 = pps as f64;

        // Compute buffered duration directly (avoid Duration intermediary)
        let buffered_secs = buffered_points as f64 / pps_f64;
        let buffered = Duration::from_secs_f64(buffered_secs);

        // Calculate start time for this chunk (when these points will play).
        let start = self.state.current_instant;

        // Use cached config values (avoid per-iteration as_secs_f64() calls)
        let target_buffer_secs = self.state.target_buffer_secs;
        let min_buffer_secs = self.state.min_buffer_secs;

        if self.info.caps.output_model == OutputModel::UdpTimed {
            let device_queued_points = self.backend.as_ref().and_then(|b| b.queued_points());
            return ChunkRequest {
                start,
                pps,
                min_points: max_points,
                target_points: max_points,
                buffered_points,
                buffered,
                device_queued_points,
            };
        }

        // target_points: ceil((target_buffer - buffered) * pps), clamped to max_points
        let deficit_target = (target_buffer_secs - buffered_secs).max(0.0);
        let target_points = (deficit_target * pps_f64).ceil() as usize;
        let target_points = target_points.min(max_points);

        // min_points: ceil((min_buffer - buffered) * pps) - minimum to avoid underrun
        let deficit_min = (min_buffer_secs - buffered_secs).max(0.0);
        let min_points = (deficit_min * pps_f64).ceil() as usize;
        let min_points = min_points.min(max_points);

        // Use buffered_points as the device queue estimate (already computed by
        // estimate_buffer_points, which includes hardware feedback when available).
        // This avoids a redundant second call to queued_points().
        let device_queued_points = self.backend.as_ref().and_then(|b| b.queued_points());

        ChunkRequest {
            start,
            pps,
            min_points,
            target_points,
            buffered_points,
            buffered,
            device_queued_points,
        }
    }

    /// Wait for queued points to drain, then blank/park the laser.
    ///
    /// Called on graceful shutdown (`ChunkResult::End`) to let buffered content
    /// play out before stopping. Uses `drain_timeout` from config to cap the wait.
    ///
    /// - If `queued_points()` is available: polls until queue empties or timeout
    /// - If `queued_points()` is `None`: sleeps for estimated buffer duration, capped by timeout
    ///
    /// After drain (or timeout), closes shutter and outputs blank points.
    fn drain_and_blank(&mut self) {
        use std::time::Instant;

        let timeout = self.config.drain_timeout;
        if timeout.is_zero() {
            // Skip drain entirely if timeout is zero
            self.blank_and_close_shutter();
            return;
        }

        let deadline = Instant::now() + timeout;
        let pps = self.config.pps;

        // Check if backend supports queue depth reporting
        let has_queue_depth = self
            .backend
            .as_ref()
            .and_then(|b| b.queued_points())
            .is_some();

        if has_queue_depth {
            // Poll until queue empties or timeout
            const POLL_INTERVAL: Duration = Duration::from_millis(5);
            while Instant::now() < deadline {
                if let Some(queued) = self.backend.as_ref().and_then(|b| b.queued_points()) {
                    if queued == 0 {
                        break;
                    }
                } else {
                    // Backend disconnected or stopped reporting
                    break;
                }

                // Process control messages during drain (allow stop to interrupt)
                if self.process_control_messages() {
                    break;
                }

                std::thread::sleep(POLL_INTERVAL);
            }
        } else {
            // No queue depth available: sleep for estimated buffer time, capped by timeout
            let estimated_drain =
                Duration::from_secs_f64(self.state.scheduled_ahead as f64 / pps as f64);
            let wait_time = estimated_drain.min(timeout);

            // Sleep in slices to allow control message processing
            const SLEEP_SLICE: Duration = Duration::from_millis(10);
            let mut remaining = wait_time;
            while remaining > Duration::ZERO && Instant::now() < deadline {
                let slice = remaining.min(SLEEP_SLICE);
                std::thread::sleep(slice);
                remaining = remaining.saturating_sub(slice);

                if self.process_control_messages() {
                    break;
                }
            }
        }

        self.blank_and_close_shutter();
    }

    /// Output blank points and close the hardware shutter.
    ///
    /// Best-effort safety shutdown - errors are ignored since we're already
    /// in shutdown path.
    fn blank_and_close_shutter(&mut self) {
        // Close shutter (best-effort)
        if let Some(b) = &mut self.backend {
            let _ = b.set_shutter(false);
        }
        self.state.shutter_open = false;

        // Output a small blank chunk to ensure laser is off
        // (some DACs may hold the last point otherwise)
        if let Some(b) = &mut self.backend {
            let blank_point = LaserPoint::blanked(0.0, 0.0);
            let blank_chunk = [blank_point; 16];
            let _ = b.try_write(self.config.pps, &blank_chunk);
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

// =============================================================================
// Device
// =============================================================================

/// A connected device that can start streaming sessions.
///
/// When starting a stream, the device is consumed and the backend ownership
/// transfers to the stream. The `DacInfo` is returned alongside the stream
/// so metadata remains accessible.
///
/// # Example
///
/// ```ignore
/// use laser_dac::{open_device, StreamConfig};
///
/// let device = open_device("my-device")?;
/// let config = StreamConfig::new(30_000);
/// let (stream, info) = device.start_stream(config)?;
/// println!("Streaming to: {}", info.name);
/// ```
pub struct Dac {
    info: DacInfo,
    backend: Option<BackendKind>,
    pub(crate) reconnect_target: Option<ReconnectTarget>,
}

impl Dac {
    /// Create a new device from a backend.
    pub fn new(info: DacInfo, backend: BackendKind) -> Self {
        Self {
            info,
            backend: Some(backend),
            reconnect_target: None,
        }
    }

    /// Set a custom discovery factory for reconnection.
    ///
    /// When reconnection is enabled (via [`crate::FrameSessionConfig::with_reconnect`] or
    /// [`StreamConfig::with_reconnect`]), the factory is called to create a
    /// [`DacDiscovery`] instance for each reconnection attempt. This is required
    /// for custom backends registered via [`DacDiscovery::register`] — without it,
    /// reconnection uses the default discovery which only finds built-in DAC types.
    ///
    /// For most cases, prefer [`open_device_with`](crate::open_device_with) which
    /// handles both initial discovery and reconnection in one call. Use this method
    /// when you build `Dac` instances yourself via `scan()` + `connect()` + `Dac::new()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use laser_dac::{Dac, DacDiscovery, EnabledDacTypes, FrameSessionConfig, ReconnectConfig};
    ///
    /// // Device opened through custom discovery path
    /// let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
    /// discovery.register(Box::new(MyCustomDiscoverer::new()));
    /// let devices = discovery.scan();
    /// let backend = discovery.connect(devices.into_iter().next().unwrap())?;
    /// let dac = Dac::new(info, backend);
    ///
    /// // Attach factory so reconnection can also find custom backends
    /// let dac = dac.with_discovery_factory(|| {
    ///     let mut d = DacDiscovery::new(EnabledDacTypes::all());
    ///     d.register(Box::new(MyCustomDiscoverer::new()));
    ///     d
    /// });
    ///
    /// let config = FrameSessionConfig::new(30_000)
    ///     .with_reconnect(ReconnectConfig::new());
    /// let (session, _info) = dac.start_frame_session(config)?;
    /// ```
    pub fn with_discovery_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn() -> DacDiscovery + Send + 'static,
    {
        match self.reconnect_target {
            Some(ref mut target) => {
                target.discovery_factory = Some(Box::new(factory));
            }
            None => {
                self.reconnect_target = Some(ReconnectTarget {
                    device_id: self.info.id.clone(),
                    discovery_factory: Some(Box::new(factory)),
                });
            }
        }
        self
    }

    /// Returns the device info.
    pub fn info(&self) -> &DacInfo {
        &self.info
    }

    /// Returns the device ID.
    pub fn id(&self) -> &str {
        &self.info.id
    }

    /// Returns the device name.
    pub fn name(&self) -> &str {
        &self.info.name
    }

    /// Returns the DAC type.
    pub fn kind(&self) -> &DacType {
        &self.info.kind
    }

    /// Returns the device capabilities.
    pub fn caps(&self) -> &DacCapabilities {
        &self.info.caps
    }

    /// Returns whether the device has a backend (not yet used for a stream).
    pub fn has_backend(&self) -> bool {
        self.backend.is_some()
    }

    /// Consume the Dac and return the backend, if available.
    pub(crate) fn into_backend(mut self) -> Option<BackendKind> {
        self.backend.take()
    }

    /// Returns whether the device is connected.
    pub fn is_connected(&self) -> bool {
        self.backend.as_ref().is_some_and(|b| b.is_connected())
    }

    /// Starts a streaming session, consuming the device.
    ///
    /// # Ownership
    ///
    /// This method consumes the `Dac` because:
    /// - Each device can only have one active stream at a time.
    /// - The backend is moved into the `Stream` to ensure exclusive access.
    /// - This prevents accidental reuse of a device that's already streaming.
    ///
    /// The method returns both the `Stream` and a copy of `DacInfo`, so you
    /// retain access to device metadata (id, name, capabilities) after starting.
    ///
    /// # Connection
    ///
    /// If the device is not already connected, this method will establish the
    /// connection before creating the stream. Connection failures are returned
    /// as errors.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The device backend has already been used for a stream.
    /// - The configuration is invalid (PPS out of range, invalid chunk size, etc.).
    /// - The backend fails to connect.
    pub fn start_stream(mut self, mut cfg: StreamConfig) -> Result<(Stream, DacInfo)> {
        // Extract reconnect config before consuming cfg
        let reconnect_config = cfg.reconnect.take();

        let mut backend = self.backend.take().ok_or_else(|| {
            Error::invalid_config("device backend has already been used for a stream")
        })?;

        if backend.is_frame_swap() {
            return Err(Error::invalid_config(
                "streaming is not supported on frame-swap DACs (e.g. Helios); \
                 use start_frame_session() instead",
            ));
        }

        let cfg = Self::apply_backend_buffer_defaults(&self.info.caps, cfg);

        Self::validate_pps(&self.info.caps, cfg.pps)?;

        // Connect the backend if not already connected
        if !backend.is_connected() {
            backend.connect()?;
        }

        let mut stream = Stream::with_backend(self.info.clone(), backend, cfg);

        // Always preserve the reopen target on the stream (for into_dac recovery)
        stream.reconnect_target = self.reconnect_target.take();

        // Wire reconnect policy if configured
        if let Some(rc) = reconnect_config {
            let target = stream.reconnect_target.take().ok_or_else(|| {
                Error::invalid_config("reconnect requires a reconnect target — use open_device(), open_device_with(), or Dac::with_discovery_factory()")
            })?;
            stream.reconnect_policy = Some(ReconnectPolicy::new(rc, target));
        }

        Ok((stream, self.info))
    }

    fn apply_backend_buffer_defaults(
        caps: &DacCapabilities,
        mut cfg: StreamConfig,
    ) -> StreamConfig {
        let untouched_defaults = cfg.target_buffer == StreamConfig::DEFAULT_TARGET_BUFFER
            && cfg.min_buffer == StreamConfig::DEFAULT_MIN_BUFFER;

        if untouched_defaults
            && matches!(
                caps.output_model,
                OutputModel::NetworkFifo | OutputModel::UdpTimed
            )
        {
            cfg.target_buffer = StreamConfig::NETWORK_DEFAULT_TARGET_BUFFER;
            cfg.min_buffer = StreamConfig::NETWORK_DEFAULT_MIN_BUFFER;
        }

        cfg
    }

    fn validate_pps(caps: &DacCapabilities, pps: u32) -> Result<()> {
        if pps < caps.pps_min || pps > caps.pps_max {
            return Err(Error::invalid_config(format!(
                "PPS {} is outside device range [{}, {}]",
                pps, caps.pps_min, caps.pps_max
            )));
        }

        Ok(())
    }

    /// Starts a frame-mode session, consuming the device.
    ///
    /// Similar to [`start_stream`](Self::start_stream) but uses the frame-first
    /// API where you submit complete [`crate::presentation::Frame`]s instead of filling
    /// point buffers via callback.
    ///
    /// Returns a [`crate::presentation::FrameSession`] that owns the scheduler thread and a
    /// [`DacInfo`] with device metadata.
    pub fn start_frame_session(
        mut self,
        mut config: crate::presentation::FrameSessionConfig,
    ) -> Result<(crate::presentation::FrameSession, DacInfo)> {
        let reconnect_config = config.reconnect.take();

        let backend = self.backend.take().ok_or_else(|| {
            Error::invalid_config("device backend has already been used for a session")
        })?;

        Self::validate_pps(backend.caps(), config.pps)?;

        let reconnect_policy = match reconnect_config {
            Some(rc) => {
                let target = self.reconnect_target.take().ok_or_else(|| {
                    Error::invalid_config("reconnect requires a reconnect target — use open_device(), open_device_with(), or Dac::with_discovery_factory()")
                })?;
                Some(ReconnectPolicy::new(rc, target))
            }
            None => None,
        };

        let session = crate::presentation::FrameSession::start(backend, config, reconnect_policy)?;
        Ok((session, self.info))
    }
}

#[cfg(test)]
mod tests;
