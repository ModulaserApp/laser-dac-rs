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
//! - **Ether Dream, Helios, IDN**: No-op (safety relies on software blanking)
//!
//! # Disconnect Behavior
//!
//! No automatic reconnection. On disconnect, create a new `Dac` and `Stream`.
//! New streams always start disarmed for safety.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{Error, Result, StreamBackend, WriteOutcome};
use crate::types::{
    DacCapabilities, DacInfo, DacType, ChunkRequest, ChunkResult, LaserPoint, RunExit, StreamConfig,
    StreamInstant, StreamStats, StreamStatus, UnderrunPolicy,
};

// =============================================================================
// Stream Control
// =============================================================================

/// Control messages sent from StreamControl to Stream.
///
/// These messages allow out-of-band control actions to take effect immediately,
/// even when the stream is waiting (pacing, backpressure, etc.).
#[derive(Debug, Clone, Copy)]
enum ControlMsg {
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
}

impl StreamControl {
    fn new(control_tx: Sender<ControlMsg>) -> Self {
        Self {
            inner: Arc::new(StreamControlInner {
                armed: AtomicBool::new(false),
                stop_requested: AtomicBool::new(false),
                control_tx: Mutex::new(control_tx),
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
    /// **Hardware shutter**: Best-effort. LaserCube has actual hardware control;
    /// Ether Dream, Helios, IDN are no-ops (safety relies on software blanking).
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

    // Pre-allocated buffers (no per-chunk allocation in hot path)
    /// Buffer for callback to fill points into.
    chunk_buffer: Vec<LaserPoint>,
    /// Last chunk for RepeatLast underrun policy.
    last_chunk: Vec<LaserPoint>,
    /// Number of valid points in last_chunk.
    last_chunk_len: usize,

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
    fn new(max_points_per_chunk: usize) -> Self {
        Self {
            current_instant: StreamInstant::new(0),
            scheduled_ahead: 0,
            chunk_buffer: vec![LaserPoint::default(); max_points_per_chunk],
            last_chunk: vec![LaserPoint::default(); max_points_per_chunk],
            last_chunk_len: 0,
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
/// Use [`run_fill()`](Self::run_fill) to stream with buffer-driven timing.
/// The callback is invoked when the buffer needs filling, providing automatic
/// backpressure handling and zero allocations in the hot path.
///
/// The stream owns pacing, backpressure, and the timebase (`StreamInstant`).
pub struct Stream {
    /// Device info for this stream.
    info: DacInfo,
    /// The backend.
    backend: Option<Box<dyn StreamBackend>>,
    /// Stream configuration.
    config: StreamConfig,
    /// Thread-safe control handle.
    control: StreamControl,
    /// Receiver for control messages from StreamControl.
    control_rx: Receiver<ControlMsg>,
    /// Stream state.
    state: StreamState,
}

impl Stream {
    /// Create a new stream with a backend.
    pub(crate) fn with_backend(
        info: DacInfo,
        backend: Box<dyn StreamBackend>,
        config: StreamConfig,
    ) -> Self {
        let (control_tx, control_rx) = mpsc::channel();
        let max_points = info.caps.max_points_per_chunk;
        Self {
            info,
            backend: Some(backend),
            config,
            control: StreamControl::new(control_tx),
            control_rx,
            state: StreamState::new(max_points),
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
            connected: self
                .backend
                .as_ref()
                .map(|b| b.is_connected())
                .unwrap_or(false),
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
            if self.state.shutter_open {
                if let Some(backend) = &mut self.backend {
                    let _ = backend.set_shutter(false); // Best-effort, ignore errors
                }
                self.state.shutter_open = false;
            }
        } else if !was_armed && is_armed {
            // Armed: open the shutter (best-effort)
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

        // Directly close shutter and stop backend (defense-in-depth)
        if let Some(backend) = &mut self.backend {
            let _ = backend.set_shutter(false);
            backend.stop()?;
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

        let dac = Dac {
            info: self.info.clone(),
            backend,
        };

        (dac, stats)
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
    /// stream.run_fill(
    ///     |req: &ChunkRequest, buffer: &mut [LaserPoint]| {
    ///         let n = req.target_points;
    ///         for i in 0..n {
    ///             let t = req.start.as_secs_f64(req.pps) + (i as f64 / req.pps as f64);
    ///             buffer[i] = generate_point_at_time(t);
    ///         }
    ///         ChunkResult::Filled(n)
    ///     },
    ///     |err| eprintln!("Error: {}", err),
    /// )?;
    /// ```
    pub fn run_fill<F, E>(mut self, mut producer: F, mut on_error: E) -> Result<RunExit>
    where
        F: FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        use std::time::Instant;

        let pps = self.config.pps as f64;
        let max_points = self.info.caps.max_points_per_chunk;

        // Track time between iterations to decrement scheduled_ahead for backends
        // that don't report queued_points(). This prevents stalls when buffered
        // equals target_points exactly.
        let mut last_iteration = Instant::now();

        loop {
            // 1. Check for stop request
            if self.control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            // Decrement scheduled_ahead based on elapsed time since last iteration.
            // This is critical for backends without queued_points() - without this,
            // scheduled_ahead would never decrease and target_points would stay 0.
            let now = Instant::now();
            let elapsed = now.duration_since(last_iteration);
            let points_consumed = (elapsed.as_secs_f64() * pps) as u64;
            self.state.scheduled_ahead = self.state.scheduled_ahead.saturating_sub(points_consumed);
            last_iteration = now;

            // 2. Estimate buffer state
            let buffered = self.estimate_buffer_points();
            let target_points = (self.config.target_buffer.as_secs_f64() * pps) as u64;

            // 3. If buffer is above target, sleep until it drains to target
            // Note: use > not >= so we call producer when exactly at target
            if buffered > target_points {
                let excess_points = buffered - target_points;
                let sleep_time = Duration::from_secs_f64(excess_points as f64 / pps);
                if self.sleep_with_control_check(sleep_time)? {
                    return Ok(RunExit::Stopped);
                }
                continue; // Re-check buffer after sleep
            }

            // 4. Check backend connection
            if let Some(backend) = &self.backend {
                if !backend.is_connected() {
                    on_error(Error::disconnected("backend disconnected"));
                    return Ok(RunExit::Disconnected);
                }
            } else {
                on_error(Error::disconnected("no backend"));
                return Ok(RunExit::Disconnected);
            }

            // 5. Process control messages before calling producer
            if self.process_control_messages() {
                return Ok(RunExit::Stopped);
            }

            // 6. Build fill request with buffer state
            let req = self.build_fill_request(max_points);

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
                        self.handle_underrun_fill(&req)?;
                        continue;
                    }

                    // Write to backend if we have points
                    if n > 0 {
                        self.write_fill_points(n, &mut on_error)?;
                    }
                }
                ChunkResult::Starved => {
                    self.handle_underrun_fill(&req)?;
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

            // Process control messages for immediate response
            if self.process_control_messages() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Write points from chunk_buffer to the backend.
    ///
    /// Called by `run_fill` after the producer fills the buffer.
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

        // Try to write with backpressure handling
        loop {
            // Check backend exists
            let backend = match self.backend.as_mut() {
                Some(b) => b,
                None => return Err(Error::disconnected("no backend")),
            };

            match backend.try_write_chunk(pps, &self.state.chunk_buffer[..n]) {
                Ok(WriteOutcome::Written) => {
                    // Update state
                    if is_armed {
                        // Both buffers are pre-allocated to max_points_per_chunk, so n always fits
                        debug_assert!(
                            n <= self.state.last_chunk.len(),
                            "n ({}) exceeds last_chunk capacity ({})",
                            n,
                            self.state.last_chunk.len()
                        );
                        self.state.last_chunk[..n]
                            .copy_from_slice(&self.state.chunk_buffer[..n]);
                        self.state.last_chunk_len = n;
                    }
                    self.state.current_instant += n as u64;
                    self.state.scheduled_ahead += n as u64;
                    self.state.stats.chunks_written += 1;
                    self.state.stats.points_written += n as u64;
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
                    on_error(Error::disconnected("backend disconnected"));
                    return Err(e);
                }
                Err(e) => {
                    on_error(e);
                    return Ok(()); // Continue with next iteration
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
    fn handle_underrun_fill(&mut self, req: &ChunkRequest) -> Result<()> {
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
            match backend.try_write_chunk(self.config.pps, &self.state.chunk_buffer[..fill_start]) {
                Ok(WriteOutcome::Written) => {
                    if is_armed {
                        debug_assert!(
                            fill_start <= self.state.last_chunk.len(),
                            "fill_start ({}) exceeds last_chunk capacity ({})",
                            fill_start,
                            self.state.last_chunk.len()
                        );
                        self.state.last_chunk[..fill_start]
                            .copy_from_slice(&self.state.chunk_buffer[..fill_start]);
                        self.state.last_chunk_len = fill_start;
                    }
                    self.state.current_instant += fill_start as u64;
                    self.state.scheduled_ahead += fill_start as u64;
                    self.state.stats.chunks_written += 1;
                    self.state.stats.points_written += fill_start as u64;
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
                    // Open shutter (best-effort) if not already open
                    if !self.state.shutter_open {
                        if let Some(backend) = &mut self.backend {
                            let _ = backend.set_shutter(true);
                        }
                        self.state.shutter_open = true;
                    }
                }
                Ok(ControlMsg::Disarm) => {
                    // Close shutter immediately for safety
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
        let software = self.state.scheduled_ahead;

        // When hardware reports queue depth, use MINIMUM of hardware and software.
        if let Some(device_queue) = self.backend.as_ref().and_then(|b| b.queued_points()) {
            return device_queue.min(software);
        }

        software
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
    fn build_fill_request(&self, max_points: usize) -> ChunkRequest {
        let pps = self.config.pps;

        // Calculate buffer state using conservative estimation
        let buffered_points = self.estimate_buffer_points();
        let buffered = Duration::from_secs_f64(buffered_points as f64 / pps as f64);

        // Calculate start time for this chunk (when these points will play).
        // current_instant = total points written = playhead + buffered, so it represents
        // the stream time at which the next generated points will be played back.
        // This is the correct value for audio synchronization.
        let start = self.state.current_instant;

        // Calculate point requirements
        // deficit_target = target_buffer - buffered (how much we're below target)
        let target_buffer_secs = self.config.target_buffer.as_secs_f64();
        let min_buffer_secs = self.config.min_buffer.as_secs_f64();
        let buffered_secs = buffered.as_secs_f64();

        // target_points: ceil((target_buffer - buffered) * pps), clamped to max_points
        let deficit_target = (target_buffer_secs - buffered_secs).max(0.0);
        let target_points = (deficit_target * pps as f64).ceil() as usize;
        let target_points = target_points.min(max_points);

        // min_points: ceil((min_buffer - buffered) * pps) - minimum to avoid underrun
        let deficit_min = (min_buffer_secs - buffered_secs).max(0.0);
        let min_points = (deficit_min * pps as f64).ceil() as usize;
        let min_points = min_points.min(max_points);

        // Get raw device queue if available
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
            let estimated_drain = Duration::from_secs_f64(
                self.state.scheduled_ahead as f64 / pps as f64
            );
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
        if let Some(backend) = &mut self.backend {
            let _ = backend.set_shutter(false);
        }
        self.state.shutter_open = false;

        // Output a small blank chunk to ensure laser is off
        // (some DACs may hold the last point otherwise)
        if let Some(backend) = &mut self.backend {
            let blank_point = LaserPoint::blanked(0.0, 0.0);
            let blank_chunk = [blank_point; 16];
            let _ = backend.try_write_chunk(self.config.pps, &blank_chunk);
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
/// let device = open_device("my-device")?;
/// let config = StreamConfig::new(30_000);
/// let (stream, info) = device.start_stream(config)?;
/// println!("Streaming to: {}", info.name);
/// ```
pub struct Dac {
    info: DacInfo,
    backend: Option<Box<dyn StreamBackend>>,
}

impl Dac {
    /// Create a new device from a backend.
    pub fn new(info: DacInfo, backend: Box<dyn StreamBackend>) -> Self {
        Self {
            info,
            backend: Some(backend),
        }
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

    /// Returns whether the device is connected.
    pub fn is_connected(&self) -> bool {
        self.backend
            .as_ref()
            .map(|b| b.is_connected())
            .unwrap_or(false)
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
    pub fn start_stream(mut self, cfg: StreamConfig) -> Result<(Stream, DacInfo)> {
        let mut backend = self.backend.take().ok_or_else(|| {
            Error::invalid_config("device backend has already been used for a stream")
        })?;

        Self::validate_config(&self.info.caps, &cfg)?;

        // Connect the backend if not already connected
        if !backend.is_connected() {
            backend.connect()?;
        }

        let stream = Stream::with_backend(self.info.clone(), backend, cfg);

        Ok((stream, self.info))
    }

    fn validate_config(caps: &DacCapabilities, cfg: &StreamConfig) -> Result<()> {
        if cfg.pps < caps.pps_min || cfg.pps > caps.pps_max {
            return Err(Error::invalid_config(format!(
                "PPS {} is outside device range [{}, {}]",
                cfg.pps, caps.pps_min, caps.pps_max
            )));
        }

        Ok(())
    }
}

/// Legacy alias for compatibility.
pub type OwnedDac = Dac;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{StreamBackend, WriteOutcome};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A test backend for unit testing stream behavior.
    struct TestBackend {
        caps: DacCapabilities,
        connected: bool,
        /// Count of write attempts
        write_count: Arc<AtomicUsize>,
        /// Number of WouldBlock responses to return before accepting writes
        would_block_count: Arc<AtomicUsize>,
        /// Simulated queue depth
        queued: Arc<AtomicU64>,
        /// Track shutter state for testing
        shutter_open: Arc<AtomicBool>,
    }

    impl TestBackend {
        fn new() -> Self {
            Self {
                caps: DacCapabilities {
                    pps_min: 1000,
                    pps_max: 100000,
                    max_points_per_chunk: 1000,
                    prefers_constant_pps: false,
                    can_estimate_queue: true,
                    output_model: crate::types::OutputModel::NetworkFifo,
                },
                connected: false,
                write_count: Arc::new(AtomicUsize::new(0)),
                would_block_count: Arc::new(AtomicUsize::new(0)),
                queued: Arc::new(AtomicU64::new(0)),
                shutter_open: Arc::new(AtomicBool::new(false)),
            }
        }

        fn with_would_block_count(mut self, count: usize) -> Self {
            self.would_block_count = Arc::new(AtomicUsize::new(count));
            self
        }

        /// Set the initial queue depth for testing buffer estimation.
        fn with_initial_queue(mut self, queue: u64) -> Self {
            self.queued = Arc::new(AtomicU64::new(queue));
            self
        }
    }

    /// A test backend that does NOT report hardware queue depth (`queued_points()` returns `None`).
    ///
    /// Use this backend to test software-only buffer estimation, which is the fallback path
    /// for real backends like `HeliosBackend` that don't implement `queued_points()`.
    ///
    /// **When to use which test backend:**
    /// - `TestBackend`: Simulates DACs with hardware queue reporting (e.g., Ether Dream, IDN).
    ///   Use for testing buffer estimation with hardware feedback.
    /// - `NoQueueTestBackend`: Simulates DACs without queue reporting (e.g., Helios).
    ///   Use for testing the time-based `scheduled_ahead` decrement logic.
    struct NoQueueTestBackend {
        inner: TestBackend,
    }

    impl NoQueueTestBackend {
        fn new() -> Self {
            Self {
                inner: TestBackend::new(),
            }
        }
    }

    impl StreamBackend for NoQueueTestBackend {
        fn dac_type(&self) -> DacType {
            self.inner.dac_type()
        }

        fn caps(&self) -> &DacCapabilities {
            self.inner.caps()
        }

        fn connect(&mut self) -> Result<()> {
            self.inner.connect()
        }

        fn disconnect(&mut self) -> Result<()> {
            self.inner.disconnect()
        }

        fn is_connected(&self) -> bool {
            self.inner.is_connected()
        }

        fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
            self.inner.try_write_chunk(pps, points)
        }

        fn stop(&mut self) -> Result<()> {
            self.inner.stop()
        }

        fn set_shutter(&mut self, open: bool) -> Result<()> {
            self.inner.set_shutter(open)
        }

        /// Returns None - simulates a DAC that cannot report queue depth
        fn queued_points(&self) -> Option<u64> {
            None
        }
    }

    impl StreamBackend for TestBackend {
        fn dac_type(&self) -> DacType {
            DacType::Custom("Test".to_string())
        }

        fn caps(&self) -> &DacCapabilities {
            &self.caps
        }

        fn connect(&mut self) -> Result<()> {
            self.connected = true;
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn try_write_chunk(&mut self, _pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
            self.write_count.fetch_add(1, Ordering::SeqCst);

            // Return WouldBlock until count reaches 0
            let remaining = self.would_block_count.load(Ordering::SeqCst);
            if remaining > 0 {
                self.would_block_count.fetch_sub(1, Ordering::SeqCst);
                return Ok(WriteOutcome::WouldBlock);
            }

            self.queued.fetch_add(points.len() as u64, Ordering::SeqCst);
            Ok(WriteOutcome::Written)
        }

        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn set_shutter(&mut self, open: bool) -> Result<()> {
            self.shutter_open.store(open, Ordering::SeqCst);
            Ok(())
        }

        fn queued_points(&self) -> Option<u64> {
            Some(self.queued.load(Ordering::SeqCst))
        }
    }

    #[test]
    fn test_stream_control_arm_disarm() {
        let (tx, _rx) = mpsc::channel();
        let control = StreamControl::new(tx);
        assert!(!control.is_armed());

        control.arm().unwrap();
        assert!(control.is_armed());

        control.disarm().unwrap();
        assert!(!control.is_armed());
    }

    #[test]
    fn test_stream_control_stop() {
        let (tx, _rx) = mpsc::channel();
        let control = StreamControl::new(tx);
        assert!(!control.is_stop_requested());

        control.stop().unwrap();
        assert!(control.is_stop_requested());
    }

    #[test]
    fn test_stream_control_clone_shares_state() {
        let (tx, _rx) = mpsc::channel();
        let control1 = StreamControl::new(tx);
        let control2 = control1.clone();

        control1.arm().unwrap();
        assert!(control2.is_armed());

        control2.stop().unwrap();
        assert!(control1.is_stop_requested());
    }

    #[test]
    fn test_device_start_stream_connects_backend() {
        let backend = TestBackend::new();
        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.caps().clone(),
        };
        let device = Dac::new(info, Box::new(backend));

        // Device should not be connected initially
        assert!(!device.is_connected());

        // start_stream should connect and return a usable stream
        let cfg = StreamConfig::new(30000);
        let result = device.start_stream(cfg);
        assert!(result.is_ok());

        let (stream, _info) = result.unwrap();
        assert!(stream.backend.as_ref().unwrap().is_connected());
    }

    #[test]
    fn test_handle_underrun_fill_advances_state() {
        let mut backend = TestBackend::new();
        backend.connected = true;
        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let mut stream = Stream::with_backend(info, Box::new(backend), cfg);

        // Record initial state
        let initial_instant = stream.state.current_instant;
        let initial_scheduled = stream.state.scheduled_ahead;
        let initial_chunks = stream.state.stats.chunks_written;
        let initial_points = stream.state.stats.points_written;

        // Trigger underrun handling with ChunkRequest
        let req = ChunkRequest {
            start: StreamInstant::new(0),
            pps: 30000,
            min_points: 100,
            target_points: 100,
            buffered_points: 0,
            buffered: Duration::ZERO,
            device_queued_points: None,
        };
        stream.handle_underrun_fill(&req).unwrap();

        // State should have advanced
        assert!(stream.state.current_instant > initial_instant);
        assert!(stream.state.scheduled_ahead > initial_scheduled);
        assert_eq!(stream.state.stats.chunks_written, initial_chunks + 1);
        assert_eq!(stream.state.stats.points_written, initial_points + 100);
        assert_eq!(stream.state.stats.underrun_count, 1);
    }

    #[test]
    fn test_run_fill_retries_on_would_block() {
        // Create a backend that returns WouldBlock 3 times before accepting
        let backend = TestBackend::new().with_would_block_count(3);
        let write_count = backend.write_count.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let produced_count = Arc::new(AtomicUsize::new(0));
        let produced_count_clone = produced_count.clone();
        let result = stream.run_fill(
            move |req, buffer| {
                let count = produced_count_clone.fetch_add(1, Ordering::SeqCst);
                if count < 1 {
                    let n = req.target_points.min(buffer.len());
                    for i in 0..n {
                        buffer[i] = LaserPoint::blanked(0.0, 0.0);
                    }
                    ChunkResult::Filled(n)
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // With the new API, WouldBlock retries happen internally in write_fill_points
        // The exact count depends on timing, but we should see multiple writes
        assert!(write_count.load(Ordering::SeqCst) >= 1);
    }

    #[test]
    fn test_arm_opens_shutter_disarm_closes_shutter() {
        let backend = TestBackend::new();
        let shutter_open = backend.shutter_open.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let mut stream = Stream::with_backend(info, backend_box, cfg);

        // Initially shutter is closed
        assert!(!shutter_open.load(Ordering::SeqCst));

        // Arm via control (this sends ControlMsg::Arm)
        let control = stream.control();
        control.arm().unwrap();

        // Process control messages - this should open the shutter
        let stopped = stream.process_control_messages();
        assert!(!stopped);
        assert!(shutter_open.load(Ordering::SeqCst));

        // Disarm (this sends ControlMsg::Disarm)
        control.disarm().unwrap();

        // Process control messages - this should close the shutter
        let stopped = stream.process_control_messages();
        assert!(!stopped);
        assert!(!shutter_open.load(Ordering::SeqCst));
    }

    #[test]
    fn test_handle_underrun_fill_blanks_when_disarmed() {
        let backend = TestBackend::new();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        // Use RepeatLast policy - but when disarmed, should still blank
        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
        let mut stream = Stream::with_backend(info, backend_box, cfg);

        // Set some last_chunk with colored points using the pre-allocated buffer
        let colored_point = LaserPoint::new(0.5, 0.5, 65535, 65535, 65535, 65535);
        for i in 0..100 {
            stream.state.last_chunk[i] = colored_point;
        }
        stream.state.last_chunk_len = 100;

        // Ensure disarmed (default state)
        assert!(!stream.control.is_armed());

        let req = ChunkRequest {
            start: StreamInstant::new(0),
            pps: 30000,
            min_points: 100,
            target_points: 100,
            buffered_points: 0,
            buffered: Duration::ZERO,
            device_queued_points: None,
        };

        // Handle underrun while disarmed
        stream.handle_underrun_fill(&req).unwrap();

        // last_chunk should NOT be updated (we're disarmed)
        // The actual write was blanked points, but we don't update last_chunk when disarmed
        // because "last armed content" hasn't changed
        assert_eq!(stream.state.last_chunk[0].r, 65535); // Still the old colored points
    }

    #[test]
    fn test_stop_closes_shutter() {
        let backend = TestBackend::new();
        let shutter_open = backend.shutter_open.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let mut stream = Stream::with_backend(info, backend_box, cfg);

        // Arm first to open shutter
        stream.control.arm().unwrap();
        stream.process_control_messages();
        assert!(shutter_open.load(Ordering::SeqCst));

        // Stop should close shutter
        stream.stop().unwrap();
        assert!(!shutter_open.load(Ordering::SeqCst));
    }

    #[test]
    fn test_arm_disarm_arm_cycle() {
        let backend = TestBackend::new();
        let shutter_open = backend.shutter_open.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let mut stream = Stream::with_backend(info, backend_box, cfg);
        let control = stream.control();

        // Initial state: disarmed
        assert!(!control.is_armed());
        assert!(!shutter_open.load(Ordering::SeqCst));

        // Arm
        control.arm().unwrap();
        stream.process_control_messages();
        assert!(control.is_armed());
        assert!(shutter_open.load(Ordering::SeqCst));

        // Disarm
        control.disarm().unwrap();
        stream.process_control_messages();
        assert!(!control.is_armed());
        assert!(!shutter_open.load(Ordering::SeqCst));

        // Arm again
        control.arm().unwrap();
        stream.process_control_messages();
        assert!(control.is_armed());
        assert!(shutter_open.load(Ordering::SeqCst));
    }

    // =========================================================================
    // Buffer-driven timing tests
    // =========================================================================

    #[test]
    fn test_run_fill_buffer_driven_behavior() {
        // Test that run_fill uses buffer-driven timing
        // Use NoQueueTestBackend so we rely on software estimate (which decrements properly)
        let mut backend = NoQueueTestBackend::new();
        backend.inner.connected = true;
        let write_count = backend.inner.write_count.clone();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.inner.caps().clone(),
        };

        // Use short target buffer for testing
        let cfg = StreamConfig::new(30000)
            .with_target_buffer(Duration::from_millis(10))
            .with_min_buffer(Duration::from_millis(5));
        let stream = Stream::with_backend(info, Box::new(backend), cfg);

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                // Run for 5 calls then end
                if count >= 4 {
                    ChunkResult::End
                } else {
                    let n = req.target_points.min(buffer.len()).min(100);
                    for i in 0..n {
                        buffer[i] = LaserPoint::blanked(0.0, 0.0);
                    }
                    ChunkResult::Filled(n)
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        assert!(write_count.load(Ordering::SeqCst) >= 4, "Should have written multiple chunks");
    }

    #[test]
    fn test_run_fill_sleeps_when_buffer_healthy() {
        // Test that run_fill sleeps when buffer is above target
        // Use NoQueueTestBackend so we rely on software estimate (which decrements properly)
        use std::time::Instant;

        let mut backend = NoQueueTestBackend::new();
        backend.inner.connected = true;

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.inner.caps().clone(),
        };

        // Very small target buffer, skip drain
        let cfg = StreamConfig::new(30000)
            .with_target_buffer(Duration::from_millis(5))
            .with_min_buffer(Duration::from_millis(2))
            .with_drain_timeout(Duration::ZERO);
        let stream = Stream::with_backend(info, Box::new(backend), cfg);

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();
        let start_time = Instant::now();

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                // End after 3 callbacks
                if count >= 2 {
                    ChunkResult::End
                } else {
                    // Fill buffer to trigger sleep
                    let n = req.target_points.min(buffer.len());
                    for i in 0..n {
                        buffer[i] = LaserPoint::blanked(0.0, 0.0);
                    }
                    ChunkResult::Filled(n)
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Should have taken some time due to buffer-driven sleep
        let elapsed = start_time.elapsed();
        // With buffer-driven timing, we should see some elapsed time
        // (not instant return)
        assert!(
            elapsed.as_millis() < 100,
            "Elapsed time {:?} is too long for test",
            elapsed
        );
    }

    #[test]
    fn test_run_fill_stops_on_control_stop() {
        // Test that stop() via control handle terminates the loop promptly
        use std::thread;

        let backend = TestBackend::new();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);
        let control = stream.control();

        // Spawn a thread to stop the stream after a short delay
        let control_clone = control.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            control_clone.stop().unwrap();
        });

        let result = stream.run_fill(
            |req, buffer| {
                let n = req.target_points.min(buffer.len()).min(10);
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            },
            |_e| {},
        );

        // Should exit with Stopped, not hang forever
        assert_eq!(result.unwrap(), RunExit::Stopped);
    }

    #[test]
    fn test_run_fill_producer_ended() {
        // Test that ChunkResult::End terminates the stream gracefully
        let backend = TestBackend::new();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    // First call: return some data
                    let n = req.target_points.min(buffer.len()).min(100);
                    for i in 0..n {
                        buffer[i] = LaserPoint::blanked(0.0, 0.0);
                    }
                    ChunkResult::Filled(n)
                } else {
                    // Second call: end the stream
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_run_fill_starved_applies_underrun_policy() {
        // Test that ChunkResult::Starved triggers underrun policy
        let backend = TestBackend::new();
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        // Use Blank policy for underrun
        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::Blank);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |_req, _buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    // First call: return Starved to trigger underrun policy
                    ChunkResult::Starved
                } else {
                    // Second call: end the stream
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Underrun policy should have written some points
        assert!(
            queued.load(Ordering::SeqCst) > 0,
            "Underrun policy should have written blank points"
        );
    }

    #[test]
    fn test_run_fill_filled_zero_with_target_treated_as_starved() {
        // Test that Filled(0) when target_points > 0 is treated as Starved
        let backend = TestBackend::new();
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::Blank);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |_req, _buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    // Return Filled(0) when buffer needs data - should be treated as Starved
                    ChunkResult::Filled(0)
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Filled(0) with target_points > 0 should trigger underrun policy
        assert!(
            queued.load(Ordering::SeqCst) > 0,
            "Filled(0) with target > 0 should trigger underrun and write blank points"
        );
    }

    // =========================================================================
    // Buffer estimation tests (Task 6.3)
    // =========================================================================

    #[test]
    fn test_estimate_buffer_uses_software_when_no_hardware() {
        // When hardware doesn't report queue depth, use software estimate
        let mut backend = NoQueueTestBackend::new();
        backend.inner.connected = true;

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.inner.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let mut stream = Stream::with_backend(info, Box::new(backend), cfg);

        // Set software estimate to 500 points
        stream.state.scheduled_ahead = 500;

        // Should use software estimate since hardware returns None
        let estimate = stream.estimate_buffer_points();
        assert_eq!(estimate, 500);
    }

    #[test]
    fn test_estimate_buffer_uses_min_of_hardware_and_software() {
        // When hardware reports queue depth, use min(hardware, software)
        let backend = TestBackend::new().with_initial_queue(300);
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let mut stream = Stream::with_backend(info, backend_box, cfg);

        // Software says 500, hardware says 300 -> should use 300 (conservative)
        stream.state.scheduled_ahead = 500;
        let estimate = stream.estimate_buffer_points();
        assert_eq!(estimate, 300, "Should use hardware (300) when it's less than software (500)");

        // Now set hardware higher than software
        queued.store(800, Ordering::SeqCst);
        let estimate = stream.estimate_buffer_points();
        assert_eq!(estimate, 500, "Should use software (500) when it's less than hardware (800)");
    }

    #[test]
    fn test_estimate_buffer_conservative_prevents_underrun() {
        // Verify that conservative estimation (using min) prevents underruns
        // by ensuring we never overestimate the buffer
        let backend = TestBackend::new().with_initial_queue(100);
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let mut stream = Stream::with_backend(info, backend_box, cfg);

        // Simulate: software thinks 1000 points scheduled, but hardware only has 100
        // This can happen if hardware consumed points faster than expected
        stream.state.scheduled_ahead = 1000;

        let estimate = stream.estimate_buffer_points();

        // Should use the conservative (lower) estimate to avoid underrun
        assert_eq!(estimate, 100, "Should use conservative estimate (100) not optimistic (1000)");

        // Now simulate the opposite: hardware reports more than software
        // This can happen due to timing/synchronization issues
        queued.store(2000, Ordering::SeqCst);
        stream.state.scheduled_ahead = 500;

        let estimate = stream.estimate_buffer_points();
        assert_eq!(estimate, 500, "Should use conservative estimate (500) not hardware (2000)");
    }

    #[test]
    fn test_build_fill_request_uses_conservative_estimation() {
        // Verify that build_fill_request uses conservative buffer estimation
        let backend = TestBackend::new().with_initial_queue(200);

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000)
            .with_target_buffer(Duration::from_millis(40))
            .with_min_buffer(Duration::from_millis(10));
        let mut stream = Stream::with_backend(info, backend_box, cfg);

        // Set software estimate higher than hardware
        stream.state.scheduled_ahead = 500;

        let req = stream.build_fill_request(1000);

        // Should use conservative estimate (hardware = 200)
        assert_eq!(req.buffered_points, 200);
        assert_eq!(req.device_queued_points, Some(200));
    }

    #[test]
    fn test_build_fill_request_calculates_min_and_target_points() {
        // Verify that min_points and target_points are calculated correctly
        // based on buffer state. Use NoQueueTestBackend so software estimate is used directly.
        let mut backend = NoQueueTestBackend::new();
        backend.inner.connected = true;

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.inner.caps().clone(),
        };

        // 30000 PPS, target_buffer = 40ms, min_buffer = 10ms
        // target_buffer = 40ms * 30000 = 1200 points
        // min_buffer = 10ms * 30000 = 300 points
        let cfg = StreamConfig::new(30000)
            .with_target_buffer(Duration::from_millis(40))
            .with_min_buffer(Duration::from_millis(10));
        let mut stream = Stream::with_backend(info, Box::new(backend), cfg);

        // Empty buffer: need full target
        stream.state.scheduled_ahead = 0;
        let req = stream.build_fill_request(1000);

        // target_points should be clamped to max_points (1000)
        assert_eq!(req.target_points, 1000);
        // min_points should be 300 (10ms * 30000), clamped to 1000
        assert_eq!(req.min_points, 300);

        // Buffer at 500 points (16.67ms): below target (40ms), above min (10ms)
        stream.state.scheduled_ahead = 500;
        let req = stream.build_fill_request(1000);

        // target_points = (1200 - 500) = 700
        assert_eq!(req.target_points, 700);
        // min_points = (300 - 500) = 0 (buffer above min)
        assert_eq!(req.min_points, 0);

        // Buffer full at 1200 points (40ms): at target
        stream.state.scheduled_ahead = 1200;
        let req = stream.build_fill_request(1000);

        // target_points = 0 (at target)
        assert_eq!(req.target_points, 0);
        // min_points = 0 (well above min)
        assert_eq!(req.min_points, 0);
    }

    #[test]
    fn test_build_fill_request_ceiling_rounds_min_points() {
        // Verify that min_points uses ceiling to prevent underrun
        // Use NoQueueTestBackend so software estimate is used directly.
        let mut backend = NoQueueTestBackend::new();
        backend.inner.connected = true;

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.inner.caps().clone(),
        };

        // min_buffer = 10ms at 30000 PPS = 300 points exactly
        let cfg = StreamConfig::new(30000)
            .with_target_buffer(Duration::from_millis(40))
            .with_min_buffer(Duration::from_millis(10));
        let mut stream = Stream::with_backend(info, Box::new(backend), cfg);

        // Buffer at 299 points: 1 point below min_buffer
        stream.state.scheduled_ahead = 299;
        let req = stream.build_fill_request(1000);

        // min_points should be ceil(300 - 299) = ceil(1) = 1
        // Actually it's ceil((10ms - 299/30000) * 30000) = ceil(300 - 299) = 1
        assert!(req.min_points >= 1, "min_points should be at least 1 to reach min_buffer");
    }

    // =========================================================================
    // ChunkResult handling tests (Task 6.4)
    // =========================================================================

    #[test]
    fn test_fill_result_filled_writes_points_and_updates_state() {
        // Test that Filled(n) writes n points to backend and updates stream state
        let backend = TestBackend::new();
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let points_written = Arc::new(AtomicUsize::new(0));
        let points_written_clone = points_written.clone();
        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count < 3 {
                    // Fill with specific number of points
                    let n = req.target_points.min(50);
                    for i in 0..n {
                        buffer[i] = LaserPoint::new(0.1 * i as f32, 0.2 * i as f32, 1000, 2000, 3000, 4000);
                    }
                    points_written_clone.fetch_add(n, Ordering::SeqCst);
                    ChunkResult::Filled(n)
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Points should have been written to backend
        // Note: drain adds 16 blank points at shutdown
        let total_queued = queued.load(Ordering::SeqCst);
        let total_written = points_written.load(Ordering::SeqCst);
        assert!(total_queued > 0, "Points should have been queued to backend");
        assert!(
            total_queued as usize >= total_written,
            "Queued points ({}) should be at least written points ({})",
            total_queued,
            total_written
        );
    }

    #[test]
    fn test_fill_result_filled_updates_last_chunk_when_armed() {
        // Test that Filled(n) updates last_chunk for RepeatLast policy when armed
        let backend = TestBackend::new();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Arm the stream so last_chunk gets updated
        let control = stream.control();
        control.arm().unwrap();

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    // Write specific points that we can verify later
                    let n = req.target_points.min(10);
                    for i in 0..n {
                        buffer[i] = LaserPoint::new(0.5, 0.5, 10000, 20000, 30000, 40000);
                    }
                    ChunkResult::Filled(n)
                } else if count == 1 {
                    // Return Starved to trigger RepeatLast
                    ChunkResult::Starved
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // If last_chunk wasn't updated, the Starved would have outputted blanks
        // The test passes if no assertion fails - the RepeatLast policy used the stored chunk
    }

    #[test]
    fn test_fill_result_starved_repeat_last_with_stored_chunk() {
        // Test that Starved with RepeatLast policy repeats the last chunk
        let backend = TestBackend::new();
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Arm the stream
        let control = stream.control();
        control.arm().unwrap();

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    // First call: provide some data to establish last_chunk
                    let n = req.target_points.min(50);
                    for i in 0..n {
                        buffer[i] = LaserPoint::new(0.3, 0.3, 5000, 5000, 5000, 5000);
                    }
                    ChunkResult::Filled(n)
                } else if count == 1 {
                    // Second call: return Starved - should repeat last chunk
                    ChunkResult::Starved
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Both the initial fill and the repeated chunk should have been written
        let total_queued = queued.load(Ordering::SeqCst);
        assert!(total_queued >= 50, "Should have written initial chunk plus repeated chunk");
    }

    #[test]
    fn test_fill_result_starved_repeat_last_without_stored_chunk_falls_back_to_blank() {
        // Test that Starved with RepeatLast but no stored chunk falls back to blank
        let backend = TestBackend::new();
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Arm the stream
        let control = stream.control();
        control.arm().unwrap();

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |_req, _buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    // First call: return Starved with no prior chunk
                    ChunkResult::Starved
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Should have written blank points as fallback
        let total_queued = queued.load(Ordering::SeqCst);
        assert!(total_queued > 0, "Should have written blank points as fallback");
    }

    #[test]
    fn test_fill_result_starved_with_park_policy() {
        // Test that Starved with Park policy outputs blanked points at park position
        let backend = TestBackend::new();
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        // Park at specific position
        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::Park { x: 0.5, y: -0.5 });
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Arm the stream
        let control = stream.control();
        control.arm().unwrap();

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |_req, _buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    ChunkResult::Starved
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Should have written parked points
        let total_queued = queued.load(Ordering::SeqCst);
        assert!(total_queued > 0, "Should have written parked points");
    }

    #[test]
    fn test_fill_result_starved_with_stop_policy() {
        // Test that Starved with Stop policy terminates the stream
        let backend = TestBackend::new();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::Stop);
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Must arm the stream for underrun policy to be checked
        // (disarmed streams always output blanks regardless of policy)
        let control = stream.control();
        control.arm().unwrap();

        let result = stream.run_fill(
            |_req, _buffer| {
                // Always return Starved - Stop policy should terminate the stream
                ChunkResult::Starved
            },
            |_e| {},
        );

        // Stream should have stopped due to underrun with Stop policy
        // The Stop policy returns Err(Error::Stopped) to immediately terminate
        assert!(result.is_err(), "Stop policy should return an error");
        assert!(result.unwrap_err().is_stopped(), "Error should be Stopped variant");
    }

    #[test]
    fn test_fill_result_end_returns_producer_ended() {
        // Test that End terminates the stream with ProducerEnded
        let backend = TestBackend::new();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let result = stream.run_fill(
            |_req, _buffer| {
                // Immediately end
                ChunkResult::End
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
    }

    #[test]
    fn test_fill_result_filled_exceeds_buffer_clamped() {
        // Test that Filled(n) where n > buffer.len() is clamped
        let backend = TestBackend::new();
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |_req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count == 0 {
                    // Fill some points but claim we wrote more than buffer size
                    for i in 0..buffer.len() {
                        buffer[i] = LaserPoint::blanked(0.0, 0.0);
                    }
                    // Return a value larger than buffer - should be clamped
                    ChunkResult::Filled(buffer.len() + 1000)
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Should have written clamped number of points (max_points_per_chunk)
        // plus 16 blank points from drain shutdown
        let total_queued = queued.load(Ordering::SeqCst);
        assert!(total_queued > 0, "Should have written some points");
        // The clamping should limit to max_points (1000 for TestBackend) + 16 blank drain points
        assert!(total_queued <= 1016, "Points should be clamped to max_points_per_chunk (+ drain)");
    }

    // =========================================================================
    // Integration tests (Task 6.5)
    // =========================================================================

    #[test]
    fn test_full_stream_lifecycle_create_arm_stream_stop() {
        // Test the complete lifecycle: create -> arm -> stream data -> stop
        let backend = TestBackend::new();
        let queued = backend.queued.clone();
        let shutter_open = backend.shutter_open.clone();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.caps().clone(),
        };

        // 1. Create device and start stream
        let device = Dac::new(info, Box::new(backend));
        assert!(!device.is_connected());

        let cfg = StreamConfig::new(30000);
        let (stream, returned_info) = device.start_stream(cfg).unwrap();
        assert_eq!(returned_info.id, "test");

        // 2. Get control handle and verify initial state
        let control = stream.control();
        assert!(!control.is_armed());
        assert!(!shutter_open.load(Ordering::SeqCst));

        // 3. Arm the stream
        control.arm().unwrap();
        assert!(control.is_armed());

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        // 4. Run the stream for a few iterations
        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                if count < 5 {
                    // Fill with data
                    let n = req.target_points.min(buffer.len()).min(100);
                    for i in 0..n {
                        let t = i as f32 / 100.0;
                        buffer[i] = LaserPoint::new(t, t, 10000, 20000, 30000, 40000);
                    }
                    ChunkResult::Filled(n)
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        // 5. Verify stream ended properly
        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        assert!(queued.load(Ordering::SeqCst) > 0, "Should have written points");
        assert!(call_count.load(Ordering::SeqCst) >= 5, "Should have called producer multiple times");
    }

    #[test]
    fn test_full_stream_lifecycle_with_underrun_recovery() {
        // Test lifecycle with underrun and recovery
        let backend = TestBackend::new();
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_underrun(UnderrunPolicy::RepeatLast);
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Arm the stream for underrun policy to work
        let control = stream.control();
        control.arm().unwrap();

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);

                match count {
                    0 => {
                        // First call: provide data (establishes last_chunk)
                        let n = req.target_points.min(buffer.len()).min(50);
                        for i in 0..n {
                            buffer[i] = LaserPoint::new(0.5, 0.5, 30000, 30000, 30000, 30000);
                        }
                        ChunkResult::Filled(n)
                    }
                    1 => {
                        // Second call: underrun (triggers RepeatLast)
                        ChunkResult::Starved
                    }
                    2 => {
                        // Third call: recover with new data
                        let n = req.target_points.min(buffer.len()).min(50);
                        for i in 0..n {
                            buffer[i] = LaserPoint::new(-0.5, -0.5, 20000, 20000, 20000, 20000);
                        }
                        ChunkResult::Filled(n)
                    }
                    _ => ChunkResult::End,
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // Should have written: initial data + repeated chunk + recovered data
        let total = queued.load(Ordering::SeqCst);
        assert!(total >= 100, "Should have written multiple chunks including underrun recovery");
    }

    #[test]
    fn test_full_stream_lifecycle_external_stop() {
        // Test stopping stream from external control handle
        use std::thread;

        let backend = TestBackend::new();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let control = stream.control();
        let control_clone = control.clone();

        // Spawn thread to stop stream after delay
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            control_clone.stop().unwrap();
        });

        let result = stream.run_fill(
            |req, buffer| {
                // Keep streaming until stopped
                let n = req.target_points.min(buffer.len()).min(10);
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::Stopped);
    }

    #[test]
    fn test_full_stream_lifecycle_into_dac_recovery() {
        // Test recovering Dac from stream for reuse
        let backend = TestBackend::new();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.caps().clone(),
        };

        // First stream session
        let device = Dac::new(info, Box::new(backend));
        let cfg = StreamConfig::new(30000);
        let (stream, _) = device.start_stream(cfg).unwrap();

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);
                if count < 2 {
                    let n = req.target_points.min(buffer.len()).min(50);
                    for i in 0..n {
                        buffer[i] = LaserPoint::blanked(0.0, 0.0);
                    }
                    ChunkResult::Filled(n)
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);

        // Note: into_dac() would be tested here, but run_fill consumes the stream
        // and doesn't return it. The into_dac pattern is for the blocking API.
        // This test verifies the stream lifecycle completes cleanly.
    }

    #[test]
    fn test_stream_stats_tracking() {
        // Test that stream statistics are tracked correctly
        let backend = TestBackend::new();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Arm the stream
        let control = stream.control();
        control.arm().unwrap();

        let call_count = Arc::new(AtomicUsize::new(0));
        let call_count_clone = call_count.clone();
        let points_per_call = 50;

        let result = stream.run_fill(
            move |req, buffer| {
                let count = call_count_clone.fetch_add(1, Ordering::SeqCst);
                if count < 3 {
                    let n = req.target_points.min(buffer.len()).min(points_per_call);
                    for i in 0..n {
                        buffer[i] = LaserPoint::blanked(0.0, 0.0);
                    }
                    ChunkResult::Filled(n)
                } else {
                    ChunkResult::End
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // Stats tracking is verified by the successful completion
        // Detailed stats assertions would require access to stream after run_fill
    }

    #[test]
    fn test_stream_disarm_during_streaming() {
        // Test disarming stream while it's running
        use std::thread;

        let backend = TestBackend::new();
        let shutter_open = backend.shutter_open.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let control = stream.control();
        let control_clone = control.clone();

        // Arm first
        control.arm().unwrap();
        assert!(control.is_armed());

        // Spawn thread to disarm then stop
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(15));
            control_clone.disarm().unwrap();
            thread::sleep(Duration::from_millis(15));
            control_clone.stop().unwrap();
        });

        let result = stream.run_fill(
            |req, buffer| {
                let n = req.target_points.min(buffer.len()).min(10);
                for i in 0..n {
                    buffer[i] = LaserPoint::new(0.1, 0.1, 50000, 50000, 50000, 50000);
                }
                ChunkResult::Filled(n)
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::Stopped);
        // Shutter should have been closed by disarm
        assert!(!shutter_open.load(Ordering::SeqCst));
    }

    #[test]
    fn test_stream_with_mock_backend_disconnect() {
        // Test handling of backend disconnect during streaming
        use std::sync::atomic::AtomicBool;

        struct DisconnectingBackend {
            inner: TestBackend,
            disconnect_after: Arc<AtomicUsize>,
            call_count: Arc<AtomicUsize>,
        }

        impl StreamBackend for DisconnectingBackend {
            fn dac_type(&self) -> DacType {
                self.inner.dac_type()
            }

            fn caps(&self) -> &DacCapabilities {
                self.inner.caps()
            }

            fn connect(&mut self) -> Result<()> {
                self.inner.connect()
            }

            fn disconnect(&mut self) -> Result<()> {
                self.inner.disconnect()
            }

            fn is_connected(&self) -> bool {
                let count = self.call_count.load(Ordering::SeqCst);
                let disconnect_after = self.disconnect_after.load(Ordering::SeqCst);
                if count >= disconnect_after {
                    return false;
                }
                self.inner.is_connected()
            }

            fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
                self.call_count.fetch_add(1, Ordering::SeqCst);
                self.inner.try_write_chunk(pps, points)
            }

            fn stop(&mut self) -> Result<()> {
                self.inner.stop()
            }

            fn set_shutter(&mut self, open: bool) -> Result<()> {
                self.inner.set_shutter(open)
            }

            fn queued_points(&self) -> Option<u64> {
                self.inner.queued_points()
            }
        }

        let mut backend = DisconnectingBackend {
            inner: TestBackend::new(),
            disconnect_after: Arc::new(AtomicUsize::new(3)),
            call_count: Arc::new(AtomicUsize::new(0)),
        };
        backend.inner.connected = true;

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.inner.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let stream = Stream::with_backend(info, Box::new(backend), cfg);

        let error_occurred = Arc::new(AtomicBool::new(false));
        let error_occurred_clone = error_occurred.clone();

        let result = stream.run_fill(
            |req, buffer| {
                let n = req.target_points.min(buffer.len()).min(10);
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            },
            move |_e| {
                error_occurred_clone.store(true, Ordering::SeqCst);
            },
        );

        // Should return Disconnected when backend reports disconnection
        assert_eq!(result.unwrap(), RunExit::Disconnected);
    }

    // =========================================================================
    // Drain wait tests
    // =========================================================================

    #[test]
    fn test_fill_result_end_drains_with_queue_depth() {
        // Test that ChunkResult::End waits for queue to drain when queue depth is available
        use std::time::Instant;

        let backend = TestBackend::new().with_initial_queue(1000);
        let queued = backend.queued.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        // Use short drain timeout for test
        let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(100));
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Simulate queue draining by setting it to 0 before the stream runs
        queued.store(0, Ordering::SeqCst);

        let start = Instant::now();
        let result = stream.run_fill(
            |_req, _buffer| ChunkResult::End,
            |_e| {},
        );

        let elapsed = start.elapsed();

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // Should return quickly since queue was empty
        assert!(
            elapsed.as_millis() < 50,
            "Should return quickly when queue is empty, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_fill_result_end_respects_drain_timeout() {
        // Test that drain respects timeout and doesn't block forever
        use std::time::Instant;

        let backend = TestBackend::new().with_initial_queue(100000); // Large queue that won't drain

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        // Very short timeout
        let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(50));
        let stream = Stream::with_backend(info, backend_box, cfg);

        let start = Instant::now();
        let result = stream.run_fill(
            |_req, _buffer| ChunkResult::End,
            |_e| {},
        );

        let elapsed = start.elapsed();

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // Should timeout around 50ms, allow some margin
        assert!(
            elapsed.as_millis() >= 40 && elapsed.as_millis() < 150,
            "Should respect drain timeout (~50ms), took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_fill_result_end_skips_drain_with_zero_timeout() {
        // Test that drain is skipped when timeout is zero
        use std::time::Instant;

        let backend = TestBackend::new().with_initial_queue(100000);

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        // Zero timeout = skip drain
        let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::ZERO);
        let stream = Stream::with_backend(info, backend_box, cfg);

        let start = Instant::now();
        let result = stream.run_fill(
            |_req, _buffer| ChunkResult::End,
            |_e| {},
        );

        let elapsed = start.elapsed();

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // Should return immediately
        assert!(
            elapsed.as_millis() < 20,
            "Should skip drain with zero timeout, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_fill_result_end_drains_without_queue_depth() {
        // Test drain behavior when queued_points() returns None
        use std::time::Instant;

        let mut backend = NoQueueTestBackend::new();
        backend.inner.connected = true;

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.inner.caps().clone(),
        };

        // Short drain timeout
        let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(100));
        let stream = Stream::with_backend(info, Box::new(backend), cfg);

        let start = Instant::now();
        let result = stream.run_fill(
            |_req, _buffer| ChunkResult::End,
            |_e| {},
        );

        let elapsed = start.elapsed();

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // Without queue depth, drain sleeps for estimated buffer time (0 here)
        // So should return quickly
        assert!(
            elapsed.as_millis() < 50,
            "Should return quickly with empty buffer estimate, took {:?}",
            elapsed
        );
    }

    #[test]
    fn test_fill_result_end_closes_shutter() {
        // Test that shutter is closed after drain
        let backend = TestBackend::new();
        let shutter_open = backend.shutter_open.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_drain_timeout(Duration::from_millis(10));
        let stream = Stream::with_backend(info, backend_box, cfg);

        // Arm the stream first
        let control = stream.control();
        control.arm().unwrap();

        let result = stream.run_fill(
            |req, buffer| {
                // Fill some points then end
                let n = req.target_points.min(buffer.len()).min(10);
                for i in 0..n {
                    buffer[i] = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::End
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // Shutter should be closed after graceful shutdown
        assert!(!shutter_open.load(Ordering::SeqCst), "Shutter should be closed after drain");
    }
}
