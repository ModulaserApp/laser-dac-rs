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
use super::state::{buffer_total_from_max, SharedTransportState, TransportState};
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
    // Samples carried by each still-outstanding (sent-but-unacked) packet,
    // indexed by packet sequence. Used to discount in-flight points from an
    // ACK's reported free space. Zero means the slot is idle/acked.
    packet_sample_counts: [u16; 256],
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
            packet_sample_counts: [0; 256],
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
            let deadline = self.next_wake(now);
            self.sleep_until_precise(deadline, &priority_rx);
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
        let mut ack_count = 0u64;
        for _ in 0..MAX_ACK_DRAIN_PER_LOOP {
            match self.cmd_socket.recv(&mut self.recv_buffer) {
                Ok(len) if len >= 1 && self.recv_buffer[0] == CMD_GET_FULL_INFO => {
                    if let Ok(status) =
                        LaserCubeNetworkStatus::parse(&self.recv_buffer[..len], self.device.ip())
                    {
                        let reported_output_enabled = status.output_enabled;
                        let interlock_enabled = status.interlock_enabled;
                        {
                            let mut state = self
                                .state
                                .inner
                                .lock()
                                .expect("LaserCube transport state poisoned");
                            apply_status(&mut state, status, now);
                        }
                        self.reconcile_output_enable(reported_output_enabled, interlock_enabled);
                    }
                }
                Ok(len) => {
                    if let Ok(ack) = parse_command_ack(&self.recv_buffer[..len]) {
                        self.apply_ack(now, ack);
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
                        self.apply_ack(now, ack);
                        ack_count = ack_count.saturating_add(1);
                    }
                }
                Err(e) if would_block(&e) => break,
                Err(_) => break,
            }
        }
        if ack_count > 0 {
            let mut state = self
                .state
                .inner
                .lock()
                .expect("LaserCube transport state poisoned");
            state.acks_received = state.acks_received.saturating_add(ack_count);
        }
    }

    /// Total samples still outstanding across all sent-but-unacked packets.
    fn in_flight_samples(&self) -> usize {
        self.packet_sample_counts.iter().map(|&c| c as usize).sum()
    }

    fn apply_ack(&mut self, now: Instant, ack: BufferAck) {
        match ack.packet_sequence {
            Some(sequence) => self.apply_data_ack(now, ack, sequence),
            None => self.apply_command_ack(now, ack),
        }
    }

    fn apply_data_ack(&mut self, now: Instant, ack: BufferAck, sequence: u8) {
        let prev_sequence = {
            let state = self
                .state
                .inner
                .lock()
                .expect("LaserCube transport state poisoned");
            state.last_data_ack_sequence
        };
        // Take this packet's send slot regardless (records RTT, frees the slot).
        let rtt = self.packet_send_times[sequence as usize]
            .take()
            .map(|sent_at| now.saturating_duration_since(sent_at));
        self.packet_sample_counts[sequence as usize] = 0;

        // Reject ACKs that are not strictly newer than the last applied one:
        // out-of-order or duplicated ACKs must not rewind the buffer estimate.
        if let Some(prev) = prev_sequence {
            if !seq_newer(prev, sequence) {
                let mut state = self
                    .state
                    .inner
                    .lock()
                    .expect("LaserCube transport state poisoned");
                state.last_comms = Some(now);
                if let Some(rtt) = rtt {
                    state.last_ack_rtt = Some(rtt);
                }
                return;
            }
            // Packets between the last applied ACK and this one are delivered;
            // release their slots so they don't linger as phantom in-flight.
            let mut s = prev.wrapping_add(1);
            while s != sequence {
                self.packet_send_times[s as usize] = None;
                self.packet_sample_counts[s as usize] = 0;
                s = s.wrapping_add(1);
            }
        }

        // Remaining outstanding packets were sent after this ACK's packet and
        // are not yet reflected in its reported free space — discount them.
        let in_flight = self.in_flight_samples();
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        let adjusted_free = (ack.free_space as usize).saturating_sub(in_flight);
        state.free_estimate = adjusted_free.min(state.buffer_total);
        state.last_estimate = now;
        state.last_ack_free_space = Some(ack.free_space);
        state.last_data_ack_sequence = Some(sequence);
        if let Some(rtt) = rtt {
            state.last_ack_rtt = Some(rtt);
        }
        state.last_ack = Some(now);
        state.last_comms = Some(now);
    }

    fn apply_command_ack(&mut self, now: Instant, ack: BufferAck) {
        // Command-channel ACKs carry no packet sequence, so we cannot order them
        // against data ACKs. Discount all outstanding in-flight samples to stay
        // conservative.
        let in_flight = self.in_flight_samples();
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        let adjusted_free = (ack.free_space as usize).saturating_sub(in_flight);
        state.free_estimate = adjusted_free.min(state.buffer_total);
        state.last_estimate = now;
        state.last_ack_free_space = Some(ack.free_space);
        state.last_ack = Some(now);
        state.last_comms = Some(now);
    }

    /// Re-assert the intended output state when the device reports otherwise.
    /// A correlated loss of both `set_output` datagrams would otherwise leave the
    /// device out of sync with the worker's intent (dark show / stuck-on beam).
    fn reconcile_output_enable(&mut self, reported_enabled: bool, interlock_enabled: bool) {
        if reported_enabled == self.output_enabled {
            return;
        }
        if self.output_enabled {
            if interlock_enabled {
                log::warn!(
                    "LaserCube network: output intended ON but device reports OFF with interlock \
                     OPEN; re-sending set_output(true) (enable will not take effect until the \
                     interlock closes)"
                );
            } else {
                log::warn!(
                    "LaserCube network: output enable was lost; re-sending set_output(true)"
                );
            }
            let _ = send_repeated(&self.cmd_socket, &command::set_output(true));
        } else {
            log::warn!(
                "LaserCube network: output intended OFF but device reports ON; re-sending \
                 set_output(false)"
            );
            let _ = send_repeated(&self.cmd_socket, &command::set_output(false));
        }
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

    /// Send as many due packets as the device buffer will accept for this wake,
    /// catching up after an oversleep instead of dribbling one packet per wake.
    fn try_send_due_packet(&mut self, now: Instant) {
        if self.active_generation != self.generation.load(Ordering::SeqCst) {
            return;
        }
        while now >= self.next_send_due && !self.queue.is_empty() {
            if !self.send_one_packet(now) {
                break;
            }
        }
    }

    /// Attempt to send a single packet. Returns `true` if a packet was sent and
    /// the caller should keep topping up the device buffer this wake, `false` if
    /// it should stop (paced, waiting for buffer room, or a send error).
    fn send_one_packet(&mut self, now: Instant) -> bool {
        let (budget, free_estimate, buffer_total, remote_buffer_cutoff, full_packet) = {
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
                state.profile.max_udp_samples_per_packet,
            )
        };

        // Coalesce to full packets: a full profile-sized packet when the queue
        // holds at least that much, otherwise the queue tail. Only flush a
        // partial packet when it is genuinely the tail of the queue.
        let intended = full_packet.min(self.queue.len());
        if intended == 0 {
            return false;
        }
        if budget < intended {
            // Device is at/above the remote cutoff: it cannot accept a whole
            // packet yet. Wait for it to drain rather than sending a runt packet.
            self.next_send_due = now + self.device.profile.wait_buffer_sleep;
            return false;
        }

        self.packet_buffer.clear();
        for _ in 0..intended {
            if let Some(point) = self.queue.pop_front() {
                self.packet_buffer.push(point);
            }
        }
        if self.packet_buffer.is_empty() {
            return false;
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
            self.packet_sample_counts[self.packet_sequence as usize] = sent as u16;
            self.record_send(now, sent);
            self.packet_sequence = self.packet_sequence.wrapping_add(1);
            // NOTE: transfer_sequence advances in lockstep with packet_sequence
            // here. Reference LaserCube senders bump transfer_sequence only per
            // logical frame/transfer; we intentionally keep the simpler per-packet
            // increment, which the firmware tolerates. Left unchanged on purpose.
            self.transfer_sequence = self.transfer_sequence.wrapping_add(1);
            if should_continue_topup(free_estimate, buffer_total, remote_buffer_cutoff, sent) {
                self.next_send_due = now;
                true
            } else {
                self.next_send_due = now + packet_interval(sent, self.current_rate);
                false
            }
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
            self.next_send_due = now + self.device.profile.wait_buffer_sleep;
            false
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

    /// The instant the worker should next wake to do useful work. When points
    /// are queued this is the pacing deadline; when idle it honors the profile's
    /// `wait_buffer_sleep` so we don't busy-poll an empty queue.
    fn next_wake(&self, now: Instant) -> Instant {
        if self.queue.is_empty() {
            now + self.device.profile.wait_buffer_sleep
        } else {
            self.next_send_due.max(now)
        }
    }

    /// Sleep until `deadline`, staying responsive to priority commands (disarm /
    /// shutdown) by processing them between short sleep slices, and finishing the
    /// last sub-millisecond with a busy-wait so send deadlines are hit precisely
    /// even on coarse (e.g. Windows 15.6 ms) OS timers.
    fn sleep_until_precise(&mut self, deadline: Instant, priority_rx: &Receiver<PriorityCommand>) {
        const BUSY_WAIT_THRESHOLD: Duration = Duration::from_micros(500);
        loop {
            let now = Instant::now();
            if now >= deadline || !self.running {
                return;
            }
            self.process_priority_commands(priority_rx);
            if !self.running {
                return;
            }
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let remaining = deadline.saturating_duration_since(now);
            if remaining > BUSY_WAIT_THRESHOLD {
                thread::sleep(
                    remaining
                        .saturating_sub(BUSY_WAIT_THRESHOLD)
                        .min(MAX_IDLE_SLEEP),
                );
            } else {
                thread::yield_now();
            }
        }
    }
}

/// RFC 1982-style sequence comparison: is `candidate` strictly newer than `last`
/// under u8 wraparound?
fn seq_newer(last: u8, candidate: u8) -> bool {
    (candidate.wrapping_sub(last) as i8) > 0
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
    state.buffer_total = buffer_total_from_max(status.buffer_max);
    state.free_estimate = (status.buffer_free as usize).min(state.buffer_total);
    if status.point_rate > 0 {
        state.point_rate = super::super::clamp_point_rate(&status, status.point_rate);
    }
    state.last_estimate = now;
    state.last_full_info = Some(now);
    state.last_comms = Some(now);
    state.status = status;
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

    /// Build a minimal 64-byte full-info payload with the given output/interlock
    /// flags and a 6000-point buffer.
    fn full_info_bytes(output_enabled: bool, interlock: bool) -> Vec<u8> {
        let mut d = vec![0u8; 64];
        d[0] = CMD_GET_FULL_INFO;
        d[3] = 1; // firmware major
        d[4] = 24; // firmware minor (new flag layout)
        let mut flags = 0u8;
        if output_enabled {
            flags |= 0x01;
        }
        if interlock {
            flags |= 0x02;
        }
        d[5] = flags;
        d[19] = 0x70; // buffer_free = 6000 (0x1770 LE)
        d[20] = 0x17;
        d[21] = 0x70; // buffer_max = 6000
        d[22] = 0x17;
        d
    }

    #[test]
    fn seq_newer_respects_wraparound() {
        assert!(seq_newer(5, 6));
        assert!(!seq_newer(6, 5));
        assert!(!seq_newer(5, 5));
        assert!(seq_newer(255, 0));
        assert!(!seq_newer(0, 255));
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
        // Slot 9 is released after its ACK.
        assert_eq!(worker.packet_send_times[9], None);
    }

    #[test]
    fn data_ack_discounts_in_flight_samples() {
        let (mut worker, _cmd_socket, _data_socket) = fake_worker();
        let now = Instant::now();
        // A later packet (seq 20) is still outstanding with 140 samples.
        worker.packet_sample_counts[20] = 140;
        let ack = BufferAck {
            source: AckSource::Data,
            packet_sequence: Some(10),
            free_space: 1000,
        };

        worker.apply_ack(now, ack);

        let diag = worker.state.diagnostics();
        assert_eq!(diag.device_free_estimate, 1000 - 140);
        assert_eq!(diag.last_data_ack_sequence, Some(10));
    }

    #[test]
    fn stale_data_ack_does_not_rewind_estimate() {
        let (mut worker, _cmd_socket, _data_socket) = fake_worker();
        let now = Instant::now();
        worker.apply_ack(
            now,
            BufferAck {
                source: AckSource::Data,
                packet_sequence: Some(10),
                free_space: 1000,
            },
        );
        assert_eq!(worker.state.diagnostics().device_free_estimate, 1000);

        // A reordered, older ACK must not overwrite the estimate.
        worker.apply_ack(
            now,
            BufferAck {
                source: AckSource::Data,
                packet_sequence: Some(5),
                free_space: 5000,
            },
        );
        assert_eq!(worker.state.diagnostics().device_free_estimate, 1000);
        assert_eq!(worker.state.diagnostics().last_data_ack_sequence, Some(10));
    }

    #[test]
    fn data_ack_releases_delivered_intermediate_slots() {
        let (mut worker, _cmd_socket, _data_socket) = fake_worker();
        let now = Instant::now();
        // Packets 3, 4, 5 outstanding; ACK for 5 implies 3 and 4 delivered.
        worker.packet_sample_counts[3] = 80;
        worker.packet_sample_counts[4] = 80;
        worker.packet_sample_counts[5] = 80;
        worker.apply_ack(
            now,
            BufferAck {
                source: AckSource::Data,
                packet_sequence: Some(2),
                free_space: 6000,
            },
        );
        // Now ACK 5: intermediate 3 and 4 are released, 5 taken.
        worker.apply_ack(
            now,
            BufferAck {
                source: AckSource::Data,
                packet_sequence: Some(5),
                free_space: 6000,
            },
        );
        assert_eq!(worker.packet_sample_counts[3], 0);
        assert_eq!(worker.packet_sample_counts[4], 0);
        assert_eq!(worker.packet_sample_counts[5], 0);
    }

    #[test]
    fn full_info_reconciles_lost_output_enable() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
        worker.output_enabled = true;
        cmd_socket.push_recv(full_info_bytes(false, false));

        worker.drain_acks(Instant::now());

        assert_eq!(
            cmd_socket.sent_packets(),
            vec![vec![0x80, 0x01], vec![0x80, 0x01]]
        );
    }

    #[test]
    fn full_info_does_not_resend_when_output_matches() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
        worker.output_enabled = true;
        cmd_socket.push_recv(full_info_bytes(true, false));

        worker.drain_acks(Instant::now());

        assert!(cmd_socket.sent_packets().is_empty());
    }

    #[test]
    fn sends_full_packets_and_flushes_only_the_tail() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(200).unwrap();
        worker.queue.extend(vec![Point::blank(); 200]);
        reservation.commit();
        let now = Instant::now();
        worker.next_send_due = now;

        worker.try_send_due_packet(now);

        let sent = data_socket.sent_packets();
        // 200 points below the remote cutoff -> two full 80-point packets plus a
        // 40-point tail, all in a single wake (coalescing + catch-up).
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].len(), 4 + 80 * 10);
        assert_eq!(sent[1].len(), 4 + 80 * 10);
        assert_eq!(sent[2].len(), 4 + 40 * 10);
        assert!(worker.queue.is_empty());
        assert_eq!(worker.state.diagnostics().host_queue_len, 0);
    }

    #[test]
    fn catches_up_with_multiple_full_packets_after_oversleep() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        // A large backlog (more than the remote cutoff worth of points).
        let reservation = worker.state.reserve_host_points(3000).unwrap();
        worker.queue.extend(vec![Point::blank(); 3000]);
        reservation.commit();
        let now = Instant::now();
        worker.next_send_due = now;

        worker.try_send_due_packet(now);

        let sent = data_socket.sent_packets();
        // Fills the device up to the remote cutoff (1800) in 80-point packets,
        // then stops and paces; the remaining backlog stays queued.
        assert_eq!(sent.len(), 22);
        assert!(sent.iter().all(|p| p.len() == 4 + 80 * 10));
        assert!(worker.next_send_due > now);
        assert_eq!(worker.queue.len(), 3000 - 22 * 80);
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
