use std::collections::VecDeque;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use super::super::ack::{parse_command_ack, parse_data_ack, BufferAck};
use super::super::command;
use super::super::pacing::{packet_interval, send_budget, PacerInputs};
use super::super::packetizer::encode_sample_packet;
use super::super::protocol::{Point, CMD_GET_FULL_INFO, DEFAULT_POINT_RATE};
use super::super::status::LaserCubeNetworkStatus;
use super::state::{SharedTransportState, TransportState};
use super::{
    send_repeated, would_block, AddressedDevice, DatagramSocket, PriorityCommand, TransportCommand,
    DIAGNOSTIC_LOG_PERIOD, FULL_INFO_POLL_ACTIVE, FULL_INFO_POLL_INACTIVE, MAX_ACK_DRAIN_PER_LOOP,
    MAX_CONTROL_DRAIN_PER_LOOP, MAX_IDLE_SLEEP,
};

pub(super) struct TransportWorker<S> {
    device: AddressedDevice,
    cmd_socket: S,
    data_socket: S,
    state: SharedTransportState,
    queue: VecDeque<Point>,
    packet_buffer: Vec<Point>,
    send_buffer: Vec<u8>,
    recv_buffer: [u8; 1500],
    generation: Arc<AtomicU64>,
    active_generation: u64,
    packet_sequence: u8,
    transfer_sequence: u8,
    packet_send_times: [Option<Instant>; 256],
    current_rate: u32,
    output_enabled: bool,
    next_send_due: Instant,
    next_full_info_due: Instant,
    next_diagnostic_log_due: Instant,
    running: bool,
}

impl<S: DatagramSocket> TransportWorker<S> {
    pub(super) fn new(
        device: AddressedDevice,
        cmd_socket: S,
        data_socket: S,
        state: SharedTransportState,
        generation: Arc<AtomicU64>,
    ) -> Self {
        let current_rate = super::super::clamp_point_rate(&device.status, DEFAULT_POINT_RATE);
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
            packet_send_times: [None; 256],
            current_rate,
            output_enabled: false,
            next_send_due: now,
            next_full_info_due: now + FULL_INFO_POLL_INACTIVE,
            next_diagnostic_log_due: now + DIAGNOSTIC_LOG_PERIOD,
            running: true,
        }
    }

    pub(super) fn run(
        &mut self,
        rx: Receiver<TransportCommand>,
        priority_rx: Receiver<PriorityCommand>,
    ) {
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
                    self.next_full_info_due =
                        Instant::now() + full_info_poll_period(self.output_enabled);
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
                    _reservation,
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
                    _reservation.commit();
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
        let point_rate = {
            let state = self
                .state
                .inner
                .lock()
                .expect("LaserCube transport state poisoned");
            super::super::clamp_point_rate(&state.status, point_rate)
        };
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

    fn apply_ack(&mut self, now: Instant, ack: BufferAck, ack_count: u64) {
        let (data_ack_sequence, ack_rtt) =
            ack_sequence_and_rtt(&mut self.packet_send_times, ack, now);
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.free_estimate = (ack.free_space as usize).min(state.buffer_total);
        state.last_estimate = now;
        state.acks_received = state.acks_received.saturating_add(ack_count);
        state.last_ack_free_space = Some(ack.free_space);
        if let Some(sequence) = data_ack_sequence {
            state.last_data_ack_sequence = Some(sequence);
        }
        if let Some(rtt) = ack_rtt {
            state.last_ack_rtt = Some(rtt);
        }
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

        let (budget, free_estimate, buffer_total, remote_buffer_cutoff) = {
            let state = self
                .state
                .inner
                .lock()
                .expect("LaserCube transport state poisoned");
            let budget = send_budget(PacerInputs {
                queue_len: self.queue.len(),
                free_estimate: state.free_estimate,
                buffer_total: state.buffer_total,
                remote_buffer_cutoff: state.profile.remote_buffer_cutoff,
                per_tick_packet_budget: state.profile.max_udp_samples_per_packet,
            });
            (
                budget,
                state.free_estimate,
                state.buffer_total,
                state.profile.remote_buffer_cutoff,
            )
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
            self.packet_send_times[self.packet_sequence as usize] = Some(now);
            self.record_send(now, sent);
            self.packet_sequence = self.packet_sequence.wrapping_add(1);
            self.transfer_sequence = self.transfer_sequence.wrapping_add(1);
            self.next_send_due =
                if should_continue_topup(free_estimate, buffer_total, remote_buffer_cutoff, sent) {
                    now
                } else {
                    now + packet_interval(sent, self.current_rate)
                };
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
        (status.buffer_max as usize).max(super::super::protocol::DEFAULT_BUFFER_CAPACITY as usize);
    state.free_estimate = (status.buffer_free as usize).min(state.buffer_total);
    if status.point_rate > 0 {
        state.point_rate = super::super::clamp_point_rate(&status, status.point_rate);
    }
    state.last_estimate = now;
    state.last_full_info = Some(now);
    state.last_comms = Some(now);
    state.status = status;
}

fn ack_sequence_and_rtt(
    packet_send_times: &mut [Option<Instant>; 256],
    ack: BufferAck,
    now: Instant,
) -> (Option<u8>, Option<Duration>) {
    let Some(sequence) = ack.packet_sequence else {
        return (None, None);
    };
    let sent_at = packet_send_times[sequence as usize].take();
    (
        Some(sequence),
        sent_at.map(|sent_at| now.saturating_duration_since(sent_at)),
    )
}

fn should_continue_topup(
    free_estimate_before_send: usize,
    buffer_total: usize,
    remote_buffer_cutoff: usize,
    sent: usize,
) -> bool {
    let buffered_before_send = buffer_total.saturating_sub(free_estimate_before_send);
    buffered_before_send.saturating_add(sent) < remote_buffer_cutoff
}

#[cfg(test)]
mod tests {
    use super::super::super::ack::AckSource;
    use super::*;
    use std::collections::VecDeque;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::atomic::AtomicU64;
    use std::sync::{mpsc, Arc, Mutex};

    #[derive(Clone, Default)]
    struct FakeSocket {
        sent: Arc<Mutex<Vec<Vec<u8>>>>,
        recv_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
    }

    impl FakeSocket {
        fn push_recv(&self, packet: Vec<u8>) {
            self.recv_queue.lock().unwrap().push_back(packet);
        }

        fn sent_packets(&self) -> Vec<Vec<u8>> {
            self.sent.lock().unwrap().clone()
        }
    }

    impl DatagramSocket for FakeSocket {
        fn send(&self, buffer: &[u8]) -> io::Result<usize> {
            self.sent.lock().unwrap().push(buffer.to_vec());
            Ok(buffer.len())
        }

        fn recv(&self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some(packet) = self.recv_queue.lock().unwrap().pop_front() else {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "empty fake socket",
                ));
            };
            let len = packet.len().min(buffer.len());
            buffer[..len].copy_from_slice(&packet[..len]);
            Ok(len)
        }
    }

    fn fake_worker() -> (TransportWorker<FakeSocket>, FakeSocket, FakeSocket) {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        status.buffer_free = 6000;
        status.buffer_max = 6000;
        let profile = super::super::super::profiles::ConnectionProfile::unknown_conservative(6000);
        let device = AddressedDevice {
            source_addr: "127.0.0.1:45457".parse().unwrap(),
            status: status.clone(),
            profile,
        };
        let state = SharedTransportState::new(&status, profile);
        let generation = Arc::new(AtomicU64::new(0));
        let cmd_socket = FakeSocket::default();
        let data_socket = FakeSocket::default();
        let worker = TransportWorker::new(
            device,
            cmd_socket.clone(),
            data_socket.clone(),
            state,
            generation,
        );
        (worker, cmd_socket, data_socket)
    }

    #[test]
    fn full_info_poll_period_uses_active_and_inactive_cadence() {
        assert_eq!(full_info_poll_period(false), FULL_INFO_POLL_INACTIVE);
        assert_eq!(full_info_poll_period(true), FULL_INFO_POLL_ACTIVE);
    }

    #[test]
    fn data_ack_sequence_records_rtt_and_clears_send_slot() {
        let now = Instant::now();
        let mut packet_send_times = [None; 256];
        packet_send_times[7] = Some(now - Duration::from_millis(12));
        let ack = BufferAck {
            source: AckSource::Data,
            packet_sequence: Some(7),
            free_space: 1234,
        };

        let (sequence, rtt) = ack_sequence_and_rtt(&mut packet_send_times, ack, now);

        assert_eq!(sequence, Some(7));
        assert_eq!(rtt, Some(Duration::from_millis(12)));
        assert_eq!(packet_send_times[7], None);
    }

    #[test]
    fn command_ack_has_no_data_sequence_or_rtt() {
        let now = Instant::now();
        let mut packet_send_times = [None; 256];
        packet_send_times[7] = Some(now - Duration::from_millis(12));
        let ack = BufferAck {
            source: AckSource::Command,
            packet_sequence: None,
            free_space: 1234,
        };

        let (sequence, rtt) = ack_sequence_and_rtt(&mut packet_send_times, ack, now);

        assert_eq!(sequence, None);
        assert_eq!(rtt, None);
        assert!(packet_send_times[7].is_some());
    }

    #[test]
    fn fake_socket_worker_applies_data_ack() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let now = Instant::now();
        worker.packet_send_times[9] = Some(now - Duration::from_millis(5));
        data_socket.push_recv(vec![0x8A, 0x09, 0x34, 0x12]);

        worker.drain_acks(now);

        let diagnostics = worker.state.diagnostics();
        assert_eq!(diagnostics.last_data_ack_sequence, Some(9));
        assert_eq!(diagnostics.last_ack_free_space, Some(0x1234));
        assert_eq!(diagnostics.last_ack_rtt, Some(Duration::from_millis(5)));
        assert_eq!(diagnostics.acks_received, 1);
    }

    #[test]
    fn fake_socket_worker_sends_one_packet_per_due_send() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(200).unwrap();
        worker.queue.extend(vec![Point::blank(); 200]);
        reservation.commit();
        let now = Instant::now();
        worker.next_send_due = now;

        worker.try_send_due_packet(now);

        let sent = data_socket.sent_packets();
        assert_eq!(sent.len(), 1);
        assert_eq!(&sent[0][..4], &[0xA9, 0x00, 0x00, 0x00]);
        assert_eq!(sent[0].len(), 4 + 80 * 10);
        assert_eq!(worker.queue.len(), 120);
        assert_eq!(worker.state.diagnostics().host_queue_len, 120);
    }

    #[test]
    fn worker_keeps_send_due_immediate_while_topping_up_remote_buffer() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(200).unwrap();
        worker.queue.extend(vec![Point::blank(); 200]);
        reservation.commit();
        let now = Instant::now();
        worker.next_send_due = now;

        worker.try_send_due_packet(now);

        assert_eq!(data_socket.sent_packets().len(), 1);
        assert_eq!(worker.next_send_due, now);
    }

    #[test]
    fn worker_paces_after_remote_buffer_reaches_cutoff() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        {
            let mut state = worker.state.inner.lock().unwrap();
            state.free_estimate = 6000
                - (state.profile.remote_buffer_cutoff - state.profile.max_udp_samples_per_packet);
        }
        let reservation = worker.state.reserve_host_points(200).unwrap();
        worker.queue.extend(vec![Point::blank(); 200]);
        reservation.commit();
        let now = Instant::now();
        worker.next_send_due = now;

        worker.try_send_due_packet(now);

        assert_eq!(data_socket.sent_packets().len(), 1);
        assert!(worker.next_send_due > now);
    }

    #[test]
    fn process_commands_updates_worker_owned_host_queue_len() {
        let (mut worker, _cmd_socket, _data_socket) = fake_worker();
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(TransportCommand::Enqueue {
            generation: 0,
            point_rate: DEFAULT_POINT_RATE,
            points: vec![Point::blank(); 5],
            _reservation: worker.state.reserve_host_points(5).unwrap(),
        })
        .unwrap();

        worker.process_commands(&rx);

        assert_eq!(worker.queue.len(), 5);
        assert_eq!(worker.state.diagnostics().host_queue_len, 5);
    }

    #[test]
    fn priority_output_disable_clears_pending_points() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(5).unwrap();
        worker.queue.extend(vec![Point::blank(); 5]);
        reservation.commit();
        let (tx, rx) = mpsc::channel();
        tx.send(PriorityCommand::SetOutput {
            enabled: false,
            generation: 1,
        })
        .unwrap();

        worker.process_priority_commands(&rx);

        assert!(worker.queue.is_empty());
        assert_eq!(worker.state.diagnostics().host_queue_len, 0);
        let sent = cmd_socket.sent_packets();
        assert_eq!(sent, vec![vec![0x80, 0x00], vec![0x80, 0x00]]);
    }
}
