//! Background worker for non-blocking DAC frame writing.
//!
//! The `DacWorker` spawns a background thread that handles frame writing
//! to any DAC backend. Frames are sent via a bounded channel (capacity 1),
//! and old frames are automatically dropped if the device is busy.

use std::sync::mpsc::{self, Receiver as MpscReceiver, Sender, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

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

/// Background worker that writes frames to a single DAC device.
///
/// Frames are sent via a bounded channel (capacity 1). If the consumer is busy,
/// old frames are automatically dropped to prevent backlog.
///
/// # Example
///
/// ```ignore
/// use laser_dac::{DacWorker, DacType, LaserFrame, LaserPoint};
///
/// // Assume `backend` is a connected [`DacBackend`]
/// let worker = DacWorker::new("My DAC".to_string(), DacType::Helios, backend);
///
/// // Submit frames (non-blocking)
/// let frame = LaserFrame::new(30000, vec![...]);
/// worker.submit_frame(frame);
///
/// // Poll for status updates
/// worker.update();
///
/// // Stop output when done
/// worker.stop_output();
/// ```
pub struct DacWorker {
    device_name: String,
    dac_type: DacType,
    command_tx: SyncSender<WorkerCommand>,
    status_rx: MpscReceiver<WorkerStatus>,
    handle: Option<JoinHandle<()>>,
    state: DacConnectionState,
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
        let handle = thread::spawn(move || {
            Self::worker_loop(backend, command_rx, status_tx, name_for_loop, disconnect_tx);
        });

        Self {
            device_name: device_name.clone(),
            dac_type,
            command_tx,
            status_rx,
            handle: Some(handle),
            state: DacConnectionState::Connected { name: device_name },
        }
    }

    /// Returns the device name.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Returns the DAC type.
    pub fn dac_type(&self) -> DacType {
        self.dac_type
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
    }
}
