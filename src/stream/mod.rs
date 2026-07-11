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
//! - **LaserCube USB/Network**: Actual hardware control
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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{BackendKind, Error, Result};
use crate::config::StreamConfig;
use crate::device::{DacCapabilities, DacInfo, DacType};
use crate::discovery::DacDiscovery;
use crate::point::LaserPoint;
use crate::reconnect::{ReconnectPolicy, ReconnectTarget};

pub(crate) mod chunk_producer;

// =============================================================================
// Stream protocol types
// =============================================================================

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// Represents a point in stream time, anchored to estimated playback position.
///
/// `StreamInstant` represents the **estimated playback time** of points, not merely
/// "points sent so far." When used in `ChunkRequest::start`, it represents:
///
/// `start` = playhead + buffered
///
/// Where:
/// - `playhead` = stream_epoch + estimated_consumed_points
/// - `buffered` = points sent but not yet played
///
/// This allows callbacks to generate content for the exact time it will be displayed,
/// enabling accurate audio synchronization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StreamInstant(pub u64);

impl StreamInstant {
    /// Create a new stream instant from a point count.
    pub fn new(points: u64) -> Self {
        Self(points)
    }

    /// Returns the number of points since stream start.
    pub fn points(&self) -> u64 {
        self.0
    }

    /// Convert this instant to seconds at the given points-per-second rate.
    pub fn as_seconds(&self, pps: u32) -> f64 {
        self.0 as f64 / pps as f64
    }

    /// Convert to seconds at the given PPS.
    ///
    /// This is an alias for `as_seconds()` for consistency with standard Rust
    /// duration naming conventions (e.g., `Duration::as_secs_f64()`).
    #[inline]
    pub fn as_secs_f64(&self, pps: u32) -> f64 {
        self.as_seconds(pps)
    }

    /// Create a stream instant from a duration in seconds at the given PPS.
    pub fn from_seconds(seconds: f64, pps: u32) -> Self {
        Self((seconds * pps as f64) as u64)
    }

    /// Add a number of points to this instant.
    pub fn add_points(&self, points: u64) -> Self {
        Self(self.0.saturating_add(points))
    }

    /// Subtract a number of points from this instant (saturating at 0).
    pub fn sub_points(&self, points: u64) -> Self {
        Self(self.0.saturating_sub(points))
    }
}

impl std::ops::Add<u64> for StreamInstant {
    type Output = Self;
    fn add(self, rhs: u64) -> Self::Output {
        self.add_points(rhs)
    }
}

impl std::ops::Sub<u64> for StreamInstant {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self::Output {
        self.sub_points(rhs)
    }
}

impl std::ops::AddAssign<u64> for StreamInstant {
    fn add_assign(&mut self, rhs: u64) {
        self.0 = self.0.saturating_add(rhs);
    }
}

impl std::ops::SubAssign<u64> for StreamInstant {
    fn sub_assign(&mut self, rhs: u64) {
        self.0 = self.0.saturating_sub(rhs);
    }
}

/// A request to fill a buffer with points for streaming.
///
/// The callback receives a `ChunkRequest` describing the next chunk's timing
/// requirements and fills points into a library-owned buffer.
///
/// `target_points` is the ideal number of points to reach the target buffer
/// level, clamped to the provided buffer length. Returning fewer points
/// triggers the configured [`IdlePolicy`] for the
/// remainder.
#[derive(Clone, Debug)]
pub struct ChunkRequest {
    /// Estimated playback time when this chunk starts.
    ///
    /// Use this for audio synchronization.
    pub start: StreamInstant,

    /// Points per second (current value, may change via `StreamControl::set_pps`).
    pub pps: u32,

    /// Ideal number of points to reach target buffer level.
    ///
    /// Calculated as: `ceil((target_buffer - buffered) * pps)`, clamped to buffer length.
    pub target_points: usize,
}

/// Result returned by the fill callback indicating how the buffer was filled.
///
/// This enum allows the callback to communicate three distinct states:
/// - Successfully filled some number of points
/// - Temporarily unable to provide data (underrun policy applies)
/// - Stream should end gracefully
///
/// # `Filled(0)` Semantics
///
/// - If `target_points == 0`: Buffer is full, nothing needed. This is fine.
/// - If `target_points > 0`: We needed points but got none. Treated as `Starved`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkResult {
    /// Wrote n points to the buffer.
    ///
    /// `n` must be <= `buffer.len()`. Partial fills (`n < target_points`) are
    /// accepted; the remainder is filled by the configured idle policy.
    Filled(usize),

    /// No data available right now.
    ///
    /// Underrun policy is applied (repeat last chunk or blank).
    /// Stream continues; callback will be called again when buffer needs filling.
    Starved,

    /// Stream is finished. Shutdown sequence:
    /// 1. Stop calling callback
    /// 2. Let queued points drain (play out)
    /// 3. Blank/park the laser at last position
    /// 4. Return from stream() with `RunExit::ProducerEnded`
    End,
}

/// Current status of a stream.
#[derive(Clone, Debug)]
pub struct StreamStatus {
    /// Whether the device is connected.
    pub connected: bool,
    /// Library-owned scheduled amount.
    pub scheduled_ahead_points: u64,
    /// Best-effort device/backend estimate.
    pub device_queued_points: Option<u64>,
    /// Optional statistics for diagnostics.
    pub stats: Option<StreamStats>,
}

/// Stream statistics for diagnostics and debugging.
#[derive(Clone, Debug, Default)]
pub struct StreamStats {
    /// Number of times the stream underran.
    pub underrun_count: u64,
    /// Number of chunks that arrived late.
    pub late_chunk_count: u64,
    /// Number of times the device reconnected.
    pub reconnect_count: u64,
    /// Total chunks written since stream start.
    pub chunks_written: u64,
    /// Total points written since stream start.
    pub points_written: u64,
}

/// How a callback-mode stream run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunExit {
    /// Stream was stopped via `StreamControl::stop()`.
    Stopped,
    /// Producer returned `None` (graceful completion).
    ProducerEnded,
    /// Device disconnected. No auto-reconnect; new streams start disarmed.
    Disconnected,
}

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
    /// Points per second (hot-swappable without session restart).
    pps: AtomicU32,
}

impl StreamControl {
    pub(crate) fn new(control_tx: Sender<ControlMsg>, color_delay: Duration, pps: u32) -> Self {
        Self {
            inner: Arc::new(StreamControlInner {
                armed: AtomicBool::new(false),
                stop_requested: AtomicBool::new(false),
                control_tx: Mutex::new(control_tx),
                color_delay_micros: AtomicU64::new(color_delay.as_micros() as u64),
                pps: AtomicU32::new(pps),
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

    /// Set the points per second rate.
    ///
    /// Takes effect within one chunk period — the stream loop reads this
    /// atomically each iteration and recalculates timing on the fly.
    /// No session restart required.
    pub fn set_pps(&self, pps: u32) {
        self.inner.pps.store(pps, Ordering::SeqCst);
    }

    /// Get the current points per second rate.
    pub fn pps(&self) -> u32 {
        self.inner.pps.load(Ordering::SeqCst)
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

/// Diagnostics carried across a stream's lifetime. The production streaming
/// path lives entirely in [`crate::presentation::driver::run`] via
/// [`chunk_producer::ChunkProducer`]; the only state the `Stream` wrapper still
/// keeps is the stats snapshot surfaced by [`Stream::status`] and
/// [`Stream::into_dac`].
#[derive(Default)]
struct StreamState {
    /// Statistics.
    stats: StreamStats,
}

impl StreamState {
    fn new() -> Self {
        Self::default()
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
    /// Create a new stream with a backend.
    pub(crate) fn with_backend(info: DacInfo, backend: BackendKind, config: StreamConfig) -> Self {
        let (control_tx, control_rx) = mpsc::channel();
        let color_delay = config.color_delay;
        let pps = config.pps;
        Self {
            info,
            backend: Some(backend),
            config,
            control: StreamControl::new(control_tx, color_delay, pps),
            control_rx,
            state: StreamState::new(),
            reconnect_policy: None,
            reconnect_target: None,
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
        let buffered = self.estimate_buffer_points();
        Ok(StreamStatus {
            connected: self.backend.as_ref().is_some_and(|b| b.is_connected()),
            scheduled_ahead_points: buffered,
            device_queued_points: Some(buffered),
            stats: Some(self.state.stats.clone()),
        })
    }

    /// Disarm, close shutter, and stop the backend (best-effort).
    ///
    /// Shared shutdown sequence used by `stop()`, `into_dac()`, and `Drop`.
    /// All errors are ignored since this is a safety-critical shutdown path.
    fn shutdown_backend(&mut self) {
        let _ = self.control.disarm();
        let _ = self.control.stop();
        if let Some(b) = &mut self.backend {
            let _ = b.set_shutter(false);
            let _ = b.stop();
        }
    }

    /// Stop the stream and terminate output.
    ///
    /// Disarms the output (software blanking + hardware shutter) before stopping
    /// the backend to prevent the "freeze on last bright point" hazard.
    /// Use `disarm()` instead if you want to keep the stream alive but safe.
    pub fn stop(&mut self) -> Result<()> {
        self.shutdown_backend();

        // Disconnect is needed for protocols like IDN where stop() only blanks
        // but the DAC keeps replaying its buffer until the session is closed.
        if let Some(b) = &mut self.backend {
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
        self.shutdown_backend();

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

    /// Run the stream with the zero-allocation callback API.
    ///
    /// This method uses **pure buffer-driven timing**:
    /// - Callback is invoked when `buffered <= target_buffer`
    /// - Points requested varies based on buffer headroom (`target_points`)
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
    pub fn run<F, E>(mut self, producer: F, on_error: E) -> Result<RunExit>
    where
        F: FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        use crate::presentation::driver::{self, DriverInputs, SourceOwned};
        use crate::presentation::FrameSessionMetrics;

        let backend = self
            .backend
            .take()
            .ok_or_else(|| Error::disconnected("backend already consumed"))?;
        if backend.is_frame_swap() {
            return Err(Error::invalid_config(
                "Stream::run is FIFO-only; use start_frame_session for frame-swap DACs",
            ));
        }

        let max_points = self.info.caps.max_points_per_chunk;
        let chunk_producer = chunk_producer::ChunkProducer::new(
            producer,
            self.control.clone(),
            self.config.idle_policy.clone(),
            self.config.startup_blank,
            max_points,
        );

        let validator = Self::build_reconnect_validator();
        let metrics = FrameSessionMetrics::new(true);
        // Move the control receiver out of self; the Receiver isn't Clone so we
        // swap in a fresh dummy channel that nothing will drive.
        let (_dummy_tx, dummy_rx) = mpsc::channel();
        let control_rx = std::mem::replace(&mut self.control_rx, dummy_rx);

        driver::run(DriverInputs {
            backend,
            source: SourceOwned::Fifo(Box::new(chunk_producer)),
            control: self.control.clone(),
            control_rx,
            metrics,
            reconnect_policy: self.reconnect_policy.take(),
            validator,
            error_sink: Box::new(on_error),
            target_buffer: self.config.target_buffer,
            drain_timeout: self.config.drain_timeout,
            pending_frame: None,
        })
    }

    fn build_reconnect_validator() -> crate::presentation::driver::ReconnectValidator {
        Box::new(
            move |_info: &DacInfo, new_backend: &BackendKind, pps: u32| {
                if new_backend.is_frame_swap() {
                    log::error!("reconnected device is frame-swap, incompatible with streaming");
                    return Err(RunExit::Disconnected);
                }
                if Dac::validate_pps(new_backend.caps(), pps).is_err() {
                    log::error!("reconnected device PPS range incompatible with stream config");
                    return Err(RunExit::Disconnected);
                }
                Ok(())
            },
        )
    }

    /// Estimate the current buffer level in points via the backend's
    /// [`BufferEstimator`].
    ///
    /// Each FIFO backend owns a strategy (status-anchored, dual-track ACK,
    /// runtime-authority, or pure software) and updates it from inside its
    /// own protocol code. The scheduler trusts that single source of truth.
    fn estimate_buffer_points(&self) -> u64 {
        let pps = self.config.pps;
        let now = std::time::Instant::now();
        self.backend
            .as_ref()
            .and_then(|b| b.estimator())
            .map_or(0, |e| e.estimated_fullness(now, pps))
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
    /// For ID-based opens, prefer [`open_device_with`](crate::open_device_with).
    /// For worker-owned scan results, compose
    /// [`DacDiscovery::open_discovered`](crate::DacDiscovery::open_discovered)
    /// with this method. Use this method directly when you construct a `Dac`
    /// yourself around an application-owned backend.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use laser_dac::{Dac, DacDiscovery, EnabledDacTypes, FrameSessionConfig, ReconnectConfig};
    ///
    /// // `info` and `backend` come from application-owned construction, not
    /// // from DacDiscovery::open_discovered*.
    /// let dac = Dac::new(info, backend).with_discovery_factory(|| {
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

        let cfg = Self::apply_backend_buffer_defaults(&self.info, cfg);

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

    fn apply_backend_buffer_defaults(info: &DacInfo, mut cfg: StreamConfig) -> StreamConfig {
        if cfg.target_buffer == StreamConfig::DEFAULT_TARGET_BUFFER {
            cfg.target_buffer =
                StreamConfig::default_target_buffer_for(&info.kind, &info.caps.output_model);
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
