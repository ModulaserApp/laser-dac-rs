use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use super::super::error::CommunicationError;
use super::super::protocol::{CMD_PORT, DATA_PORT};
use super::state::SharedTransportState;
use super::worker::TransportWorker;
use super::{
    send_repeated, startup_commands, AddressedDevice, PriorityCommand, TransportCommand,
    POINT_QUEUE_CAPACITY,
};

pub struct TransportHandle {
    tx: SyncSender<TransportCommand>,
    priority_tx: Sender<PriorityCommand>,
    generation: Arc<AtomicU64>,
    join: Option<JoinHandle<()>>,
    state: SharedTransportState,
}

impl TransportHandle {
    pub fn connect(device: AddressedDevice) -> Result<Self, CommunicationError> {
        let cmd_socket = UdpSocket::bind("0.0.0.0:0")?;
        cmd_socket.connect(SocketAddr::new(device.ip(), CMD_PORT))?;
        cmd_socket.set_nonblocking(true)?;

        let data_socket = UdpSocket::bind("0.0.0.0:0")?;
        data_socket.connect(SocketAddr::new(device.ip(), DATA_PORT))?;
        data_socket.set_nonblocking(true)?;

        for cmd in startup_commands(&device.status, device.profile) {
            send_repeated(&cmd_socket, &cmd)?;
        }

        let state = SharedTransportState::new(&device.status, device.profile);
        let worker_state = state.clone();
        let generation = Arc::new(AtomicU64::new(0));
        let worker_generation = generation.clone();
        let (tx, rx) = mpsc::sync_channel(POINT_QUEUE_CAPACITY);
        let (priority_tx, priority_rx) = mpsc::channel();
        let join = thread::Builder::new()
            .name("lasercube-network-transport".to_string())
            .spawn(move || {
                let mut worker = TransportWorker::new(
                    device,
                    cmd_socket,
                    data_socket,
                    worker_state,
                    worker_generation,
                );
                worker.run(rx, priority_rx);
            })
            .map_err(|e| CommunicationError::Protocol(e.to_string()))?;

        Ok(Self {
            tx,
            priority_tx,
            generation,
            join: Some(join),
            state,
        })
    }

    pub fn enqueue(
        &self,
        point_rate: u32,
        points: Vec<super::super::protocol::Point>,
    ) -> Result<(), CommunicationError> {
        let reservation = self.state.reserve_host_points(points.len())?;
        let generation = self.generation.load(Ordering::SeqCst);
        match self.tx.try_send(TransportCommand::Enqueue {
            generation,
            point_rate,
            points,
            _reservation: reservation,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err(CommunicationError::QueueFull),
            Err(TrySendError::Disconnected(_)) => Err(CommunicationError::WorkerStopped),
        }
    }

    pub fn set_output(&self, enabled: bool) -> Result<(), CommunicationError> {
        let generation = if enabled {
            self.generation.load(Ordering::SeqCst)
        } else {
            self.generation.fetch_add(1, Ordering::SeqCst) + 1
        };
        self.priority_tx
            .send(PriorityCommand::SetOutput {
                enabled,
                generation,
            })
            .map_err(|_| CommunicationError::WorkerStopped)
    }

    pub fn stop_output(&self) -> Result<(), CommunicationError> {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.priority_tx
            .send(PriorityCommand::StopOutput { generation })
            .map_err(|_| CommunicationError::WorkerStopped)
    }

    pub fn state(&self) -> &SharedTransportState {
        &self.state
    }

    pub fn is_usable(&self) -> bool {
        self.state.is_usable()
    }

    pub fn shutdown(&mut self) {
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let _ = self
            .priority_tx
            .send(PriorityCommand::Shutdown { generation });
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.state.mark_disconnected();
    }
}

impl Drop for TransportHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
