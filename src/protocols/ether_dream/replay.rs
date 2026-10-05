//! Hardware-informed replay tests for the Ether Dream TCP path.
//!
//! The fixtures under `tests/fixtures/ether_dream/` come from probing a real
//! Ether Dream 2 (firmware build `r331-ed4bef5`) with only blanked points.
//! There are two sets:
//!
//! * `ed2_r331_hw_*.jsonl`: raw request headers and reply bytes recorded by
//!   `examples/ether_dream_capture.rs`, plus one UDP broadcast.
//! * `ed2_r331_{identity,lifecycle,capacity}.jsonl`: an earlier probe.
//!   `identity` holds raw bytes, the other two were transcribed from the
//!   decoded probe log, so their responses are rebuilt from the logged fields.
//!
//! The fixture README lists what was and was not captured byte for byte.
//!
//! The tests do two things:
//!
//! * Parse the real hello and version bytes with the production decoders and
//!   identify the firmware profile from them.
//! * Replay every command against [`EtherDreamModel`] with the ED2 profile, at
//!   the time it was sent, and compare each reply with what the hardware
//!   answered. This keeps the simulator honest: if it drifts from the
//!   hardware, the backend tests that rely on it stop meaning anything.

use std::time::Duration;

use serde::Deserialize;

use crate::protocols::ether_dream::dac;
use crate::protocols::ether_dream::profile::FirmwareProfile;
use crate::protocols::ether_dream::protocol::{
    command::Version, DacBroadcast, DacResponse, DacStatus, ReadBytes, SizeBytes,
};
use crate::protocols::ether_dream::sim::{EtherDreamModel, Reply};

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/ether_dream/",
            $name
        ))
    };
}

const IDENTITY_JSONL: &str = fixture!("ed2_r331_identity.jsonl");
const LIFECYCLE_JSONL: &str = fixture!("ed2_r331_lifecycle.jsonl");
const CAPACITY_JSONL: &str = fixture!("ed2_r331_capacity.jsonl");
const HW_IDENTITY_JSONL: &str = fixture!("ed2_r331_hw_identity.jsonl");
const HW_LIFECYCLE_JSONL: &str = fixture!("ed2_r331_hw_lifecycle.jsonl");
const HW_CAPACITY_JSONL: &str = fixture!("ed2_r331_hw_capacity.jsonl");
const HW_DRAIN_JSONL: &str = fixture!("ed2_r331_hw_drain.jsonl");
const HW_BROADCAST_JSONL: &str = fixture!("ed2_r331_hw_broadcast.jsonl");
const HWCHECK_DISCOVERY_JSONL: &str = fixture!("ed2_r331_hwcheck_broadcast_discovery.jsonl");
const HWCHECK_STALE_JSONL: &str = fixture!("ed2_r331_hwcheck_stale_full_reconnect.jsonl");
const HWCHECK_UNDERFLOW_JSONL: &str = fixture!("ed2_r331_hwcheck_underflow_recovery.jsonl");
const HWCHECK_ESTOP_JSONL: &str = fixture!("ed2_r331_hwcheck_estop_recovery.jsonl");
const HWCHECK_RATE_JSONL: &str = fixture!("ed2_r331_hwcheck_rate_change.jsonl");
const HWCHECK_FULL_CHUNK_JSONL: &str = fixture!("ed2_r331_hwcheck_full_capacity_chunk.jsonl");
const HWCHECK_RATE_BEFORE_FIX_JSONL: &str =
    fixture!("ed2_r331_hwcheck_rate_change_before_fix.jsonl");
const HWCHECK_RATE_UP_BEFORE_FIX_JSONL: &str =
    fixture!("ed2_r331_hwcheck_rate_up_before_fix.jsonl");
const HWCHECK_SESSION_STOP_JSONL: &str = fixture!("ed2_r331_hwcheck_session_stop.jsonl");
const HWCHECK_PARTIAL_ROOM_JSONL: &str = fixture!("ed2_r331_hwcheck_partial_room.jsonl");
const HWCHECK_CLAMP_JSONL: &str = fixture!("ed2_r331_hwcheck_clamp_guard.jsonl");
const HWCHECK_STEADY_JSONL: &str = fixture!("ed2_r331_hwcheck_steady_stream_60s.jsonl");
const HWCHECK_STEADY_UNDERFLOW_JSONL: &str =
    fixture!("ed2_r331_hwcheck_steady_underflow_excerpt.jsonl");

/// Playback flags the DAC reported while idle in the earlier probe.
const FLAGS_FIRST_BOOT: u16 = 0x5f31;
/// Playback flags the same DAC reported while idle after a power cycle, in
/// the `hw` captures. Only the undocumented upper byte differs.
const FLAGS_SECOND_BOOT: u16 = 0x7731;

/// One captured TCP operation. `dt_us` is ignored.
#[derive(Debug, Deserialize)]
struct TraceEvent {
    seq: u32,
    /// Earlier probe: `connect` for hellos, `pre` before the first prepare,
    /// then `stream` or `reconnect`. `hw` captures: the scenario name.
    phase: String,
    op: String,
    /// Command bytes sent, hex. Blanked point payloads are elided: pad with
    /// zero bytes up to `n`.
    req: String,
    /// Reply bytes, hex. Empty when the DAC reset or did not answer.
    resp: String,
    /// `ok`, `reset` or `timeout`.
    status: String,
    /// Total bytes sent on the wire for this command.
    n: usize,
    /// Time since the probe connected, in microseconds, taken when the reply
    /// arrived.
    t_us: u64,
    /// Time from the start of the send to the reply, in microseconds. Absent
    /// in the transcribed fixtures.
    #[serde(default)]
    dur_us: Option<u64>,
    note: String,
}

impl TraceEvent {
    fn request(&self) -> Vec<u8> {
        let mut req = hex(&self.req);
        assert!(req.len() <= self.n, "seq {}: req longer than n", self.seq);
        req.resize(self.n, 0);
        req
    }

    fn response(&self) -> DacResponse {
        let raw = hex(&self.resp);
        assert_eq!(raw.len(), DacResponse::SIZE_BYTES, "seq {}", self.seq);
        (&raw[..])
            .read_bytes::<DacResponse>()
            .expect("fixture response decodes")
    }
}

fn hex(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2), "odd-length hex");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn load(jsonl: &str) -> Vec<TraceEvent> {
    jsonl
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line is a valid event"))
        .collect()
}

fn find<'a>(events: &'a [TraceEvent], op: &str) -> &'a TraceEvent {
    events
        .iter()
        .find(|e| e.op == op)
        .unwrap_or_else(|| panic!("no {op} event"))
}

// --- Raw captured bytes -------------------------------------------------------

#[test]
fn captured_hello_parses_as_idle_ready_status() {
    let events = load(IDENTITY_JSONL);
    let hello = find(&events, "hello").response();
    assert_eq!(hello.response, DacResponse::ACK);
    assert_eq!(hello.command, b'?', "the hello is an ACK for a ping");

    let st = dac::Status::from_protocol(&hello.dac_status).expect("valid status");
    assert_eq!(st.light_engine, dac::LightEngine::Ready);
    assert_eq!(st.playback, dac::Playback::Idle);
    // An idle DAC still reports the fullness of the stream it last played.
    assert_eq!(st.buffer_fullness, 1321);
    assert_eq!(hello.dac_status.playback_flags, 0x5f31);
}

#[test]
fn captured_version_identifies_the_ed2_profile() {
    let events = load(IDENTITY_JSONL);
    let raw = hex(&find(&events, "version").resp);
    assert_eq!(raw.len(), Version::RESPONSE_SIZE_BYTES);
    // The reply is the bare string: no ACK byte, no command echo.
    assert_ne!(raw[0], DacResponse::ACK);

    let version = Version::decode_response(&raw);
    assert_eq!(version, "r331-ed4bef5");
    let profile = FirmwareProfile::identify(&version).expect("known firmware");
    assert_eq!(profile.name, FirmwareProfile::ed2_r331().name);
    assert_eq!(profile.version_string, Some("r331-ed4bef5"));
}

#[test]
fn ed2_profile_constants_match_the_capture() {
    let p = FirmwareProfile::ed2_r331();
    let idle = find(&load(IDENTITY_JSONL), "hello").response();
    assert_eq!(p.always_set_playback_flags, idle.dac_status.playback_flags);
    assert!(!p.naks_full, "ED2 never answered NAK-Full");

    // The broadcast advertises the ring size, and the ring holds exactly that.
    let bc = captured_broadcast();
    assert_eq!(bc.buffer_capacity, p.buffer_capacity);
    assert_eq!(p.ring_points, p.buffer_capacity);
    let fullness = |jsonl| {
        load(jsonl)
            .iter()
            .filter(|e| e.status == "ok" && e.op != "version")
            .map(|e| e.response().dac_status.buffer_fullness)
            .max()
            .unwrap()
    };
    assert_eq!(fullness(HW_CAPACITY_JSONL), p.ring_points);
    // The earlier probe stopped filling at 3871, below the ring size.
    assert!(fullness(CAPACITY_JSONL) <= p.ring_points);
}

#[test]
fn captured_broadcast_matches_the_ed2_profile() {
    let p = FirmwareProfile::ed2_r331();
    let bc = captured_broadcast();
    assert_eq!(bc.hw_revision, p.hw_revision);
    assert_eq!(bc.sw_revision, p.sw_revision);
    assert_eq!(bc.buffer_capacity, p.buffer_capacity);
    assert_eq!(bc.max_point_rate, p.max_point_rate);
    // Heard after the drain capture ended in an underflow: idle, with the
    // sticky underflow bit still set.
    let st = &bc.dac_status;
    assert_eq!(st.playback_state, DacStatus::PLAYBACK_IDLE);
    assert_eq!(st.buffer_fullness, 0);
    assert_eq!(st.playback_flags, FLAGS_SECOND_BOOT | 0x2);
}

#[test]
fn hw_capture_hello_and_version_parse() {
    let events = load(HW_IDENTITY_JSONL);
    let hello = find(&events, "hello").response();
    assert_eq!(hello.response, DacResponse::ACK);
    let st = dac::Status::from_protocol(&hello.dac_status).expect("valid status");
    assert_eq!(st.light_engine, dac::LightEngine::Ready);
    assert_eq!(st.playback, dac::Playback::Idle);
    // Stale fullness left behind by a stream another client played.
    assert_eq!(st.buffer_fullness, 1400);
    assert_eq!(hello.dac_status.playback_flags, FLAGS_SECOND_BOOT);

    let version = Version::decode_response(&hex(&find(&events, "version").resp));
    assert_eq!(version, "r331-ed4bef5");
    // The connection stays in sync after the raw 32-byte reply.
    let after = events.last().unwrap();
    assert_eq!(after.op, "ping");
    assert_eq!(after.response().command, b'?');
}

fn captured_broadcast() -> DacBroadcast {
    let raw = hex(&find(&load(HW_BROADCAST_JSONL), "broadcast").resp);
    assert_eq!(raw.len(), DacBroadcast::SIZE_BYTES);
    (&raw[..])
        .read_bytes::<DacBroadcast>()
        .expect("fixture broadcast decodes")
}

// --- Model conformance --------------------------------------------------------

/// Replay `events` against a fresh ED2 model and compare every reply.
///
/// `flags` is the idle playback-flag word of the boot the capture came from,
/// since its upper byte changes across power cycles.
///
/// Hellos and events before the first prepare are skipped, because they
/// reflect DAC state left over from earlier sessions. Buffer fullness, point rate and
/// point count of a playing stream depend on exactly when the DAC sampled
/// them, and the log only has 0.1 ms resolution, so those get a tolerance of
/// about 1.5 ms of playback. A stop freezes the fullness at that moment, so
/// the stale idle fullness after it keeps the same tolerance until the next
/// prepare.
fn replay(name: &str, events: &[TraceEvent], flags: u16) -> usize {
    let mut profile = FirmwareProfile::ed2_r331();
    profile.always_set_playback_flags = flags;
    let mut model = EtherDreamModel::new(profile);
    let mut compared = 0;
    let mut stale_tol = 0;
    // A rate update starts a new segment at the model's idea of "now", so
    // any fullness error the model had at the old rate is frozen in from
    // then on. Carry the old rate's tolerance across the switch until the
    // next prepare.
    let mut carried_tol = 0;
    let mut prev_end = 0;
    let mut prepared = false;
    for e in events {
        prepared |= e.op == "prepare";
        // `blocked` and `synthesized` events (hwcheck traces) never reached
        // the DAC: the capture proxy refused or answered them itself.
        if !prepared
            || e.op == "hello"
            || e.phase == "pre"
            || matches!(e.status.as_str(), "timeout" | "blocked" | "synthesized")
        {
            continue;
        }
        // A large data command takes over a millisecond to send, and the DAC
        // takes in its points as they arrive. Feed each command at the middle
        // of its send so the model does not underflow while the hardware was
        // still receiving points.
        // A command pipelined behind another (the backend sends data then
        // 'u' in one write) is processed only after the one before it, so
        // never feed it before the previous reply arrived.
        let now = Duration::from_micros((e.t_us - e.dur_us.unwrap_or(0) / 2).max(prev_end));
        prev_end = e.t_us;
        if e.note.contains("UNEXPLAINED") {
            // Keep the model in step, but do not judge the reply.
            model.feed(now, &e.request());
            continue;
        }
        let replies = model.feed(now, &e.request());
        let ctx = format!("{name} seq {} ({} {:?})", e.seq, e.op, e.note);

        if e.op == "version" {
            // The raw 32-byte string, not a status frame.
            let want = hex(&e.resp);
            assert!(
                matches!(&replies[..], [Reply::Version(got)] if got[..] == want[..]),
                "{ctx}: {replies:?}"
            );
            compared += 1;
            continue;
        }

        if e.status == "reset" {
            assert_eq!(replies, vec![Reply::Reset], "{ctx}");
            compared += 1;
            continue;
        }

        assert_eq!(replies.len(), 1, "{ctx}: {replies:?}");
        let Reply::Response(got) = replies[0] else {
            panic!("{ctx}: expected a response, got {:?}", replies[0]);
        };
        let want = e.response();
        assert_eq!(got.response, want.response, "{ctx}: response code");
        assert_eq!(got.command, want.command, "{ctx}: command echo");
        if e.op == "update" {
            carried_tol = carried_tol.max(stale_tol);
        }
        let tol = playback_tolerance(&want.dac_status)
            .max(stale_tol)
            .max(carried_tol);
        assert_status(&ctx, &got.dac_status, &want.dac_status, tol);
        if want.dac_status.playback_state == DacStatus::PLAYBACK_PLAYING {
            stale_tol = playback_tolerance(&want.dac_status);
        } else if e.op == "prepare" {
            stale_tol = 0;
            carried_tol = 0;
        }
        compared += 1;
    }
    compared
}

/// Points played in about 1.5 ms, plus a little slack.
fn playback_tolerance(st: &DacStatus) -> i64 {
    if st.playback_state == DacStatus::PLAYBACK_PLAYING {
        (st.point_rate as u64 * 3 / 2000) as i64 + 2
    } else {
        0
    }
}

fn assert_status(ctx: &str, got: &DacStatus, want: &DacStatus, tol: i64) {
    assert_eq!(
        got.light_engine_state, want.light_engine_state,
        "{ctx}: light engine"
    );
    assert_eq!(got.playback_state, want.playback_state, "{ctx}: playback");
    assert_eq!(got.source, want.source, "{ctx}: source");
    assert_eq!(
        got.light_engine_flags, want.light_engine_flags,
        "{ctx}: le flags"
    );
    assert_eq!(
        got.playback_flags, want.playback_flags,
        "{ctx}: playback flags"
    );
    assert_eq!(got.source_flags, want.source_flags, "{ctx}: source flags");
    assert_eq!(got.point_rate, want.point_rate, "{ctx}: point rate");

    let close = |a: u32, b: u32| (a as i64 - b as i64).abs() <= tol;
    assert!(
        close(got.buffer_fullness as u32, want.buffer_fullness as u32),
        "{ctx}: fullness model {} vs hardware {} (tolerance {tol})",
        got.buffer_fullness,
        want.buffer_fullness
    );
    assert!(
        close(got.point_count, want.point_count),
        "{ctx}: point count model {} vs hardware {} (tolerance {tol})",
        got.point_count,
        want.point_count
    );
}

#[test]
fn model_replays_the_ed2_lifecycle_capture() {
    // Prepare, fill, begin, drain to underflow, data while idle, begin while
    // idle, update, queued rate, stop twice, e-stop, clear, unknown command.
    let compared = replay("lifecycle", &load(LIFECYCLE_JSONL), FLAGS_FIRST_BOOT);
    assert!(compared >= 30, "only {compared} events compared");
}

#[test]
fn model_replays_the_ed2_capacity_capture() {
    // Fill past the advertised capacity, slow playback, queued rate without a
    // control bit, underflow detected through a data NAK, refill while
    // playing, reconnect, 0xff e-stop, split data command.
    let compared = replay("capacity", &load(CAPACITY_JSONL), FLAGS_FIRST_BOOT);
    assert!(compared >= 40, "only {compared} events compared");
}

#[test]
fn model_replays_the_hw_lifecycle_capture() {
    // Drain to underflow, data and begin while idle, update, queued rate,
    // stop twice, data while idle, e-stop, clear, prepare.
    let compared = replay("hw lifecycle", &load(HW_LIFECYCLE_JSONL), FLAGS_SECOND_BOOT);
    assert!(compared >= 24, "only {compared} events compared");
}

#[test]
fn model_replays_the_hw_capacity_capture() {
    // Fill to exactly the advertised capacity, NAK on the next write, then
    // play at 1 kpps.
    let compared = replay("hw capacity", &load(HW_CAPACITY_JSONL), FLAGS_SECOND_BOOT);
    assert!(compared >= 8, "only {compared} events compared");
}

#[test]
fn model_replays_the_hw_drain_capture() {
    // A 30 kpps drain sampled every 3 ms down to underflow, then a refill
    // loop that falls behind and hits underflow mid-stream.
    let compared = replay("hw drain", &load(HW_DRAIN_JSONL), FLAGS_SECOND_BOOT);
    assert!(compared >= 44, "only {compared} events compared");
}

#[test]
fn transcribed_requests_match_the_production_encoders() {
    use crate::protocols::ether_dream::sim::model::cmd;
    let events = load(LIFECYCLE_JSONL);
    let begin = events.iter().find(|e| e.op == "begin").unwrap();
    assert_eq!(begin.request(), cmd::begin(30_000));
    let data = events.iter().find(|e| e.op == "data").unwrap();
    assert_eq!(data.request(), cmd::blank_data(500));
    let update = events.iter().find(|e| e.op == "update").unwrap();
    assert_eq!(update.request(), cmd::update(20_000));
    let rate = events.iter().find(|e| e.op == "point_rate").unwrap();
    assert_eq!(rate.request(), cmd::queue_rate(25_000));
}

// --- Backend checks on hardware (examples/ether_dream_hwcheck.rs) -------------
//
// These traces were recorded by a proxy between the production backend and
// the DAC, so the requests are exactly what `EtherDreamBackend` sent.

/// Events of TCP connection `n` (1-based), counted by hellos.
fn connection(events: &[TraceEvent], n: usize) -> Vec<&TraceEvent> {
    let mut conn = 0;
    events
        .iter()
        .filter(|e| {
            conn += usize::from(e.op == "hello");
            conn == n
        })
        .collect()
}

#[test]
fn hwcheck_broadcast_matches_the_ed2_profile() {
    let p = FirmwareProfile::ed2_r331();
    let raw = hex(&find(&load(HWCHECK_DISCOVERY_JSONL), "broadcast").resp);
    let bc = (&raw[..]).read_bytes::<DacBroadcast>().unwrap();
    assert_eq!(bc.buffer_capacity, p.buffer_capacity);
    assert_eq!(bc.max_point_rate, p.max_point_rate);
    assert_eq!(bc.dac_status.playback_state, DacStatus::PLAYBACK_IDLE);
    // The device half of the MAC is redacted.
    assert_eq!(bc.mac_address[3..], [0, 0, 0]);
}

#[test]
fn hwcheck_stale_full_ring_is_reprepared_on_connect() {
    let events = load(HWCHECK_STALE_JSONL);
    // A raw client left the ring full (3899 points) and stopped.
    let setup = connection(&events, 1);
    let stop = setup.iter().find(|e| e.op == "stop").unwrap().response();
    assert_eq!(stop.dac_status.playback_state, DacStatus::PLAYBACK_IDLE);
    assert_eq!(stop.dac_status.buffer_fullness, 3899);

    // The backend connects, sees the stale fullness in the hello, and
    // prepares before anything else instead of waiting for room.
    let backend = connection(&events, 2);
    let hello = backend[0].response();
    assert_eq!(hello.dac_status.buffer_fullness, 3899);
    let ops: Vec<&str> = backend.iter().map(|e| e.op.as_str()).take(5).collect();
    assert_eq!(ops, ["hello", "version", "prepare", "data", "begin"]);
    let begin = backend.iter().find(|e| e.op == "begin").unwrap();
    assert_eq!(
        begin.response().dac_status.playback_state,
        DacStatus::PLAYBACK_PLAYING
    );
    assert!(
        begin.t_us - backend[0].t_us < 1_000_000,
        "Playing within 1 s"
    );

    let compared = replay("hwcheck stale", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 100, "only {compared} events compared");
}

#[test]
fn hwcheck_tcp_close_stops_playback_and_keeps_fullness() {
    // The stream ended without a stop command; the backend just closed the
    // socket while playing at 1 kpps with about 48 points queued. The next
    // hello shows the DAC idle with a nonzero fullness and no underflow bit,
    // so the firmware stopped playback when the connection closed rather
    // than playing the ring out.
    let events = load(HWCHECK_STALE_JSONL);
    let backend = connection(&events, 2);
    let last = backend.last().unwrap().response().dac_status;
    assert_eq!(last.playback_state, DacStatus::PLAYBACK_PLAYING);
    let hello = connection(&events, 3)[0].response().dac_status;
    assert_eq!(hello.playback_state, DacStatus::PLAYBACK_IDLE);
    assert!(hello.buffer_fullness > 0 && hello.buffer_fullness < last.buffer_fullness);
    assert_eq!(hello.playback_flags & 0x2, 0, "no underflow");
}

fn is_idle(e: &TraceEvent) -> bool {
    e.response().dac_status.playback_state == DacStatus::PLAYBACK_IDLE
}

#[test]
fn hwcheck_underflow_is_recovered_by_reprepare() {
    let events = load(HWCHECK_UNDERFLOW_JSONL);
    let stream = connection(&events, 1);
    // The host withheld data for 400 ms; the first write after that found
    // the DAC idle with the sticky underflow bit and drew NAK-Invalid.
    let i = stream
        .iter()
        .position(|e| e.op == "data" && e.response().response == DacResponse::NAK_INVALID)
        .expect("data NAK after the underflow");
    let nak = stream[i].response().dac_status;
    assert_eq!(nak.playback_state, DacStatus::PLAYBACK_IDLE);
    assert_eq!(nak.playback_flags, FLAGS_SECOND_BOOT | 0x2);
    // The backend re-prepares, resends and begins, all ACKed.
    let ops: Vec<&str> = stream[i + 1..i + 4].iter().map(|e| e.op.as_str()).collect();
    assert_eq!(ops, ["prepare", "data", "begin"]);
    assert!(stream[i + 1..i + 4]
        .iter()
        .all(|e| e.response().response == DacResponse::ACK));
    let begin = stream[i + 3].response().dac_status;
    assert_eq!(begin.playback_state, DacStatus::PLAYBACK_PLAYING);
    // No data was ACKed while the DAC was idle, so no points were lost
    // silently.
    assert!(!stream
        .iter()
        .any(|e| e.op == "data" && e.response().response == DacResponse::ACK && is_idle(e)));

    let compared = replay("hwcheck underflow", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 100, "only {compared} events compared");
}

#[test]
fn hwcheck_estop_flag_survives_clear_until_prepare() {
    let events = load(HWCHECK_ESTOP_JSONL);
    let estop = find(&events, "estop").response().dac_status;
    assert_eq!(
        estop.light_engine_state,
        DacStatus::LIGHT_ENGINE_EMERGENCY_STOP
    );
    assert_eq!(estop.playback_state, DacStatus::PLAYBACK_IDLE);
    assert_eq!(estop.playback_flags, FLAGS_SECOND_BOOT | 0x4);

    // The backend only clears and pings while stopped, one clear per second.
    let clears: Vec<&TraceEvent> = events.iter().filter(|e| e.op == "clear_estop").collect();
    assert_eq!(clears.len(), 2);
    let gap = clears[1].t_us - clears[0].t_us;
    assert!((950_000..1_100_000).contains(&gap), "clear gap {gap} us");
    // A clear readies the light engine at once, but the e-stop playback flag
    // stays until the next prepare.
    let cleared = clears[1].response().dac_status;
    assert_eq!(cleared.light_engine_state, DacStatus::LIGHT_ENGINE_READY);
    assert_eq!(cleared.playback_flags, FLAGS_SECOND_BOOT | 0x4);
    let i = events.iter().position(|e| e.seq == clears[1].seq).unwrap();
    let prepare = events[i..].iter().find(|e| e.op == "prepare").unwrap();
    assert_eq!(
        prepare.response().dac_status.playback_flags,
        FLAGS_SECOND_BOOT
    );
    let begin = events[i..].iter().find(|e| e.op == "begin").unwrap();
    assert_eq!(
        begin.response().dac_status.playback_state,
        DacStatus::PLAYBACK_PLAYING
    );
    // Only data and prepare from before the backend saw the e-stop were held
    // back by the capture proxy; nothing else was attempted.
    let held: Vec<&str> = events
        .iter()
        .filter(|e| e.status == "synthesized")
        .map(|e| e.op.as_str())
        .collect();
    assert_eq!(held, ["data", "prepare"]);

    let compared = replay("hwcheck estop", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 100, "only {compared} events compared");
}

#[test]
fn hwcheck_rate_changes_reach_a_playing_dac() {
    // set_pps 1000 -> 20000 -> 1000 -> 30000 -> 1000 through the
    // presentation layer. Every change is sent as data, then 'u', so the
    // queued points never play out at the new rate before the data lands,
    // and the DAC keeps playing throughout.
    let events = load(HWCHECK_RATE_JSONL);
    let ups: Vec<usize> = (0..events.len())
        .filter(|&i| events[i].op == "update")
        .collect();
    let rates: Vec<u32> = ups
        .iter()
        .map(|&i| events[i].response().dac_status.point_rate)
        .collect();
    assert_eq!(rates, [20_000, 1_000, 30_000, 1_000]);
    for &i in &ups {
        assert_eq!(events[i - 1].op, "data", "seq {}", events[i].seq);
        let s = events[i].response().dac_status;
        assert_eq!(s.playback_state, DacStatus::PLAYBACK_PLAYING);
        assert!(s.buffer_fullness > 800, "fullness {}", s.buffer_fullness);
    }
    // One prepare for the whole run: no underflow.
    assert_eq!(events.iter().filter(|e| e.op == "prepare").count(), 1);
    assert!(events
        .iter()
        .filter(|e| e.status == "ok" && e.resp.len() == 2 * DacResponse::SIZE_BYTES)
        .all(|e| e.response().dac_status.playback_flags & 0x2 == 0));

    let compared = replay("hwcheck rate change", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 300, "only {compared} events compared");
}

#[test]
fn hwcheck_rate_drop_before_fix_reached_an_idle_dac() {
    // Recorded evidence of the bug fixed in the presentation layer: after
    // set_pps(20000 -> 1000) no write happened for about 950 ms, because the
    // estimate decayed the ~990 queued points at the new 1 kpps although the
    // DAC still played them at 20 kpps. The update to 1 kpps then landed on
    // an idle DAC.
    let events = load(HWCHECK_RATE_BEFORE_FIX_JSONL);
    let updates: Vec<&TraceEvent> = events.iter().filter(|e| e.op == "update").collect();
    assert_eq!(updates.len(), 2);
    assert_eq!(updates[0].response().dac_status.point_rate, 20_000);
    let i = events.iter().position(|e| e.seq == updates[1].seq).unwrap();
    let before = &events[i - 1];
    assert_eq!(before.response().dac_status.point_rate, 20_000);
    assert!(before.response().dac_status.buffer_fullness > 900);
    let silence = updates[1].t_us - before.t_us;
    assert!(silence > 900_000, "silence {silence} us");
    let late = updates[1].response().dac_status;
    assert_eq!(late.playback_state, DacStatus::PLAYBACK_IDLE);
    assert_eq!(late.playback_flags, FLAGS_SECOND_BOOT | 0x2);

    let compared = replay("hwcheck rate change before fix", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 300, "only {compared} events compared");
}

#[test]
fn hwcheck_rate_raise_before_fix_underflowed_a_short_ring() {
    // Excerpt of a run from before the backend sent data ahead of 'u'.
    // set_pps(1000 -> 20000) found only 33 points queued. The 'u' went
    // first; 33 points last 1.65 ms at 20 kpps, the data landed ~6 ms later,
    // and the DAC had gone idle. The data was NAKed and the backend
    // re-prepared.
    let events = load(HWCHECK_RATE_UP_BEFORE_FIX_JSONL);
    let up = find(&events, "update");
    let r = up.response().dac_status;
    assert_eq!(r.point_rate, 20_000);
    assert_eq!(r.playback_state, DacStatus::PLAYBACK_PLAYING);
    assert!(r.buffer_fullness < 50, "fullness {}", r.buffer_fullness);
    let i = events.iter().position(|e| e.seq == up.seq).unwrap();
    let next = &events[i + 1];
    assert_eq!(next.op, "data");
    assert_eq!(next.response().response, DacResponse::NAK_INVALID);
    assert!(is_idle(next));
    assert_eq!(
        next.response().dac_status.playback_flags,
        FLAGS_SECOND_BOOT | 0x2
    );
    assert_eq!(events[i + 2].op, "prepare");

    let compared = replay("hwcheck rate raise before fix", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 100, "only {compared} events compared");
}

#[test]
fn hwcheck_session_stop_sends_one_stop_then_disconnects() {
    // Stream::run for 2 s at 10 kpps, then SessionControl::stop(): the
    // backend sends exactly one 's' after the begin, and the next
    // connection's hello shows the DAC idle.
    let events = load(HWCHECK_SESSION_STOP_JSONL);
    let stream = connection(&events, 1);
    let b = stream.iter().position(|e| e.op == "begin").unwrap();
    let stops: Vec<&&TraceEvent> = stream[b..].iter().filter(|e| e.op == "stop").collect();
    assert_eq!(stops.len(), 1);
    assert!(stops[0].note.is_empty(), "stop came from the backend");
    assert_eq!(stops[0].response().response, DacResponse::ACK);
    assert!(is_idle(stops[0]));
    assert_eq!(stream.last().unwrap().op, "stop");
    let hello = connection(&events, 2)[0];
    assert_eq!(hello.op, "hello");
    assert!(is_idle(hello));

    let compared = replay("hwcheck session stop", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 50, "only {compared} events compared");
}

#[test]
fn hwcheck_partial_room_writes_what_fits_then_naks_invalid() {
    // 3849 of 3899 points queued while prepared, then one 'd' of 100: the
    // ED2 keeps the 50 that fit and answers NAK-Invalid, as the ED2 profile
    // assumes (data_partial_write_then_nak_invalid). The rest of the payload
    // is consumed; the next reply is in sync.
    let events = load(HWCHECK_PARTIAL_ROOM_JSONL);
    let pings: Vec<&TraceEvent> = events.iter().filter(|e| e.op == "ping").collect();
    assert_eq!(pings[0].response().dac_status.buffer_fullness, 3849);
    let d = events
        .iter()
        .find(|e| e.op == "data" && e.n == 3 + 18 * 100)
        .unwrap();
    let r = d.response();
    assert_eq!(r.response, DacResponse::NAK_INVALID);
    assert_eq!(r.command, b'd');
    assert_eq!(r.dac_status.buffer_fullness, 3899);
    assert_eq!(r.dac_status.playback_state, DacStatus::PLAYBACK_PREPARED);
    assert_eq!(pings[1].response().command, b'?');
    assert_eq!(pings[1].response().dac_status.buffer_fullness, 3899);
    assert!(FirmwareProfile::ed2_r331().data_partial_write_then_nak_invalid);

    let compared = replay("hwcheck partial room", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 6, "only {compared} events compared");
}

#[test]
fn hwcheck_full_capacity_chunk_then_nak_invalid() {
    let events = load(HWCHECK_FULL_CHUNK_JSONL);
    // One data command of exactly the advertised capacity is ACKed.
    let big = events.iter().find(|e| e.op == "data").unwrap();
    assert_eq!(big.n, 3 + 18 * 3899);
    let r = big.response();
    assert_eq!(r.response, DacResponse::ACK);
    assert_eq!(r.dac_status.buffer_fullness, 3899);
    // With the ring full, a one-point write is refused with NAK-Invalid,
    // not NAK-Full, and the DAC stays prepared.
    let one = events.iter().find(|e| e.op == "data" && e.n == 21).unwrap();
    let r = one.response();
    assert_eq!(r.response, DacResponse::NAK_INVALID);
    assert_eq!(r.dac_status.buffer_fullness, 3899);
    assert_eq!(r.dac_status.playback_state, DacStatus::PLAYBACK_PREPARED);
    // A prepared ring with no room never drains, so the backend's next write
    // begins playback instead of blocking forever.
    let i = events.iter().position(|e| e.seq == one.seq).unwrap();
    let b = &events[i + 1];
    assert_eq!(b.op, "begin");
    assert_eq!(b.response().response, DacResponse::ACK);
    assert_eq!(
        b.response().dac_status.playback_state,
        DacStatus::PLAYBACK_PLAYING
    );
    // The backend's stop on disconnect while idle draws NAK-Invalid.
    let last_stop = events.iter().rfind(|e| e.op == "stop").unwrap();
    assert_eq!(last_stop.response().response, DacResponse::NAK_INVALID);

    let compared = replay("hwcheck full chunk", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 11, "only {compared} events compared");
}

#[test]
fn hwcheck_clamped_rates_on_the_wire() {
    let events = load(HWCHECK_CLAMP_JSONL);
    let rate = |e: &TraceEvent| u32::from_le_bytes(e.request()[3..7].try_into().unwrap());
    // pps 0 is sent as max/16.
    let begin = find(&events, "begin");
    assert_eq!(rate(begin), 100_000 / 16);
    assert_eq!(begin.response().dac_status.point_rate, 6_250);
    // pps 200000 is clamped to the advertised max. The capture proxy only
    // allows rates up to 30 kpps near a laser, so it was not sent.
    let update = find(&events, "update");
    assert_eq!(rate(update), 100_000);
    assert_eq!(update.status, "blocked");
    // The stream API refuses rate 0 and over-max rates with InvalidInput
    // before sending: its connection carried nothing after the hello.
    assert_eq!(connection(&events, 1).len(), 1);

    let compared = replay("hwcheck clamp", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 3, "only {compared} events compared");
}

#[test]
fn hwcheck_steady_stream_holds_the_target_buffer() {
    // First 3 s of 60 s at 30 kpps through the presentation layer.
    let events = load(HWCHECK_STEADY_JSONL);
    let playing: Vec<u16> = events
        .iter()
        .filter(|e| e.op == "data" && !is_idle(e))
        .map(|e| e.response().dac_status.buffer_fullness)
        .filter(|&f| f > 0)
        .collect();
    assert!(playing.len() > 500);
    // Recorded under the old 50 ms default: 1500 points. The DAC never ran low.
    assert!(playing.iter().skip(1).all(|&f| (1300..=1500).contains(&f)));

    let compared = replay("hwcheck steady", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 500, "only {compared} events compared");
}

#[test]
fn hwcheck_steady_underflow_excerpt_replays() {
    // An 87 ms host stall at 30 kpps (49 ms queued) in the first 60 s run:
    // the next data write is NAKed on an idle DAC, then the backend
    // re-prepares, refills and begins within a millisecond.
    let events = load(HWCHECK_STEADY_UNDERFLOW_JSONL);
    let i = events.iter().position(is_idle).unwrap();
    assert_eq!(events[i].op, "data");
    assert_eq!(events[i].response().response, DacResponse::NAK_INVALID);
    assert!(events[i].t_us - events[i - 1].t_us > 80_000);
    let ops: Vec<&str> = events[i + 1..i + 5].iter().map(|e| e.op.as_str()).collect();
    assert_eq!(ops, ["prepare", "data", "data", "begin"]);

    let compared = replay("hwcheck steady underflow", &events, FLAGS_SECOND_BOOT);
    assert!(compared >= 30, "only {compared} events compared");
}
