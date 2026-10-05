//! Ether Dream DAC streaming backend implementation.
//!
//! The backend reacts only to what the DAC reports in its status frames. It
//! never branches on a firmware model: buffer capacity and maximum rate come
//! from the broadcast (or conservative defaults), and every decision is made
//! from the playback state, flags and fullness the DAC just sent.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::{BufferEstimator, StatusDecayEstimator};
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;
use crate::protocols::ether_dream::dac::stream::{
    self, CommunicationError, Nak, ResponseErrorKind, SYNTHETIC_BUFFER_CAPACITY,
    SYNTHETIC_MAX_POINT_RATE,
};
use crate::protocols::ether_dream::dac::{LightEngine, Playback, Status};
use crate::protocols::ether_dream::profile::FirmwareProfile;
use crate::protocols::ether_dream::protocol::{
    self, Command, DacBroadcast, DacPoint, COMMUNICATION_PORT,
};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

/// Minimum spacing between status-refresh pings while the light engine is
/// warming up or cooling down.
const WARMUP_PING_INTERVAL: Duration = Duration::from_millis(100);

/// Minimum spacing between clear-emergency-stop attempts while the DAC is stuck
/// in an emergency-stop condition (avoids hammering the firmware / hot-looping
/// reconnects when a hardware interlock is engaged).
const ESTOP_RETRY_INTERVAL: Duration = Duration::from_secs(1);

/// TCP connect timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Ether Dream DAC backend (network).
pub struct EtherDreamBackend {
    broadcast: Option<DacBroadcast>,
    addr: SocketAddr,
    stream: Option<stream::Stream>,
    caps: DacCapabilities,
    /// When the reply carrying the last status was read.
    ///
    /// The DAC samples its status at some instant between our request
    /// leaving and its reply arriving, so the status has two anchors:
    ///
    /// * Send time ([`stream::Stream::status_sent_at`]): the DAC cannot have
    ///   sampled earlier, so decaying from it gives a lower bound on fullness.
    ///   The estimator uses it. Over-estimating the drain makes the adapter
    ///   write a little early, which is the safe direction against underflow.
    /// * Receive time (this field): the DAC sampled no later, so decaying from
    ///   it gives an upper bound on fullness. The admission check uses it, so
    ///   a chunk is only sent when it is sure to fit. With the send time, a
    ///   20 ms reply on ED2 hid ~600 points at 30 kpps, and an overfull chunk
    ///   was partially written, NAKed and resent in a loop.
    last_status_received: Option<Instant>,
    /// The point rate from the last write (for decay calculation).
    last_point_rate: u32,
    /// When we last sent a status-refresh ping during warmup/cooldown.
    last_ping_time: Option<Instant>,
    /// When we last attempted to clear an emergency-stop condition.
    last_estop_attempt: Option<Instant>,
    /// Status-anchored buffer estimator, consulted by the NetworkFifo adapter
    /// for pacing and rebased on every authoritative status report.
    estimator: DacRateEstimator,
    /// Build string from the `'v'` command, if the firmware answered it.
    firmware_version: Option<String>,
    /// Set once `'v'` broke a connection, so later reconnects skip it.
    skip_version_query: bool,
}

impl EtherDreamBackend {
    /// Backend for a DAC found through its UDP broadcast.
    pub fn new(broadcast: DacBroadcast, ip_addr: IpAddr) -> Self {
        Self::with_address(
            SocketAddr::new(ip_addr, COMMUNICATION_PORT),
            Some(broadcast),
        )
    }

    /// Backend for a DAC at a known address, with or without its broadcast.
    ///
    /// Without a broadcast the capabilities assume 1799 points, the smallest
    /// known ring (ED1), and 100 000 pps, which every known Ether Dream
    /// advertises. See [`stream::connect_to`].
    pub fn with_address(addr: SocketAddr, broadcast: Option<DacBroadcast>) -> Self {
        let (capacity, max_rate) = match &broadcast {
            Some(b) => (b.buffer_capacity, b.max_point_rate),
            None => (SYNTHETIC_BUFFER_CAPACITY, SYNTHETIC_MAX_POINT_RATE),
        };
        Self {
            broadcast,
            addr,
            stream: None,
            caps: caps_for(capacity, max_rate),
            last_status_received: None,
            last_point_rate: 0,
            last_ping_time: None,
            last_estop_attempt: None,
            estimator: DacRateEstimator::default(),
            firmware_version: None,
            skip_version_query: false,
        }
    }

    /// The firmware build string reported by the `'v'` command on the last
    /// connect, if the firmware supports it.
    pub fn firmware_version(&self) -> Option<&str> {
        self.firmware_version.as_deref()
    }

    fn open_stream(&self) -> Result<stream::Stream> {
        stream::connect_to(self.addr, self.broadcast.as_ref(), CONNECT_TIMEOUT)
            .map_err(Error::backend)
    }
}

/// Capabilities derived from what the DAC advertises. Zero values (a DAC that
/// reports nothing useful) fall back to the defaults.
pub(super) fn caps_for(capacity: u16, max_rate: u32) -> DacCapabilities {
    let mut caps = super::default_capabilities();
    if capacity > 0 {
        caps.max_points_per_chunk = capacity as usize;
    }
    if max_rate > 0 {
        caps.pps_max = max_rate;
    }
    caps
}

impl DacBackend for EtherDreamBackend {
    fn dac_type(&self) -> DacType {
        DacType::EtherDream
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        let mut stream = self.open_stream()?;

        // Nothing learned on an earlier connection still holds.
        self.estimator = DacRateEstimator::default();
        self.last_point_rate = 0;
        self.last_ping_time = None;
        self.last_estop_attempt = None;
        self.firmware_version = None;

        if !self.skip_version_query {
            match stream.query_version() {
                Ok(Some(version)) => {
                    match FirmwareProfile::identify(&version) {
                        Some(p) => log::info!("Ether Dream firmware {version} ({})", p.name),
                        None => log::info!("Ether Dream firmware {version}"),
                    }
                    self.firmware_version = Some(version);
                }
                Ok(None) => {
                    log::debug!("Ether Dream firmware NAKed 'v'");
                    self.skip_version_query = true;
                }
                Err(e) => {
                    // Firmware without 'v' may reset the connection or stay
                    // silent. Either way the stream is unusable: reconnect and
                    // never ask again.
                    log::debug!("Ether Dream 'v' failed ({e}); reconnecting without it");
                    self.skip_version_query = true;
                    drop(stream);
                    stream = self.open_stream()?;
                }
            }
        }

        let dac = stream.dac();
        self.caps = caps_for(dac.buffer_capacity, dac.max_point_rate);
        self.last_status_received = Some(stream.status_received_at());
        self.stream = Some(stream);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            // Leave the DAC idle. Skip the stop when the last status already
            // says so (e.g. right after `stop()`): firmware NAKs a stop while
            // idle, and the ring's stale fullness only clears on prepare.
            if stream.dac().status.playback != Playback::Idle {
                let _ = stream.queue_commands().stop().submit();
            }
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
                Err(e) => {
                    // The link is broken or out of sync. Drop it so
                    // `disconnect()` does not wait out another stop.
                    self.stream = None;
                    return Err(Error::backend(e));
                }
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
                        // Also answered when the clear only starts a warmup;
                        // the ping below then shows it.
                        Some(Nak::StopCondition)
                            if stream.dac().status.light_engine == LightEngine::EmergencyStop =>
                        {
                            log::warn!(
                                "Ether Dream stuck in emergency stop - check hardware interlock"
                            );
                            return Ok(WriteOutcome::WouldBlock);
                        }
                        Some(Nak::StopCondition) => {}
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
                let now = stream.status_sent_at();
                self.last_status_received = Some(stream.status_received_at());
                self.estimator.record_status(now, &stream.dac().status);
                if stream.dac().status.light_engine != LightEngine::Ready {
                    return Ok(WriteOutcome::WouldBlock);
                }
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
                    self.last_status_received = Some(stream.status_received_at());
                }
                return Ok(WriteOutcome::WouldBlock);
            }
            LightEngine::Ready => {}
        }

        let max_rate = effective_max_rate(stream.dac().max_point_rate);
        let point_rate = clamp_rate(pps, max_rate);
        let capacity = effective_capacity(stream.dac().buffer_capacity);
        let threshold = begin_threshold(point_rate, capacity);

        // At most two passes: the second runs only when the DAC turned out to
        // be idle after all (it underflowed or stopped between our last status
        // and this write), so whatever it held is gone and the chunk must go
        // into a freshly prepared stream.
        let mut retried = false;
        let write_start = Instant::now();
        loop {
            // (a) Prepare *before* the headroom check. An idle DAC may still
            // report the fullness of a stream that was stopped (the firmware
            // only clears its ring on prepare), and checking headroom against
            // that stale value blocks forever.
            let fullness = if stream.dac().status.playback == Playback::Idle {
                match stream.queue_commands().prepare_stream().submit() {
                    Ok(()) => {}
                    // The DAC refuses to prepare (light engine not ready, or a
                    // race with another client). Try again later.
                    Err(e) if nak_of(&e).is_some() => {
                        self.last_status_received = Some(stream.status_received_at());
                        self.estimator
                            .record_status(stream.status_sent_at(), &stream.dac().status);
                        return Ok(WriteOutcome::WouldBlock);
                    }
                    Err(e) => return Err(Error::backend(e)),
                }
                self.last_status_received = Some(stream.status_received_at());
                // A prepared stream starts empty, whatever the ACK carried.
                0
            } else {
                let status = &stream.dac().status;
                let rate = if status.point_rate > 0 {
                    status.point_rate
                } else {
                    self.last_point_rate
                };
                // Upper bound: decay from when the reply arrived, so the
                // check never admits a chunk the ring cannot take.
                decay_fullness(
                    status.buffer_fullness,
                    capacity,
                    self.last_status_received,
                    rate,
                    status.playback == Playback::Playing,
                )
            };

            // The advertised capacity is the number of points the DAC accepts,
            // so a chunk fits when it is no larger than the free space.
            let available = capacity.saturating_sub(fullness) as usize;

            // Never silently truncate. The adapter commits the full slice on
            // Written, so a clamped write would drop the tail forever; block and
            // retry once the ring has drained enough to take the whole chunk.
            if available < points.len() {
                // A prepared stream never drains, so waiting cannot make room.
                // This happens when a small first write stayed below the begin
                // threshold, when another client filled the ring without
                // beginning, or when a begin was refused. Begin now with what
                // is buffered so the ring starts draining.
                let status = &stream.dac().status;
                if status.playback == Playback::Prepared && status.buffer_fullness > 0 {
                    match stream.queue_commands().begin(0, point_rate).submit() {
                        Ok(()) => {}
                        Err(e) if nak_of(&e).is_some() => {}
                        Err(e) => return Err(Error::backend(e)),
                    }
                    let now = stream.status_sent_at();
                    let status = &stream.dac().status;
                    self.last_status_received = Some(stream.status_received_at());
                    self.last_point_rate = point_rate;
                    self.estimator.record_status(now, status);
                }
                return Ok(WriteOutcome::WouldBlock);
            }

            // `CommandQueue::data` converts straight into the stream's own
            // staging buffer, so each point is written once rather than staged
            // through a second backend-owned buffer first.
            let status = &stream.dac().status;
            let send_result =
                if status.playback == Playback::Playing && status.point_rate != point_rate {
                    // Data first, then the rate switch. The other order lets
                    // the few points left in the ring play at the faster
                    // rate while the larger 'd' is still in flight, and the
                    // DAC underflows before the data lands (seen on ED2).
                    stream
                        .queue_commands()
                        .data(points.iter().map(DacPoint::from))
                        .update(0, point_rate)
                        .submit()
                } else {
                    stream
                        .queue_commands()
                        .data(points.iter().map(DacPoint::from))
                        .submit()
                };

            match send_result {
                Ok(()) => {}
                // The data was ACKed and only the rate switch was refused, so
                // the chunk landed. The next write retries the switch, and the
                // idle check below catches a stream that stopped meanwhile.
                Err(e) if nak_command(&e) == Some(protocol::command::Update::START_BYTE) => {}
                Err(e) => match nak_of(&e) {
                    // (c) Only some firmware sends NAK-Full; others answer an
                    // overfull write with NAK-Invalid. Either way the response
                    // carries a fresh status, so re-sync from it instead of
                    // guessing from what we sent.
                    Some(Nak::Full) | Some(Nak::Invalid) => {
                        self.last_status_received = Some(stream.status_received_at());
                        let now_idle = stream.dac().status.playback == Playback::Idle;
                        if now_idle && !retried {
                            // The DAC dropped the payload because it is idle.
                            retried = true;
                            continue;
                        }
                        // Not idle: the ring was fuller than estimated. The
                        // admission check uses an upper bound on fullness, so
                        // this is only reachable when another client wrote to
                        // the shared ring, or after a second pass below. Some firmware keeps the points that
                        // fit, but we cannot tell how many, so resending the
                        // whole chunk later is the only safe option.
                        self.estimator
                            .record_status(stream.status_sent_at(), &stream.dac().status);
                        return Ok(WriteOutcome::WouldBlock);
                    }
                    // The DAC e-stopped after our last status. The write is
                    // refused but the stream is still in sync; the next call
                    // sees the cached e-stop and clears it.
                    Some(Nak::StopCondition) if estopped(&stream.dac().status) => {
                        let now = stream.status_sent_at();
                        self.last_status_received = Some(stream.status_received_at());
                        self.estimator.record_status(now, &stream.dac().status);
                        return Ok(WriteOutcome::WouldBlock);
                    }
                    // Other NAK-StopCondition, or an IO/timeout/protocol error:
                    // fatal. Retrying into a desynced stream makes the firmware
                    // close the connection.
                    _ => return Err(Error::backend(e)),
                },
            }

            // Firmware that drops data while idle may still ACK it. An idle
            // status right after the write means the chunk did not land.
            if stream.dac().status.playback == Playback::Idle && !retried {
                retried = true;
                continue;
            }

            // Recompute the begin decision from the status re-read after the
            // data submit (every response refreshes dac().status), so a
            // post-underflow restart isn't delayed a whole loop iteration.
            let status = &stream.dac().status;
            if status.playback == Playback::Prepared && status.buffer_fullness >= threshold {
                match stream.queue_commands().begin(0, point_rate).submit() {
                    Ok(()) => {}
                    Err(e) if matches!(nak_of(&e), Some(Nak::Invalid)) => {}
                    Err(e)
                        if matches!(nak_of(&e), Some(Nak::StopCondition))
                            && estopped(&stream.dac().status) => {}
                    Err(e) => return Err(Error::backend(e)),
                }
                // (b) Some firmware ACKs 'b' without starting (for example when
                // it went idle in the meantime). Trust the status, not the ACK.
                if stream.dac().status.playback == Playback::Idle && !retried {
                    log::debug!("Ether Dream begin did not start playback; re-preparing");
                    retried = true;
                    continue;
                }
            }
            break;
        }

        // The estimator anchors at the send time, the lower bound on fullness
        // (see `last_status_received`).
        let now = stream.status_sent_at();
        let status = &stream.dac().status;
        log::trace!(
            "ED write n={} rtt={:?} full={} playback={:?} flags={:?} count={}",
            points.len(),
            write_start.elapsed(),
            status.buffer_fullness,
            status.playback,
            status.playback_flags,
            status.point_count,
        );
        self.last_status_received = Some(stream.status_received_at());
        self.last_point_rate = point_rate;
        // The ACK's buffer_fullness already includes the points just sent
        // (verified against j4cDAC firmware: status is sampled after the ring
        // write), so record only the authoritative status. Also calling
        // record_send would double-count one chunk.
        self.estimator.record_status(now, &stream.dac().status);
        Ok(WriteOutcome::Written)
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }

    /// 80 % of the ring the DAC advertises (3119 of 3899 points on ED2, 1439
    /// of 1799 on ED1).
    fn target_buffer_ceiling(&self) -> Option<usize> {
        let capacity = match &self.stream {
            Some(stream) => effective_capacity(stream.dac().buffer_capacity) as usize,
            None => self.caps.max_points_per_chunk,
        };
        Some(capacity * TARGET_CEILING_PERCENT / 100)
    }
}

/// Share of the ring the adapter may aim to keep full.
///
/// The rest, at least 20 % (780 points on ED2, 360 on ED1), is headroom above
/// the target. Steady-state quantum writes then always fit, and the
/// estimator's slop cannot push writes into NAK territory: at 120 ms (3600 of
/// 3899 points) on ED2 the 299 points left over were not enough and the
/// stream livelocked. The reference libetherdream driver fills to 94 % but
/// writes chunks of at most 80 points; our chunks are larger, so we keep more
/// headroom.
const TARGET_CEILING_PERCENT: usize = 80;

/// Status-anchored estimate that drains at the rate the DAC reports.
///
/// A new rate reaches the DAC only with the next `'u'`, so after a rate
/// change the caller's rate is wrong until then. On hardware, a drop from
/// 20 kpps to 1 kpps read ~990 points as a second of cover while the DAC
/// emptied them in 50 ms. While playing, the status rate is the truth; when
/// it is unknown (prepared, idle) the caller's rate is used.
#[derive(Default)]
struct DacRateEstimator {
    inner: StatusDecayEstimator,
    /// Rate from the last status while playing, or 0.
    playing_rate: u32,
}

impl DacRateEstimator {
    /// Rebase on a status the DAC just sent.
    ///
    /// An idle DAC never plays what its ring holds: the fullness it reports
    /// is a leftover the next prepare discards, so it counts as empty.
    /// Freezing the estimate at that leftover would keep the adapter above
    /// its target, and it would never write (and so never prepare) again.
    ///
    /// A prepared stream is reported as draining. The adapter stops writing
    /// once the estimate reaches its target buffer, and a frozen estimate
    /// below the begin threshold would never start playback.
    fn record_status(&mut self, now: Instant, status: &Status) {
        let idle = status.playback == Playback::Idle;
        self.playing_rate = if status.playback == Playback::Playing {
            status.point_rate
        } else {
            0
        };
        self.inner.set_playing(!idle);
        let fullness = if idle { 0 } else { status.buffer_fullness };
        self.inner.record_status(now, fullness as u64);
    }
}

impl BufferEstimator for DacRateEstimator {
    fn estimated_fullness(&self, now: Instant, pps: u32) -> u64 {
        let rate = if self.playing_rate > 0 {
            self.playing_rate
        } else {
            pps
        };
        self.inner.estimated_fullness(now, rate)
    }
}

/// Whether a status shows the light engine stopped. Commands sent before the
/// client saw an e-stop are NAKed with '!', which is expected and not fatal.
fn estopped(status: &Status) -> bool {
    status.light_engine != LightEngine::Ready
}

/// The command byte a NAK answered, if `err` is a NAK.
fn nak_command(err: &CommunicationError) -> Option<u8> {
    match err {
        CommunicationError::Response(re) if matches!(re.kind, ResponseErrorKind::Nak(_)) => {
            Some(re.response.command)
        }
        _ => None,
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

fn effective_max_rate(advertised: u32) -> u32 {
    if advertised == 0 {
        SYNTHETIC_MAX_POINT_RATE
    } else {
        advertised
    }
}

fn effective_capacity(advertised: u16) -> u16 {
    if advertised == 0 {
        SYNTHETIC_BUFFER_CAPACITY
    } else {
        advertised
    }
}

/// The rate to send in `'b'` or `'u'`: never 0 (which hangs some firmware) and
/// never above the advertised maximum (which is NAKed after only the opcode
/// byte is consumed, desyncing the stream). A request of 0 means "a sensible
/// default", one sixteenth of the maximum.
fn clamp_rate(pps: u32, max_rate: u32) -> u32 {
    let rate = if pps == 0 { max_rate / 16 } else { pps };
    rate.clamp(1, max_rate.max(1))
}

/// Smallest begin threshold, in points. Matches the network FIFO adapter's
/// minimum write, so one write always reaches it at low rates.
const MIN_BEGIN_THRESHOLD: u32 = 16;

/// Number of points to buffer before issuing `begin`, derived from the point
/// rate: roughly 10 ms of cover, at least [`MIN_BEGIN_THRESHOLD`] and at most
/// 1700 points or half the advertised capacity, whichever is smaller, so the
/// threshold is always reachable.
fn begin_threshold(point_rate: u32, capacity: u16) -> u16 {
    let ten_ms = point_rate / 100;
    let ceiling = (capacity as u32 / 2).clamp(1, 1700);
    ten_ms.clamp(MIN_BEGIN_THRESHOLD.min(ceiling), ceiling) as u16
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
    anchor: Option<Instant>,
    point_rate: u32,
    playing: bool,
) -> u16 {
    let raw = raw.min(capacity);
    if !playing {
        return raw;
    }
    match anchor {
        Some(last_time) => {
            let consumed = last_time.elapsed().as_secs_f64() * point_rate as f64;
            raw.saturating_sub(consumed.min(u16::MAX as f64) as u16)
        }
        None => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::ether_dream::protocol::{DacResponse, DacStatus};
    use crate::protocols::ether_dream::sim::model::cmd;
    use crate::protocols::ether_dream::sim::{Faults, SimServer, SimServerConfig};

    // -- harness -------------------------------------------------------------

    fn start(profile: FirmwareProfile) -> SimServer {
        SimServer::loopback(profile).expect("start simulator")
    }

    fn start_with(profile: FirmwareProfile, faults: Faults) -> SimServer {
        SimServer::start(SimServerConfig::new(profile).with_faults(faults)).expect("start")
    }

    fn connect(srv: &SimServer) -> EtherDreamBackend {
        let mut b = EtherDreamBackend::with_address(srv.addr(), Some(srv.broadcast()));
        b.connect().expect("connect to simulator");
        b
    }

    /// Run commands on the DAC as if another client had sent them.
    fn feed(srv: &SimServer, bytes: &[u8]) {
        srv.with_model(|m| {
            let now = m.now();
            m.feed(now, bytes);
        });
    }

    fn inject_before(srv: &SimServer, opcode: u8, bytes: Vec<u8>) {
        srv.with_model(|m| m.inject_before(opcode, bytes));
    }

    fn points(n: usize) -> Vec<LaserPoint> {
        vec![LaserPoint::new(0.0, 0.0, 0, 0, 0, 0); n]
    }

    /// Points with distinct coordinates and colours, so a dropped, duplicated
    /// or reordered point cannot coincidentally match.
    fn distinct_points(n: usize) -> Vec<LaserPoint> {
        (0..n)
            .map(|i| {
                let t = i as f32 / n as f32;
                let i = i as u16;
                LaserPoint::new(t * 2.0 - 1.0, 1.0 - t, i * 7, i * 11, i * 13, i * 17)
            })
            .collect()
    }

    fn encoded(pts: &[LaserPoint]) -> Vec<DacPoint> {
        pts.iter().map(DacPoint::from).collect()
    }

    fn count(ops: &[u8], op: u8) -> usize {
        ops.iter().filter(|&&c| c == op).count()
    }

    fn each_profile(f: impl Fn(FirmwareProfile)) {
        for p in FirmwareProfile::all() {
            f(p);
        }
    }

    // -- pure helpers --------------------------------------------------------

    #[test]
    fn begin_threshold_is_rate_derived_and_capacity_bounded() {
        // ~10 ms of points.
        assert_eq!(begin_threshold(30_000, 1799), 300);
        assert_eq!(begin_threshold(100_000, 8191), 1000);
        // Floor matches the adapter's minimum write, so a 1 kpps stream with a
        // 50 ms target buffer (50 points) still begins. The old floor of 64
        // was unreachable there.
        assert_eq!(begin_threshold(1_000, 1799), 16);
        // Never more than half the advertised buffer.
        assert_eq!(begin_threshold(100_000, 1799), 899);
        assert_eq!(begin_threshold(100_000, 100), 50);
        assert_eq!(begin_threshold(1_000_000, 65_535), 1700);
        // Tiny buffers still get a reachable threshold.
        assert_eq!(begin_threshold(30_000, 2), 1);
        assert_eq!(begin_threshold(30_000, 0), 1);
    }

    #[test]
    fn clamp_rate_never_sends_zero_or_above_max() {
        // Regression: 'b' with rate 0 hung an ED2, and a rate above the max is
        // NAKed after one byte, which desyncs the stream.
        assert_eq!(clamp_rate(0, 100_000), 6_250);
        assert_eq!(clamp_rate(0, 8), 1);
        assert_eq!(clamp_rate(200_000, 100_000), 100_000);
        assert_eq!(clamp_rate(30_000, 100_000), 30_000);
        assert_eq!(clamp_rate(5, 0), 1);
    }

    #[test]
    fn decay_fullness_is_bounded_by_capacity() {
        // Regression: with no status time the raw value escaped the clamp.
        assert_eq!(decay_fullness(3871, 1799, None, 30_000, true), 1799);
        assert_eq!(decay_fullness(3871, 1799, None, 30_000, false), 1799);
        let long_ago = Instant::now() - Duration::from_secs(10);
        assert_eq!(decay_fullness(1000, 1799, Some(long_ago), 30_000, true), 0);
        assert_eq!(
            decay_fullness(1000, 1799, Some(long_ago), 30_000, false),
            1000
        );
    }

    #[test]
    fn caps_follow_the_advertised_buffer_and_rate() {
        each_profile(|p| {
            let srv = start(p.clone());
            let b = EtherDreamBackend::with_address(srv.addr(), Some(srv.broadcast()));
            assert_eq!(b.caps().max_points_per_chunk, p.buffer_capacity as usize);
            assert_eq!(b.caps().pps_max, p.max_point_rate);
        });
        let b = EtherDreamBackend::with_address("127.0.0.1:1".parse().unwrap(), None);
        assert_eq!(b.caps().max_points_per_chunk, 1799);
        assert_eq!(b.caps().pps_max, 100_000);
    }

    #[test]
    fn target_ceiling_is_80_percent_of_the_ring() {
        let expected = |p: &FirmwareProfile| match p.name {
            "ed1-j4cdac" => 1439,
            "ed2-r331" => 3119,
            _ => p.buffer_capacity as usize * 4 / 5,
        };
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = EtherDreamBackend::with_address(srv.addr(), Some(srv.broadcast()));
            // Not connected: from the broadcast capacity.
            assert_eq!(b.target_buffer_ceiling(), Some(expected(&p)), "{}", p.name);
            // Connected: from the capacity in the connection.
            b.connect().unwrap();
            assert_eq!(b.target_buffer_ceiling(), Some(expected(&p)), "{}", p.name);
        });
        // No broadcast and not connected: the conservative 1799-point default.
        let b = EtherDreamBackend::with_address("127.0.0.1:1".parse().unwrap(), None);
        assert_eq!(b.target_buffer_ceiling(), Some(1439));
    }

    // -- normal streaming ----------------------------------------------------

    #[test]
    fn write_prepares_begins_and_sends_every_point_once() {
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            let pts = distinct_points(400);
            assert_eq!(
                b.try_write_points(30_000, &pts).unwrap(),
                WriteOutcome::Written
            );
            let ops = srv.commands();
            assert_eq!(count(&ops, b'p'), 1, "{}: {ops:?}", p.name);
            assert_eq!(count(&ops, b'b'), 1, "{}: {ops:?}", p.name);
            assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
            assert_eq!(
                srv.with_model(|m| m.received_points().to_vec()),
                encoded(&pts),
                "{}",
                p.name
            );
        });
    }

    #[test]
    fn full_capacity_chunk_fits_an_empty_buffer() {
        // Regression: headroom used to be capacity - fullness - 1, so a chunk
        // of max_points_per_chunk (= capacity) could never be written.
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            let n = b.caps().max_points_per_chunk;
            assert_eq!(
                b.try_write_points(30_000, &points(n)).unwrap(),
                WriteOutcome::Written,
                "{}",
                p.name
            );
        });
    }

    #[test]
    fn ack_fullness_is_not_double_counted() {
        let srv = start(FirmwareProfile::ed2_r331());
        let mut b = connect(&srv);
        let sent = Instant::now();
        assert_eq!(
            b.try_write_points(30_000, &points(10)).unwrap(),
            WriteOutcome::Written
        );
        // Prepared with 10 points (below the begin threshold). The estimate is
        // the ACK fullness, not ACK + sent (20). It decays from when the 'd'
        // was sent, so allow the points the round trip may have taken.
        let now = Instant::now();
        let decay = (now.duration_since(sent).as_secs_f64() * 30_000.0).ceil() as u64;
        let est = b.estimator().estimated_fullness(now, 30_000);
        assert!(
            est <= 10 && est + decay >= 10,
            "estimate {est}, decay {decay}"
        );
    }

    #[test]
    fn partial_fit_blocks_without_writing() {
        each_profile(|p| {
            let srv = start(p.clone());
            feed(&srv, &cmd::prepare());
            feed(&srv, &cmd::blank_data(p.buffer_capacity as usize - 5));
            let mut b = connect(&srv);
            let before = srv.commands().len();
            let accepted = srv.with_model(|m| m.accepted_total());
            assert_eq!(
                b.try_write_points(30_000, &points(10)).unwrap(),
                WriteOutcome::WouldBlock
            );
            let ops = srv.commands();
            assert_eq!(count(&ops[before..], b'd'), 0, "{}: no data sent", p.name);
            assert_eq!(srv.with_model(|m| m.accepted_total()), accepted);
        });
    }

    #[test]
    fn prepared_ring_without_room_is_begun() {
        // Regression: a prepared stream does not drain, so a chunk that does
        // not fit blocked forever. Nothing refreshed the status or sent 'b',
        // and the adapter disconnected after its stall timeout. Here another
        // client filled the ring and never began.
        each_profile(|p| {
            let srv = start(p.clone());
            feed(&srv, &cmd::prepare());
            feed(&srv, &cmd::blank_data(p.buffer_capacity as usize - 5));
            let mut b = connect(&srv);
            assert_eq!(
                b.try_write_points(30_000, &points(10)).unwrap(),
                WriteOutcome::WouldBlock
            );
            assert_eq!(
                srv.status().playback_state,
                DacStatus::PLAYBACK_PLAYING,
                "{}",
                p.name
            );
            assert_eq!(srv.status().point_rate, 30_000, "{}", p.name);
            std::thread::sleep(Duration::from_millis(5));
            assert_eq!(
                b.try_write_points(30_000, &points(10)).unwrap(),
                WriteOutcome::Written,
                "{}",
                p.name
            );
        });
    }

    #[test]
    fn small_first_write_then_full_chunk_begins() {
        // Regression: our own first write stayed below the begin threshold,
        // then a chunk larger than the remaining room blocked forever.
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            let cap = b.caps().max_points_per_chunk;
            assert_eq!(
                b.try_write_points(30_000, &points(10)).unwrap(),
                WriteOutcome::Written
            );
            assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PREPARED);
            assert_eq!(
                b.try_write_points(30_000, &points(cap)).unwrap(),
                WriteOutcome::WouldBlock
            );
            // Ten points play out in a third of a millisecond, so check the
            // wire rather than a playback state that may already be idle.
            let ops = srv.commands();
            assert_eq!(count(&ops, b'b'), 1, "{}: {ops:?}", p.name);
            assert_eq!(*ops.last().unwrap(), b'b', "{}: {ops:?}", p.name);
        });
    }

    #[test]
    fn decay_uses_the_reported_rate_after_reconnect() {
        // Regression (fix 7): a fresh backend has no rate of its own yet. The
        // DAC is playing another client's stream at 1 kpps, so the fullness
        // must decay at the rate in the status, not at 0.
        let p = FirmwareProfile::ed2_r331();
        let srv = start(p.clone());
        feed(&srv, &cmd::prepare());
        feed(&srv, &cmd::blank_data(p.buffer_capacity as usize));
        feed(&srv, &cmd::begin(1_000));
        let mut b = connect(&srv);
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(
            b.try_write_points(1_000, &points(50)).unwrap(),
            WriteOutcome::Written
        );
    }

    #[test]
    fn estimate_drains_at_the_dac_rate_until_the_new_rate_is_sent() {
        // Regression (hardware, ED2 r331): after set_pps(20000 -> 1000) the
        // estimate decayed at the requested 1000 pps although the DAC kept
        // playing at 20000 until the next 'u'. The adapter read ~990 points
        // as a second of cover; the DAC drained them in 50 ms and underflowed.
        let srv = start(FirmwareProfile::ed2_r331());
        let mut b = connect(&srv);
        assert_eq!(
            b.try_write_points(20_000, &points(1_000)).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        assert_eq!(srv.status().point_rate, 20_000);
        let now = Instant::now();
        let start = b.estimator().estimated_fullness(now, 1_000);
        let later = b
            .estimator()
            .estimated_fullness(now + Duration::from_millis(20), 1_000);
        // 20 ms at 20 kpps is 400 points; at 1 kpps it would be 20.
        let drained = start - later;
        assert!((390..=410).contains(&drained), "drained {drained}");
    }

    #[test]
    fn reconnect_after_tcp_close_mid_stream_resumes() {
        // Hardware (ED2 r331): a TCP close while playing leaves the DAC idle
        // with the leftover fullness frozen and no underflow flag. A fresh
        // backend, as the reconnect path builds, must prepare and play again.
        each_profile(|p| {
            if !p.close_stops_playback {
                return;
            }
            let srv = start(p.clone());
            let mut first = connect(&srv);
            assert_eq!(
                first.try_write_points(1_000, &points(400)).unwrap(),
                WriteOutcome::Written
            );
            assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
            drop(first); // no 's': the connection just goes away
            let deadline = Instant::now() + Duration::from_secs(2);
            while srv.active_connections() > 0 {
                assert!(Instant::now() < deadline, "{}: close not seen", p.name);
                std::thread::sleep(Duration::from_millis(2));
            }
            let st = srv.status();
            assert_eq!(st.playback_state, DacStatus::PLAYBACK_IDLE, "{}", p.name);
            assert!(st.buffer_fullness > 0, "{}: stale fullness", p.name);

            let mut b = connect(&srv);
            assert_eq!(
                b.estimator().estimated_fullness(Instant::now(), 1_000),
                0,
                "{}: fresh backend starts empty",
                p.name
            );
            assert_eq!(
                b.try_write_points(1_000, &points(400)).unwrap(),
                WriteOutcome::Written,
                "{}",
                p.name
            );
            let st = srv.status();
            assert_eq!(st.playback_state, DacStatus::PLAYBACK_PLAYING, "{}", p.name);
            assert!(
                st.buffer_fullness <= 400,
                "{}: stale points dropped",
                p.name
            );
            assert_eq!(srv.with_model(|m| m.underflows()), 0, "{}", p.name);
        });
    }

    #[test]
    fn reconnect_while_still_playing_keeps_feeding_the_stream() {
        // j4cDAC keeps playing after a TCP close. A fresh backend sees a
        // playing stream in the hello and must append to it.
        let srv = start(FirmwareProfile::ed1_j4cdac());
        let mut first = connect(&srv);
        assert_eq!(
            first.try_write_points(1_000, &points(400)).unwrap(),
            WriteOutcome::Written
        );
        drop(first);
        let deadline = Instant::now() + Duration::from_secs(2);
        while srv.active_connections() > 0 {
            assert!(Instant::now() < deadline, "close not seen");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);

        let mut b = connect(&srv);
        assert_eq!(
            b.try_write_points(1_000, &points(400)).unwrap(),
            WriteOutcome::Written
        );
        let st = srv.status();
        assert_eq!(st.playback_state, DacStatus::PLAYBACK_PLAYING);
        assert!(st.buffer_fullness > 400, "appended, not re-prepared");
        assert_eq!(srv.with_model(|m| m.underflows()), 0);
    }

    #[test]
    fn idle_dac_counts_as_empty_despite_stale_fullness() {
        // Regression: after an e-stop the DAC reports idle with its leftover
        // fullness. Recording that froze the estimate above the adapter's
        // target, so it never wrote again and never cleared the e-stop.
        let srv = start(FirmwareProfile::ed2_r331());
        let mut b = connect(&srv);
        assert_eq!(
            b.try_write_points(1_000, &points(1_000)).unwrap(),
            WriteOutcome::Written
        );
        let now = srv.now();
        srv.with_model(|m| m.trigger_estop(now));
        assert_eq!(
            b.try_write_points(1_000, &points(100)).unwrap(),
            WriteOutcome::WouldBlock
        );
        let st = srv.status();
        assert_eq!(st.playback_state, DacStatus::PLAYBACK_IDLE);
        assert!(st.buffer_fullness > 900, "the DAC keeps a stale fullness");
        assert_eq!(b.estimator().estimated_fullness(Instant::now(), 1_000), 0);
    }

    #[test]
    fn estimate_drains_while_prepared() {
        // Regression (fix 8): a prepared stream below the begin threshold was
        // reported as not draining. A stream whose target buffer is below the
        // threshold then never asked for more points, so it never began.
        let srv = start(FirmwareProfile::ed2_r331());
        let mut b = connect(&srv);
        assert_eq!(
            b.try_write_points(30_000, &points(100)).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PREPARED);
        let later = Instant::now() + Duration::from_millis(2);
        assert!(b.estimator().estimated_fullness(later, 30_000) < 100);
    }

    #[test]
    fn prepare_nak_blocks_instead_of_failing() {
        // Regression (fix 9): another client prepares just before our 'p', so
        // ours is NAKed. That is a race, not a broken connection.
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            inject_before(&srv, b'p', cmd::prepare());
            assert_eq!(
                b.try_write_points(30_000, &points(10)).unwrap(),
                WriteOutcome::WouldBlock,
                "{}",
                p.name
            );
            assert_eq!(
                b.try_write_points(30_000, &points(10)).unwrap(),
                WriteOutcome::Written,
                "{}",
                p.name
            );
        });
    }

    #[test]
    fn rate_change_while_playing_updates_rate_and_sends_all_points() {
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            b.try_write_points(30_000, &points(400)).unwrap();
            assert_eq!(srv.status().point_rate, 30_000);

            let pts = distinct_points(10);
            assert_eq!(
                b.try_write_points(20_000, &pts).unwrap(),
                WriteOutcome::Written
            );
            assert_eq!(count(&srv.commands(), b'u'), 1);
            assert_eq!(srv.status().point_rate, 20_000);
            let got = srv.with_model(|m| m.received_points().to_vec());
            assert_eq!(&got[got.len() - 10..], encoded(&pts).as_slice());
        });
    }

    // -- bug (a): stale fullness after stop ----------------------------------

    #[test]
    fn stale_fullness_while_idle_does_not_block() {
        // Regression: a stopped DAC keeps reporting its old fullness until the
        // next prepare (the ED2 hello showed 1321 while idle). The headroom
        // check ran before prepare, so the backend blocked forever.
        each_profile(|p| {
            let srv = start(p.clone());
            feed(&srv, &cmd::prepare());
            feed(&srv, &cmd::blank_data(p.buffer_capacity as usize - 10));
            feed(&srv, b"s");
            let mut b = connect(&srv);
            let n = 100;
            assert_eq!(
                b.try_write_points(30_000, &points(n)).unwrap(),
                WriteOutcome::Written,
                "{}",
                p.name
            );
            assert_eq!(srv.status().buffer_fullness as usize, n);
        });
    }

    // -- bug (b): begin acknowledged but not started -------------------------

    #[test]
    fn begin_that_does_not_start_is_retried() {
        // Another client stops the DAC between our 'd' and our 'b'. ED2 ACKs a
        // 'b' while idle and ignores it. The backend must notice from the
        // status, re-prepare, resend the chunk and begin again.
        let mut nak_variant = FirmwareProfile::ed2_r331();
        nak_variant.begin_while_idle_acked = false;
        let mut profiles = FirmwareProfile::all();
        profiles.push(nak_variant);
        for p in profiles {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            inject_before(&srv, b'b', b"s".to_vec());
            let pts = distinct_points(400);
            assert_eq!(
                b.try_write_points(30_000, &pts).unwrap(),
                WriteOutcome::Written
            );
            let ops = srv.commands();
            assert_eq!(count(&ops, b'b'), 2, "{}: {ops:?}", p.name);
            assert_eq!(
                srv.status().playback_state,
                DacStatus::PLAYBACK_PLAYING,
                "{}",
                p.name
            );
            // The chunk sits in the ring exactly once.
            assert!(srv.status().buffer_fullness <= 400);
        }
    }

    #[test]
    fn data_acked_while_idle_is_resent() {
        // Firmware variant that ACKs and drops data while idle.
        let mut p = FirmwareProfile::ed2_r331();
        p.data_while_idle_nak_invalid = false;
        let srv = start(p);
        let mut b = connect(&srv);
        inject_before(&srv, b'd', b"s".to_vec());
        // First write: prepare (ours), [other client stops], data ACKed but
        // dropped -> re-prepare and resend.
        assert_eq!(
            b.try_write_points(30_000, &points(10)).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(srv.status().buffer_fullness, 10);
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PREPARED);
    }

    // -- bug (c): no reliance on NAK-Full ------------------------------------

    #[test]
    fn overfull_nak_invalid_resyncs_from_status() {
        // Another client filled the ring, so our view (empty) is wrong. Firmware
        // without NAK-Full answers NAK-Invalid; the backend must back off and
        // adopt the reported fullness.
        let mut profiles = FirmwareProfile::all();
        let mut full_variant = FirmwareProfile::ed1_j4cdac();
        full_variant.naks_full = true;
        profiles.push(full_variant);
        for p in profiles {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            b.try_write_points(30_000, &points(10)).unwrap(); // Prepared, 10 points
            let fill = p.ring_points as usize - 20;
            inject_before(&srv, b'd', cmd::blank_data(fill));
            let sent = Instant::now();
            let outcome = b.try_write_points(30_000, &points(50)).unwrap();
            assert_eq!(outcome, WriteOutcome::WouldBlock, "{}", p.name);
            // The status is anchored at the send time of the 'd', so allow
            // the decay since then (a prepared ring is estimated as draining).
            let now = Instant::now();
            let decay = (now.duration_since(sent).as_secs_f64() * 30_000.0).ceil() as usize;
            let est = b.estimator().estimated_fullness(now, 30_000);
            assert!(
                est as usize + decay >= fill,
                "{}: estimate {est} not re-synced",
                p.name
            );
        }
    }

    #[test]
    fn recovers_from_underflow_between_writes() {
        // The DAC underflowed after our last status. Its cached view says
        // Playing; the data NAK carries Idle+UNDERFLOW, so re-prepare and
        // resend the identical chunk.
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            b.try_write_points(30_000, &points(400)).unwrap();
            std::thread::sleep(Duration::from_millis(40)); // 400 pts = 13 ms
            assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_IDLE);
            let pts = distinct_points(500);
            assert_eq!(
                b.try_write_points(30_000, &pts).unwrap(),
                WriteOutcome::Written
            );
            let got = srv.with_model(|m| m.received_points().to_vec());
            let tail = encoded(&pts);
            assert_eq!(&got[got.len() - 500..], tail.as_slice(), "{}", p.name);
            assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        });
    }

    // -- light engine --------------------------------------------------------

    #[test]
    fn warmup_pings_rate_limited() {
        let mut p = FirmwareProfile::ed2_r331();
        p.estop_clear_immediate = false;
        let srv = start(p);
        feed(&srv, &[0x00]);
        feed(&srv, b"c"); // warmup for 100 ms
        let mut b = connect(&srv);
        let before = srv.commands().len();
        assert_eq!(
            b.try_write_points(30_000, &points(10)).unwrap(),
            WriteOutcome::WouldBlock
        );
        assert_eq!(
            b.try_write_points(30_000, &points(10)).unwrap(),
            WriteOutcome::WouldBlock
        );
        assert_eq!(&srv.commands()[before..], b"?", "exactly one ping");
    }

    #[test]
    fn warmup_ping_refreshes_the_status_time() {
        let mut p = FirmwareProfile::ed2_r331();
        p.estop_clear_immediate = false;
        let srv = start(p);
        feed(&srv, &[0x00]);
        feed(&srv, b"c");
        let mut b = connect(&srv);
        let connected_at = b.last_status_received.unwrap();
        std::thread::sleep(Duration::from_millis(2));
        b.try_write_points(30_000, &points(10)).unwrap();
        assert!(b.last_status_received.unwrap() > connected_at);
    }

    #[test]
    fn estop_stop_condition_blocks_and_rate_limits() {
        let srv = start(FirmwareProfile::ed2_r331());
        feed(&srv, &[0x00]);
        srv.with_model(|m| m.set_interlock(false));
        let mut b = connect(&srv);
        let before = srv.commands().len();
        assert_eq!(
            b.try_write_points(30_000, &points(10)).unwrap(),
            WriteOutcome::WouldBlock
        );
        assert_eq!(
            b.try_write_points(30_000, &points(10)).unwrap(),
            WriteOutcome::WouldBlock
        );
        assert_eq!(&srv.commands()[before..], b"c", "one clear per second");
    }

    #[test]
    fn estop_is_cleared_and_streaming_resumes() {
        each_profile(|p| {
            let srv = start(p.clone());
            feed(&srv, &cmd::prepare());
            feed(&srv, &cmd::blank_data(300));
            feed(&srv, &[0xff]);
            let mut b = connect(&srv);
            assert_eq!(
                b.try_write_points(30_000, &points(400)).unwrap(),
                WriteOutcome::Written,
                "{}",
                p.name
            );
            let st = srv.status();
            assert_eq!(st.light_engine_state, DacStatus::LIGHT_ENGINE_READY);
            assert_eq!(st.playback_state, DacStatus::PLAYBACK_PLAYING);
            assert_eq!(
                st.playback_flags & 0x4,
                0,
                "prepare cleared the e-stop flag"
            );
        });
    }

    /// Stream at 1000 pps until the DAC plays, then e-stop it just before the
    /// next `'d'`, as when the interlock trips between two writes. The
    /// backend only learns about the e-stop from the reply to that `'d'`.
    /// `reply` overrides the NAK code of the `'d'` and the retry `'p'`.
    fn estop_before_data(p: &FirmwareProfile, reply: Option<u8>) {
        let srv = start(p.clone());
        let mut b = connect(&srv);
        assert_eq!(
            b.try_write_points(1000, &points(400)).unwrap(),
            WriteOutcome::Written,
            "{}",
            p.name
        );
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);

        inject_before(&srv, b'd', vec![0x00]);
        if let Some(code) = reply {
            srv.with_model(|m| {
                m.override_reply_code(b'd', code);
            });
        }
        let before = srv.commands().len();
        let outcome = b.try_write_points(1000, &points(16));
        assert!(
            matches!(outcome, Ok(WriteOutcome::WouldBlock)),
            "{}: {outcome:?}",
            p.name
        );
        assert!(b.is_connected(), "{}", p.name);
        let st = srv.status();
        assert_eq!(
            st.light_engine_state,
            DacStatus::LIGHT_ENGINE_EMERGENCY_STOP,
            "{}",
            p.name
        );
        assert!(
            !srv.commands()[before..].contains(&b's'),
            "{}: {:?}",
            p.name,
            &srv.commands()[before..]
        );

        // The next write clears the e-stop and streams again.
        assert_eq!(
            b.try_write_points(1000, &points(400)).unwrap(),
            WriteOutcome::Written,
            "{}",
            p.name
        );
        let st = srv.status();
        assert_eq!(st.light_engine_state, DacStatus::LIGHT_ENGINE_READY);
        assert_eq!(st.playback_state, DacStatus::PLAYBACK_PLAYING, "{}", p.name);
    }

    #[test]
    fn estop_between_writes_is_not_fatal() {
        each_profile(|p| estop_before_data(&p, None));
    }

    #[test]
    fn estop_nak_on_data_is_not_fatal() {
        each_profile(|p| {
            estop_before_data(&p, Some(DacResponse::NAK_STOP_CONDITION));
        });
    }

    #[test]
    fn estop_before_begin_is_not_fatal() {
        for code in [DacResponse::NAK_INVALID, DacResponse::NAK_STOP_CONDITION] {
            each_profile(|p| {
                let srv = start(p.clone());
                let mut b = connect(&srv);
                inject_before(&srv, b'b', vec![0x00]);
                srv.with_model(|m| m.override_reply_code(b'b', code));
                let outcome = b.try_write_points(1000, &points(400));
                assert!(outcome.is_ok(), "{}: {outcome:?}", p.name);
                assert_eq!(
                    srv.status().light_engine_state,
                    DacStatus::LIGHT_ENGINE_EMERGENCY_STOP
                );
                let mut written = false;
                for _ in 0..3 {
                    if b.try_write_points(1000, &points(400)).unwrap() == WriteOutcome::Written {
                        written = true;
                        break;
                    }
                }
                assert!(written, "{}", p.name);
                assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
            });
        }
    }

    #[test]
    fn estop_before_prepare_is_not_fatal() {
        for code in [DacResponse::NAK_INVALID, DacResponse::NAK_STOP_CONDITION] {
            each_profile(|p| {
                let srv = start(p.clone());
                let mut b = connect(&srv);
                inject_before(&srv, b'p', vec![0x00]);
                srv.with_model(|m| m.override_reply_code(b'p', code));
                let outcome = b.try_write_points(1000, &points(400));
                assert!(
                    matches!(outcome, Ok(WriteOutcome::WouldBlock)),
                    "{}: {outcome:?}",
                    p.name
                );
                assert_eq!(
                    b.try_write_points(1000, &points(400)).unwrap(),
                    WriteOutcome::Written,
                    "{}",
                    p.name
                );
                assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
            });
        }
    }

    /// ED2 r331 hardware: rising from 1000 to 20000 pps with ~33 points left.
    /// The 965-point 'd' took ~6 ms to arrive; sent after the 'u', the old
    /// points ran out at the new rate first and the DAC underflowed.
    #[test]
    fn rising_rate_does_not_underflow_while_data_is_in_flight() {
        let f = Faults {
            data_delay: Duration::from_millis(6),
            ..Faults::default()
        };
        let srv = start_with(FirmwareProfile::ed2_r331(), f);
        let mut b = connect(&srv);
        assert_eq!(
            b.try_write_points(1_000, &points(60)).unwrap(),
            WriteOutcome::Written
        );
        let deadline = Instant::now() + Duration::from_secs(1);
        while srv.status().buffer_fullness > 33 {
            assert!(Instant::now() < deadline, "never drained");
            std::thread::sleep(Duration::from_micros(200));
        }
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);

        assert_eq!(
            b.try_write_points(20_000, &points(965)).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(srv.with_model(|m| m.underflows()), 0);
        let st = srv.status();
        assert_eq!(st.playback_state, DacStatus::PLAYBACK_PLAYING);
        assert_eq!(st.point_rate, 20_000);
    }

    /// ED2 r331 hardware: a reply delayed 27-35 ms made the estimate ~1000
    /// points high at 30 kpps. The DAC samples its status when it handles
    /// the command, so the estimate must decay from the send time.
    #[test]
    fn reply_latency_does_not_inflate_the_estimate() {
        let f = Faults {
            reply_delay: Duration::from_millis(30),
            ..Faults::default()
        };
        let srv = start_with(FirmwareProfile::ed2_r331(), f);
        let mut b = connect(&srv);
        assert_eq!(
            b.try_write_points(10_000, &points(1_000)).unwrap(),
            WriteOutcome::Written
        );
        let (now, actual) = srv.with_model(|m| (Instant::now(), m.status().buffer_fullness));
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        let est = b.estimator().estimated_fullness(now, 10_000);
        // 30 ms of reply delay is 300 points at 10 kpps.
        assert!(
            est <= u64::from(actual) + 30,
            "estimate {est} vs actual {actual}"
        );
    }

    /// ED2 r331 hardware (120 ms target at 30 kpps): a 20 ms reply made the
    /// send-time anchor under-read fullness by ~600 points. The backend sent
    /// 1214 points into ~1070 points of room; ED2 kept what fit, answered
    /// NAK-Invalid, and the whole chunk was resent every 38 ms. Admission
    /// must decay from when the reply arrived, an upper bound on fullness.
    #[test]
    fn slow_reply_does_not_admit_an_overfull_chunk() {
        const PPS: u32 = 30_000;
        let p = FirmwareProfile::ed2_r331();
        let srv = start(p.clone());
        let mut b = connect(&srv);
        assert_eq!(
            b.try_write_points(PPS, &points(3_000)).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);

        // One slow data reply: the DAC handles the 'd' 20 ms after it was
        // sent, so the status it reports is 600 points fresher than the send
        // time suggests.
        srv.set_faults(Faults {
            data_delay: Duration::from_millis(20),
            ..Faults::default()
        });
        assert_eq!(
            b.try_write_points(PPS, &points(100)).unwrap(),
            WriteOutcome::Written
        );
        srv.set_faults(Faults::default());

        // A chunk 300 points (10 ms) larger than the true room. The send-time
        // anchor reads ~600 points of extra room and would admit it.
        let room = p.buffer_capacity as usize - srv.status().buffer_fullness as usize;
        let chunk = room + 300;
        let (d_before, naks_before) = (
            count(&srv.commands(), b'd'),
            srv.with_model(|m| m.nak_replies()),
        );
        assert_eq!(
            b.try_write_points(PPS, &points(chunk)).unwrap(),
            WriteOutcome::WouldBlock,
            "room {room}, chunk {chunk}"
        );
        assert_eq!(count(&srv.commands(), b'd'), d_before, "nothing sent");

        // Once the ring has drained enough, the same chunk goes in whole.
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match b.try_write_points(PPS, &points(chunk)).unwrap() {
                WriteOutcome::Written => break,
                WriteOutcome::WouldBlock => {
                    assert!(Instant::now() < deadline, "never admitted");
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        assert_eq!(count(&srv.commands(), b'd'), d_before + 1);
        srv.with_model(|m| {
            assert_eq!(m.overfull_writes(), 0, "no partial write");
            assert_eq!(m.nak_replies(), naks_before, "no NAK");
            assert_eq!(m.underflows(), 0);
        });
    }

    // -- version query (d) ---------------------------------------------------

    #[test]
    fn connect_reads_the_firmware_version() {
        each_profile(|p| {
            let srv = start(p.clone());
            let b = connect(&srv);
            assert_eq!(b.firmware_version(), p.version_string, "{}", p.name);
            assert_eq!(srv.total_connections(), 1);
        });
    }

    #[test]
    fn firmware_that_resets_on_v_is_reconnected_once() {
        let mut p = FirmwareProfile::ed2_r331();
        p.version_string = None; // 'v' is unknown -> TCP reset
        let srv = start(p);
        let mut b = connect(&srv);
        assert_eq!(b.firmware_version(), None);
        assert_eq!(srv.total_connections(), 2, "one reconnect after the reset");
        assert_eq!(
            b.try_write_points(30_000, &points(10)).unwrap(),
            WriteOutcome::Written
        );
        b.disconnect().unwrap();
        b.connect().unwrap();
        assert_eq!(srv.total_connections(), 3, "'v' is not retried");
    }

    #[test]
    fn firmware_that_naks_v_keeps_the_connection() {
        let mut p = FirmwareProfile::ed2_r331();
        p.version_string = None;
        p.unknown_command_resets_tcp = false;
        let srv = start(p);
        let mut b = connect(&srv);
        assert_eq!(b.firmware_version(), None);
        assert_eq!(srv.total_connections(), 1);
        assert_eq!(
            b.try_write_points(30_000, &points(10)).unwrap(),
            WriteOutcome::Written
        );
    }

    // -- connect by address (e) ----------------------------------------------

    #[test]
    fn connect_by_address_without_broadcast() {
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = EtherDreamBackend::with_address(srv.addr(), None);
            b.connect().unwrap();
            // Without a broadcast the conservative 1799-point default applies.
            assert_eq!(b.caps().max_points_per_chunk, 1799);
            assert_eq!(
                b.try_write_points(30_000, &points(400)).unwrap(),
                WriteOutcome::Written
            );
            assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        });
    }

    // -- transport faults ----------------------------------------------------

    #[test]
    fn io_error_on_send_is_fatal_not_retried() {
        // Replies: 0 = 'v', 1 = 'p', 2 = 'd' (dropped -> connection closed).
        let srv = start_with(
            FirmwareProfile::ed2_r331(),
            Faults {
                drop_after_replies: Some(2),
                ..Faults::default()
            },
        );
        let mut b = connect(&srv);
        assert!(b.try_write_points(30_000, &points(10)).is_err());
        assert_eq!(count(&srv.commands(), b'd'), 1, "no retry on IO error");
    }

    #[test]
    fn truncated_reply_is_fatal() {
        let srv = start_with(
            FirmwareProfile::ed2_r331(),
            Faults {
                truncate_reply: Some(1),
                ..Faults::default()
            },
        );
        let mut b = connect(&srv);
        assert!(b.try_write_points(30_000, &points(10)).is_err());
    }

    #[test]
    fn survives_duplicate_and_delayed_replies() {
        for faults in [
            Faults {
                duplicate_replies: true,
                ..Faults::default()
            },
            Faults {
                reply_delay: Duration::from_millis(5),
                ..Faults::default()
            },
        ] {
            let srv = start_with(FirmwareProfile::ed2_r331(), faults.clone());
            let mut b = connect(&srv);
            let mut written = 0;
            for _ in 0..20 {
                match b.try_write_points(30_000, &points(100)) {
                    Ok(WriteOutcome::Written) => written += 1,
                    Ok(WriteOutcome::WouldBlock) => {}
                    Err(e) => panic!("{faults:?}: {e}"),
                }
            }
            assert!(written > 0, "{faults:?}");
        }
    }

    #[test]
    fn not_connected_is_disconnected_error() {
        let mut b = EtherDreamBackend::with_address("127.0.0.1:1".parse().unwrap(), None);
        let err = b.try_write_points(30_000, &points(1)).unwrap_err();
        assert!(err.is_disconnected());
    }

    #[test]
    fn stop_while_idle_is_not_an_error() {
        each_profile(|p| {
            let srv = start(p.clone());
            let mut b = connect(&srv);
            b.stop().unwrap();
        });
    }
}
