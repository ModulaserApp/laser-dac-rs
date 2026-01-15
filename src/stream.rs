//! Stream and Device types for point output.
//!
//! This module provides the `Stream` type for streaming point chunks to a DAC,
//! `StreamControl` for out-of-band control (arm/disarm/stop), and `Device` for
//! connected devices that can start streaming sessions.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::backend::{Error, Result, StreamBackend, WriteOutcome};
use crate::types::{
    Caps, ChunkRequest, DeviceInfo, DacType, LaserPoint, RunExit, StreamConfig, StreamInstant,
    StreamStats, StreamStatus, UnderrunPolicy,
};

// =============================================================================
// Stream Control
// =============================================================================

/// Thread-safe control handle for safety-critical actions.
///
/// This allows out-of-band control of the stream (arm/disarm/stop) from
/// a different thread, e.g., for E-stop functionality.
#[derive(Clone)]
pub struct StreamControl {
    inner: Arc<StreamControlInner>,
}

struct StreamControlInner {
    /// Whether output is armed (laser can fire).
    armed: AtomicBool,
    /// Whether a stop has been requested.
    stop_requested: AtomicBool,
}

impl StreamControl {
    fn new() -> Self {
        Self {
            inner: Arc::new(StreamControlInner {
                armed: AtomicBool::new(false),
                stop_requested: AtomicBool::new(false),
            }),
        }
    }

    /// Arm the output (allow laser to fire).
    pub fn arm(&self) -> Result<()> {
        self.inner.armed.store(true, Ordering::SeqCst);
        Ok(())
    }

    /// Disarm the output (force laser off).
    pub fn disarm(&self) -> Result<()> {
        self.inner.armed.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Check if the output is armed.
    pub fn is_armed(&self) -> bool {
        self.inner.armed.load(Ordering::SeqCst)
    }

    /// Request the stream to stop.
    pub fn stop(&self) -> Result<()> {
        self.inner.stop_requested.store(true, Ordering::SeqCst);
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
}

impl StreamState {
    fn new() -> Self {
        Self {
            current_instant: StreamInstant::new(0),
            scheduled_ahead: 0,
            last_chunk: None,
            stats: StreamStats::default(),
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
    info: DeviceInfo,
    /// The backend.
    backend: Option<Box<dyn StreamBackend>>,
    /// Stream configuration.
    config: StreamConfig,
    /// Resolved chunk size.
    chunk_points: usize,
    /// Thread-safe control handle.
    control: StreamControl,
    /// Stream state.
    state: StreamState,
}

impl Stream {
    /// Create a new stream with a backend.
    pub(crate) fn with_backend(
        info: DeviceInfo,
        backend: Box<dyn StreamBackend>,
        config: StreamConfig,
        chunk_points: usize,
    ) -> Self {
        Self {
            info,
            backend: Some(backend),
            config,
            chunk_points,
            control: StreamControl::new(),
            state: StreamState::new(),
        }
    }

    /// Returns the device info.
    pub fn info(&self) -> &DeviceInfo {
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
            return Err(Error::disconnected("stop requested"));
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
    /// # Contract
    ///
    /// - `points.len()` must equal `req.n_points`.
    /// - The request must be the most recent one from `next_request()`.
    pub fn write(&mut self, req: &ChunkRequest, points: &[LaserPoint]) -> Result<()> {
        // Validate point count
        if points.len() != req.n_points {
            return Err(Error::invalid_config(format!(
                "expected {} points, got {}",
                req.n_points,
                points.len()
            )));
        }

        // Check for stop request
        if self.control.is_stop_requested() {
            return Err(Error::disconnected("stop requested"));
        }

        // Apply arm/disarm: if disarmed, blank all points
        let output_points: Vec<LaserPoint> = if self.control.is_armed() {
            points.to_vec()
        } else {
            points
                .iter()
                .map(|p| LaserPoint::blanked(p.x, p.y))
                .collect()
        };

        // Write to backend
        let backend = self
            .backend
            .as_mut()
            .ok_or_else(|| Error::disconnected("no backend"))?;

        match backend.try_write_chunk(self.config.pps, &output_points)? {
            WriteOutcome::Written => {
                // Update state
                self.state.last_chunk = Some(output_points);
                self.state.current_instant += self.chunk_points as u64;
                self.state.scheduled_ahead += self.chunk_points as u64;
                self.state.stats.chunks_written += 1;
                self.state.stats.points_written += self.chunk_points as u64;
                Ok(())
            }
            WriteOutcome::WouldBlock => Err(Error::WouldBlock),
        }
    }

    /// Stop the stream.
    pub fn stop(&mut self) -> Result<()> {
        self.control.stop()?;

        if let Some(backend) = &mut self.backend {
            backend.stop()?;
        }

        Ok(())
    }

    /// Run the stream in callback mode.
    ///
    /// The producer is called whenever the stream needs a new chunk.
    /// Return `Some(points)` to continue, or `None` to end the stream.
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
                Err(e) if e.is_disconnected() && self.control.is_stop_requested() => {
                    return Ok(RunExit::Stopped);
                }
                Err(e) => {
                    on_error(e);
                    continue;
                }
            };

            // Call producer
            match producer(req.clone()) {
                Some(points) => {
                    if let Err(e) = self.write(&req, &points) {
                        on_error(e);
                        if let Err(e2) = self.handle_underrun(&req) {
                            on_error(e2);
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

    /// Wait until we're ready for the next chunk (pacing).
    fn wait_for_ready(&mut self) -> Result<()> {
        let target = self.config.target_queue_points as u64;

        if self.state.scheduled_ahead < target {
            return Ok(());
        }

        let points_to_drain = self.state.scheduled_ahead.saturating_sub(target / 2);
        let seconds_to_wait = points_to_drain as f64 / self.config.pps as f64;
        let wait_duration = Duration::from_secs_f64(seconds_to_wait.min(0.1));

        std::thread::sleep(wait_duration);

        let elapsed = wait_duration.as_secs_f64();
        let points_drained = (elapsed * self.config.pps as f64) as u64;
        self.state.scheduled_ahead = self.state.scheduled_ahead.saturating_sub(points_drained);

        Ok(())
    }

    /// Handle an underrun by applying the underrun policy.
    fn handle_underrun(&mut self, req: &ChunkRequest) -> Result<()> {
        self.state.stats.underrun_count += 1;

        let fill_points: Vec<LaserPoint> = match &self.config.underrun {
            UnderrunPolicy::RepeatLast => {
                self.state
                    .last_chunk
                    .clone()
                    .unwrap_or_else(|| vec![LaserPoint::blanked(0.0, 0.0); req.n_points])
            }
            UnderrunPolicy::Blank => {
                vec![LaserPoint::blanked(0.0, 0.0); req.n_points]
            }
            UnderrunPolicy::Park { x, y } => {
                vec![LaserPoint::blanked(*x, *y); req.n_points]
            }
            UnderrunPolicy::Stop => {
                self.control.stop()?;
                return Err(Error::disconnected("underrun with Stop policy"));
            }
        };

        if let Some(backend) = &mut self.backend {
            let _ = backend.try_write_chunk(self.config.pps, &fill_points);
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
/// transfers to the stream. The `DeviceInfo` is returned alongside the stream
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
pub struct Device {
    info: DeviceInfo,
    backend: Option<Box<dyn StreamBackend>>,
}

impl Device {
    /// Create a new device from a backend.
    pub fn new(info: DeviceInfo, backend: Box<dyn StreamBackend>) -> Self {
        Self {
            info,
            backend: Some(backend),
        }
    }

    /// Returns the device info.
    pub fn info(&self) -> &DeviceInfo {
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
    pub fn caps(&self) -> &Caps {
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
    /// Returns both the stream and the device info, so metadata remains accessible.
    pub fn start_stream(mut self, cfg: StreamConfig) -> Result<(Stream, DeviceInfo)> {
        let backend = self.backend.take().ok_or_else(|| {
            Error::invalid_config("device backend has already been used for a stream")
        })?;

        Self::validate_config(&self.info.caps, &cfg)?;

        let chunk_points = cfg
            .chunk_points
            .unwrap_or_else(|| Self::compute_default_chunk_size(&self.info.caps, cfg.pps));

        let stream = Stream::with_backend(self.info.clone(), backend, cfg, chunk_points);

        Ok((stream, self.info))
    }

    fn validate_config(caps: &Caps, cfg: &StreamConfig) -> Result<()> {
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

    fn compute_default_chunk_size(caps: &Caps, pps: u32) -> usize {
        let target_chunk_ms = 10;
        let target_points = (pps as usize * target_chunk_ms) / 1000;
        let max_points = caps.max_points_per_chunk;
        let min_points = 100;
        target_points.clamp(min_points, max_points)
    }
}

/// Legacy alias for compatibility.
pub type OwnedDevice = Device;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_control_arm_disarm() {
        let control = StreamControl::new();
        assert!(!control.is_armed());

        control.arm().unwrap();
        assert!(control.is_armed());

        control.disarm().unwrap();
        assert!(!control.is_armed());
    }

    #[test]
    fn test_stream_control_stop() {
        let control = StreamControl::new();
        assert!(!control.is_stop_requested());

        control.stop().unwrap();
        assert!(control.is_stop_requested());
    }

    #[test]
    fn test_stream_control_clone_shares_state() {
        let control1 = StreamControl::new();
        let control2 = control1.clone();

        control1.arm().unwrap();
        assert!(control2.is_armed());

        control2.stop().unwrap();
        assert!(control1.is_stop_requested());
    }
}
