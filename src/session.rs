//! Reconnecting session wrapper for automatic reconnection.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{Error, Result};
use crate::discovery::DacDiscovery;
use crate::stream::{Dac, StreamControl};
use crate::types::{ChunkRequest, DacInfo, LaserPoint, ProducerResult, RunExit, StreamConfig};

type DisconnectCallback = Box<dyn FnMut(&Error) + Send + 'static>;
type ReconnectCallback = Box<dyn FnMut(&DacInfo) + Send + 'static>;
type DiscoveryFactory = Box<dyn Fn() -> DacDiscovery + Send + 'static>;

// =============================================================================
// Session Control
// =============================================================================

/// Control handle for a [`ReconnectingSession`].
///
/// This mirrors `StreamControl`, but survives reconnections by attaching
/// to each new stream as it is created.
#[derive(Clone)]
pub struct SessionControl {
    inner: Arc<SessionControlInner>,
}

struct SessionControlInner {
    armed: AtomicBool,
    stop_requested: AtomicBool,
    current: Mutex<Option<StreamControl>>,
}

impl SessionControl {
    fn new() -> Self {
        Self {
            inner: Arc::new(SessionControlInner {
                armed: AtomicBool::new(false),
                stop_requested: AtomicBool::new(false),
                current: Mutex::new(None),
            }),
        }
    }

    fn attach(&self, control: StreamControl) {
        *self.inner.current.lock().unwrap() = Some(control.clone());

        if self.inner.stop_requested.load(Ordering::SeqCst) {
            let _ = control.stop();
            return;
        }

        if self.inner.armed.load(Ordering::SeqCst) {
            let _ = control.arm();
        } else {
            let _ = control.disarm();
        }
    }

    fn detach(&self) {
        *self.inner.current.lock().unwrap() = None;
    }

    /// Arm the output (allow laser to fire).
    pub fn arm(&self) -> Result<()> {
        self.inner.armed.store(true, Ordering::SeqCst);
        if let Some(control) = self.inner.current.lock().unwrap().as_ref() {
            let _ = control.arm();
        }
        Ok(())
    }

    /// Disarm the output (force laser off).
    pub fn disarm(&self) -> Result<()> {
        self.inner.armed.store(false, Ordering::SeqCst);
        if let Some(control) = self.inner.current.lock().unwrap().as_ref() {
            let _ = control.disarm();
        }
        Ok(())
    }

    /// Check if the output is armed.
    pub fn is_armed(&self) -> bool {
        self.inner.armed.load(Ordering::SeqCst)
    }

    /// Request the session to stop.
    pub fn stop(&self) -> Result<()> {
        self.inner.stop_requested.store(true, Ordering::SeqCst);
        if let Some(control) = self.inner.current.lock().unwrap().as_ref() {
            let _ = control.stop();
        }
        Ok(())
    }

    /// Check if a stop has been requested.
    pub fn is_stop_requested(&self) -> bool {
        self.inner.stop_requested.load(Ordering::SeqCst)
    }
}

// =============================================================================
// Reconnecting Session
// =============================================================================

/// A reconnecting wrapper around the streaming API.
///
/// This helper reconnects to a device by ID and restarts streaming
/// automatically when a disconnection occurs.
///
/// By default this uses `open_device()` internally. To use custom DAC
/// backends, call [`with_discovery`](Self::with_discovery) with a factory
/// function that creates a configured [`DacDiscovery`].
///
/// # Example
///
/// ```no_run
/// use laser_dac::{ReconnectingSession, StreamConfig, ProducerResult};
/// use std::time::Duration;
///
/// let mut session = ReconnectingSession::new("my-device", StreamConfig::new(30_000))
///     .with_max_retries(5)
///     .with_backoff(Duration::from_secs(1))
///     .on_disconnect(|err| eprintln!("Lost connection: {}", err))
///     .on_reconnect(|info| println!("Reconnected to {}", info.name));
///
/// session.control().arm()?;
///
/// session.run(
///     |req, buffer| {
///         // Fill buffer with points (pre-filled with blanks)
///         ProducerResult::Continue
///     },
///     |err| eprintln!("Stream error: {}", err),
/// )?;
/// # Ok::<(), laser_dac::Error>(())
/// ```
pub struct ReconnectingSession {
    device_id: String,
    config: StreamConfig,
    max_retries: Option<u32>,
    backoff: Duration,
    on_disconnect: Arc<Mutex<Option<DisconnectCallback>>>,
    on_reconnect: Option<ReconnectCallback>,
    control: SessionControl,
    discovery_factory: Option<DiscoveryFactory>,
}

impl ReconnectingSession {
    /// Create a new reconnecting session for a device ID.
    pub fn new(device_id: impl Into<String>, config: StreamConfig) -> Self {
        Self {
            device_id: device_id.into(),
            config,
            max_retries: None,
            backoff: Duration::from_secs(1),
            on_disconnect: Arc::new(Mutex::new(None)),
            on_reconnect: None,
            control: SessionControl::new(),
            discovery_factory: None,
        }
    }

    /// Set the maximum number of reconnect attempts.
    ///
    /// `None` (default) retries forever. `Some(0)` disables retries.
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Set a fixed backoff duration between reconnect attempts.
    pub fn with_backoff(mut self, backoff: Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Register a callback invoked when a disconnect is detected.
    pub fn on_disconnect<F>(self, f: F) -> Self
    where
        F: FnMut(&Error) + Send + 'static,
    {
        *self.on_disconnect.lock().unwrap() = Some(Box::new(f));
        self
    }

    /// Register a callback invoked after a successful reconnect.
    pub fn on_reconnect<F>(mut self, f: F) -> Self
    where
        F: FnMut(&DacInfo) + Send + 'static,
    {
        self.on_reconnect = Some(Box::new(f));
        self
    }

    /// Use a custom discovery factory for opening devices.
    ///
    /// This allows using custom DAC backends by providing a factory function
    /// that creates a [`DacDiscovery`] with external discoverers registered.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use laser_dac::{DacDiscovery, EnabledDacTypes, ReconnectingSession, StreamConfig};
    ///
    /// let session = ReconnectingSession::new("custom:my-device", StreamConfig::new(30_000))
    ///     .with_discovery(|| {
    ///         let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
    ///         // discovery.register(my_custom_discoverer);
    ///         discovery
    ///     });
    /// ```
    pub fn with_discovery<F>(mut self, factory: F) -> Self
    where
        F: Fn() -> DacDiscovery + Send + 'static,
    {
        self.discovery_factory = Some(Box::new(factory));
        self
    }

    /// Returns a control handle for arm/disarm/stop.
    pub fn control(&self) -> SessionControl {
        self.control.clone()
    }

    /// Run the stream with a reusable buffer, automatically reconnecting on disconnection.
    ///
    /// The producer receives a `ChunkRequest` and a mutable buffer slice to fill.
    /// The buffer is per-stream (fresh on each reconnect) and reused across calls.
    pub fn run<F, E>(&mut self, producer: F, on_error: E) -> Result<RunExit>
    where
        F: FnMut(ChunkRequest, &mut [LaserPoint]) -> ProducerResult + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        let producer = Arc::new(Mutex::new(producer));
        let on_error = Arc::new(Mutex::new(on_error));
        let on_disconnect = Arc::clone(&self.on_disconnect);
        let mut connected_once = false;
        let mut retries = 0u32;

        loop {
            if self.control.is_stop_requested() {
                return Ok(RunExit::Stopped);
            }

            if let Some(max) = self.max_retries {
                if retries >= max {
                    return Ok(RunExit::Disconnected);
                }
            }

            let device = match self.open_device() {
                Ok(device) => device,
                Err(err) => {
                    if !Self::is_retriable_connect_error(&err) {
                        return Err(err);
                    }
                    {
                        let mut handler = on_error.lock().unwrap();
                        handler(err);
                    }
                    retries = retries.saturating_add(1);
                    if let Some(max) = self.max_retries {
                        if retries >= max {
                            return Ok(RunExit::Disconnected);
                        }
                    }
                    if self.sleep_with_stop(self.backoff) {
                        return Ok(RunExit::Stopped);
                    }
                    continue;
                }
            };

            let (stream, info) = match device.start_stream(self.config.clone()) {
                Ok(result) => result,
                Err(err) => {
                    if !Self::is_retriable_connect_error(&err) {
                        return Err(err);
                    }
                    {
                        let mut handler = on_error.lock().unwrap();
                        handler(err);
                    }
                    retries = retries.saturating_add(1);
                    if let Some(max) = self.max_retries {
                        if retries >= max {
                            return Ok(RunExit::Disconnected);
                        }
                    }
                    if self.sleep_with_stop(self.backoff) {
                        return Ok(RunExit::Stopped);
                    }
                    continue;
                }
            };

            if connected_once {
                if let Some(cb) = self.on_reconnect.as_mut() {
                    cb(&info);
                }
            }
            connected_once = true;
            retries = 0;

            self.control.attach(stream.control());

            let producer_handle = Arc::clone(&producer);
            let on_error_handle = Arc::clone(&on_error);
            let on_disconnect_handle = Arc::clone(&on_disconnect);
            let exit = match stream.run(
                move |req, buffer| {
                    let mut handler = producer_handle.lock().unwrap();
                    handler(req, buffer)
                },
                move |err| {
                    if err.is_disconnected() {
                        if let Some(cb) = on_disconnect_handle.lock().unwrap().as_mut() {
                            cb(&err);
                        }
                    }
                    let mut handler = on_error_handle.lock().unwrap();
                    handler(err)
                },
            ) {
                Ok(exit) => exit,
                Err(err) => {
                    self.control.detach();
                    return Err(err);
                }
            };

            self.control.detach();

            match exit {
                RunExit::Disconnected => {
                    if let Some(max) = self.max_retries {
                        if retries >= max {
                            return Ok(RunExit::Disconnected);
                        }
                    }
                    if self.sleep_with_stop(self.backoff) {
                        return Ok(RunExit::Stopped);
                    }
                    continue;
                }
                other => return Ok(other),
            }
        }
    }

    /// Run the stream with legacy allocating API, automatically reconnecting.
    ///
    /// This is a convenience adapter that accepts the legacy signature
    /// `FnMut(ChunkRequest) -> Option<Vec<LaserPoint>>`. Prefer `run()` for
    /// allocation-free streaming.
    pub fn run_alloc<F, E>(&mut self, mut producer: F, on_error: E) -> Result<RunExit>
    where
        F: FnMut(ChunkRequest) -> Option<Vec<LaserPoint>> + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        self.run(
            move |req, buffer| match producer(req.clone()) {
                Some(points) => {
                    let n = points.len().min(buffer.len());
                    buffer[..n].copy_from_slice(&points[..n]);
                    if n == req.n_points {
                        ProducerResult::Continue
                    } else {
                        ProducerResult::ContinuePartial { filled: n }
                    }
                }
                None => ProducerResult::End,
            },
            on_error,
        )
    }

    fn open_device(&mut self) -> Result<Dac> {
        if let Some(factory) = &self.discovery_factory {
            let mut discovery = factory();
            let discovered = discovery.scan();

            let device = discovered
                .into_iter()
                .find(|d| d.info().stable_id() == self.device_id)
                .ok_or_else(|| {
                    Error::disconnected(format!("device not found: {}", self.device_id))
                })?;

            let info = device.info();
            let name = info.name();
            let dac_type = device.dac_type();
            let stream_backend = discovery.connect(device)?;

            let device_info = crate::types::DacInfo {
                id: self.device_id.clone(),
                name,
                kind: dac_type,
                caps: stream_backend.caps().clone(),
            };

            Ok(Dac::new(device_info, stream_backend))
        } else {
            crate::open_device(&self.device_id)
        }
    }

    fn is_retriable_connect_error(err: &Error) -> bool {
        !matches!(err, Error::InvalidConfig(_) | Error::Stopped)
    }

    fn sleep_with_stop(&self, duration: Duration) -> bool {
        const SLICE: Duration = Duration::from_millis(50);
        let mut remaining = duration;
        while remaining > Duration::ZERO {
            if self.control.is_stop_requested() {
                return true;
            }
            let slice = remaining.min(SLICE);
            std::thread::sleep(slice);
            remaining = remaining.saturating_sub(slice);
        }
        self.control.is_stop_requested()
    }
}
