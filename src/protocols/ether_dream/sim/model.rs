//! Transport-free, clock-injected model of an Ether Dream DAC.
//!
//! [`EtherDreamModel`] consumes raw command bytes and returns the bytes a DAC
//! running a given [`FirmwareProfile`] would send back. Time is passed in by
//! the caller, so tests are deterministic. The TCP/UDP wrapper lives in
//! [`super::server`].

use std::collections::VecDeque;
use std::time::Duration;

use byteorder::{ByteOrder, LE};

use crate::protocols::ether_dream::profile::FirmwareProfile;
use crate::protocols::ether_dream::protocol::{
    DacBroadcast, DacPoint, DacResponse, DacStatus, ReadBytes, SizeBytes,
};

/// Playback flag: shutter open.
const PBF_SHUTTER: u16 = 0x1;
/// Playback flag: last stream ended in an underflow.
const PBF_UNDERFLOW: u16 = 0x2;
/// Playback flag: last stream ended in an emergency stop.
const PBF_ESTOP: u16 = 0x4;
/// Point control bit that pops the next queued rate.
const CONTROL_CHANGE_RATE: u16 = 0x8000;
/// j4cDAC `DAC_RATE_BUFFER_SIZE`. The ring keeps one slot free to tell full
/// from empty, so it holds at most `RATE_QUEUE_LEN - 1` rates.
const RATE_QUEUE_LEN: usize = 200;
/// Warmup duration after an e-stop clear on firmware without
/// [`FirmwareProfile::estop_clear_immediate`].
const WARMUP: Duration = Duration::from_millis(100);
const NANOS_PER_SEC: u128 = 1_000_000_000;

/// One thing the DAC sends (or does) in answer to a complete command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// A normal 22-byte response frame.
    Response(DacResponse),
    /// The 32-byte reply to `'v'`.
    Version([u8; 32]),
    /// The firmware closes the TCP connection.
    Reset,
    /// The firmware stops answering anything, on every connection.
    Hang,
}

impl Reply {
    /// Wire bytes of this reply. `Reset` and `Hang` have none.
    pub fn to_bytes(&self) -> Vec<u8> {
        use crate::protocols::ether_dream::protocol::WriteBytes;
        match self {
            Reply::Response(r) => {
                let mut v = Vec::with_capacity(DacResponse::SIZE_BYTES);
                v.write_bytes(*r).expect("Vec write cannot fail");
                v
            }
            Reply::Version(b) => b.to_vec(),
            Reply::Reset | Reply::Hang => Vec::new(),
        }
    }
}

/// Simulated Ether Dream state machine.
#[derive(Clone, Debug)]
pub struct EtherDreamModel {
    profile: FirmwareProfile,
    now: Duration,
    input: Vec<u8>,
    light_engine: u8,
    warmup_until: Option<Duration>,
    interlock_ok: bool,
    playback: u8,
    sticky: u16,
    ring: VecDeque<DacPoint>,
    point_count: u32,
    rate_queue: VecDeque<u32>,
    seg_start: Duration,
    seg_rate: u32,
    seg_played: u64,
    hung: bool,
    accepted_total: u64,
    played_total: u64,
    underflows: u64,
    overfull_writes: u64,
    nak_replies: u64,
    record_output: bool,
    output: Vec<DacPoint>,
    record_input: bool,
    commands: Vec<u8>,
    received_points: Vec<DacPoint>,
    inject: Option<(u8, Vec<u8>)>,
    reply_override: Option<(u8, u8)>,
}

impl EtherDreamModel {
    /// A freshly booted DAC: Ready, Idle, empty buffer, time zero.
    pub fn new(profile: FirmwareProfile) -> Self {
        Self {
            profile,
            now: Duration::ZERO,
            input: Vec::new(),
            light_engine: DacStatus::LIGHT_ENGINE_READY,
            warmup_until: None,
            interlock_ok: true,
            playback: DacStatus::PLAYBACK_IDLE,
            sticky: 0,
            ring: VecDeque::new(),
            point_count: 0,
            rate_queue: VecDeque::new(),
            seg_start: Duration::ZERO,
            seg_rate: 0,
            seg_played: 0,
            hung: false,
            accepted_total: 0,
            played_total: 0,
            underflows: 0,
            overfull_writes: 0,
            nak_replies: 0,
            record_output: false,
            output: Vec::new(),
            record_input: false,
            commands: Vec::new(),
            received_points: Vec::new(),
            inject: None,
            reply_override: None,
        }
    }

    /// The profile this model simulates.
    pub fn profile(&self) -> &FirmwareProfile {
        &self.profile
    }

    /// Current model time.
    pub fn now(&self) -> Duration {
        self.now
    }

    /// True once the firmware has hung (for example after a rate-0 `'b'`).
    pub fn is_hung(&self) -> bool {
        self.hung
    }

    /// Opening the interlock (`false`) e-stops the DAC like the hardware
    /// does, and `'c'` cannot clear the e-stop until it closes again.
    pub fn set_interlock(&mut self, ok: bool) {
        if self.interlock_ok && !ok {
            self.estop();
        }
        self.interlock_ok = ok;
    }

    /// Record every point the DAC plays, for [`Self::take_output`].
    pub fn set_record_output(&mut self, on: bool) {
        self.record_output = on;
    }

    /// Drain the recorded output.
    pub fn take_output(&mut self) -> Vec<DacPoint> {
        std::mem::take(&mut self.output)
    }

    /// Record every complete command opcode and every point carried by a
    /// `'d'` (accepted or not), for [`Self::commands`] and
    /// [`Self::received_points`].
    pub fn set_record_input(&mut self, on: bool) {
        self.record_input = on;
    }

    /// Opcodes of every complete command processed while recording.
    pub fn commands(&self) -> &[u8] {
        &self.commands
    }

    /// Points carried by every `'d'` processed while recording, in order.
    pub fn received_points(&self) -> &[DacPoint] {
        &self.received_points
    }

    /// The next time a command with `opcode` is about to be processed, first
    /// run `bytes` as if another client had sent them. Their replies are
    /// discarded and they are not recorded. One-shot.
    ///
    /// Lets tests reproduce races such as the DAC going idle between a
    /// client's `'d'` and its `'b'`.
    pub fn inject_before(&mut self, opcode: u8, bytes: Vec<u8>) {
        self.inject = Some((opcode, bytes));
    }

    /// Answer the next command with `opcode` with response code `code`
    /// instead of its usual one. One-shot.
    ///
    /// The command still runs as usual; only the code in its reply changes.
    /// Use it where the command is refused anyway (for example after an
    /// [`inject_before`](Self::inject_before) e-stop) to cover a NAK the
    /// model does not produce on its own, such as `'!'` instead of `'I'`.
    pub fn override_reply_code(&mut self, opcode: u8, code: u8) {
        self.reply_override = Some((opcode, code));
    }

    /// Points accepted into the ring buffer since boot.
    pub fn accepted_total(&self) -> u64 {
        self.accepted_total
    }

    /// Points played since boot.
    pub fn played_total(&self) -> u64 {
        self.played_total
    }

    /// Underflows since boot.
    pub fn underflows(&self) -> u64 {
        self.underflows
    }

    /// `'d'` commands that held more points than the ring had room for,
    /// whatever the firmware then did with them (partial write, NAK-Full or
    /// NAK-Invalid).
    pub fn overfull_writes(&self) -> u64 {
        self.overfull_writes
    }

    /// Replies sent with any NAK code since boot.
    pub fn nak_replies(&self) -> u64 {
        self.nak_replies
    }

    /// Force an emergency stop, as the hardware interlock or a front-panel
    /// button would.
    pub fn trigger_estop(&mut self, now: Duration) {
        self.advance_to(now);
        self.estop();
    }

    /// The client's TCP connection closed. With
    /// [`FirmwareProfile::close_stops_playback`] this ends a playing stream
    /// like a stop, without the underflow flag. ED2 r331 hardware showed Idle
    /// on the next hello, with the leftover fullness (35 and 43 points) frozen
    /// until the next prepare. Only the playing case was observed, so a
    /// prepared stream is left as it is.
    pub fn connection_closed(&mut self, now: Duration) {
        self.advance_to(now);
        if !self.profile.close_stops_playback || self.playback != DacStatus::PLAYBACK_PLAYING {
            return;
        }
        self.playback = DacStatus::PLAYBACK_IDLE;
        self.seg_rate = 0;
        self.point_count = 0;
        self.rate_queue.clear();
        if !self.profile.stale_fullness_until_prepare {
            self.ring.clear();
        }
    }

    /// Advance the clock, playing points and handling underflow and warmup.
    /// Time never goes backwards; an earlier `now` is ignored.
    pub fn advance_to(&mut self, now: Duration) {
        if now <= self.now {
            return;
        }
        self.now = now;
        if let Some(until) = self.warmup_until {
            if now >= until {
                self.warmup_until = None;
                self.light_engine = DacStatus::LIGHT_ENGINE_READY;
            }
        }
        while self.playback == DacStatus::PLAYBACK_PLAYING {
            let elapsed = now.saturating_sub(self.seg_start).as_nanos();
            let due = (elapsed * self.seg_rate as u128 / NANOS_PER_SEC) as u64;
            if self.seg_played >= due {
                break;
            }
            let Some(point) = self.ring.pop_front() else {
                self.underflow();
                break;
            };
            self.seg_played += 1;
            self.played_total += 1;
            self.point_count = self.point_count.wrapping_add(1);
            if self.record_output {
                self.output.push(point);
            }
            if point.control & CONTROL_CHANGE_RATE != 0 {
                if let Some(rate) = self.rate_queue.pop_front() {
                    let at = self.point_time(self.seg_played);
                    self.start_segment(at, rate);
                }
            }
        }
    }

    /// Time at which the `k`-th point (1-based) of the current segment plays.
    fn point_time(&self, k: u64) -> Duration {
        let ns = (k as u128 * NANOS_PER_SEC).div_ceil(self.seg_rate.max(1) as u128);
        self.seg_start + Duration::from_nanos(ns as u64)
    }

    fn start_segment(&mut self, at: Duration, rate: u32) {
        self.seg_start = at;
        self.seg_rate = rate;
        self.seg_played = 0;
    }

    fn underflow(&mut self) {
        self.underflows += 1;
        self.playback = DacStatus::PLAYBACK_IDLE;
        self.sticky |= PBF_UNDERFLOW;
        self.seg_rate = 0;
        self.point_count = 0;
        self.rate_queue.clear();
    }

    fn estop(&mut self) {
        self.light_engine = DacStatus::LIGHT_ENGINE_EMERGENCY_STOP;
        self.warmup_until = None;
        self.playback = DacStatus::PLAYBACK_IDLE;
        self.sticky |= PBF_ESTOP;
        self.seg_rate = 0;
        self.rate_queue.clear();
        if !self.profile.stale_fullness_until_prepare {
            self.ring.clear();
        }
    }

    /// Status as the firmware reports it right now.
    pub fn status(&self) -> DacStatus {
        let playing = self.playback == DacStatus::PLAYBACK_PLAYING;
        let lef = if self.light_engine == DacStatus::LIGHT_ENGINE_EMERGENCY_STOP {
            if self.profile.light_engine_flags_mirror_state {
                0x3
            } else {
                0x1
            }
        } else {
            0
        };
        DacStatus {
            protocol: 0,
            light_engine_state: self.light_engine,
            playback_state: self.playback,
            source: DacStatus::SOURCE_NETWORK_STREAMING,
            light_engine_flags: lef,
            playback_flags: self.profile.always_set_playback_flags
                | self.sticky
                | if playing { PBF_SHUTTER } else { 0 },
            source_flags: 0,
            buffer_fullness: self.ring.len().min(u16::MAX as usize) as u16,
            point_rate: if playing { self.seg_rate } else { 0 },
            point_count: if playing { self.point_count } else { 0 },
        }
    }

    /// The greeting sent on every new TCP connection: an ACK for `'?'`.
    pub fn hello(&self) -> DacResponse {
        self.respond(DacResponse::ACK, b'?')
    }

    /// The UDP broadcast frame.
    pub fn broadcast(&self) -> DacBroadcast {
        DacBroadcast {
            mac_address: self.profile.mac_address,
            hw_revision: self.profile.hw_revision,
            sw_revision: self.profile.sw_revision,
            buffer_capacity: self.profile.buffer_capacity,
            max_point_rate: self.profile.max_point_rate,
            dac_status: self.status(),
        }
    }

    fn respond(&self, code: u8, command: u8) -> DacResponse {
        DacResponse {
            response: code,
            command,
            dac_status: self.status(),
        }
    }

    /// Feed bytes from the (single) default connection.
    ///
    /// Incomplete commands are kept until the rest arrives.
    pub fn feed(&mut self, now: Duration, bytes: &[u8]) -> Vec<Reply> {
        let mut input = std::mem::take(&mut self.input);
        input.extend_from_slice(bytes);
        let replies = self.process(now, &mut input);
        self.input = input;
        replies
    }

    /// Consume every complete command at the front of `input`, which is a
    /// per-connection buffer owned by the caller. Playback state is shared
    /// by all connections, as on the hardware.
    ///
    /// After a [`Reply::Reset`] the caller should close the connection;
    /// `input` is cleared. After [`Reply::Hang`] nothing is answered again.
    pub fn process(&mut self, now: Duration, input: &mut Vec<u8>) -> Vec<Reply> {
        self.process_with(now, input, None, usize::MAX)
    }

    /// Like [`process`](Self::process), but stop in front of the next
    /// command that starts with `opcode`, leaving it in `input`.
    pub fn process_before(&mut self, now: Duration, input: &mut Vec<u8>, opcode: u8) -> Vec<Reply> {
        self.process_with(now, input, Some(opcode), usize::MAX)
    }

    /// Like [`process`](Self::process), but handle at most one command.
    pub fn process_one(&mut self, now: Duration, input: &mut Vec<u8>) -> Vec<Reply> {
        self.process_with(now, input, None, 1)
    }

    fn process_with(
        &mut self,
        now: Duration,
        input: &mut Vec<u8>,
        stop_before: Option<u8>,
        max_commands: usize,
    ) -> Vec<Reply> {
        self.advance_to(now);
        let mut replies = Vec::new();
        if self.hung {
            input.clear();
            return replies;
        }
        let mut handled = 0;
        while handled < max_commands {
            if stop_before.is_some() && input.first().copied() == stop_before {
                break;
            }
            handled += 1;
            if let (Some(&op), Some((want, _))) = (input.first(), &self.inject) {
                // Only once the whole command has arrived: a large 'd' comes
                // in over several reads, and the race is with handling it.
                if op == *want && command_complete(input) {
                    let (_, mut other) = self.inject.take().expect("checked above");
                    while let Some((n, _)) = self.step(&other) {
                        other.drain(..n);
                    }
                    if self.hung {
                        input.clear();
                        break;
                    }
                }
            }
            let Some((consumed, mut reply)) = self.step(input) else {
                break;
            };
            if let Reply::Response(r) = &mut reply {
                if self.reply_override.is_some_and(|(op, _)| op == r.command) {
                    let (_, code) = self.reply_override.take().expect("checked above");
                    r.response = code;
                }
                if r.response != DacResponse::ACK {
                    self.nak_replies += 1;
                }
            }
            if self.record_input {
                self.record(&input[..consumed]);
            }
            input.drain(..consumed);
            let stop = matches!(reply, Reply::Reset | Reply::Hang);
            replies.push(reply);
            if stop {
                input.clear();
                break;
            }
        }
        replies
    }

    fn record(&mut self, bytes: &[u8]) {
        let Some(&op) = bytes.first() else { return };
        self.commands.push(op);
        if op == b'd' && bytes.len() >= 3 {
            let mut rest = &bytes[3..];
            while rest.len() >= DacPoint::SIZE_BYTES {
                let p = rest.read_bytes::<DacPoint>().expect("length checked");
                self.received_points.push(p);
            }
        }
    }

    /// Handle one command at the front of `input`. Returns `None` if the
    /// command is not complete yet (see [`command_complete`]).
    fn step(&mut self, input: &[u8]) -> Option<(usize, Reply)> {
        let &cmd = input.first()?;
        let p = self.profile.clone();
        let ack = |m: &Self| Reply::Response(m.respond(DacResponse::ACK, cmd));
        let nak_i = |m: &Self| Reply::Response(m.respond(DacResponse::NAK_INVALID, cmd));
        match cmd {
            b'p' => {
                if self.playback != DacStatus::PLAYBACK_IDLE
                    || self.light_engine != DacStatus::LIGHT_ENGINE_READY
                {
                    return Some((1, nak_i(self)));
                }
                self.playback = DacStatus::PLAYBACK_PREPARED;
                self.ring.clear();
                self.point_count = 0;
                self.rate_queue.clear();
                self.sticky = 0;
                Some((1, ack(self)))
            }
            b'b' | b'u' => {
                if input.len() < 7 {
                    return None;
                }
                let rate = LE::read_u32(&input[3..7]);
                if let Some(r) = self.check_rate(cmd, rate, 7) {
                    return Some(r);
                }
                if cmd == b'u' || self.playback == DacStatus::PLAYBACK_PLAYING {
                    if self.playback == DacStatus::PLAYBACK_PLAYING {
                        let now = self.now;
                        self.start_segment(now, rate);
                    }
                    return Some((7, ack(self)));
                }
                match self.playback {
                    DacStatus::PLAYBACK_PREPARED => {
                        self.playback = DacStatus::PLAYBACK_PLAYING;
                        let now = self.now;
                        self.start_segment(now, rate);
                        self.point_count = 0;
                        Some((7, ack(self)))
                    }
                    _ if p.begin_while_idle_acked => Some((7, ack(self))),
                    _ => Some((7, nak_i(self))),
                }
            }
            b'q' => {
                if input.len() < 5 {
                    return None;
                }
                let rate = LE::read_u32(&input[1..5]);
                if let Some(r) = self.check_rate(cmd, rate, 5) {
                    return Some(r);
                }
                if self.playback != DacStatus::PLAYBACK_IDLE
                    && self.rate_queue.len() < RATE_QUEUE_LEN - 1
                {
                    self.rate_queue.push_back(rate);
                }
                Some((5, ack(self)))
            }
            b'd' => {
                if input.len() < 3 {
                    return None;
                }
                let n = LE::read_u16(&input[1..3]) as usize;
                let total = 3 + n * DacPoint::SIZE_BYTES;
                if input.len() < total {
                    return None;
                }
                if n == 0 {
                    return Some((total, ack(self)));
                }
                if self.playback == DacStatus::PLAYBACK_IDLE {
                    let reply = if p.data_while_idle_nak_invalid {
                        nak_i(self)
                    } else {
                        ack(self)
                    };
                    return Some((total, reply));
                }
                let space = (p.ring_points as usize).saturating_sub(self.ring.len());
                if n > space {
                    self.overfull_writes += 1;
                }
                let (take, code) = if n <= space {
                    (n, DacResponse::ACK)
                } else if p.naks_full {
                    (0, DacResponse::NAK_FULL)
                } else if p.data_partial_write_then_nak_invalid {
                    (space, DacResponse::NAK_INVALID)
                } else {
                    (0, DacResponse::NAK_INVALID)
                };
                let mut bytes = &input[3..3 + take * DacPoint::SIZE_BYTES];
                for _ in 0..take {
                    let point = bytes
                        .read_bytes::<DacPoint>()
                        .expect("length checked above");
                    self.ring.push_back(point);
                }
                self.accepted_total += take as u64;
                Some((total, Reply::Response(self.respond(code, cmd))))
            }
            b's' => {
                if self.playback == DacStatus::PLAYBACK_IDLE {
                    let reply = if p.stop_while_idle_naks {
                        nak_i(self)
                    } else {
                        ack(self)
                    };
                    return Some((1, reply));
                }
                self.playback = DacStatus::PLAYBACK_IDLE;
                self.seg_rate = 0;
                self.rate_queue.clear();
                if !p.stale_fullness_until_prepare {
                    self.ring.clear();
                }
                Some((1, ack(self)))
            }
            0x00 | 0xff => {
                self.estop();
                Some((1, ack(self)))
            }
            b'c' => {
                if !self.interlock_ok {
                    return Some((
                        1,
                        Reply::Response(self.respond(DacResponse::NAK_STOP_CONDITION, cmd)),
                    ));
                }
                if self.light_engine == DacStatus::LIGHT_ENGINE_EMERGENCY_STOP {
                    if p.estop_clear_immediate {
                        self.light_engine = DacStatus::LIGHT_ENGINE_READY;
                    } else {
                        self.light_engine = DacStatus::LIGHT_ENGINE_WARMUP;
                        self.warmup_until = Some(self.now + WARMUP);
                    }
                }
                // j4cDAC answers '!' unless the clear left the light engine
                // ready, so a clear that starts a warmup is NAKed too.
                if self.light_engine != DacStatus::LIGHT_ENGINE_READY {
                    return Some((
                        1,
                        Reply::Response(self.respond(DacResponse::NAK_STOP_CONDITION, cmd)),
                    ));
                }
                Some((1, ack(self)))
            }
            b'?' => Some((1, ack(self))),
            b'v' if p.version_string.is_some() => {
                let mut raw = [0u8; 32];
                let s = p.version_string.unwrap_or_default().as_bytes();
                let n = s.len().min(raw.len());
                raw[..n].copy_from_slice(&s[..n]);
                Some((1, Reply::Version(raw)))
            }
            _ if p.unknown_command_resets_tcp => Some((input.len(), Reply::Reset)),
            _ => Some((1, nak_i(self))),
        }
    }

    /// Range-check a rate argument. `None` means the rate is acceptable.
    fn check_rate(&mut self, cmd: u8, rate: u32, full_len: usize) -> Option<(usize, Reply)> {
        if rate == 0 && self.profile.zero_rate_hangs {
            self.hung = true;
            return Some((full_len, Reply::Hang));
        }
        if rate == 0 || rate > self.profile.max_point_rate {
            let consumed = if self.profile.rate_nak_consumes_one_byte {
                1
            } else {
                full_len
            };
            return Some((
                consumed,
                Reply::Response(self.respond(DacResponse::NAK_INVALID, cmd)),
            ));
        }
        None
    }
}

/// Whether `input` starts with a complete command, using the same lengths
/// as the model's command handling.
pub(crate) fn command_complete(input: &[u8]) -> bool {
    match input.first() {
        None => false,
        Some(b'b' | b'u') => input.len() >= 7,
        Some(b'q') => input.len() >= 5,
        Some(b'd') => {
            input.len() >= 3
                && input.len() >= 3 + LE::read_u16(&input[1..3]) as usize * DacPoint::SIZE_BYTES
        }
        Some(_) => true,
    }
}

/// Wire encoders for building command byte streams in tests and tools.
pub mod cmd {
    use crate::protocols::ether_dream::protocol::{DacPoint, WriteBytes};

    /// `'p'`.
    pub fn prepare() -> Vec<u8> {
        vec![b'p']
    }
    /// `'b'` with the given rate.
    pub fn begin(rate: u32) -> Vec<u8> {
        rate_cmd(b'b', 0, rate)
    }
    /// `'u'` with the given rate.
    pub fn update(rate: u32) -> Vec<u8> {
        rate_cmd(b'u', 0, rate)
    }
    /// `'q'` with the given rate.
    pub fn queue_rate(rate: u32) -> Vec<u8> {
        let mut v = vec![b'q'];
        v.extend_from_slice(&rate.to_le_bytes());
        v
    }
    /// `'d'` carrying `points`.
    pub fn data(points: &[DacPoint]) -> Vec<u8> {
        let n = u16::try_from(points.len()).expect("a 'd' carries at most 65535 points");
        let mut v = vec![b'd'];
        v.extend_from_slice(&n.to_le_bytes());
        for p in points {
            v.write_bytes(p).expect("Vec write cannot fail");
        }
        v
    }
    /// `'d'` carrying `n` blanked points at the origin.
    pub fn blank_data(n: usize) -> Vec<u8> {
        data(&vec![blank(); n])
    }
    /// A blanked point at the origin.
    pub fn blank() -> DacPoint {
        DacPoint {
            control: 0,
            x: 0,
            y: 0,
            r: 0,
            g: 0,
            b: 0,
            i: 0,
            u1: 0,
            u2: 0,
        }
    }

    fn rate_cmd(op: u8, low_water: u16, rate: u32) -> Vec<u8> {
        let mut v = vec![op];
        v.extend_from_slice(&low_water.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::cmd::*;
    use super::*;

    fn ms(v: u64) -> Duration {
        Duration::from_millis(v)
    }
    fn us(v: u64) -> Duration {
        Duration::from_micros(v)
    }

    /// Feed one command, expect exactly one normal response.
    fn one(m: &mut EtherDreamModel, now: Duration, bytes: &[u8]) -> DacResponse {
        let r = m.feed(now, bytes);
        assert_eq!(r.len(), 1, "expected one reply, got {r:?}");
        match r[0] {
            Reply::Response(resp) => resp,
            ref other => panic!("expected a response, got {other:?}"),
        }
    }

    fn ed2() -> EtherDreamModel {
        EtherDreamModel::new(FirmwareProfile::ed2_r331())
    }

    fn each_profile(f: impl Fn(EtherDreamModel)) {
        for p in FirmwareProfile::all() {
            f(EtherDreamModel::new(p));
        }
    }

    #[test]
    fn hello_matches_ed2_capture_layout() {
        // Captured hello: 613f000000000000315f0000 2905... (buf 1321, stale).
        let m = ed2();
        let h = m.hello();
        assert_eq!(h.response, DacResponse::ACK);
        assert_eq!(h.command, b'?');
        assert_eq!(h.dac_status.playback_flags, 0x5f31);
        assert_eq!(h.dac_status.playback_state, DacStatus::PLAYBACK_IDLE);
    }

    #[test]
    fn drain_is_exact_and_underflow_resets_status() {
        // ED2 fact: 500 points at 30 kpps -> buf 334, count 166 at +5.5 ms,
        // underflow at about 16.7 ms.
        let mut m = ed2();
        assert_eq!(one(&mut m, ms(0), &prepare()).response, DacResponse::ACK);
        assert_eq!(
            one(&mut m, ms(0), &blank_data(500)).response,
            DacResponse::ACK
        );
        assert_eq!(
            one(&mut m, ms(0), &begin(30_000)).response,
            DacResponse::ACK
        );
        m.advance_to(us(5_534));
        let s = m.status();
        assert_eq!(s.buffer_fullness, 334);
        assert_eq!(s.point_count, 166);
        assert_eq!(s.point_rate, 30_000);
        assert_eq!(s.playback_flags, 0x5f31 | PBF_SHUTTER);

        m.advance_to(us(16_660));
        assert_eq!(m.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        m.advance_to(us(16_700));
        let s = m.status();
        assert_eq!(s.playback_state, DacStatus::PLAYBACK_IDLE);
        assert_eq!(s.buffer_fullness, 0);
        assert_eq!(s.point_rate, 0);
        assert_eq!(s.point_count, 0);
        assert_eq!(s.playback_flags, 0x5f33);
        assert_eq!(m.underflows(), 1);

        // After underflow, data is rejected until prepare.
        let r = one(&mut m, ms(20), &blank_data(10));
        assert_eq!(r.response, DacResponse::NAK_INVALID);
        assert_eq!(one(&mut m, ms(20), &prepare()).response, DacResponse::ACK);
        assert_eq!(m.status().playback_flags, 0x5f31, "prepare clears sticky");
    }

    #[test]
    fn begin_while_idle_is_acked_but_ignored() {
        let mut m = ed2();
        let r = one(&mut m, ms(0), &begin(30_000));
        assert_eq!(r.response, DacResponse::ACK);
        assert_eq!(r.dac_status.playback_state, DacStatus::PLAYBACK_IDLE);

        let mut p = FirmwareProfile::ed2_r331();
        p.begin_while_idle_acked = false;
        let mut m = EtherDreamModel::new(p);
        assert_eq!(
            one(&mut m, ms(0), &begin(30_000)).response,
            DacResponse::NAK_INVALID
        );
    }

    #[test]
    fn update_while_playing_changes_rate_immediately() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(1000));
        one(&mut m, ms(0), &begin(10_000));
        m.advance_to(ms(10)); // 100 played
        assert_eq!(m.status().buffer_fullness, 900);
        let r = one(&mut m, ms(10), &update(20_000));
        assert_eq!(r.response, DacResponse::ACK);
        assert_eq!(r.dac_status.point_rate, 20_000);
        m.advance_to(ms(20)); // +200
        assert_eq!(m.status().buffer_fullness, 700);
    }

    #[test]
    fn queued_rate_applies_at_control_bit() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        assert_eq!(
            one(&mut m, ms(0), &queue_rate(20_000)).response,
            DacResponse::ACK
        );
        let mut pts = vec![blank(); 400];
        pts[99].control = CONTROL_CHANGE_RATE;
        one(&mut m, ms(0), &data(&pts));
        one(&mut m, ms(0), &begin(10_000));
        m.advance_to(ms(10)); // exactly 100 played; 100th switches the rate
        assert_eq!(m.status().point_rate, 20_000);
        m.advance_to(ms(15)); // +100 at 20 kpps
        assert_eq!(m.status().buffer_fullness, 200);
    }

    #[test]
    fn rate_queue_holds_one_less_than_its_size() {
        // j4cDAC dac_rate_queue rejects once 199 rates are queued (the ring
        // keeps one slot free) but still ACKs the command.
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        for _ in 0..RATE_QUEUE_LEN + 5 {
            assert_eq!(
                one(&mut m, ms(0), &queue_rate(20_000)).response,
                DacResponse::ACK
            );
        }
        assert_eq!(m.rate_queue.len(), RATE_QUEUE_LEN - 1);
    }

    #[test]
    fn queue_rate_while_idle_is_acked_and_dropped() {
        let mut m = ed2();
        assert_eq!(
            one(&mut m, ms(0), &queue_rate(20_000)).response,
            DacResponse::ACK
        );
        one(&mut m, ms(0), &prepare()); // also clears the queue
        assert!(m.rate_queue.is_empty());
    }

    #[test]
    fn closing_the_connection_stops_playback_and_freezes_fullness() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(300));
        one(&mut m, ms(0), &begin(1_000));
        // 10 ms at 1 kpps plays 10 points; 290 are left.
        m.connection_closed(ms(10));
        let st = m.status();
        assert_eq!(st.playback_state, DacStatus::PLAYBACK_IDLE);
        assert_eq!(st.buffer_fullness, 290);
        assert_eq!(st.point_rate, 0);
        assert_eq!(st.playback_flags & PBF_UNDERFLOW, 0, "no underflow flag");
        assert_eq!(m.underflows(), 0);
        m.advance_to(ms(1_000));
        assert_eq!(m.status().buffer_fullness, 290, "frozen while idle");
        assert_eq!(m.hello().dac_status, m.status());
        assert_eq!(
            one(&mut m, ms(1_000), &prepare())
                .dac_status
                .buffer_fullness,
            0
        );
    }

    #[test]
    fn closing_the_connection_keeps_playing_when_the_profile_says_so() {
        let mut m = EtherDreamModel::new(FirmwareProfile::ed1_j4cdac());
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(300));
        one(&mut m, ms(0), &begin(1_000));
        m.connection_closed(ms(10));
        assert_eq!(m.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        m.advance_to(ms(400));
        assert_eq!(m.status().playback_state, DacStatus::PLAYBACK_IDLE);
        assert_eq!(m.underflows(), 1, "played out and underflowed");
    }

    #[test]
    fn closing_the_connection_leaves_a_prepared_stream() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(300));
        m.connection_closed(ms(10));
        assert_eq!(m.status().playback_state, DacStatus::PLAYBACK_PREPARED);
        assert_eq!(m.status().buffer_fullness, 300);
    }

    /// ED2 r331 hardware: a 'u' to a faster rate ACKed with 33 points left,
    /// then a 965-point 'd' that finished arriving 6 ms later. The 33 points
    /// lasted 1.65 ms at 20 kpps, so the 'd' hit an idle DAC.
    #[test]
    fn faster_rate_before_slow_data_underflows() {
        let setup = || {
            let mut m = ed2();
            one(&mut m, ms(0), &prepare());
            one(&mut m, ms(0), &blank_data(40));
            one(&mut m, ms(0), &begin(1_000));
            m.advance_to(ms(7));
            assert_eq!(m.status().buffer_fullness, 33);
            m
        };

        let mut m = setup();
        assert_eq!(
            one(&mut m, ms(7), &update(20_000)).response,
            DacResponse::ACK
        );
        let r = one(&mut m, ms(13), &blank_data(965));
        assert_eq!(r.response, DacResponse::NAK_INVALID);
        assert_eq!(r.dac_status.playback_state, DacStatus::PLAYBACK_IDLE);
        assert_ne!(r.dac_status.playback_flags & PBF_UNDERFLOW, 0);
        assert_eq!(m.underflows(), 1);

        // Data first: it lands at the old rate, then the switch happens with
        // a full ring.
        let mut m = setup();
        assert_eq!(
            one(&mut m, ms(13), &blank_data(965)).response,
            DacResponse::ACK
        );
        let r = one(&mut m, ms(13), &update(20_000));
        assert_eq!(r.response, DacResponse::ACK);
        assert_eq!(r.dac_status.buffer_fullness, 27 + 965);
        assert_eq!(m.underflows(), 0);
    }

    #[test]
    fn process_before_stops_in_front_of_the_opcode() {
        let mut m = ed2();
        let mut input = [prepare(), blank_data(10), begin(1_000)].concat();
        assert_eq!(m.process_before(ms(0), &mut input, b'd').len(), 1);
        assert_eq!(input.first(), Some(&b'd'));
        assert_eq!(m.process_one(ms(0), &mut input).len(), 1);
        assert_eq!(input.first(), Some(&b'b'));
        assert_eq!(m.process(ms(0), &mut input).len(), 1);
        assert!(input.is_empty());
    }

    #[test]
    fn reply_override_is_one_shot_and_per_opcode() {
        let mut m = ed2();
        m.override_reply_code(b'p', DacResponse::NAK_STOP_CONDITION);
        assert_eq!(one(&mut m, ms(0), b"?").response, DacResponse::ACK);
        let r = one(&mut m, ms(0), &prepare());
        assert_eq!(r.response, DacResponse::NAK_STOP_CONDITION);
        assert_eq!(r.command, b'p');
        // The command itself still ran.
        assert_eq!(r.dac_status.playback_state, DacStatus::PLAYBACK_PREPARED);
        assert_eq!(one(&mut m, ms(0), b"?").response, DacResponse::ACK);
    }

    #[test]
    fn stop_leaves_stale_fullness_until_prepare() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(300));
        let r = one(&mut m, ms(0), b"s");
        assert_eq!(r.response, DacResponse::ACK);
        assert_eq!(r.dac_status.playback_state, DacStatus::PLAYBACK_IDLE);
        assert_eq!(r.dac_status.buffer_fullness, 300, "stale");
        m.advance_to(ms(500));
        assert_eq!(m.status().buffer_fullness, 300, "does not drain while idle");
        // Stop again while idle -> NAK-Invalid.
        assert_eq!(
            one(&mut m, ms(500), b"s").response,
            DacResponse::NAK_INVALID
        );
        assert_eq!(
            one(&mut m, ms(500), &prepare()).dac_status.buffer_fullness,
            0
        );
    }

    #[test]
    fn stop_clears_ring_without_stale_flag() {
        let mut p = FirmwareProfile::ed2_r331();
        p.stale_fullness_until_prepare = false;
        p.stop_while_idle_naks = false;
        let mut m = EtherDreamModel::new(p);
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(300));
        assert_eq!(one(&mut m, ms(0), b"s").dac_status.buffer_fullness, 0);
        assert_eq!(one(&mut m, ms(0), b"s").response, DacResponse::ACK);
    }

    #[test]
    fn estop_clears_ring_without_stale_flag() {
        let mut p = FirmwareProfile::ed2_r331();
        p.stale_fullness_until_prepare = false;
        let mut m = EtherDreamModel::new(p);
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(300));
        assert_eq!(one(&mut m, ms(0), &[0x00]).dac_status.buffer_fullness, 0);
    }

    #[test]
    fn data_while_idle_naks_invalid() {
        each_profile(|mut m| {
            let r = one(&mut m, ms(0), &blank_data(5));
            assert_eq!(r.response, DacResponse::NAK_INVALID, "{}", m.profile.name);
            assert_eq!(m.accepted_total(), 0);
            // n == 0 is always ACKed.
            assert_eq!(
                one(&mut m, ms(0), &blank_data(0)).response,
                DacResponse::ACK
            );
        });
    }

    #[test]
    fn estop_and_clear_follow_ed2() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(100));
        let r = one(&mut m, ms(0), &[0x00]);
        assert_eq!(r.response, DacResponse::ACK);
        assert_eq!(r.dac_status.light_engine_state, 3);
        assert_eq!(r.dac_status.light_engine_flags, 0x3);
        assert_eq!(r.dac_status.playback_flags, 0x5f35);
        assert_eq!(
            one(&mut m, ms(0), &blank_data(1)).response,
            DacResponse::NAK_INVALID
        );
        assert_eq!(
            one(&mut m, ms(0), &prepare()).response,
            DacResponse::NAK_INVALID,
            "cannot prepare during e-stop"
        );
        let r = one(&mut m, ms(0), b"c");
        assert_eq!(r.dac_status.light_engine_state, 0);
        assert_eq!(r.dac_status.playback_flags, 0x5f35, "sticky until prepare");
        assert_eq!(
            one(&mut m, ms(0), &prepare()).dac_status.playback_flags,
            0x5f31
        );
        // The alternate opcode also e-stops.
        assert_eq!(one(&mut m, ms(1), &[0xff]).dac_status.light_engine_state, 3);
    }

    #[test]
    fn estop_clear_with_warmup_and_interlock() {
        let mut p = FirmwareProfile::ed2_r331();
        p.estop_clear_immediate = false;
        let mut m = EtherDreamModel::new(p);
        one(&mut m, ms(0), &[0x00]);
        m.set_interlock(false);
        let r = one(&mut m, ms(0), b"c");
        assert_eq!(r.response, DacResponse::NAK_STOP_CONDITION);
        m.set_interlock(true);
        let r = one(&mut m, ms(0), b"c");
        // j4cDAC NAKs a clear that does not leave the light engine ready.
        assert_eq!(r.response, DacResponse::NAK_STOP_CONDITION);
        assert_eq!(
            r.dac_status.light_engine_state,
            DacStatus::LIGHT_ENGINE_WARMUP
        );
        let r = one(&mut m, ms(50), b"c");
        assert_eq!(
            r.response,
            DacResponse::NAK_STOP_CONDITION,
            "still warming up"
        );
        m.advance_to(ms(100));
        assert_eq!(m.status().light_engine_state, DacStatus::LIGHT_ENGINE_READY);
    }

    #[test]
    fn opening_the_interlock_estops() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(1000));
        one(&mut m, ms(0), &begin(30_000));
        assert_eq!(m.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        m.set_interlock(false);
        let st = m.status();
        assert_eq!(st.playback_state, DacStatus::PLAYBACK_IDLE);
        assert_eq!(
            st.light_engine_state,
            DacStatus::LIGHT_ENGINE_EMERGENCY_STOP
        );
    }

    /// Regression: the injection fired as soon as the opcode byte arrived, so
    /// a `'d'` split across reads raced against its first few bytes.
    #[test]
    fn inject_before_waits_for_the_whole_command() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        m.inject_before(b'd', vec![0x00]);
        let data = blank_data(100);
        let (head, tail) = data.split_at(5);
        assert!(m.feed(ms(0), head).is_empty());
        assert_eq!(m.status().light_engine_state, DacStatus::LIGHT_ENGINE_READY);
        let r = m.feed(ms(0), tail);
        assert_eq!(r.len(), 1, "{r:?}");
        assert_eq!(
            m.status().light_engine_state,
            DacStatus::LIGHT_ENGINE_EMERGENCY_STOP
        );
    }

    #[test]
    fn overfull_data_partially_writes_then_naks_invalid() {
        let mut m = EtherDreamModel::new(FirmwareProfile::ed1_j4cdac());
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(1700));
        let r = one(&mut m, ms(0), &blank_data(200));
        assert_eq!(r.response, DacResponse::NAK_INVALID);
        assert_eq!(r.dac_status.buffer_fullness, 1799);
        assert!(m.input.is_empty(), "the rest is swallowed");
    }

    #[test]
    fn ed2_ring_holds_exactly_the_advertised_capacity() {
        // ED2 fact: 1000 + 2899 points were ACKed, the next 100 got
        // NAK-Invalid with fullness unchanged at 3899.
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        assert_eq!(
            one(&mut m, ms(0), &blank_data(1000)).response,
            DacResponse::ACK
        );
        assert_eq!(
            one(&mut m, ms(0), &blank_data(2899)).response,
            DacResponse::ACK
        );
        let r = one(&mut m, ms(0), &blank_data(100));
        assert_eq!(r.response, DacResponse::NAK_INVALID);
        assert_eq!(r.dac_status.buffer_fullness, 3899);
        assert!(m.input.is_empty(), "the rejected payload is swallowed");
    }

    #[test]
    fn naks_full_rejects_whole_command() {
        let mut p = FirmwareProfile::ed1_j4cdac();
        p.naks_full = true;
        let mut m = EtherDreamModel::new(p);
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(1700));
        let r = one(&mut m, ms(0), &blank_data(200));
        assert_eq!(r.response, DacResponse::NAK_FULL);
        assert_eq!(r.dac_status.buffer_fullness, 1700);
    }

    #[test]
    fn overfull_without_partial_write_drops_everything() {
        let mut p = FirmwareProfile::ed1_j4cdac();
        p.data_partial_write_then_nak_invalid = false;
        let mut m = EtherDreamModel::new(p);
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &blank_data(1700));
        let r = one(&mut m, ms(0), &blank_data(200));
        assert_eq!(r.response, DacResponse::NAK_INVALID);
        assert_eq!(r.dac_status.buffer_fullness, 1700);
    }

    #[test]
    fn rate_out_of_range_naks_after_one_byte() {
        let mut m = ed2();
        // 'b' with rate 100_001: the opcode is NAKed, then the six argument
        // bytes are parsed as commands. 0x00 is e-stop, so pick a low-water
        // mark that is not; here the args start with 0x01 (unknown -> reset).
        let mut bytes = vec![b'b'];
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&100_001u32.to_le_bytes());
        let r = m.feed(ms(0), &bytes);
        assert!(matches!(r[0], Reply::Response(ref x) if x.response == DacResponse::NAK_INVALID));
        assert_eq!(r[1], Reply::Reset);

        let mut p = FirmwareProfile::ed2_r331();
        p.rate_nak_consumes_one_byte = false;
        let mut m = EtherDreamModel::new(p);
        let r = m.feed(ms(0), &bytes);
        assert_eq!(r.len(), 1);
        assert!(m.input.is_empty());
    }

    #[test]
    fn zero_rate_hangs_or_naks() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        assert_eq!(m.feed(ms(0), &begin(0)), vec![Reply::Hang]);
        assert!(m.is_hung());
        assert!(
            m.feed(ms(1), b"?").is_empty(),
            "hung firmware answers nothing"
        );

        let mut p = FirmwareProfile::ed2_r331();
        p.zero_rate_hangs = false;
        p.rate_nak_consumes_one_byte = false;
        let mut m = EtherDreamModel::new(p);
        assert_eq!(
            one(&mut m, ms(0), &update(0)).response,
            DacResponse::NAK_INVALID
        );
    }

    #[test]
    fn unknown_command_resets_or_naks() {
        let mut m = ed2();
        assert_eq!(m.feed(ms(0), b"z?"), vec![Reply::Reset]);
        assert!(m.input.is_empty());

        let mut p = FirmwareProfile::ed2_r331();
        p.unknown_command_resets_tcp = false;
        let mut m = EtherDreamModel::new(p);
        let r = m.feed(ms(0), b"z?");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn version_reply_is_32_nul_padded_bytes() {
        let mut m = ed2();
        let r = m.feed(ms(0), b"v");
        let Reply::Version(raw) = r[0] else {
            panic!("{r:?}")
        };
        assert_eq!(&raw[..12], b"r331-ed4bef5");
        assert!(raw[12..].iter().all(|&b| b == 0));

        let mut p = FirmwareProfile::ed2_r331();
        p.version_string = None;
        let mut m = EtherDreamModel::new(p);
        assert_eq!(m.feed(ms(0), b"v"), vec![Reply::Reset]);
    }

    #[test]
    fn half_written_command_waits() {
        let mut m = ed2();
        let bytes = blank_data(3);
        assert!(m.feed(ms(0), &bytes[..10]).is_empty());
        assert!(m.feed(ms(5_000), &bytes[10..20]).is_empty());
        let r = m.feed(ms(10_000), &bytes[20..]);
        assert!(matches!(r[0], Reply::Response(ref x) if x.response == DacResponse::NAK_INVALID));
    }

    #[test]
    fn several_commands_in_one_feed_answer_in_order() {
        let mut m = ed2();
        let mut bytes = prepare();
        bytes.extend(blank_data(10));
        bytes.extend(begin(1000));
        bytes.push(b'?');
        let r = m.feed(ms(0), &bytes);
        let cmds: Vec<u8> = r
            .iter()
            .map(|x| match x {
                Reply::Response(r) => r.command,
                _ => 0,
            })
            .collect();
        assert_eq!(cmds, vec![b'p', b'd', b'b', b'?']);
    }

    #[test]
    fn begin_with_empty_ring_underflows_on_first_point() {
        let mut m = ed2();
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &begin(1000));
        m.advance_to(us(999));
        assert_eq!(m.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        m.advance_to(ms(1));
        assert_eq!(m.status().playback_state, DacStatus::PLAYBACK_IDLE);
    }

    #[test]
    fn recorded_output_preserves_order() {
        let mut m = ed2();
        m.set_record_output(true);
        let pts: Vec<DacPoint> = (0..5).map(|i| DacPoint { x: i, ..blank() }).collect();
        one(&mut m, ms(0), &prepare());
        one(&mut m, ms(0), &data(&pts));
        one(&mut m, ms(0), &begin(1000));
        m.advance_to(ms(5));
        assert_eq!(m.take_output(), pts);
        assert_eq!(m.played_total(), 5);
    }

    #[test]
    fn broadcast_reports_profile() {
        each_profile(|m| {
            let b = m.broadcast();
            assert_eq!(b.buffer_capacity, m.profile.buffer_capacity);
            assert_eq!(b.max_point_rate, m.profile.max_point_rate);
            assert_eq!(b.mac_address, m.profile.mac_address);
        });
    }
}
