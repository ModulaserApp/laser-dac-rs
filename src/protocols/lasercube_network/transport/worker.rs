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
use super::state::{buffer_total_from_max, decayed_free, SharedTransportState, TransportState};
use super::{
    send_repeated, would_block, AddressedDevice, DatagramSocket, PriorityCommand, TransportCommand,
    DIAGNOSTIC_LOG_PERIOD, FULL_INFO_POLL_ACTIVE, FULL_INFO_POLL_INACTIVE, MAX_ACK_DRAIN_PER_LOOP,
    MAX_CONTROL_DRAIN_PER_LOOP, MAX_IDLE_SLEEP,
};

// LaserCube Wi-Fi hardware drops packets when a producer sends more than about
// 20 datagrams in one burst. Prime conservatively, then limit deadline catch-up
// so scheduler stalls cannot turn into another unbounded burst.
const INITIAL_TOPUP_PACKET_LIMIT: usize = 4;
const INITIAL_TOPUP_PAUSE: Duration = Duration::from_millis(10);
const MAX_CATCHUP_PACKETS_PER_WAKE: usize = 2;
const SEND_ERROR_BACKOFF: Duration = Duration::from_millis(10);
const MAX_CONSECUTIVE_SEND_ERRORS: usize = 100;
const RATE_COMMAND_SETTLE: Duration = Duration::from_millis(50);

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
    /// Requested output state from the presentation session.
    output_enabled: bool,
    /// State last commanded on the device. Enabling is deferred until the first
    /// enqueue establishes the requested point rate.
    output_commanded_enabled: bool,
    rate_initialized: bool,
    startup_burst_done: bool,
    next_send_due: Instant,
    next_full_info_due: Instant,
    next_diagnostic_log_due: Instant,
    consecutive_send_errors: usize,
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
        // Keep the device-reported rate until the first enqueue establishes the
        // session's requested rate.
        let current_rate = if device.status.point_rate > 0 {
            super::super::clamp_point_rate(&device.status, device.status.point_rate)
        } else {
            super::super::clamp_point_rate(&device.status, DEFAULT_POINT_RATE)
        };
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
            output_commanded_enabled: false,
            rate_initialized: false,
            startup_burst_done: false,
            next_send_due: now,
            next_full_info_due: now + FULL_INFO_POLL_INACTIVE,
            next_diagnostic_log_due: now + DIAGNOSTIC_LOG_PERIOD,
            consecutive_send_errors: 0,
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
                    if !enabled || self.rate_initialized {
                        self.command_output(enabled);
                    }
                }
                Ok(PriorityCommand::StopOutput { generation }) => {
                    self.active_generation = generation;
                    self.clear_pending_points();
                    self.output_enabled = false;
                    self.command_output(false);
                }
                Ok(PriorityCommand::Shutdown { generation }) => {
                    self.active_generation = generation;
                    self.clear_pending_points();
                    self.output_enabled = false;
                    self.command_output(false);
                    self.running = false;
                    break;
                }
                Err(TryRecvError::Disconnected) => {
                    self.active_generation = self.generation.load(Ordering::SeqCst);
                    self.clear_pending_points();
                    self.output_enabled = false;
                    self.command_output(false);
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
                    if (!self.rate_initialized || point_rate != self.current_rate)
                        && self.apply_rate(point_rate).is_err()
                    {
                        continue;
                    }
                    if self.output_enabled && !self.output_commanded_enabled {
                        self.command_output(true);
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

    fn command_output(&mut self, enabled: bool) {
        self.output_commanded_enabled = enabled;
        self.record_output_enabled(enabled);
        let _ = send_repeated(&self.cmd_socket, &command::set_output(enabled));
    }

    fn clear_pending_points(&mut self) {
        self.queue.clear();
        self.packet_buffer.clear();
        self.rate_initialized = false;
        self.startup_burst_done = false;
        self.next_send_due = Instant::now();
        self.state.clear_host_queue();
    }

    /// Apply a runtime rate while output is off. Firmware 0.21 can ignore rate
    /// commands sent in the same UDP burst as output enable.
    fn apply_rate(&mut self, point_rate: u32) -> io::Result<()> {
        let restore_output = self.output_commanded_enabled;
        if restore_output {
            self.command_output(false);
        }
        self.set_rate(point_rate)?;
        self.rate_initialized = true;
        thread::sleep(RATE_COMMAND_SETTLE);
        if restore_output {
            self.command_output(true);
        }
        Ok(())
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
        // This is the intended runtime rate, used for local pacing/decay. Keep
        // the status snapshot unchanged until full-info confirms the device
        // applied the UDP command.
        state.point_rate = point_rate;
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
                        let reported_rate = status.point_rate;
                        let applied_rate = (reported_rate > 0)
                            .then(|| super::super::clamp_point_rate(&status, reported_rate));
                        {
                            let mut state = self
                                .state
                                .inner
                                .lock()
                                .expect("LaserCube transport state poisoned");
                            apply_status(&mut state, status, now);
                        }
                        if applied_rate.is_some_and(|rate| rate != self.current_rate) {
                            log::warn!(
                                "LaserCube network: device reports {reported_rate} pps, expected {} pps; re-applying rate",
                                self.current_rate
                            );
                            let _ = self.apply_rate(self.current_rate);
                        } else {
                            self.reconcile_output_enable(
                                reported_output_enabled,
                                interlock_enabled,
                            );
                        }
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
        let rtt = self.packet_send_times[sequence as usize]
            .take()
            .map(|sent_at| now.saturating_duration_since(sent_at));

        // Reject reordered/duplicate ACKs and clear skipped RTT slots so a later
        // sequence wrap cannot produce a bogus multi-second measurement.
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
            let mut s = prev.wrapping_add(1);
            while s != sequence {
                self.packet_send_times[s as usize] = None;
                s = s.wrapping_add(1);
            }
        } else {
            let mut s = 0;
            while s != sequence {
                self.packet_send_times[s as usize] = None;
                s = s.wrapping_add(1);
            }
        }

        // Data ACKs on Wi-Fi can be delayed beyond a complete 8-bit sequence
        // cycle. Keep them for liveness/diagnostics, but do not let stale free-
        // space readings perturb wire pacing. Local decay and periodic full-info
        // snapshots remain the authoritative fullness estimate.
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.last_ack_free_space = Some(ack.free_space);
        state.last_data_ack_sequence = Some(sequence);
        if let Some(rtt) = rtt {
            state.last_ack_rtt = Some(rtt);
        }
        state.last_ack = Some(now);
        state.last_comms = Some(now);
    }

    fn apply_command_ack(&mut self, now: Instant, ack: BufferAck) {
        // Command-channel free-space ACKs are likewise diagnostic only; they
        // cannot be ordered against the data stream.
        let mut state = self
            .state
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.last_ack_free_space = Some(ack.free_space);
        state.last_ack = Some(now);
        state.last_comms = Some(now);
    }

    /// Re-assert the intended output state when the device reports otherwise.
    /// A correlated loss of both `set_output` datagrams would otherwise leave the
    /// device out of sync with the worker's intent (dark show / stuck-on beam).
    fn reconcile_output_enable(&mut self, reported_enabled: bool, interlock_enabled: bool) {
        if reported_enabled == self.output_commanded_enabled {
            return;
        }
        if self.output_commanded_enabled {
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

    /// Send up to four currently queued packets in one startup burst, then use
    /// point-rate deadlines. Catch-up remains bounded so scheduler stalls cannot
    /// flood the Cube's small Wi-Fi receive queue.
    fn try_send_due_packet(&mut self, now: Instant) {
        if self.active_generation != self.generation.load(Ordering::SeqCst)
            || now < self.next_send_due
            || self.queue.is_empty()
        {
            return;
        }

        if !self.startup_burst_done {
            let mut sent = 0;
            while sent < INITIAL_TOPUP_PACKET_LIMIT && !self.queue.is_empty() {
                if !self.send_one_packet(now) {
                    break;
                }
                sent += 1;
            }
            if sent > 0 {
                self.transfer_sequence = self.transfer_sequence.wrapping_add(1);
                self.startup_burst_done = true;
                self.next_send_due = now + INITIAL_TOPUP_PAUSE;
            }
            return;
        }

        let mut sent = 0;
        while now >= self.next_send_due
            && !self.queue.is_empty()
            && sent < MAX_CATCHUP_PACKETS_PER_WAKE
        {
            if !self.send_one_packet(now) {
                break;
            }
            sent += 1;
        }
        if sent > 0 {
            // All packets emitted by one worker wake form one logical transfer.
            self.transfer_sequence = self.transfer_sequence.wrapping_add(1);
        }
        if sent == MAX_CATCHUP_PACKETS_PER_WAKE && now >= self.next_send_due {
            // Drop excess catch-up debt. Sustained throughput remains governed
            // by the requested point rate instead of a burst backlog.
            self.next_send_due = now
                + packet_interval(
                    self.device.profile.max_udp_samples_per_packet,
                    self.current_rate,
                );
        }
    }

    /// Attempt to send one packet and advance the point-rate deadline.
    fn send_one_packet(&mut self, now: Instant) -> bool {
        let (budget, full_packet) = {
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
            (budget, state.profile.max_udp_samples_per_packet)
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

        let send_result = encode_sample_packet(
            self.packet_sequence,
            self.transfer_sequence,
            &self.packet_buffer,
            &mut self.send_buffer,
        )
        .and_then(|_| self.data_socket.send(&self.send_buffer).map(|_| ()));
        if send_result.is_ok() {
            let sent = self.packet_buffer.len();
            self.packet_send_times[self.packet_sequence as usize] = Some(now);
            self.record_send(now, sent);
            self.consecutive_send_errors = 0;
            self.packet_sequence = self.packet_sequence.wrapping_add(1);
            self.next_send_due += packet_interval(sent, self.current_rate);
            true
        } else {
            let mut state = self
                .state
                .inner
                .lock()
                .expect("LaserCube transport state poisoned");
            state.send_errors = state.send_errors.saturating_add(1);
            let send_errors = state.send_errors;
            drop(state);
            self.consecutive_send_errors = self.consecutive_send_errors.saturating_add(1);
            let error = send_result.expect_err("failed send result");
            if send_errors == 1 || send_errors.is_multiple_of(100) {
                log::warn!(
                    "LaserCube network data send failed ({send_errors} total, {} consecutive): {error}",
                    self.consecutive_send_errors
                );
            }
            for point in self.packet_buffer.drain(..).rev() {
                self.queue.push_front(point);
            }
            self.next_send_due = now + SEND_ERROR_BACKOFF;
            if self.consecutive_send_errors >= MAX_CONSECUTIVE_SEND_ERRORS {
                log::warn!(
                    "LaserCube network data sends failed {} times consecutively; disconnecting transport",
                    self.consecutive_send_errors
                );
                self.state.mark_disconnected();
                self.running = false;
            }
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
    let local_free = decayed_free(state, now);
    state.buffer_total = buffer_total_from_max(status.buffer_max);
    let reported_free = (status.buffer_free as usize).min(state.buffer_total);
    state.free_estimate = local_free.min(reported_free);
    if status.point_rate > 0 {
        state.point_rate = super::super::clamp_point_rate(&status, status.point_rate);
    }
    state.last_estimate = now;
    state.last_full_info = Some(now);
    state.last_comms = Some(now);
    state.status = status;
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
        send_error: Arc<Mutex<Option<io::ErrorKind>>>,
    }

    impl FakeSocket {
        fn push_recv(&self, packet: Vec<u8>) {
            self.recv_queue.lock().unwrap().push_back(packet);
        }

        fn sent_packets(&self) -> Vec<Vec<u8>> {
            self.sent.lock().unwrap().clone()
        }

        fn set_send_error(&self, kind: Option<io::ErrorKind>) {
            *self.send_error.lock().unwrap() = kind;
        }
    }

    impl DatagramSocket for FakeSocket {
        fn send(&self, buffer: &[u8]) -> io::Result<usize> {
            if let Some(kind) = *self.send_error.lock().unwrap() {
                return Err(io::Error::new(kind, "injected send failure"));
            }
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
            cmd_port: super::super::super::protocol::CMD_PORT,
            data_port: super::super::super::protocol::DATA_PORT,
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
    fn data_ack_does_not_perturb_pacing_estimate() {
        let (mut worker, _cmd_socket, _data_socket) = fake_worker();
        let now = Instant::now();
        let ack = BufferAck {
            source: AckSource::Data,
            packet_sequence: Some(10),
            free_space: 1000,
        };

        worker.apply_ack(now, ack);

        let diag = worker.state.diagnostics();
        assert_eq!(diag.device_free_estimate, 6000);
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
        assert_eq!(worker.state.diagnostics().device_free_estimate, 6000);

        // A reordered, older ACK must not overwrite diagnostics or pacing.
        worker.apply_ack(
            now,
            BufferAck {
                source: AckSource::Data,
                packet_sequence: Some(5),
                free_space: 5000,
            },
        );
        assert_eq!(worker.state.diagnostics().device_free_estimate, 6000);
        assert_eq!(worker.state.diagnostics().last_data_ack_sequence, Some(10));
    }

    #[test]
    fn data_ack_clears_skipped_rtt_slots() {
        let (mut worker, _cmd_socket, _data_socket) = fake_worker();
        let now = Instant::now();
        // Packets 3, 4, 5 outstanding; ACK for 5 implies 3 and 4 delivered.
        worker.packet_send_times[3] = Some(now);
        worker.packet_send_times[4] = Some(now);
        worker.packet_send_times[5] = Some(now);
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
        assert_eq!(worker.packet_send_times[3], None);
        assert_eq!(worker.packet_send_times[4], None);
        assert_eq!(worker.packet_send_times[5], None);
    }

    #[test]
    fn first_data_ack_clears_earlier_rtt_slots() {
        let (mut worker, _cmd_socket, _data_socket) = fake_worker();
        let now = Instant::now();
        // Fresh connection: packets 0..=3 sent, their ACKs (0,1,2) were lost,
        // so the first applied data ACK is for seq 3.
        worker.packet_send_times[0] = Some(now);
        worker.packet_send_times[1] = Some(now);
        worker.packet_send_times[2] = Some(now);
        worker.packet_send_times[3] = Some(now);
        worker.apply_ack(
            now,
            BufferAck {
                source: AckSource::Data,
                packet_sequence: Some(3),
                free_space: 6000,
            },
        );
        // Earlier slots cannot produce meaningful RTT measurements once their
        // ACKs have been skipped.
        assert_eq!(worker.packet_send_times[0], None);
        assert_eq!(worker.packet_send_times[1], None);
        assert_eq!(worker.packet_send_times[2], None);
        assert_eq!(worker.packet_send_times[3], None);
        assert_eq!(worker.state.diagnostics().device_free_estimate, 6000);
    }

    #[test]
    fn full_info_reconciles_lost_output_enable() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
        worker.output_enabled = true;
        worker.output_commanded_enabled = true;
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
        worker.output_commanded_enabled = true;
        cmd_socket.push_recv(full_info_bytes(true, false));

        worker.drain_acks(Instant::now());

        assert!(cmd_socket.sent_packets().is_empty());
    }

    #[test]
    fn full_info_retries_unapplied_rate_change() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
        worker.current_rate = 30_000;
        let mut info = full_info_bytes(false, false);
        info[10..14].copy_from_slice(&15_000u32.to_le_bytes());
        info[14..18].copy_from_slice(&30_000u32.to_le_bytes());
        cmd_socket.push_recv(info);

        worker.drain_acks(Instant::now());

        let expected = command::set_rate(30_000).to_vec();
        assert_eq!(cmd_socket.sent_packets(), vec![expected.clone(), expected]);
        assert_eq!(worker.current_rate, 30_000);
        assert_eq!(worker.state.diagnostics().status.point_rate, 15_000);
    }

    #[test]
    fn combined_output_and_rate_mismatch_reconciles_rate_first() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
        worker.current_rate = 10_000;
        worker.output_enabled = true;
        worker.output_commanded_enabled = true;
        let mut info = full_info_bytes(false, false);
        info[10..14].copy_from_slice(&30_000u32.to_le_bytes());
        info[14..18].copy_from_slice(&30_000u32.to_le_bytes());
        cmd_socket.push_recv(info);

        worker.drain_acks(Instant::now());

        assert_eq!(
            cmd_socket.sent_packets(),
            vec![
                command::set_output(false).to_vec(),
                command::set_output(false).to_vec(),
                command::set_rate(10_000).to_vec(),
                command::set_rate(10_000).to_vec(),
                command::set_output(true).to_vec(),
                command::set_output(true).to_vec(),
            ]
        );
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
    fn initial_topup_is_bounded_below_firmware_burst_limit() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(3000).unwrap();
        worker.queue.extend(vec![Point::blank(); 3000]);
        reservation.commit();
        let now = Instant::now();
        worker.next_send_due = now;

        worker.try_send_due_packet(now);

        let sent = data_socket.sent_packets();
        assert_eq!(sent.len(), INITIAL_TOPUP_PACKET_LIMIT);
        assert!(sent.iter().all(|p| p.len() == 4 + 80 * 10));
        assert!(worker.startup_burst_done);
        assert_eq!(worker.next_send_due, now + INITIAL_TOPUP_PAUSE);
        assert_eq!(worker.queue.len(), 3000 - INITIAL_TOPUP_PACKET_LIMIT * 80);
    }

    #[test]
    fn startup_burst_uses_only_points_already_queued() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(80).unwrap();
        worker.queue.extend(vec![Point::blank(); 80]);
        reservation.commit();
        let now = Instant::now();

        worker.try_send_due_packet(now);

        assert!(worker.startup_burst_done);
        assert_eq!(data_socket.sent_packets().len(), 1);
    }

    #[test]
    fn steady_state_catchup_is_bounded() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(3000).unwrap();
        worker.queue.extend(vec![Point::blank(); 3000]);
        reservation.commit();
        let now = Instant::now();
        worker.startup_burst_done = true;
        worker.next_send_due = now - Duration::from_secs(1);

        worker.try_send_due_packet(now);

        assert_eq!(
            data_socket.sent_packets().len(),
            MAX_CATCHUP_PACKETS_PER_WAKE
        );
        assert!(worker.next_send_due > now);
    }

    #[test]
    fn steady_state_does_not_send_before_deadline() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(200).unwrap();
        worker.queue.extend(vec![Point::blank(); 200]);
        reservation.commit();
        let now = Instant::now();
        worker.startup_burst_done = true;
        worker.next_send_due = now + Duration::from_millis(5);

        worker.try_send_due_packet(now);

        assert!(data_socket.sent_packets().is_empty());
        assert_eq!(worker.queue.len(), 200);
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
    fn transient_send_error_requeues_points_and_recovers() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(80).unwrap();
        worker.queue.extend(vec![Point::blank(); 80]);
        reservation.commit();
        let now = Instant::now();
        data_socket.set_send_error(Some(io::ErrorKind::Other));

        worker.try_send_due_packet(now);
        assert_eq!(worker.queue.len(), 80);
        assert_eq!(worker.consecutive_send_errors, 1);

        data_socket.set_send_error(None);
        worker.next_send_due = now;
        worker.try_send_due_packet(now);
        assert!(worker.queue.is_empty());
        assert_eq!(worker.consecutive_send_errors, 0);
        assert!(worker.running);
    }

    #[test]
    fn persistent_send_errors_disconnect_transport() {
        let (mut worker, _cmd_socket, data_socket) = fake_worker();
        let reservation = worker.state.reserve_host_points(80).unwrap();
        worker.queue.extend(vec![Point::blank(); 80]);
        reservation.commit();
        data_socket.set_send_error(Some(io::ErrorKind::Other));
        let now = Instant::now();

        for _ in 0..MAX_CONSECUTIVE_SEND_ERRORS {
            worker.next_send_due = now;
            worker.try_send_due_packet(now);
        }

        assert!(!worker.running);
        assert!(!worker.state.diagnostics().connected);
        assert_eq!(worker.queue.len(), 80);
    }

    #[test]
    fn first_enqueue_reasserts_same_rate_and_updates_host_queue() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
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
        let rate = command::set_rate(DEFAULT_POINT_RATE).to_vec();
        assert_eq!(cmd_socket.sent_packets(), vec![rate.clone(), rate]);
    }

    #[test]
    fn output_enable_waits_for_requested_rate() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
        let (priority_tx, priority_rx) = mpsc::channel();
        priority_tx
            .send(PriorityCommand::SetOutput {
                enabled: true,
                generation: 0,
            })
            .unwrap();
        worker.process_priority_commands(&priority_rx);
        assert!(cmd_socket.sent_packets().is_empty());

        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(TransportCommand::Enqueue {
            generation: 0,
            point_rate: 10_000,
            points: vec![Point::blank()],
            _reservation: worker.state.reserve_host_points(1).unwrap(),
        })
        .unwrap();
        worker.process_commands(&rx);

        assert_eq!(
            cmd_socket.sent_packets(),
            vec![
                command::set_rate(10_000).to_vec(),
                command::set_rate(10_000).to_vec(),
                command::set_output(true).to_vec(),
                command::set_output(true).to_vec(),
            ]
        );
    }

    #[test]
    fn active_rate_change_cycles_output() {
        let (mut worker, cmd_socket, _data_socket) = fake_worker();
        worker.rate_initialized = true;
        worker.output_enabled = true;
        worker.output_commanded_enabled = true;
        let (tx, rx) = mpsc::sync_channel(1);
        tx.send(TransportCommand::Enqueue {
            generation: 0,
            point_rate: 10_000,
            points: vec![Point::blank()],
            _reservation: worker.state.reserve_host_points(1).unwrap(),
        })
        .unwrap();

        worker.process_commands(&rx);

        assert_eq!(
            cmd_socket.sent_packets(),
            vec![
                command::set_output(false).to_vec(),
                command::set_output(false).to_vec(),
                command::set_rate(10_000).to_vec(),
                command::set_rate(10_000).to_vec(),
                command::set_output(true).to_vec(),
                command::set_output(true).to_vec(),
            ]
        );
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
        assert!(!worker.startup_burst_done);
        assert!(!worker.rate_initialized);
        let sent = cmd_socket.sent_packets();
        assert_eq!(sent, vec![vec![0x80, 0x00], vec![0x80, 0x00]]);
    }
}
