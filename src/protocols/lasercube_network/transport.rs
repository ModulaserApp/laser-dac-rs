//! Single-threaded LaserCube network transport worker.

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::buffer_estimate::BufferEstimator;

use super::ack::{parse_command_ack, parse_data_ack, BufferAck};
use super::command;
use super::diagnostics::LaserCubeNetworkDiagnostics;
use super::error::CommunicationError;
use super::pacing::{packet_interval, send_budget, PacerInputs};
use super::packetizer::encode_sample_packet;
use super::profiles::{ConnectionProfile, ConnectionType};
use super::protocol::{Point, CMD_GET_FULL_INFO, CMD_PORT, DATA_PORT, DEFAULT_POINT_RATE};
use super::status::LaserCubeNetworkStatus;

const COMMAND_REPEAT_COUNT: usize = 2;
const POINT_QUEUE_CAPACITY: usize = 128;
const MAX_ACK_DRAIN_PER_LOOP: usize = 32;
const MAX_CONTROL_DRAIN_PER_LOOP: usize = 32;
const MAX_IDLE_SLEEP: Duration = Duration::from_millis(1);
const FULL_INFO_POLL_INACTIVE: Duration = Duration::from_millis(250);
const FULL_INFO_POLL_ACTIVE: Duration = Duration::from_millis(2500);
const COMMS_STALE_TIMEOUT: Duration = Duration::from_millis(4000);
const DIAGNOSTIC_LOG_PERIOD: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct AddressedDevice {
    pub source_addr: SocketAddr,
    pub status: LaserCubeNetworkStatus,
    pub profile: ConnectionProfile,
}

impl AddressedDevice {
    pub fn ip(&self) -> IpAddr {
        self.source_addr.ip()
    }
}

#[derive(Debug)]
enum TransportCommand {
    Enqueue {
        generation: u64,
        point_rate: u32,
        points: Vec<Point>,
    },
}

#[derive(Debug)]
enum PriorityCommand {
    SetOutput { enabled: bool, generation: u64 },
    StopOutput { generation: u64 },
    Shutdown { generation: u64 },
}

#[derive(Debug)]
struct TransportState {
    profile: ConnectionProfile,
    connection_type: ConnectionType,
    status: LaserCubeNetworkStatus,
    host_queue_len: usize,
    host_queue_capacity: usize,
    free_estimate: usize,
    buffer_total: usize,
    point_rate: u32,
    last_estimate: Instant,
    packets_sent: u64,
    samples_sent: u64,
    acks_received: u64,
    command_successes: u64,
    command_failures: u64,
    send_errors: u64,
    packet_errors: u8,
    last_ack: Option<Instant>,
    last_full_info: Option<Instant>,
    last_comms: Option<Instant>,
    connected: bool,
}

#[derive(Clone, Debug)]
pub struct SharedTransportState {
    inner: Arc<Mutex<TransportState>>,
}

impl SharedTransportState {
    fn new(status: &LaserCubeNetworkStatus, profile: ConnectionProfile) -> Self {
        let now = Instant::now();
        let buffer_total = status
            .buffer_max
            .max(super::protocol::DEFAULT_BUFFER_CAPACITY) as usize;
        let host_queue_capacity = (buffer_total * 2).max(profile.max_udp_samples_per_packet * 8);
        Self {
            inner: Arc::new(Mutex::new(TransportState {
                profile,
                connection_type: status.connection_type,
                status: status.clone(),
                host_queue_len: 0,
                host_queue_capacity,
                free_estimate: status.buffer_free.min(status.buffer_max) as usize,
                buffer_total,
                point_rate: status.point_rate.max(DEFAULT_POINT_RATE),
                last_estimate: now,
                packets_sent: 0,
                samples_sent: 0,
                acks_received: 0,
                command_successes: 0,
                command_failures: 0,
                send_errors: 0,
                packet_errors: status.packet_errors,
                last_ack: None,
                last_full_info: Some(now),
                last_comms: Some(now),
                connected: true,
            })),
        }
    }

    pub fn disconnected(profile: ConnectionProfile) -> Self {
        let now = Instant::now();
        Self {
            inner: Arc::new(Mutex::new(TransportState {
                profile,
                connection_type: ConnectionType::Unknown(0),
                status: LaserCubeNetworkStatus::minimal(
                    "0.0.0.0".parse().expect("valid default IP"),
                ),
                host_queue_len: 0,
                host_queue_capacity: 0,
                free_estimate: profile.buffer_total,
                buffer_total: profile.buffer_total,
                point_rate: DEFAULT_POINT_RATE,
                last_estimate: now,
                packets_sent: 0,
                samples_sent: 0,
                acks_received: 0,
                command_successes: 0,
                command_failures: 0,
                send_errors: 0,
                packet_errors: 0,
                last_ack: None,
                last_full_info: None,
                last_comms: None,
                connected: false,
            })),
        }
    }

    fn try_reserve_host_queue(&self, points: usize) -> Result<(), CommunicationError> {
        let mut state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        if state.host_queue_len.saturating_add(points) > state.host_queue_capacity {
            return Err(CommunicationError::QueueFull);
        }
        state.host_queue_len += points;
        Ok(())
    }

    fn release_host_queue(&self, points: usize) {
        let mut state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.host_queue_len = state.host_queue_len.saturating_sub(points);
    }

    fn clear_host_queue(&self) {
        let mut state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.host_queue_len = 0;
    }

    fn mark_disconnected(&self) {
        let mut state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.connected = false;
    }

    pub fn diagnostics(&self) -> LaserCubeNetworkDiagnostics {
        let state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        let now = Instant::now();
        let free = decayed_free(&state, now);
        let last_comms_age = LaserCubeNetworkDiagnostics::age(now, state.last_comms);
        LaserCubeNetworkDiagnostics {
            profile: state.profile,
            connection_type: state.connection_type,
            status: state.status.clone(),
            connected: state.connected,
            communication_stale: communication_stale(&state, now),
            host_queue_len: state.host_queue_len,
            host_queue_capacity: state.host_queue_capacity,
            device_free_estimate: free,
            device_buffered_estimate: state.buffer_total.saturating_sub(free),
            packets_sent: state.packets_sent,
            samples_sent: state.samples_sent,
            acks_received: state.acks_received,
            command_successes: state.command_successes,
            command_failures: state.command_failures,
            send_errors: state.send_errors,
            packet_errors: state.packet_errors,
            last_ack_age: LaserCubeNetworkDiagnostics::age(now, state.last_ack),
            last_full_info_age: LaserCubeNetworkDiagnostics::age(now, state.last_full_info),
            last_comms_age,
        }
    }

    pub(crate) fn is_usable(&self) -> bool {
        let state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.connected && !communication_stale(&state, Instant::now())
    }
}

impl BufferEstimator for SharedTransportState {
    fn estimated_fullness(&self, now: Instant, _pps: u32) -> u64 {
        let state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        let free = decayed_free(&state, now);
        let device_buffered = state.buffer_total.saturating_sub(free);
        device_buffered.saturating_add(state.host_queue_len) as u64
    }
}

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

        let handle = Self {
            tx,
            priority_tx,
            generation,
            join: Some(join),
            state,
        };
        Ok(handle)
    }

    pub fn enqueue(&self, point_rate: u32, points: Vec<Point>) -> Result<(), CommunicationError> {
        let point_count = points.len();
        let generation = self.generation.load(Ordering::SeqCst);
        self.state.try_reserve_host_queue(point_count)?;
        if self.generation.load(Ordering::SeqCst) != generation {
            self.state.release_host_queue(point_count);
            return Err(CommunicationError::QueueFull);
        }
        match self.tx.try_send(TransportCommand::Enqueue {
            generation,
            point_rate,
            points,
        }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(TransportCommand::Enqueue { points, .. })) => {
                if self.generation.load(Ordering::SeqCst) == generation {
                    self.state.release_host_queue(points.len());
                }
                Err(CommunicationError::QueueFull)
            }
            Err(TrySendError::Disconnected(TransportCommand::Enqueue { points, .. })) => {
                if self.generation.load(Ordering::SeqCst) == generation {
                    self.state.release_host_queue(points.len());
                }
                Err(CommunicationError::WorkerStopped)
            }
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

struct TransportWorker {
    device: AddressedDevice,
    cmd_socket: UdpSocket,
    data_socket: UdpSocket,
    state: SharedTransportState,
    queue: VecDeque<Point>,
    packet_buffer: Vec<Point>,
    send_buffer: Vec<u8>,
    recv_buffer: [u8; 1500],
    generation: Arc<AtomicU64>,
    active_generation: u64,
    packet_sequence: u8,
    transfer_sequence: u8,
    current_rate: u32,
    output_enabled: bool,
    next_send_due: Instant,
    next_full_info_due: Instant,
    next_diagnostic_log_due: Instant,
    running: bool,
}

impl TransportWorker {
    fn new(
        device: AddressedDevice,
        cmd_socket: UdpSocket,
        data_socket: UdpSocket,
        state: SharedTransportState,
        generation: Arc<AtomicU64>,
    ) -> Self {
        let current_rate = DEFAULT_POINT_RATE;
        let now = Instant::now();
        Self {
            device,
            cmd_socket,
            data_socket,
            state,
            queue: VecDeque::new(),
            packet_buffer: Vec::new(),
            send_buffer: Vec::new(),
            recv_buffer: [0; 1500],
            generation,
            active_generation: 0,
            packet_sequence: 0,
            transfer_sequence: 0,
            current_rate,
            output_enabled: false,
            next_send_due: now,
            next_full_info_due: now + FULL_INFO_POLL_INACTIVE,
            next_diagnostic_log_due: now + DIAGNOSTIC_LOG_PERIOD,
            running: true,
        }
    }

    fn run(&mut self, rx: Receiver<TransportCommand>, priority_rx: Receiver<PriorityCommand>) {
        while self.running {
            let now = Instant::now();
            self.process_priority_commands(&priority_rx);
            if !self.running {
                break;
            }
            self.process_commands(&rx);
            self.process_priority_commands(&priority_rx);
            if !self.running {
                break;
            }
            self.drain_acks(now);
            self.decay_free_estimate(now);
            self.poll_full_info_if_due(now);
            self.try_send_due_packet(now);
            self.log_diagnostics_if_due(now);
            thread::sleep(self.next_sleep(now));
        }
        let _ = send_repeated(&self.cmd_socket, &command::set_output(false));
        self.state.mark_disconnected();
    }

    fn process_priority_commands(&mut self, rx: &Receiver<PriorityCommand>) {
        for _ in 0..MAX_CONTROL_DRAIN_PER_LOOP {
            match rx.try_recv() {
                Ok(PriorityCommand::SetOutput {
                    enabled,
                    generation,
                }) => {
                    self.active_generation = generation;
                    if !enabled {
                        self.clear_pending_points();
                    }
                    self.output_enabled = enabled;
                    self.record_output_enabled(enabled);
                    let _ = send_repeated(&self.cmd_socket, &command::set_output(enabled));
                }
                Ok(PriorityCommand::StopOutput { generation }) => {
                    self.active_generation = generation;
                    self.clear_pending_points();
                    self.output_enabled = false;
                    self.record_output_enabled(false);
                    let _ = send_repeated(&self.cmd_socket, &command::set_output(false));
                }
                Ok(PriorityCommand::Shutdown { generation }) => {
                    self.active_generation = generation;
                    self.clear_pending_points();
                    self.output_enabled = false;
                    self.record_output_enabled(false);
                    let _ = send_repeated(&self.cmd_socket, &command::set_output(false));
                    self.running = false;
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.active_generation = self.generation.load(Ordering::SeqCst);
                    self.clear_pending_points();
                    self.output_enabled = false;
                    self.record_output_enabled(false);
                    let _ = send_repeated(&self.cmd_socket, &command::set_output(false));
                    self.running = false;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }

    fn process_commands(&mut self, rx: &Receiver<TransportCommand>) {
        for _ in 0..MAX_CONTROL_DRAIN_PER_LOOP {
            match rx.try_recv() {
                Ok(TransportCommand::Enqueue {
                    generation,
                    point_rate,
                    points,
                }) => {
                    if generation != self.active_generation
                        || generation != self.generation.load(Ordering::SeqCst)
                    {
                        continue;
                    }
                    if point_rate != self.current_rate {
                        let _ = self.set_rate(point_rate);
                    }
                    self.queue.extend(points);
                }
                Err(TryRecvError::Disconnected) => {
                    self.clear_pending_points();
                    self.running = false;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }

    fn clear_pending_points(&mut self) {
        self.queue.clear();
        self.packet_buffer.clear();
        self.state.clear_host_queue();
    }

    fn set_rate(&mut self, point_rate: u32) -> io::Result<()> {
        let point_rate = point_rate.max(1);
        send_repeated(&self.cmd_socket, &command::set_rate(point_rate))?;
        self.current_rate = point_rate;
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.point_rate = point_rate;
        state.status.point_rate = point_rate;
        Ok(())
    }

    fn drain_acks(&mut self, now: Instant) {
        let mut last_ack = None;
        let mut ack_count = 0u64;
        for _ in 0..MAX_ACK_DRAIN_PER_LOOP {
            match self.cmd_socket.recv(&mut self.recv_buffer) {
                Ok(len) if len >= 1 && self.recv_buffer[0] == CMD_GET_FULL_INFO => {
                    if let Ok(status) =
                        LaserCubeNetworkStatus::parse(&self.recv_buffer[..len], self.device.ip())
                    {
                        let mut state = self
                            .state
                            .inner
                            .lock()
                            .expect("LaserCube transport state poisoned");
                        apply_status(&mut state, status, now);
                    }
                }
                Ok(len) => {
                    if let Ok(ack) = parse_command_ack(&self.recv_buffer[..len]) {
                        last_ack = Some(ack);
                        ack_count = ack_count.saturating_add(1);
                    } else {
                        self.record_command_response(now, &self.recv_buffer[..len]);
                    }
                }
                Err(e) if would_block(&e) => break,
                Err(_) => break,
            }
        }
        for _ in 0..MAX_ACK_DRAIN_PER_LOOP {
            match self.data_socket.recv(&mut self.recv_buffer) {
                Ok(len) => {
                    if let Ok(ack) = parse_data_ack(&self.recv_buffer[..len]) {
                        last_ack = Some(ack);
                        ack_count = ack_count.saturating_add(1);
                    }
                }
                Err(e) if would_block(&e) => break,
                Err(_) => break,
            }
        }
        if let Some(ack) = last_ack {
            self.apply_ack(now, ack, ack_count);
        }
    }

    fn apply_ack(&self, now: Instant, ack: BufferAck, ack_count: u64) {
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.free_estimate = (ack.free_space as usize).min(state.buffer_total);
        state.last_estimate = now;
        state.acks_received = state.acks_received.saturating_add(ack_count);
        state.last_ack = Some(now);
        state.last_comms = Some(now);
    }

    fn decay_free_estimate(&self, now: Instant) {
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        let elapsed = now.saturating_duration_since(state.last_estimate);
        let drained = (elapsed.as_secs_f64() * state.point_rate as f64) as usize;
        if drained > 0 {
            state.free_estimate = state
                .free_estimate
                .saturating_add(drained)
                .min(state.buffer_total);
            state.last_estimate = now;
        }
    }

    fn try_send_due_packet(&mut self, now: Instant) {
        if self.active_generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        if now < self.next_send_due || self.queue.is_empty() {
            return;
        }

        let budget = {
            let state = self
                .state
                .inner
                .lock()
                .expect("LaserCube transport state poisoned");
            send_budget(PacerInputs {
                queue_len: self.queue.len(),
                free_estimate: state.free_estimate,
                buffer_total: state.buffer_total,
                remote_buffer_cutoff: state.profile.remote_buffer_cutoff,
                per_tick_packet_budget: state.profile.max_udp_samples_per_packet,
            })
        };
        if budget == 0 {
            return;
        }

        self.packet_buffer.clear();
        for _ in 0..budget {
            if let Some(point) = self.queue.pop_front() {
                self.packet_buffer.push(point);
            }
        }
        if self.packet_buffer.is_empty() {
            return;
        }

        if encode_sample_packet(
            self.packet_sequence,
            self.transfer_sequence,
            &self.packet_buffer,
            &mut self.send_buffer,
        )
        .and_then(|_| self.data_socket.send(&self.send_buffer).map(|_| ()))
        .is_ok()
        {
            let sent = self.packet_buffer.len();
            self.record_send(now, sent);
            self.packet_sequence = self.packet_sequence.wrapping_add(1);
            self.transfer_sequence = self.transfer_sequence.wrapping_add(1);
            self.next_send_due = now + packet_interval(sent, self.current_rate);
        } else {
            let mut state = self
                .state
                .inner
                .lock()
                .expect("LaserCube transport state poisoned");
            state.send_errors = state.send_errors.saturating_add(1);
            drop(state);
            for point in self.packet_buffer.drain(..).rev() {
                self.queue.push_front(point);
            }
            self.next_send_due = now + self.device.profile.wait_buffer_sleep.min(MAX_IDLE_SLEEP);
        }
    }

    fn poll_full_info_if_due(&mut self, now: Instant) {
        if now < self.next_full_info_due {
            return;
        }
        let poll_period = full_info_poll_period(self.output_enabled);
        self.next_full_info_due = now + poll_period;
        if self.cmd_socket.send(&command::get_full_info()).is_err() {
            let mut state = self
                .state
                .inner
                .lock()
                .expect("LaserCube transport state poisoned");
            state.send_errors = state.send_errors.saturating_add(1);
        }
    }

    fn record_output_enabled(&self, enabled: bool) {
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.status.output_enabled = enabled;
    }

    fn record_command_response(&self, now: Instant, buffer: &[u8]) {
        if buffer.len() < 2 {
            return;
        }
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        if buffer[1] == 0 {
            state.command_successes = state.command_successes.saturating_add(1);
            state.last_comms = Some(now);
        } else {
            state.command_failures = state.command_failures.saturating_add(1);
            state.last_comms = Some(now);
        }
    }

    fn log_diagnostics_if_due(&mut self, now: Instant) {
        if now < self.next_diagnostic_log_due {
            return;
        }
        self.next_diagnostic_log_due = now + DIAGNOSTIC_LOG_PERIOD;
        if (self.output_enabled || !self.queue.is_empty()) && log::log_enabled!(log::Level::Debug) {
            log::debug!(
                "LaserCube network diagnostics: {:?}",
                self.state.diagnostics()
            );
        }
    }

    fn record_send(&self, now: Instant, sent: usize) {
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.free_estimate = state.free_estimate.saturating_sub(sent);
        state.last_estimate = now;
        state.host_queue_len = state.host_queue_len.saturating_sub(sent);
        state.packets_sent = state.packets_sent.saturating_add(1);
        state.samples_sent = state.samples_sent.saturating_add(sent as u64);
    }

    fn next_sleep(&self, now: Instant) -> Duration {
        if !self.queue.is_empty() && now < self.next_send_due {
            return self
                .next_send_due
                .saturating_duration_since(now)
                .min(MAX_IDLE_SLEEP);
        }
        if self.queue.is_empty() {
            return self.device.profile.wait_buffer_sleep.min(MAX_IDLE_SLEEP);
        }
        MAX_IDLE_SLEEP
    }
}

fn full_info_poll_period(output_enabled: bool) -> Duration {
    if output_enabled {
        FULL_INFO_POLL_ACTIVE
    } else {
        FULL_INFO_POLL_INACTIVE
    }
}

fn apply_status(state: &mut TransportState, status: LaserCubeNetworkStatus, now: Instant) {
    state.connection_type = status.connection_type;
    state.packet_errors = status.packet_errors;
    state.buffer_total =
        (status.buffer_max as usize).max(super::protocol::DEFAULT_BUFFER_CAPACITY as usize);
    state.free_estimate = (status.buffer_free as usize).min(state.buffer_total);
    if status.point_rate > 0 {
        state.point_rate = status.point_rate;
    }
    state.last_estimate = now;
    state.last_full_info = Some(now);
    state.last_comms = Some(now);
    state.status = status;
}

fn communication_stale(state: &TransportState, now: Instant) -> bool {
    match state.last_comms {
        Some(last_comms) => now.saturating_duration_since(last_comms) > COMMS_STALE_TIMEOUT,
        None => state.connected,
    }
}

fn send_repeated(socket: &UdpSocket, cmd: &[u8]) -> io::Result<()> {
    for _ in 0..COMMAND_REPEAT_COUNT {
        socket.send(cmd)?;
    }
    Ok(())
}

fn startup_commands(status: &LaserCubeNetworkStatus, profile: ConnectionProfile) -> Vec<Vec<u8>> {
    let mut commands = vec![
        command::set_output(false).to_vec(),
        command::enable_buffer_size_response(true).to_vec(),
        command::set_rate(DEFAULT_POINT_RATE).to_vec(),
    ];
    if command::threshold_supported(status) {
        let threshold = profile.remote_buffer_cutoff;
        if threshold < status.buffer_max as usize {
            commands.push(command::set_dac_buffer_threshold(threshold as u32).to_vec());
        }
    }
    commands
}

fn decayed_free(state: &TransportState, now: Instant) -> usize {
    let elapsed = now.saturating_duration_since(state.last_estimate);
    let drained = (elapsed.as_secs_f64() * state.point_rate as f64) as usize;
    state
        .free_estimate
        .saturating_add(drained)
        .min(state.buffer_total)
}

fn would_block(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn state_with_host_queue(host_queue_len: usize) -> SharedTransportState {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        status.buffer_max = 6000;
        status.buffer_free = 5000;
        let profile = ConnectionProfile::unknown_conservative(6000);
        let state = SharedTransportState::new(&status, profile);
        state.try_reserve_host_queue(host_queue_len).unwrap();
        state
    }

    #[test]
    fn estimator_includes_host_queue() {
        let state = state_with_host_queue(320);
        let now = state.inner.lock().unwrap().last_estimate;
        assert_eq!(state.estimated_fullness(now, 30_000), 1320);
    }

    #[test]
    fn bounded_queue_rejects_over_capacity() {
        let state = state_with_host_queue(0);
        let capacity = state.diagnostics().host_queue_capacity;
        assert!(state.try_reserve_host_queue(capacity).is_ok());
        assert!(state.try_reserve_host_queue(1).is_err());
    }

    #[test]
    fn diagnostics_report_comms_stale_after_timeout() {
        let state = state_with_host_queue(0);
        {
            let mut inner = state.inner.lock().unwrap();
            inner.last_comms =
                Some(Instant::now() - COMMS_STALE_TIMEOUT - Duration::from_millis(1));
        }
        let diagnostics = state.diagnostics();
        assert!(diagnostics.communication_stale);
        assert!(diagnostics.last_comms_age >= Some(COMMS_STALE_TIMEOUT));
        assert!(!state.is_usable());
    }

    #[test]
    fn transport_state_is_usable_while_comms_are_fresh() {
        let state = state_with_host_queue(0);
        assert!(state.is_usable());
    }

    #[test]
    fn diagnostics_include_latest_status_snapshot() {
        let state = state_with_host_queue(0);
        {
            let mut inner = state.inner.lock().unwrap();
            inner.status.firmware_major = 1;
            inner.status.firmware_minor = 24;
            inner.status.battery_percent = 255;
            inner.status.temperature_c = 42;
            inner.status.interlock_enabled = true;
            inner.command_successes = 2;
            inner.command_failures = 1;
        }
        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics.status.firmware_major, 1);
        assert_eq!(diagnostics.status.firmware_minor, 24);
        assert!(diagnostics.status.is_plugged_in());
        assert_eq!(diagnostics.status.temperature_c, 42);
        assert!(diagnostics.status.interlock_enabled);
        assert_eq!(diagnostics.command_successes, 2);
        assert_eq!(diagnostics.command_failures, 1);
    }

    #[test]
    fn full_info_poll_period_uses_active_and_inactive_cadence() {
        assert_eq!(full_info_poll_period(false), FULL_INFO_POLL_INACTIVE);
        assert_eq!(full_info_poll_period(true), FULL_INFO_POLL_ACTIVE);
    }

    #[test]
    fn startup_commands_do_not_include_warmup_or_clear_ringbuffer() {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        status.firmware_minor = 24;
        let profile = ConnectionProfile::unknown_conservative(6000);
        let commands = startup_commands(&status, profile);
        assert_eq!(commands[0], vec![0x80, 0x00]);
        assert_eq!(commands[1], vec![0x78, 0x01]);
        assert_eq!(commands[2], vec![0x82, 0x30, 0x75, 0x00, 0x00]);
        assert!(commands.iter().all(|cmd| cmd.first() != Some(&0x8D)));
        assert!(commands.iter().all(|cmd| cmd.first() != Some(&0xA9)));
    }
}
