use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender, SyncSender, TrySendError};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::super::command;
use super::super::error::CommunicationError;
use super::super::protocol::{CMD_GET_FULL_INFO, CMD_PORT, DATA_PORT};
use super::state::SharedTransportState;
use super::worker::TransportWorker;
use super::{
    send_repeated, startup_commands, would_block, AddressedDevice, PriorityCommand,
    TransportCommand, POINT_QUEUE_CAPACITY,
};

/// Per-attempt timeout for the connect-time reachability handshake.
const CONNECT_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(300);
/// Number of retries (in addition to the first attempt) for the handshake.
const CONNECT_HANDSHAKE_RETRIES: usize = 2;

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

        let data_socket = UdpSocket::bind("0.0.0.0:0")?;
        data_socket.connect(SocketAddr::new(device.ip(), DATA_PORT))?;
        data_socket.set_nonblocking(true)?;

        for cmd in startup_commands(&device.status, device.profile) {
            send_repeated(&cmd_socket, &cmd)?;
        }

        // Verify the device is actually reachable before reporting connected, so
        // a dead/absent cube fails fast instead of appearing connected until the
        // comms-stale timeout elapses.
        verify_reachable(&cmd_socket)?;
        cmd_socket.set_nonblocking(true)?;

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

/// Request full-info and wait for a reply, retrying a couple of times with a
/// short timeout. Returns an error if the device never answers, so `connect()`
/// fails fast for an unreachable cube. The socket is left blocking; the caller
/// switches it back to non-blocking afterwards.
fn verify_reachable(cmd_socket: &UdpSocket) -> Result<(), CommunicationError> {
    cmd_socket.set_read_timeout(Some(CONNECT_HANDSHAKE_TIMEOUT))?;
    let mut buffer = [0u8; 1500];
    for _ in 0..=CONNECT_HANDSHAKE_RETRIES {
        cmd_socket.send(&command::get_full_info())?;
        loop {
            match cmd_socket.recv(&mut buffer) {
                Ok(len) if len >= 1 && buffer[0] == CMD_GET_FULL_INFO => {
                    cmd_socket.set_read_timeout(None)?;
                    return Ok(());
                }
                // Some other response (e.g. a command ACK); keep waiting within
                // this attempt's timeout for the full-info reply.
                Ok(_) => continue,
                Err(e) if would_block(&e) => break,
                Err(e) => {
                    cmd_socket.set_read_timeout(None)?;
                    return Err(e.into());
                }
            }
        }
    }
    cmd_socket.set_read_timeout(None)?;
    Err(CommunicationError::Protocol(
        "LaserCube network device did not respond to full-info handshake".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use std::net::IpAddr;
    use std::time::Duration;

    use crate::protocols::lasercube_network::backend::mock::{self, MockDac};

    use super::super::super::profiles::{ConnectionProfile, ConnectionType};
    use super::super::super::protocol::Point;
    use super::super::super::status::LaserCubeNetworkStatus;
    use super::*;

    fn device_for(ip: IpAddr) -> AddressedDevice {
        let mut status = LaserCubeNetworkStatus::minimal(ip);
        status.buffer_free = 6000;
        status.buffer_max = 6000;
        status.point_rate_max = 30_000;
        let profile = ConnectionProfile::for_connection(ConnectionType::EthernetServer, 6000);
        AddressedDevice {
            source_addr: SocketAddr::new(ip, CMD_PORT),
            status,
            profile,
        }
    }

    #[test]
    fn connect_sends_startup_commands_and_is_usable() {
        let dac = MockDac::start();
        let handle = TransportHandle::connect(device_for(dac.ip())).unwrap();

        assert!(handle.is_usable());
        assert!(
            mock::wait_until(Duration::from_millis(1000), || {
                let cmds = dac.cmd_packets();
                cmds.iter().any(|p| p.as_slice() == [0x80, 0x00]) // set_output(false)
                    && cmds.iter().any(|p| p.as_slice() == [0x78, 0x01]) // enable buffer size resp
                    && cmds.iter().any(|p| p.first() == Some(&0x82)) // set_rate
            }),
            "startup commands missing: {:?}",
            dac.cmd_packets()
        );
    }

    #[test]
    fn set_output_emits_enable_command() {
        let dac = MockDac::start();
        let handle = TransportHandle::connect(device_for(dac.ip())).unwrap();

        handle.set_output(true).unwrap();
        assert!(
            mock::wait_until(Duration::from_millis(1000), || dac
                .cmd_packets()
                .iter()
                .any(|p| p.as_slice() == [0x80, 0x01])),
            "expected set_output(true), got {:?}",
            dac.cmd_packets()
        );
    }

    #[test]
    fn enqueue_rejects_writes_beyond_host_queue_capacity() {
        let dac = MockDac::start();
        let handle = TransportHandle::connect(device_for(dac.ip())).unwrap();

        let err = handle
            .enqueue(30_000, vec![Point::blank(); 20_000])
            .unwrap_err();
        assert!(matches!(err, CommunicationError::QueueFull));
    }

    #[test]
    fn data_acks_flow_back_into_diagnostics() {
        let dac = MockDac::start();
        let handle = TransportHandle::connect(device_for(dac.ip())).unwrap();

        // Feed points so the worker emits sample packets that the mock ACKs.
        for _ in 0..8 {
            let _ = handle.enqueue(30_000, vec![Point::blank(); 200]);
            std::thread::sleep(Duration::from_millis(10));
        }

        assert!(
            mock::wait_until(Duration::from_millis(1500), || handle
                .state()
                .diagnostics()
                .acks_received
                > 0),
            "no ACKs applied: {:?}",
            handle.state().diagnostics()
        );

        let diagnostics = handle.state().diagnostics();
        assert!(diagnostics.packets_sent > 0);
        assert!(diagnostics.last_data_ack_sequence.is_some());
        assert!(diagnostics.last_ack_free_space.is_some());
    }

    #[test]
    fn shutdown_joins_worker_and_marks_unusable() {
        let dac = MockDac::start();
        let mut handle = TransportHandle::connect(device_for(dac.ip())).unwrap();
        assert!(handle.is_usable());

        handle.shutdown();
        assert!(!handle.is_usable());

        // Shutdown is idempotent (join already taken).
        handle.shutdown();
        assert!(!handle.is_usable());
    }

    #[test]
    fn drop_shuts_down_worker_without_hanging() {
        let dac = MockDac::start();
        let handle = TransportHandle::connect(device_for(dac.ip())).unwrap();
        // Drop joins the worker thread; this must return promptly.
        drop(handle);
        drop(dac);
    }
}
