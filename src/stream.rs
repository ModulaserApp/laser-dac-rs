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
//! No automatic reconnection. On disconnect, create a new `Dac` and `Stream`.
//! New streams always start disarmed for safety.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{Error, Result, StreamBackend, WriteOutcome};
use crate::types::{
    ChunkRequest, DacCapabilities, DacInfo, DacType, LaserPoint, OutputModel, RunExit,
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
    /// `target_queue_points` bounds this latency.
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
    /// Last chunk that was produced (for repeat-last underrun policy).
    last_chunk: Option<Vec<LaserPoint>>,
    /// Statistics.
    stats: StreamStats,
    /// Track the last armed state to detect transitions.
    last_armed: bool,
    /// Whether the hardware shutter is currently open.
    shutter_open: bool,
}

impl StreamState {
    fn new() -> Self {
        Self {
            current_instant: StreamInstant::new(0),
            scheduled_ahead: 0,
            last_chunk: None,
            stats: StreamStats::default(),
            last_armed: false,
            shutter_open: false,
        }
    }
}

// =============================================================================
// Stream
// =============================================================================

/// A streaming session for outputting point chunks to a DAC.
///
/// The stream provides two modes of operation:
///
/// - **Blocking mode**: Call `next_request()` to get what to produce, then `write()`.
/// - **Callback mode**: Call `run()` with a producer closure.
///
/// The stream owns pacing, backpressure, and the timebase (`StreamInstant`).
pub struct Stream {
    /// Device info for this stream.
    info: DacInfo,
    /// The backend.
    backend: Option<Box<dyn StreamBackend>>,
    /// Stream configuration.
    config: StreamConfig,
    /// Resolved chunk size.
    chunk_points: usize,
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
        chunk_points: usize,
    ) -> Self {
        let (control_tx, control_rx) = mpsc::channel();
        Self {
            info,
            backend: Some(backend),
            config,
            chunk_points,
            control: StreamControl::new(control_tx),
            control_rx,
            state: StreamState::new(),
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

    /// The resolved chunk size chosen for this stream.
    ///
    /// This is fixed for the lifetime of the stream.
    pub fn chunk_points(&self) -> usize {
        self.chunk_points
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
            chunk_points: self.chunk_points,
            scheduled_ahead_points: self.state.scheduled_ahead,
            device_queued_points,
            stats: Some(self.state.stats.clone()),
        })
    }

    /// Blocks until the stream wants the next chunk.
    ///
    /// Returns a `ChunkRequest` describing exactly what to produce.
    /// The producer must return exactly `req.n_points` points.
    pub fn next_request(&mut self) -> Result<ChunkRequest> {
        // Check for stop request
        if self.control.is_stop_requested() {
            return Err(Error::Stopped);
        }

        // Check for backend
        let backend = self
            .backend
            .as_ref()
            .ok_or_else(|| Error::disconnected("no backend"))?;

        if !backend.is_connected() {
            return Err(Error::disconnected("backend disconnected"));
        }

        // Wait for the right time to request the next chunk.
        self.wait_for_ready()?;

        let device_queued_points = self.backend.as_ref().and_then(|b| b.queued_points());

        Ok(ChunkRequest {
            start: self.state.current_instant,
            pps: self.config.pps,
            n_points: self.chunk_points,
            scheduled_ahead_points: self.state.scheduled_ahead,
            device_queued_points,
        })
    }

    /// Writes exactly `req.n_points` points for the given request.
    ///
    /// If the device cannot accept data immediately (backpressure), this method
    /// retries automatically with brief sleeps until the write succeeds or the
    /// stream is stopped.
    ///
    /// # Contract
    ///
    /// - `points.len()` must equal `req.n_points`.
    /// - The request must be the most recent one from `next_request()`.
    ///
    /// # Shutter Control
    ///
    /// This method manages the hardware shutter based on arm state transitions:
    /// - When transitioning from armed to disarmed, the shutter is closed (best-effort).
    /// - When transitioning from disarmed to armed, the shutter is opened (best-effort).
    pub fn write(&mut self, req: &ChunkRequest, points: &[LaserPoint]) -> Result<()> {
        // Validate point count
        if points.len() != req.n_points {
            return Err(Error::invalid_config(format!(
                "expected {} points, got {}",
                req.n_points,
                points.len()
            )));
        }

        loop {
            // Check for stop request
            if self.control.is_stop_requested() {
                return Err(Error::Stopped);
            }

            let is_armed = self.control.is_armed();

            // Handle shutter transitions
            self.handle_shutter_transition(is_armed);

            // Write to backend (optimized: no allocation when armed)
            let backend = self
                .backend
                .as_mut()
                .ok_or_else(|| Error::disconnected("no backend"))?;

            let outcome = if is_armed {
                // Armed: pass points directly to backend (zero-copy)
                backend.try_write_chunk(self.config.pps, points)?
            } else {
                // Disarmed: blank all points (allocate only when needed)
                let blanked: Vec<LaserPoint> = points
                    .iter()
                    .map(|p| LaserPoint::blanked(p.x, p.y))
                    .collect();
                backend.try_write_chunk(self.config.pps, &blanked)?
            };

            match outcome {
                WriteOutcome::Written => {
                    // Update state
                    if is_armed {
                        self.state.last_chunk = Some(points.to_vec());
                    }
                    self.state.current_instant += self.chunk_points as u64;
                    if self.info.caps.output_model == OutputModel::UsbFrameSwap {
                        self.state.scheduled_ahead = self.chunk_points as u64;
                    } else {
                        self.state.scheduled_ahead += self.chunk_points as u64;
                    }
                    self.state.stats.chunks_written += 1;
                    self.state.stats.points_written += self.chunk_points as u64;
                    return Ok(());
                }
                WriteOutcome::WouldBlock => {
                    // Backend buffer full - yield and retry
                    std::thread::yield_now();

                    if self.process_control_messages() {
                        return Err(Error::Stopped);
                    }

                    std::thread::sleep(Duration::from_micros(100));

                    if self.process_control_messages() {
                        return Err(Error::Stopped);
                    }
                }
            }
        }
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

    /// Run the stream in callback mode.
    ///
    /// The producer is called whenever the stream needs a new chunk.
    /// Return `Some(points)` to continue, or `None` to end the stream.
    ///
    /// # Error Classification
    ///
    /// The `on_error` callback receives recoverable errors that don't terminate the stream.
    /// Terminal conditions result in returning from `run()`:
    ///
    /// - **`RunExit::Stopped`**: Stream was stopped via `StreamControl::stop()` or underrun policy.
    /// - **`RunExit::ProducerEnded`**: Producer returned `None`.
    /// - **`RunExit::Disconnected`**: Device disconnected or became unreachable.
    ///
    /// Recoverable errors (reported via `on_error`, stream continues):
    /// - Transient backend errors that don't indicate disconnection.
    pub fn run<F, E>(mut self, mut producer: F, mut on_error: E) -> Result<RunExit>
    where
        F: FnMut(ChunkRequest) -> Option<Vec<LaserPoint>> + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        loop {
            // Check for stop request
            if self.control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            // Get next request
            let req = match self.next_request() {
                Ok(req) => req,
                Err(e) if e.is_stopped() => {
                    return Ok(RunExit::Stopped);
                }
                Err(e) if e.is_disconnected() => {
                    on_error(e);
                    return Ok(RunExit::Disconnected);
                }
                Err(e) => {
                    // Recoverable error - report and retry
                    on_error(e);
                    continue;
                }
            };

            // Process control messages before calling producer
            if self.process_control_messages() {
                return Ok(RunExit::Stopped);
            }

            // Call producer
            match producer(req.clone()) {
                Some(points) => {
                    match self.write(&req, &points) {
                        Ok(()) => {}
                        Err(e) if e.is_stopped() => {
                            return Ok(RunExit::Stopped);
                        }
                        Err(e) if e.is_disconnected() => {
                            on_error(e);
                            return Ok(RunExit::Disconnected);
                        }
                        Err(e) => {
                            // Recoverable error - report and handle underrun
                            on_error(e);
                            if let Err(e2) = self.handle_underrun(&req) {
                                // Underrun handling can also hit terminal conditions
                                if e2.is_stopped() {
                                    return Ok(RunExit::Stopped);
                                }
                                on_error(e2);
                            }
                        }
                    }
                }
                None => {
                    return Ok(RunExit::ProducerEnded);
                }
            }
        }
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

    /// Wait until we're ready for the next chunk (pacing).
    ///
    /// Sleeps in small slices to allow processing control messages promptly.
    fn wait_for_ready(&mut self) -> Result<()> {
        // Maximum sleep slice - controls responsiveness to control messages
        const SLEEP_SLICE: Duration = Duration::from_millis(5);

        let target = self.config.target_queue_points as u64;

        // Use the more accurate queue depth when available from the device.
        // This handles cases where the device reports actual buffer state,
        // which may differ from our software-tracked scheduled_ahead.
        let effective_queue = if self.info.caps.can_estimate_queue {
            self.backend
                .as_ref()
                .and_then(|b| b.queued_points())
                .map(|device_q| device_q.max(self.state.scheduled_ahead))
                .unwrap_or(self.state.scheduled_ahead)
        } else {
            self.state.scheduled_ahead
        };

        if effective_queue < target {
            return Ok(());
        }

        let points_to_drain = effective_queue.saturating_sub(target / 2);
        let seconds_to_wait = points_to_drain as f64 / self.config.pps as f64;
        let wait_duration = Duration::from_secs_f64(seconds_to_wait.min(0.1));

        // Sleep in small slices to process control messages promptly
        let mut remaining = wait_duration;
        while remaining > Duration::ZERO {
            let slice = remaining.min(SLEEP_SLICE);
            std::thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);

            // Process control messages - handle shutter close immediately
            if self.process_control_messages() {
                return Err(Error::Stopped);
            }
        }

        let elapsed = wait_duration.as_secs_f64();
        let points_drained = (elapsed * self.config.pps as f64) as u64;
        self.state.scheduled_ahead = self.state.scheduled_ahead.saturating_sub(points_drained);

        Ok(())
    }

    /// Handle an underrun by applying the underrun policy.
    ///
    /// # Safety Behavior
    ///
    /// When disarmed, this always outputs blanked points regardless of the underrun
    /// policy. The `RepeatLast` policy means "repeat last armed content" - when
    /// disarmed, repeating content would be unsafe.
    fn handle_underrun(&mut self, req: &ChunkRequest) -> Result<()> {
        self.state.stats.underrun_count += 1;

        let is_armed = self.control.is_armed();

        // Handle shutter transitions (same safety behavior as write())
        self.handle_shutter_transition(is_armed);

        // Determine fill points based on arm state and policy
        let fill_points: Vec<LaserPoint> = if !is_armed {
            // When disarmed, always output blanked points for safety
            // RepeatLast means "repeat last armed content" - meaningless when disarmed
            vec![LaserPoint::blanked(0.0, 0.0); req.n_points]
        } else {
            match &self.config.underrun {
                UnderrunPolicy::RepeatLast => self
                    .state
                    .last_chunk
                    .clone()
                    .unwrap_or_else(|| vec![LaserPoint::blanked(0.0, 0.0); req.n_points]),
                UnderrunPolicy::Blank => {
                    vec![LaserPoint::blanked(0.0, 0.0); req.n_points]
                }
                UnderrunPolicy::Park { x, y } => {
                    vec![LaserPoint::blanked(*x, *y); req.n_points]
                }
                UnderrunPolicy::Stop => {
                    self.control.stop()?;
                    return Err(Error::Stopped);
                }
            }
        };

        if let Some(backend) = &mut self.backend {
            match backend.try_write_chunk(self.config.pps, &fill_points) {
                Ok(WriteOutcome::Written) => {
                    // Update stream state to keep timebase accurate
                    let n_points = fill_points.len();
                    // Only update last_chunk when armed (it's the "last armed content")
                    if is_armed {
                        self.state.last_chunk = Some(fill_points);
                    }
                    self.state.current_instant += n_points as u64;
                    if self.info.caps.output_model == OutputModel::UsbFrameSwap {
                        self.state.scheduled_ahead = n_points as u64;
                    } else {
                        self.state.scheduled_ahead += n_points as u64;
                    }
                    self.state.stats.chunks_written += 1;
                    self.state.stats.points_written += n_points as u64;
                }
                Ok(WriteOutcome::WouldBlock) => {
                    // Backend is full, can't write fill points - this is expected
                }
                Err(_) => {
                    // Backend error during underrun handling - ignore, we're already recovering
                }
            }
        }

        Ok(())
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

        let chunk_points = cfg.chunk_points.unwrap_or_else(|| {
            Self::compute_default_chunk_size(&self.info.caps, cfg.pps, cfg.target_queue_points)
        });

        let stream = Stream::with_backend(self.info.clone(), backend, cfg, chunk_points);

        Ok((stream, self.info))
    }

    fn validate_config(caps: &DacCapabilities, cfg: &StreamConfig) -> Result<()> {
        if cfg.pps < caps.pps_min || cfg.pps > caps.pps_max {
            return Err(Error::invalid_config(format!(
                "PPS {} is outside device range [{}, {}]",
                cfg.pps, caps.pps_min, caps.pps_max
            )));
        }

        if let Some(chunk_points) = cfg.chunk_points {
            if chunk_points > caps.max_points_per_chunk {
                return Err(Error::invalid_config(format!(
                    "chunk_points {} exceeds device max {}",
                    chunk_points, caps.max_points_per_chunk
                )));
            }
            if chunk_points == 0 {
                return Err(Error::invalid_config("chunk_points cannot be 0"));
            }
        }

        if cfg.target_queue_points == 0 {
            return Err(Error::invalid_config("target_queue_points cannot be 0"));
        }

        Ok(())
    }

    fn compute_default_chunk_size(
        caps: &DacCapabilities,
        pps: u32,
        target_queue_points: usize,
    ) -> usize {
        // Target ~10ms worth of points per chunk
        let target_chunk_ms = 10;
        let time_based_points = (pps as usize * target_chunk_ms) / 1000;

        // Also bound by target queue: aim for ~¼ of target queue per chunk.
        // This ensures we don't send huge chunks relative to our latency target.
        let queue_based_max = target_queue_points / 4;

        let max_points = caps.max_points_per_chunk.min(queue_based_max.max(100));
        let min_points = 100;

        time_based_points.clamp(min_points, max_points)
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
    fn test_handle_underrun_advances_state() {
        let mut backend = TestBackend::new();
        backend.connected = true;
        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend.caps().clone(),
        };

        let cfg = StreamConfig::new(30000);
        let mut stream = Stream::with_backend(info, Box::new(backend), cfg, 100);

        // Record initial state
        let initial_instant = stream.state.current_instant;
        let initial_scheduled = stream.state.scheduled_ahead;
        let initial_chunks = stream.state.stats.chunks_written;
        let initial_points = stream.state.stats.points_written;

        // Trigger underrun handling
        let req = ChunkRequest {
            start: StreamInstant::new(0),
            pps: 30000,
            n_points: 100,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };
        stream.handle_underrun(&req).unwrap();

        // State should have advanced
        assert!(stream.state.current_instant > initial_instant);
        assert!(stream.state.scheduled_ahead > initial_scheduled);
        assert_eq!(stream.state.stats.chunks_written, initial_chunks + 1);
        assert_eq!(stream.state.stats.points_written, initial_points + 100);
        assert_eq!(stream.state.stats.underrun_count, 1);
    }

    #[test]
    fn test_run_retries_on_would_block() {
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

        let cfg = StreamConfig::new(30000).with_target_queue_points(10000);
        let stream = Stream::with_backend(info, backend_box, cfg, 100);

        let produced_count = Arc::new(AtomicUsize::new(0));
        let produced_count_clone = produced_count.clone();
        let result = stream.run(
            move |_req| {
                let count = produced_count_clone.fetch_add(1, Ordering::SeqCst);
                if count < 1 {
                    Some(vec![LaserPoint::blanked(0.0, 0.0); 100])
                } else {
                    None // End after one chunk
                }
            },
            |_e| {},
        );

        assert_eq!(result.unwrap(), RunExit::ProducerEnded);
        // Should have attempted write 4 times (3 WouldBlock + 1 success)
        assert_eq!(write_count.load(Ordering::SeqCst), 4);
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
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);

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
    fn test_handle_underrun_blanks_when_disarmed() {
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
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);

        // Set some last_chunk with colored points
        stream.state.last_chunk = Some(vec![
            LaserPoint::new(0.5, 0.5, 65535, 65535, 65535, 65535);
            100
        ]);

        // Ensure disarmed (default state)
        assert!(!stream.control.is_armed());

        let req = ChunkRequest {
            start: StreamInstant::new(0),
            pps: 30000,
            n_points: 100,
            scheduled_ahead_points: 0,
            device_queued_points: None,
        };

        // Handle underrun while disarmed
        stream.handle_underrun(&req).unwrap();

        // last_chunk should NOT be updated (we're disarmed)
        // The actual write was blanked points, but we don't update last_chunk when disarmed
        // because "last armed content" hasn't changed
        let last = stream.state.last_chunk.as_ref().unwrap();
        assert_eq!(last[0].r, 65535); // Still the old colored points
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
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);

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
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);
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

    #[test]
    fn test_blocking_write_retries_on_would_block() {
        // Simulates the Helios scenario: device returns WouldBlock on first
        // attempts (e.g., still busy from a previous session), then accepts.
        // The blocking write() must retry internally instead of propagating
        // WouldBlock as an error to the caller.
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

        let cfg = StreamConfig::new(30000).with_target_queue_points(10000);
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);

        let req = stream.next_request().unwrap();
        let points = vec![LaserPoint::blanked(0.0, 0.0); req.n_points];

        // write() must succeed despite the backend returning WouldBlock 3 times
        stream.write(&req, &points).unwrap();

        // Should have attempted 4 times (3 WouldBlock + 1 Written)
        assert_eq!(write_count.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_write_stops_during_would_block_retry() {
        // If the backend never accepts writes, write() must not loop forever.
        // A stop() from another thread must break out of the retry loop.
        let backend = TestBackend::new().with_would_block_count(usize::MAX);

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_target_queue_points(10000);
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);

        let req = stream.next_request().unwrap();
        let points = vec![LaserPoint::blanked(0.0, 0.0); req.n_points];

        // Stop from another thread while write() is stuck retrying
        let control = stream.control();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            control.stop().unwrap();
        });

        let result = stream.write(&req, &points);
        assert!(result.unwrap_err().is_stopped());
        handle.join().unwrap();
    }

    #[test]
    fn test_write_processes_disarm_during_would_block_retry() {
        // If disarm() is called while write() is retrying on WouldBlock,
        // the shutter must be closed during the retry loop — not deferred
        // until the next write() call.
        let backend = TestBackend::new().with_would_block_count(100);
        let shutter_open = backend.shutter_open.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_target_queue_points(10000);
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);

        // Arm first so shutter is open
        stream.control.arm().unwrap();
        stream.process_control_messages();
        assert!(shutter_open.load(Ordering::SeqCst));

        let req = stream.next_request().unwrap();
        let points = vec![LaserPoint::blanked(0.0, 0.0); req.n_points];

        // Disarm from another thread while write() is retrying
        let control = stream.control();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(1));
            control.disarm().unwrap();
        });

        // write() eventually succeeds after 100 WouldBlocks
        stream.write(&req, &points).unwrap();
        handle.join().unwrap();

        // Shutter must have been closed during the retry loop
        assert!(!shutter_open.load(Ordering::SeqCst));
    }

    #[test]
    fn test_write_stats_correct_after_would_block_retries() {
        // WouldBlock retries are internal — only the final successful write
        // should be counted in stats (1 chunk, N points).
        let backend = TestBackend::new().with_would_block_count(3);

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_target_queue_points(10000);
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);

        let req = stream.next_request().unwrap();
        let points = vec![LaserPoint::blanked(0.0, 0.0); req.n_points];

        stream.write(&req, &points).unwrap();

        // Despite 3 retries, only 1 successful chunk should be recorded
        assert_eq!(stream.state.stats.chunks_written, 1);
        assert_eq!(stream.state.stats.points_written, 100);
    }

    #[test]
    fn test_write_rejects_wrong_point_count_without_retrying() {
        // Point count validation must happen before the retry loop.
        // A wrong count should fail immediately without touching the backend.
        let backend = TestBackend::new();
        let write_count = backend.write_count.clone();

        let mut backend_box: Box<dyn StreamBackend> = Box::new(backend);
        backend_box.connect().unwrap();

        let info = DacInfo {
            id: "test".to_string(),
            name: "Test Device".to_string(),
            kind: DacType::Custom("Test".to_string()),
            caps: backend_box.caps().clone(),
        };

        let cfg = StreamConfig::new(30000).with_target_queue_points(10000);
        let mut stream = Stream::with_backend(info, backend_box, cfg, 100);

        let req = stream.next_request().unwrap();
        // Wrong number of points (one extra)
        let points = vec![LaserPoint::blanked(0.0, 0.0); req.n_points + 1];

        let result = stream.write(&req, &points);
        assert!(result.is_err());

        // Backend should never have been called
        assert_eq!(write_count.load(Ordering::SeqCst), 0);
    }
}
