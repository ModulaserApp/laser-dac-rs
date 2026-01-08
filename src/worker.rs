//! Background workers for non-blocking DAC frame writing.
//!
//! This module provides two worker types for different use cases:
//!
//! ## Push-based: [`DacWorker`]
//!
//! Use when you have a main loop that generates frames at its own pace.
//! You call [`DacWorker::submit_frame()`] whenever you have a new frame.
//! If the device is busy, the frame is dropped (returns `false`).
//! Good for: game loops, animation systems, GUI applications.
//!
//! ## Callback-based: [`DacCallbackWorker`]
//!
//! Use when you want the DAC to drive the timing.
//! Your callback is invoked from the worker thread when device needs data.
//! Frames are never dropped - the callback is called when device is ready.
//! Good for: audio-synced output, precise timing, streaming from file.
//!
//! The data callback runs on a dedicated worker thread, is called sequentially
//! (never concurrent with itself), and should return quickly to maintain smooth
//! output. Return `None` to signal graceful shutdown.
//!
//! **Thread Safety**: The callback must be `Send + 'static`. Use `Arc<Mutex<T>>`
//! or channels to share state with the main thread.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver as MpscReceiver, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::backend::{DacBackend, WriteResult};
use crate::types::{DacConnectionState, DacType, LaserFrame};

/// Channel for notifying when a device disconnects (used by discovery worker).
pub(crate) type DisconnectNotifier = Sender<String>;

/// Command sent to the worker thread.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum WorkerCommand {
    /// Write a frame to the DAC.
    WriteFrame(LaserFrame),
    /// Stop laser output but keep worker alive.
    StopOutput,
    /// Stop output and shutdown the worker.
    Stop,
}

/// Status update from the worker thread.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum WorkerStatus {
    /// Device is ready for frames.
    Ready,
    /// Device is busy (frame dropped).
    Busy,
    /// Connection lost due to error.
    ConnectionLost(String),
}

// =============================================================================
// Callback Worker Types
// =============================================================================

/// Context provided to the data callback.
///
/// Contains metadata about the device and statistics about frames written.
#[derive(Debug)]
pub struct CallbackContext<'a> {
    /// Name of the DAC device.
    pub device_name: &'a str,
    /// Type of DAC hardware.
    pub dac_type: DacType,
    /// Number of frames successfully written to the device.
    pub frames_written: u64,
}

/// Errors that can occur during callback-driven output.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum CallbackError {
    /// Connection to the DAC was lost.
    ConnectionLost(String),
    /// The data callback panicked.
    CallbackPanic,
}

/// Status from the callback worker (internal use only).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CallbackStatus {
    /// Worker is running normally.
    Running,
    /// Worker stopped because callback returned None.
    Stopped,
    /// Worker stopped due to an error.
    Error(String),
}

// =============================================================================
// Push-based Worker (DacWorker)
// =============================================================================

/// Background worker that writes frames to a single DAC device.
///
/// Frames are sent via a bounded channel (capacity 1). If the consumer is busy,
/// old frames are automatically dropped to prevent backlog.
pub struct DacWorker {
    device_name: String,
    dac_type: DacType,
    command_tx: SyncSender<WorkerCommand>,
    status_rx: MpscReceiver<WorkerStatus>,
    handle: Option<JoinHandle<()>>,
    state: DacConnectionState,
    /// Optional channel to notify discovery worker when this worker is dropped.
    disconnect_tx: Option<DisconnectNotifier>,
}

impl DacWorker {
    /// Creates a new worker for the given connected DAC backend.
    ///
    /// The backend is moved into a background thread that handles frame writing.
    pub fn new(device_name: String, dac_type: DacType, backend: Box<dyn DacBackend>) -> Self {
        Self::new_internal(device_name, dac_type, backend, None)
    }

    /// Internal constructor that optionally accepts a disconnect notifier.
    pub(crate) fn new_with_disconnect_notifier(
        device_name: String,
        dac_type: DacType,
        backend: Box<dyn DacBackend>,
        disconnect_tx: DisconnectNotifier,
    ) -> Self {
        Self::new_internal(device_name, dac_type, backend, Some(disconnect_tx))
    }

    fn new_internal(
        device_name: String,
        dac_type: DacType,
        backend: Box<dyn DacBackend>,
        disconnect_tx: Option<DisconnectNotifier>,
    ) -> Self {
        let (command_tx, command_rx) = mpsc::sync_channel::<WorkerCommand>(1);
        let (status_tx, status_rx) = mpsc::channel::<WorkerStatus>();

        let name_for_loop = device_name.clone();
        let disconnect_tx_for_loop = disconnect_tx.clone();
        let handle = thread::spawn(move || {
            Self::worker_loop(
                backend,
                command_rx,
                status_tx,
                name_for_loop,
                disconnect_tx_for_loop,
            );
        });

        Self {
            device_name: device_name.clone(),
            dac_type,
            command_tx,
            status_rx,
            handle: Some(handle),
            state: DacConnectionState::Connected { name: device_name },
            disconnect_tx,
        }
    }

    /// Returns the device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Returns the DAC type.
    pub fn dac_type(&self) -> DacType {
        self.dac_type.clone()
    }

    /// Returns the current connection state.
    pub fn state(&self) -> &DacConnectionState {
        &self.state
    }

    /// Submits a frame to be written (non-blocking).
    ///
    /// Returns true if the frame was queued, false if dropped (device busy).
    pub fn submit_frame(&self, frame: LaserFrame) -> bool {
        match self.command_tx.try_send(WorkerCommand::WriteFrame(frame)) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => false,
        }
    }

    /// Polls for status updates from the worker thread.
    ///
    /// Call this periodically to update the connection state.
    pub fn update(&mut self) {
        while let Ok(status) = self.status_rx.try_recv() {
            match status {
                WorkerStatus::Ready | WorkerStatus::Busy => {
                    if matches!(self.state, DacConnectionState::Lost { .. }) {
                        self.state = DacConnectionState::Connected {
                            name: self.device_name.clone(),
                        };
                    }
                }
                WorkerStatus::ConnectionLost(error) => {
                    self.state = DacConnectionState::Lost {
                        name: self.device_name.clone(),
                        error: Some(error),
                    };
                }
            }
        }

        // Check if thread died unexpectedly
        if !self.is_alive() && matches!(self.state, DacConnectionState::Connected { .. }) {
            self.state = DacConnectionState::Lost {
                name: self.device_name.clone(),
                error: Some("Worker thread died unexpectedly".to_string()),
            };
        }
    }

    /// Checks if the worker thread is still alive.
    pub fn is_alive(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Stops laser output but keeps the worker alive for future frames.
    pub fn stop_output(&self) {
        let _ = self.command_tx.try_send(WorkerCommand::StopOutput);
    }

    /// The generic worker loop that handles all DAC backends.
    fn worker_loop(
        mut backend: Box<dyn DacBackend>,
        command_rx: MpscReceiver<WorkerCommand>,
        status_tx: mpsc::Sender<WorkerStatus>,
        device_name: String,
        disconnect_tx: Option<DisconnectNotifier>,
    ) {
        // Connect if not already connected
        if !backend.is_connected() {
            if let Err(e) = backend.connect() {
                let _ = status_tx.send(WorkerStatus::ConnectionLost(e.to_string()));
                if let Some(ref tx) = disconnect_tx {
                    let _ = tx.send(device_name);
                }
                return;
            }
        }

        while let Ok(command) = command_rx.recv() {
            match command {
                WorkerCommand::WriteFrame(frame) => {
                    match backend.write_frame(&frame) {
                        Ok(WriteResult::Written) => {
                            let _ = status_tx.send(WorkerStatus::Ready);
                        }
                        Ok(WriteResult::DeviceBusy) => {
                            let _ = status_tx.send(WorkerStatus::Busy);
                        }
                        Err(e) => {
                            let _ = status_tx.send(WorkerStatus::ConnectionLost(e.to_string()));
                            // Notify discovery worker so device can be reconnected
                            if let Some(ref tx) = disconnect_tx {
                                let _ = tx.send(device_name.clone());
                            }
                            return;
                        }
                    }
                }
                WorkerCommand::StopOutput => {
                    let _ = backend.stop();
                    let _ = status_tx.send(WorkerStatus::Ready);
                }
                WorkerCommand::Stop => {
                    let _ = backend.stop();
                    break;
                }
            }
        }
    }
}

impl Drop for DacWorker {
    fn drop(&mut self) {
        let _ = self.command_tx.try_send(WorkerCommand::Stop);
        // Notify discovery worker so device can be rediscovered
        if let Some(ref tx) = self.disconnect_tx {
            let _ = tx.send(self.device_name.clone());
        }
    }
}

// =============================================================================
// Callback-based Worker (DacCallbackWorker)
// =============================================================================

/// Callback-driven worker that invokes a closure when the device needs more points.
///
/// Unlike [`DacWorker`] where you push frames via `submit_frame()`, this worker
/// pulls frames by invoking a user-provided callback from the worker thread.
///
/// This is similar to how audio libraries (CPAL, PortAudio) work - the hardware
/// drives the timing, and your callback provides data when requested.
///
/// The worker is created in two phases:
/// 1. [`new()`](Self::new) - Creates a connected but idle worker
/// 2. [`start()`](Self::start) - Starts the worker thread with your callbacks
///
/// This allows you to inspect the device (name, type) before deciding what
/// callbacks to use, and is required when using [`DacDiscoveryWorker`](crate::DacDiscoveryWorker)
/// with callback workers.
pub struct DacCallbackWorker {
    device_name: String,
    dac_type: DacType,
    /// Backend held until start() is called.
    backend: Option<Box<dyn DacBackend>>,
    /// Stop flag (created in start()).
    stop_flag: Option<Arc<AtomicBool>>,
    /// Status receiver (created in start()).
    status_rx: Option<MpscReceiver<CallbackStatus>>,
    /// Worker thread handle (created in start()).
    handle: Option<JoinHandle<()>>,
    state: DacConnectionState,
    /// Optional channel to notify discovery worker when this worker is dropped.
    disconnect_tx: Option<DisconnectNotifier>,
}

impl DacCallbackWorker {
    /// Creates a new callback worker for the given DAC backend.
    ///
    /// The worker is created in an idle state - call [`start()`](Self::start)
    /// to begin the callback loop.
    ///
    /// This two-phase construction allows you to inspect the device before
    /// deciding what callbacks to use.
    pub fn new(device_name: String, dac_type: DacType, backend: Box<dyn DacBackend>) -> Self {
        Self {
            device_name: device_name.clone(),
            dac_type,
            backend: Some(backend),
            stop_flag: None,
            status_rx: None,
            handle: None,
            state: DacConnectionState::Connected { name: device_name },
            disconnect_tx: None,
        }
    }

    /// Internal constructor that optionally accepts a disconnect notifier.
    pub(crate) fn new_with_disconnect_notifier(
        device_name: String,
        dac_type: DacType,
        backend: Box<dyn DacBackend>,
        disconnect_tx: DisconnectNotifier,
    ) -> Self {
        Self {
            device_name: device_name.clone(),
            dac_type,
            backend: Some(backend),
            stop_flag: None,
            status_rx: None,
            handle: None,
            state: DacConnectionState::Connected { name: device_name },
            disconnect_tx: Some(disconnect_tx),
        }
    }

    /// Starts the worker with the given callbacks.
    ///
    /// The `data_callback` is invoked from the worker thread whenever the device
    /// is ready for more data. Return `Some(frame)` to send a frame, or `None`
    /// to stop output gracefully.
    ///
    /// The `error_callback` is invoked when an error occurs (connection lost,
    /// callback panic, etc).
    ///
    /// # Panics
    ///
    /// Panics if the worker has already been started.
    ///
    /// # Thread Safety
    ///
    /// **Both callbacks run on the worker thread**, not the main thread. This means:
    /// - You cannot directly update main-thread state from inside the callbacks
    /// - Use `Arc<Mutex<T>>` or channels to communicate with the main thread
    /// - The error callback in particular often needs to send a message back rather
    ///   than updating state directly
    ///
    /// Both callbacks must be `Send + 'static`.
    pub fn start<F, E>(&mut self, data_callback: F, error_callback: E)
    where
        F: FnMut(&mut CallbackContext) -> Option<LaserFrame> + Send + 'static,
        E: FnMut(CallbackError) + Send + 'static,
    {
        let backend = self
            .backend
            .take()
            .expect("DacCallbackWorker::start() called but worker was already started");

        let stop_flag = Arc::new(AtomicBool::new(false));
        let (status_tx, status_rx) = mpsc::channel::<CallbackStatus>();

        let name_for_loop = self.device_name.clone();
        let dac_type = self.dac_type.clone();
        let stop_flag_for_loop = Arc::clone(&stop_flag);
        let disconnect_tx_for_loop = self.disconnect_tx.clone();

        let handle = thread::spawn(move || {
            Self::callback_worker_loop(
                backend,
                data_callback,
                error_callback,
                status_tx,
                stop_flag_for_loop,
                name_for_loop,
                dac_type,
                disconnect_tx_for_loop,
            );
        });

        self.stop_flag = Some(stop_flag);
        self.status_rx = Some(status_rx);
        self.handle = Some(handle);
    }

    /// Returns whether the worker has been started.
    pub fn is_started(&self) -> bool {
        self.backend.is_none()
    }

    /// Returns the device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Returns the DAC type.
    pub fn dac_type(&self) -> DacType {
        self.dac_type.clone()
    }

    /// Returns the current connection state.
    pub fn state(&self) -> &DacConnectionState {
        &self.state
    }

    /// Polls for status updates from the worker thread.
    ///
    /// Call this periodically to update the connection state.
    /// Has no effect if the worker has not been started.
    pub fn update(&mut self) {
        let Some(ref status_rx) = self.status_rx else {
            return;
        };

        while let Ok(status) = status_rx.try_recv() {
            match status {
                CallbackStatus::Running => {
                    if matches!(self.state, DacConnectionState::Lost { .. }) {
                        self.state = DacConnectionState::Connected {
                            name: self.device_name.clone(),
                        };
                    }
                }
                CallbackStatus::Stopped => {
                    self.state = DacConnectionState::Stopped {
                        name: self.device_name.clone(),
                    };
                }
                CallbackStatus::Error(error) => {
                    self.state = DacConnectionState::Lost {
                        name: self.device_name.clone(),
                        error: Some(error),
                    };
                }
            }
        }

        // Check if thread died unexpectedly
        if !self.is_running() && matches!(self.state, DacConnectionState::Connected { .. }) {
            self.state = DacConnectionState::Lost {
                name: self.device_name.clone(),
                error: Some("Worker thread died unexpectedly".to_string()),
            };
        }
    }

    /// Checks if the worker thread is currently running.
    ///
    /// Returns `false` if:
    /// - The worker has not been started yet (call [`start()`](Self::start) first)
    /// - The worker has stopped (callback returned `None` or [`stop()`](Self::stop) was called)
    /// - The worker thread terminated due to an error
    pub fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Signals the worker to stop (non-blocking).
    ///
    /// The worker will finish its current frame and exit gracefully.
    /// Use [`is_running()`](Self::is_running) to check when the worker has stopped.
    ///
    /// Has no effect if the worker has not been started.
    pub fn stop(&self) {
        if let Some(ref stop_flag) = self.stop_flag {
            stop_flag.store(true, Ordering::Relaxed);
        }
    }

    /// The callback-driven worker loop.
    #[allow(clippy::too_many_arguments)]
    fn callback_worker_loop<F, E>(
        mut backend: Box<dyn DacBackend>,
        mut data_callback: F,
        mut error_callback: E,
        status_tx: Sender<CallbackStatus>,
        stop_flag: Arc<AtomicBool>,
        device_name: String,
        dac_type: DacType,
        disconnect_tx: Option<DisconnectNotifier>,
    ) where
        F: FnMut(&mut CallbackContext) -> Option<LaserFrame> + Send + 'static,
        E: FnMut(CallbackError) + Send + 'static,
    {
        // Connect if not already connected
        if !backend.is_connected() {
            if let Err(e) = backend.connect() {
                error_callback(CallbackError::ConnectionLost(e.to_string()));
                let _ = status_tx.send(CallbackStatus::Error(e.to_string()));
                if let Some(ref tx) = disconnect_tx {
                    let _ = tx.send(device_name);
                }
                return;
            }
        }

        // Notify that we're running
        let _ = status_tx.send(CallbackStatus::Running);

        let mut ctx = CallbackContext {
            device_name: &device_name,
            dac_type,
            frames_written: 0,
        };

        let mut current_frame: Option<LaserFrame> = None;

        while !stop_flag.load(Ordering::Relaxed) {
            // Get frame to write (either pending retry or new from callback)
            let frame = match current_frame.take() {
                Some(f) => f,
                None => {
                    // Request new frame from callback
                    match std::panic::catch_unwind(AssertUnwindSafe(|| data_callback(&mut ctx))) {
                        Ok(Some(f)) => f,
                        Ok(None) => {
                            // Callback signaled stop
                            let _ = backend.stop();
                            let _ = status_tx.send(CallbackStatus::Stopped);
                            return;
                        }
                        Err(_) => {
                            // Callback panicked
                            error_callback(CallbackError::CallbackPanic);
                            let _ =
                                status_tx.send(CallbackStatus::Error("Callback panicked".into()));
                            if let Some(ref tx) = disconnect_tx {
                                let _ = tx.send(device_name);
                            }
                            return;
                        }
                    }
                }
            };

            // Write frame to device
            match backend.write_frame(&frame) {
                Ok(WriteResult::Written) => {
                    ctx.frames_written += 1;
                    // Frame consumed, will request new one next iteration
                }
                Ok(WriteResult::DeviceBusy) => {
                    // Keep frame for retry
                    current_frame = Some(frame);
                    // Small sleep to avoid spinning
                    thread::sleep(Duration::from_micros(500));
                }
                Err(e) => {
                    error_callback(CallbackError::ConnectionLost(e.to_string()));
                    let _ = status_tx.send(CallbackStatus::Error(e.to_string()));
                    if let Some(ref tx) = disconnect_tx {
                        let _ = tx.send(device_name);
                    }
                    return;
                }
            }
        }

        // Stop requested via stop_flag
        let _ = backend.stop();
        let _ = status_tx.send(CallbackStatus::Stopped);
    }
}

impl Drop for DacCallbackWorker {
    fn drop(&mut self) {
        // Signal stop if started
        if let Some(ref stop_flag) = self.stop_flag {
            stop_flag.store(true, Ordering::Relaxed);
        }
        // Notify discovery worker so device can be rediscovered
        if let Some(ref tx) = self.disconnect_tx {
            let _ = tx.send(self.device_name.clone());
        }
    }
}
