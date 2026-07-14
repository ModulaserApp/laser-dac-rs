//! Ether Dream DAC streaming backend implementation.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::{BufferEstimator, StatusDecayEstimator};
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;
use crate::protocols::ether_dream::dac::stream::{
    self, CommunicationError, Nak, ResponseErrorKind,
};
use crate::protocols::ether_dream::dac::{LightEngine, Playback, PlaybackFlags};
use crate::protocols::ether_dream::protocol::{DacBroadcast, DacPoint};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Minimum spacing between status-refresh pings while the light engine is
/// warming up or cooling down.
const WARMUP_PING_INTERVAL: Duration = Duration::from_millis(100);

/// Minimum spacing between clear-emergency-stop attempts while the DAC is stuck
/// in an emergency-stop condition (avoids hammering the firmware / hot-looping
/// reconnects when a hardware interlock is engaged).
const ESTOP_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// Ether Dream DAC backend (network).
pub struct EtherDreamBackend {
    broadcast: DacBroadcast,
    ip_addr: IpAddr,
    stream: Option<stream::Stream>,
    caps: DacCapabilities,
    /// When we last received a fresh status from the DAC.
    last_status_time: Option<Instant>,
    /// The point rate from the last write (for decay calculation).
    last_point_rate: u32,
    /// When we last sent a status-refresh ping during warmup/cooldown.
    last_ping_time: Option<Instant>,
    /// When we last attempted to clear an emergency-stop condition.
    last_estop_attempt: Option<Instant>,
    /// Status-anchored buffer estimator, consulted by the NetworkFifo adapter
    /// for pacing and rebased on every authoritative status report.
    estimator: StatusDecayEstimator,
}

impl EtherDreamBackend {
    pub fn new(broadcast: DacBroadcast, ip_addr: IpAddr) -> Self {
        Self {
            broadcast,
            ip_addr,
            stream: None,
            caps: super::default_capabilities(),
            last_status_time: None,
            last_point_rate: 0,
            last_ping_time: None,
            last_estop_attempt: None,
            estimator: StatusDecayEstimator::new(),
        }
    }
}

impl DacBackend for EtherDreamBackend {
    fn dac_type(&self) -> DacType {
        DacType::EtherDream
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        let stream = stream::connect_timeout(&self.broadcast, self.ip_addr, Duration::from_secs(5))
            .map_err(Error::backend)?;

        self.stream = Some(stream);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            let _ = stream.queue_commands().stop().submit();
        }
        self.stream = None;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            match stream.queue_commands().stop().submit() {
                Ok(()) => {}
                // Stopping while already idle draws a NAK-Invalid from the
                // firmware; that's benign — we're already in the target state.
                Err(e) if matches!(nak_of(&e), Some(Nak::Invalid)) => {}
                Err(e) => return Err(Error::backend(e)),
            }
        }
        Ok(())
    }

    fn set_shutter(&mut self, _open: bool) -> Result<()> {
        Ok(())
    }
}

impl FifoBackend for EtherDreamBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        if points.is_empty() {
            return Ok(WriteOutcome::WouldBlock);
        }

        match stream.dac().status.light_engine {
            LightEngine::EmergencyStop => {
                // Rate-limit clear attempts: firmware answers '!' NAK while the
                // stop condition persists, and re-entering this path on every
                // WouldBlock spin would hammer the DAC / hot-loop reconnects.
                let now = Instant::now();
                let due = self
                    .last_estop_attempt
                    .is_none_or(|t| now.duration_since(t) >= ESTOP_RETRY_INTERVAL);
                if !due {
                    return Ok(WriteOutcome::WouldBlock);
                }
                self.last_estop_attempt = Some(now);

                match stream.queue_commands().clear_emergency_stop().submit() {
                    Ok(()) => {}
                    Err(e) => match nak_of(&e) {
                        Some(Nak::StopCondition) => {
                            log::warn!(
                                "Ether Dream stuck in emergency stop - check hardware interlock"
                            );
                            return Ok(WriteOutcome::WouldBlock);
                        }
                        _ => return Err(Error::backend(e)),
                    },
                }

                stream
                    .queue_commands()
                    .ping()
                    .submit()
                    .map_err(Error::backend)?;

                if stream.dac().status.light_engine == LightEngine::EmergencyStop {
                    log::warn!(
                        "Ether Dream still in emergency stop after clear - check hardware interlock"
                    );
                    return Ok(WriteOutcome::WouldBlock);
                }
                // Status is now fresh from the ping response.
                let now = Instant::now();
                self.last_status_time = Some(now);
                self.estimator
                    .record_status(now, stream.dac().status.buffer_fullness as u64);
            }
            LightEngine::Warmup | LightEngine::Cooldown => {
                // Livelock guard: nothing else in this branch refreshes status,
                // so send a rate-limited ping to keep the cached light-engine
                // state moving toward Ready. Without it the session would spin
                // dark forever after an estop -> warmup transition.
                let now = Instant::now();
                let due = self
                    .last_ping_time
                    .is_none_or(|t| now.duration_since(t) >= WARMUP_PING_INTERVAL);
                if due {
                    self.last_ping_time = Some(now);
                    stream
                        .queue_commands()
                        .ping()
                        .submit()
                        .map_err(Error::backend)?;
                }
                return Ok(WriteOutcome::WouldBlock);
            }
            LightEngine::Ready => {}
        }

        // Clamp the requested rate to what the hardware can sustain.
        let point_rate = if pps > 0 {
            pps
        } else {
            stream.dac().max_point_rate / 16
        };
        let max_rate = stream.dac().max_point_rate;
        let point_rate = if max_rate > 0 {
            point_rate.min(max_rate)
        } else {
            point_rate
        };

        let playback = stream.dac().status.playback;
        let buffer_capacity = stream.dac().buffer_capacity;
        let raw_fullness = stream.dac().status.buffer_fullness;
        let fullness = decay_fullness(
            raw_fullness,
            buffer_capacity,
            self.last_status_time,
            self.last_point_rate,
            playback == Playback::Playing,
        );

        let available = buffer_capacity.saturating_sub(fullness).saturating_sub(1) as usize;

        // Never silently truncate. The adapter commits the full slice on
        // Written, so a clamped write would drop the tail forever; block and
        // retry once the ring has drained enough to take the whole chunk.
        if available < points.len() {
            return Ok(WriteOutcome::WouldBlock);
        }

        let playback_flags = stream.dac().status.playback_flags;
        let current_point_rate = stream.dac().status.point_rate;

        let needs_prepare =
            playback_flags.contains(PlaybackFlags::UNDERFLOWED) || playback == Playback::Idle;

        if needs_prepare {
            stream
                .queue_commands()
                .prepare_stream()
                .submit()
                .map_err(Error::backend)?;
        }

        // `CommandQueue::data` converts straight into the stream's own staging
        // buffer, so each point is written once rather than staged through a
        // second backend-owned buffer first.
        let send_result = if playback == Playback::Playing && current_point_rate != point_rate {
            stream
                .queue_commands()
                .update(0, point_rate)
                .data(points.iter().map(DacPoint::from))
                .submit()
        } else {
            stream
                .queue_commands()
                .data(points.iter().map(DacPoint::from))
                .submit()
        };

        match send_result {
            Ok(()) => {}
            Err(e) => match nak_of(&e) {
                // Ring full: pure backpressure. Firmware discarded the payload
                // on a healthy connection, so don't advance the estimate.
                Some(Nak::Full) => {
                    self.last_status_time = Some(Instant::now());
                    return Ok(WriteOutcome::WouldBlock);
                }
                Some(Nak::Invalid) => {
                    if playback == Playback::Idle {
                        // Idle-underflow re-prepare: the firmware wants a fresh
                        // prepare before it will accept streaming data again.
                        stream
                            .queue_commands()
                            .prepare_stream()
                            .submit()
                            .map_err(Error::backend)?;
                        match stream
                            .queue_commands()
                            .data(points.iter().map(DacPoint::from))
                            .submit()
                        {
                            Ok(()) => {}
                            Err(e2) => match nak_of(&e2) {
                                Some(Nak::Full) | Some(Nak::Invalid) => {
                                    self.last_status_time = Some(Instant::now());
                                    return Ok(WriteOutcome::WouldBlock);
                                }
                                _ => return Err(Error::backend(e2)),
                            },
                        }
                    } else {
                        // Prepared/Playing: payload discarded — backpressure.
                        self.last_status_time = Some(Instant::now());
                        return Ok(WriteOutcome::WouldBlock);
                    }
                }
                // Clean NAK-StopCondition, or an IO/timeout/protocol error:
                // fatal. Retrying into a desynced stream makes the firmware
                // close the connection.
                _ => return Err(Error::backend(e)),
            },
        }

        // Recompute the begin decision from the status re-read after the data
        // submit (every response refreshes dac().status), so a post-underflow
        // restart isn't delayed a whole loop iteration.
        let playback_after = stream.dac().status.playback;
        let buffer_fullness = stream.dac().status.buffer_fullness;
        let needs_begin =
            playback_after != Playback::Playing && buffer_fullness >= begin_threshold(point_rate);

        if needs_begin {
            stream
                .queue_commands()
                .begin(0, point_rate)
                .submit()
                .map_err(Error::backend)?;
        }

        let now = Instant::now();
        self.last_status_time = Some(now);
        self.last_point_rate = point_rate;
        // The ACK's buffer_fullness already includes the points just sent
        // (verified against j4cDAC firmware: status is sampled after the ring
        // write), so record only the authoritative status. Also calling
        // record_send would double-count one chunk.
        self.estimator
            .set_playing(stream.dac().status.playback == Playback::Playing);
        self.estimator
            .record_status(now, stream.dac().status.buffer_fullness as u64);
        Ok(WriteOutcome::Written)
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

/// Extract a clean protocol NAK from a communication error, if present.
///
/// Returns `Some` only for a fully-consumed NAK response (the stream is still
/// in sync); IO/timeout/protocol-desync errors return `None` and must be
/// treated as fatal.
fn nak_of(err: &CommunicationError) -> Option<Nak> {
    match err {
        CommunicationError::Response(re) => match &re.kind {
            ResponseErrorKind::Nak(nak) => Some(*nak),
            _ => None,
        },
        _ => None,
    }
}

/// Number of points to buffer before issuing `begin`, derived from the point
/// rate: roughly 10 ms of cover, clamped to a sane range. Avoids the fixed
/// 256-point threshold's ~256 ms start latency at 1 kpps and its thin ~2.6 ms
/// cover at 100 kpps.
fn begin_threshold(point_rate: u32) -> u16 {
    // Points streamed in ~10 ms at the current rate.
    let ten_ms = point_rate / 100;
    ten_ms.clamp(64, 1700) as u16
}

/// Decay a raw buffer fullness value based on elapsed time since last status.
///
/// Used inside `try_write_points` to compute headroom against the device's
/// authoritative buffer capacity. The new [`StatusDecayEstimator`] mirrors the
/// same anchor-and-decay shape; this helper stays because the admission check
/// also needs the saturating clamp against `capacity`. The device only drains
/// the ring while actually playing, so `playing == false` freezes the estimate.
fn decay_fullness(
    raw: u16,
    capacity: u16,
    last_status_time: Option<Instant>,
    point_rate: u32,
    playing: bool,
) -> u16 {
    if !playing {
        return raw.min(capacity);
    }
    if let Some(last_time) = last_status_time {
        let elapsed_secs = last_time.elapsed().as_secs_f64();
        let consumed = (elapsed_secs * point_rate as f64) as u16;
        raw.saturating_sub(consumed).min(capacity)
    } else {
        raw
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::ether_dream::dac::{
        Addressed, Dac, DataSource, LightEngineFlags, MacAddress, Status,
    };
    use crate::protocols::ether_dream::protocol::command::Command as _;
    use crate::protocols::ether_dream::protocol::{
        self, DacResponse, DacStatus, SizeBytes, WriteToBytes,
    };
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;

    // -- begin_threshold ---------------------------------------------------

    #[test]
    fn begin_threshold_is_pps_derived() {
        // ~10 ms of points, clamped to [64, 1700].
        assert_eq!(begin_threshold(1_000), 64); // 10 → clamped up to floor
        assert_eq!(begin_threshold(6_400), 64); // 64 exactly
        assert_eq!(begin_threshold(30_000), 300); // 30000/100
        assert_eq!(begin_threshold(100_000), 1000);
        assert_eq!(begin_threshold(1_000_000), 1700); // clamped down to ceiling
    }

    // -- mock DAC harness --------------------------------------------------

    fn mk_status(le: u8, pb: u8, fullness: u16, rate: u32) -> DacStatus {
        DacStatus {
            protocol: 0,
            light_engine_state: le,
            playback_state: pb,
            source: 0,
            light_engine_flags: 0,
            playback_flags: 0,
            source_flags: 0,
            buffer_fullness: fullness,
            point_rate: rate,
            point_count: 0,
        }
    }

    fn ack(cmd: u8, status: DacStatus) -> DacResponse {
        DacResponse {
            response: DacResponse::ACK,
            command: cmd,
            dac_status: status,
        }
    }

    fn nak(kind: u8, cmd: u8, status: DacStatus) -> DacResponse {
        DacResponse {
            response: kind,
            command: cmd,
            dac_status: status,
        }
    }

    fn addressed(le: LightEngine, pb: Playback, fullness: u16, capacity: u16) -> Addressed {
        Addressed {
            mac_address: MacAddress([0; 6]),
            dac: Dac {
                hw_revision: 0,
                sw_revision: 0,
                buffer_capacity: capacity,
                max_point_rate: 100_000,
                status: Status {
                    protocol: 0,
                    light_engine: le,
                    playback: pb,
                    data_source: DataSource::NetworkStreaming,
                    light_engine_flags: LightEngineFlags::empty(),
                    playback_flags: PlaybackFlags::empty(),
                    buffer_fullness: fullness,
                    point_rate: 0,
                    point_count: 0,
                },
            },
        }
    }

    fn test_broadcast() -> DacBroadcast {
        DacBroadcast {
            mac_address: [0; 6],
            hw_revision: 0,
            sw_revision: 0,
            buffer_capacity: 1000,
            max_point_rate: 100_000,
            dac_status: mk_status(0, 0, 0, 0),
        }
    }

    /// Spin up a loopback mock DAC that answers each received command with
    /// `handler(command_byte)`. Returns a backend wired to it plus a shared log
    /// of the command bytes the mock received.
    fn connect_mock(
        initial: Addressed,
        handler: impl FnMut(u8) -> DacResponse + Send + 'static,
    ) -> (EtherDreamBackend, Arc<Mutex<Vec<u8>>>) {
        let (backend, commands, _payloads) = connect_mock_capturing(initial, handler);
        (backend, commands)
    }

    /// A mock DAC wired to a backend: the backend, the log of command bytes the
    /// mock received, and the raw `DacPoint` bytes of each Data command.
    type MockDac = (
        EtherDreamBackend,
        Arc<Mutex<Vec<u8>>>,
        Arc<Mutex<Vec<Vec<u8>>>>,
    );

    /// As [`connect_mock`], but also records the raw `DacPoint` bytes of each
    /// Data command as its own entry, so a test can assert what actually
    /// reached the wire on each individual submit.
    fn connect_mock_capturing(
        initial: Addressed,
        handler: impl FnMut(u8) -> DacResponse + Send + 'static,
    ) -> MockDac {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let commands_srv = commands.clone();
        let payloads = Arc::new(Mutex::new(Vec::new()));
        let payloads_srv = payloads.clone();
        let mut handler = handler;

        thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            loop {
                let mut cmd = [0u8; 1];
                if sock.read_exact(&mut cmd).is_err() {
                    break;
                }
                let cmd = cmd[0];
                // Consume the rest of the command so the stream stays synced.
                let extra = match cmd {
                    protocol::command::Begin::START_BYTE
                    | protocol::command::Update::START_BYTE => 6,
                    protocol::command::PointRate::START_BYTE => 4,
                    protocol::command::Data::START_BYTE => {
                        let mut n = [0u8; 2];
                        if sock.read_exact(&mut n).is_err() {
                            break;
                        }
                        u16::from_le_bytes(n) as usize * DacPoint::SIZE_BYTES
                    }
                    _ => 0,
                };
                let mut buf = vec![0u8; extra];
                if extra > 0 && sock.read_exact(&mut buf).is_err() {
                    break;
                }
                if cmd == protocol::command::Data::START_BYTE {
                    payloads_srv.lock().unwrap().push(buf);
                }
                // Record before responding: `submit()` returns as soon as it
                // reads the ACK, so a test that inspects the log after the call
                // must not race the mock's own bookkeeping.
                commands_srv.lock().unwrap().push(cmd);
                let resp = handler(cmd);
                let mut out = Vec::new();
                resp.write_to_bytes(&mut out).unwrap();
                if sock.write_all(&out).is_err() {
                    break;
                }
            }
        });

        let client = TcpStream::connect(addr).unwrap();
        let stream = stream::Stream::from_tcp_stream_for_test(initial, client).unwrap();
        let mut backend = EtherDreamBackend::new(test_broadcast(), "127.0.0.1".parse().unwrap());
        backend.stream = Some(stream);
        (backend, commands, payloads)
    }

    /// The wire bytes a Data command should carry for `pts`: each point encoded
    /// exactly once, in authored order.
    fn expected_payload(pts: &[LaserPoint]) -> Vec<u8> {
        let mut out = Vec::new();
        for p in pts {
            DacPoint::from(p)
                .write_to_bytes(&mut out)
                .expect("writing to Vec<u8> cannot fail");
        }
        out
    }

    /// Points with distinct coordinates and colours, so a dropped, duplicated
    /// or reordered point cannot coincidentally match the expected payload.
    fn distinct_points(n: usize) -> Vec<LaserPoint> {
        (0..n)
            .map(|i| {
                let t = i as f32 / n as f32;
                let i = i as u16;
                LaserPoint::new(t * 2.0 - 1.0, 1.0 - t, i * 7, i * 11, i * 13, i * 17)
            })
            .collect()
    }

    fn points(n: usize) -> Vec<LaserPoint> {
        vec![LaserPoint::new(0.0, 0.0, 0, 0, 0, 0); n]
    }

    // -- H1: estimator no longer double-counts -----------------------------

    #[test]
    fn ack_fullness_is_not_double_counted() {
        // Idle -> prepare -> data (ACK reports 500, already including the
        // points just sent) -> begin (Playing).
        let (mut backend, _cmds) = connect_mock(
            addressed(LightEngine::Ready, Playback::Idle, 0, 1000),
            |cmd| match cmd {
                protocol::command::PrepareStream::START_BYTE => {
                    ack(cmd, mk_status(0, DacStatus::PLAYBACK_PREPARED, 0, 30_000))
                }
                protocol::command::Data::START_BYTE => {
                    ack(cmd, mk_status(0, DacStatus::PLAYBACK_PREPARED, 500, 30_000))
                }
                protocol::command::Begin::START_BYTE => {
                    ack(cmd, mk_status(0, DacStatus::PLAYBACK_PLAYING, 500, 30_000))
                }
                _ => ack(cmd, mk_status(0, DacStatus::PLAYBACK_PLAYING, 500, 30_000)),
            },
        );

        let outcome = backend.try_write_points(30_000, &points(10)).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        // Estimate equals the ACK fullness (500), NOT ACK + sent count (510).
        let est = backend
            .estimator()
            .estimated_fullness(Instant::now(), 30_000);
        assert_eq!(est, 500);
    }

    // -- H2: never silently truncate --------------------------------------

    #[test]
    fn partial_fit_blocks_without_writing() {
        // available = capacity - fullness - 1 = 100 - 95 - 1 = 4, but 10 points
        // are offered: the whole chunk must be refused (no Data sent).
        let (mut backend, cmds) = connect_mock(
            addressed(LightEngine::Ready, Playback::Prepared, 95, 100),
            |cmd| ack(cmd, mk_status(0, DacStatus::PLAYBACK_PREPARED, 95, 30_000)),
        );

        let outcome = backend.try_write_points(30_000, &points(10)).unwrap();
        assert_eq!(outcome, WriteOutcome::WouldBlock);
        assert!(
            cmds.lock().unwrap().is_empty(),
            "no command should be sent when the chunk does not fit"
        );
    }

    // -- H3: warmup/cooldown sends a rate-limited ping ---------------------

    #[test]
    fn warmup_pings_rate_limited() {
        let (mut backend, cmds) = connect_mock(
            addressed(LightEngine::Warmup, Playback::Idle, 0, 1000),
            |cmd| {
                ack(
                    cmd,
                    mk_status(
                        DacStatus::LIGHT_ENGINE_WARMUP,
                        DacStatus::PLAYBACK_IDLE,
                        0,
                        0,
                    ),
                )
            },
        );

        let o1 = backend.try_write_points(30_000, &points(10)).unwrap();
        let o2 = backend.try_write_points(30_000, &points(10)).unwrap();
        assert_eq!(o1, WriteOutcome::WouldBlock);
        assert_eq!(o2, WriteOutcome::WouldBlock);

        // Exactly one ping in two rapid calls (100 ms rate limit).
        let log = cmds.lock().unwrap();
        assert_eq!(log.as_slice(), &[protocol::command::Ping::START_BYTE]);
    }

    // -- H4: NAK-Full is backpressure, not a disconnect --------------------

    #[test]
    fn nak_full_maps_to_would_block() {
        let (mut backend, cmds) = connect_mock(
            addressed(LightEngine::Ready, Playback::Prepared, 0, 1000),
            |cmd| match cmd {
                protocol::command::Data::START_BYTE => nak(
                    DacResponse::NAK_FULL,
                    cmd,
                    mk_status(0, DacStatus::PLAYBACK_PREPARED, 999, 30_000),
                ),
                _ => ack(cmd, mk_status(0, DacStatus::PLAYBACK_PREPARED, 0, 30_000)),
            },
        );

        let outcome = backend.try_write_points(30_000, &points(10)).unwrap();
        assert_eq!(outcome, WriteOutcome::WouldBlock);
        // The data command was attempted (and NAK'd), not treated as fatal.
        assert!(cmds
            .lock()
            .unwrap()
            .contains(&protocol::command::Data::START_BYTE));
    }

    // -- copy-elision contract: every call site still sends every point ----
    //
    // `try_write_points` converts each LaserPoint straight into the command
    // queue's staging buffer instead of staging it through a buffer of its
    // own. That is a pure copy elision, so each of the three `.data(...)` call
    // sites must still put every authored point on the wire exactly once, in
    // order. The two call sites below are the ones no other test reaches.

    #[test]
    fn rate_change_while_playing_updates_rate_and_sends_all_points() {
        // Playing, and the DAC's current rate (0, per `addressed`) differs from
        // the requested 30_000 — the one path that batches Update with Data.
        let (mut backend, cmds, payloads) = connect_mock_capturing(
            addressed(LightEngine::Ready, Playback::Playing, 0, 1000),
            |cmd| ack(cmd, mk_status(0, DacStatus::PLAYBACK_PLAYING, 500, 30_000)),
        );

        let pts = distinct_points(10);
        let outcome = backend.try_write_points(30_000, &pts).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);

        // The rate change went out, and the points rode along with it.
        let log = cmds.lock().unwrap();
        assert!(log.contains(&protocol::command::Update::START_BYTE));
        assert!(log.contains(&protocol::command::Data::START_BYTE));

        let payloads = payloads.lock().unwrap();
        assert_eq!(payloads.len(), 1, "exactly one Data command");
        assert_eq!(
            payloads[0],
            expected_payload(&pts),
            "the update+data call site must send every authored point, in order"
        );
    }

    #[test]
    fn idle_nak_invalid_reprepares_and_resends_every_point() {
        // Idle + NAK-Invalid on the data submit: the firmware wants a fresh
        // prepare before it will take streaming data. The retry re-converts the
        // points, so it must resend the identical payload — a retry that sent a
        // stale or empty buffer would still report Written, and the adapter
        // commits the whole slice on Written, silently dropping the chunk.
        let data_seen = Arc::new(AtomicUsize::new(0));
        let data_seen_srv = data_seen.clone();
        let (mut backend, cmds, payloads) = connect_mock_capturing(
            addressed(LightEngine::Ready, Playback::Idle, 0, 1000),
            move |cmd| {
                let idle = mk_status(0, DacStatus::PLAYBACK_IDLE, 0, 30_000);
                if cmd == protocol::command::Data::START_BYTE
                    && data_seen_srv.fetch_add(1, Ordering::SeqCst) == 0
                {
                    // Only the first Data is rejected; the retry is accepted.
                    return nak(DacResponse::NAK_INVALID, cmd, idle);
                }
                ack(cmd, mk_status(0, DacStatus::PLAYBACK_PREPARED, 10, 30_000))
            },
        );

        let pts = distinct_points(10);
        let outcome = backend.try_write_points(30_000, &pts).unwrap();
        assert_eq!(
            outcome,
            WriteOutcome::Written,
            "the accepted retry must be reported as written"
        );

        // Prepare, rejected data, re-prepare, accepted data.
        let log = cmds.lock().unwrap();
        let prepares = log
            .iter()
            .filter(|&&c| c == protocol::command::PrepareStream::START_BYTE)
            .count();
        assert_eq!(prepares, 2, "the retry re-prepares the stream");
        assert_eq!(
            data_seen.load(Ordering::SeqCst),
            2,
            "the data submit retries"
        );

        let payloads = payloads.lock().unwrap();
        let expected = expected_payload(&pts);
        assert_eq!(payloads.len(), 2);
        assert_eq!(
            payloads[1], expected,
            "the retry must resend every authored point, in order"
        );
        assert_eq!(
            payloads[0], expected,
            "the rejected attempt carried the same payload"
        );
    }

    #[test]
    fn nak_invalid_while_prepared_maps_to_would_block() {
        let (mut backend, _cmds) = connect_mock(
            addressed(LightEngine::Ready, Playback::Prepared, 0, 1000),
            |cmd| match cmd {
                protocol::command::Data::START_BYTE => nak(
                    DacResponse::NAK_INVALID,
                    cmd,
                    mk_status(0, DacStatus::PLAYBACK_PREPARED, 0, 30_000),
                ),
                _ => ack(cmd, mk_status(0, DacStatus::PLAYBACK_PREPARED, 0, 30_000)),
            },
        );

        let outcome = backend.try_write_points(30_000, &points(10)).unwrap();
        assert_eq!(outcome, WriteOutcome::WouldBlock);
    }

    // -- M1: clear-estop surfaces interlock diagnosis, rate-limited --------

    #[test]
    fn estop_stop_condition_blocks_and_rate_limits() {
        let (mut backend, cmds) = connect_mock(
            addressed(LightEngine::EmergencyStop, Playback::Idle, 0, 1000),
            |cmd| {
                // clear-estop always answers '!' (stop condition persists).
                nak(
                    DacResponse::NAK_STOP_CONDITION,
                    cmd,
                    mk_status(
                        DacStatus::LIGHT_ENGINE_EMERGENCY_STOP,
                        DacStatus::PLAYBACK_IDLE,
                        0,
                        0,
                    ),
                )
            },
        );

        let o1 = backend.try_write_points(30_000, &points(10)).unwrap();
        let o2 = backend.try_write_points(30_000, &points(10)).unwrap();
        assert_eq!(o1, WriteOutcome::WouldBlock);
        assert_eq!(o2, WriteOutcome::WouldBlock);

        // Only one clear attempt across two rapid calls (1 s rate limit); the
        // persistent stop condition does not hot-loop into reconnects.
        let log = cmds.lock().unwrap();
        assert_eq!(
            log.as_slice(),
            &[protocol::command::ClearEmergencyStop::START_BYTE]
        );
    }

    // -- M5: IO error on the data submit is fatal, not retried -------------

    #[test]
    fn io_error_on_send_is_fatal_not_retried() {
        // A mock that ACKs the prepare, then closes the connection when the
        // data command arrives (simulating a desynced/dead stream). The idle
        // re-prepare path must NOT trigger on this IO error — it must surface
        // as a fatal error so the session reconnects rather than retrying into
        // a stream the firmware will close.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let commands_srv = commands.clone();

        thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            loop {
                let mut cmd = [0u8; 1];
                if sock.read_exact(&mut cmd).is_err() {
                    break;
                }
                let cmd = cmd[0];
                if cmd == protocol::command::Data::START_BYTE {
                    // Read the point count + points, then drop the connection.
                    let mut n = [0u8; 2];
                    if sock.read_exact(&mut n).is_err() {
                        break;
                    }
                    let extra = u16::from_le_bytes(n) as usize * DacPoint::SIZE_BYTES;
                    let mut buf = vec![0u8; extra];
                    let _ = sock.read_exact(&mut buf);
                    commands_srv.lock().unwrap().push(cmd);
                    break; // close without responding
                }
                commands_srv.lock().unwrap().push(cmd);
                let resp = ack(cmd, mk_status(0, DacStatus::PLAYBACK_PREPARED, 0, 30_000));
                let mut out = Vec::new();
                resp.write_to_bytes(&mut out).unwrap();
                if sock.write_all(&out).is_err() {
                    break;
                }
            }
        });

        let client = TcpStream::connect(addr).unwrap();
        let stream = stream::Stream::from_tcp_stream_for_test(
            addressed(LightEngine::Ready, Playback::Idle, 0, 1000),
            client,
        )
        .unwrap();
        let mut backend = EtherDreamBackend::new(test_broadcast(), "127.0.0.1".parse().unwrap());
        backend.stream = Some(stream);

        let result = backend.try_write_points(30_000, &points(10));
        assert!(result.is_err(), "IO error on send must be fatal");

        // The data command was attempted exactly once — no re-prepare retry.
        let log = commands.lock().unwrap();
        assert_eq!(
            log.iter()
                .filter(|&&c| c == protocol::command::Data::START_BYTE)
                .count(),
            1
        );
    }
}
