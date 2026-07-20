//! Hardware-informed hermetic replay tests for the Helios USB DAC path.
//!
//! These tests ground the Helios wire protocol in **real traces captured from a
//! physical Helios Laser DAC** (USB `1209:E500`, firmware `5`, control-name
//! `Helios 909127731`). The captured bytes live as JSONL fixtures under
//! `tests/fixtures/helios/`; each test loads the relevant fixture, replays the
//! real device response bytes through the [`FakeUsb`] seam, and asserts the
//! production parse/state-machine logic behaves exactly as it did against
//! silicon — without any hardware or wall-clock sleeps.
//!
//! The realism gap being closed: the device returns **variable-length**
//! interrupt packets (2-byte status, 5-byte firmware, 32-byte name with an
//! uninitialised junk tail), never the fixed 32-byte buffers earlier synthetic
//! tests assumed. Frame-swap cadence is modelled as **poll counts** (how many
//! NotReady status reads precede the first Ready), not as timed sleeps.
//!
//! Seam constraint: `FakeUsb`, `HeliosDac::from_endpoints`, and the comm
//! internals are `#[cfg(test)] pub(crate)` / private, so these tests must live
//! in-crate (here), not under `tests/`. Fixtures are pulled in with
//! `include_str!` and parsed with `serde_json` (a dev-dependency).

use serde::Deserialize;

use crate::backend::{FrameSwapBackend, WriteOutcome};
use crate::discovery::slugify_device_id;
use crate::point::LaserPoint;
use crate::protocols::helios::native::test_support::FakeUsb;
use crate::protocols::helios::{DeviceStatus, HeliosBackend, HeliosDac};
use crate::protocols::usb_transfer::UsbEndpoints;

// USB endpoints, mirrored from `native.rs` (private there). The fake records
// writes per-endpoint, so tests assert the captured request bytes against these.
const ENDPOINT_INT_OUT: u8 = 0x06;
const ENDPOINT_INT_IN: u8 = 0x83;

// --- Fixture files (real captures) ------------------------------------------

macro_rules! fixture {
    ($name:literal) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/helios/",
            $name
        ))
    };
}

const INIT_JSONL: &str = fixture!("init.jsonl");
const IDENTITY_JSONL: &str = fixture!("identity.jsonl");
const STATUS_IDLE_JSONL: &str = fixture!("status_idle.sample.jsonl");
const PPS_SWEEP_JSONL: &str = fixture!("pps_sweep.jsonl");
const FRAME_CADENCE_JSON: &str = fixture!("frame_cadence.summary.json");

// --- Tiny serde_json fixture loader -----------------------------------------

/// One captured USB operation from a `*.jsonl` trace. Only the fields the
/// replay assertions need are named; serde ignores the rest (`dt_us`, `dur_us`,
/// `t_us`, `note`).
#[derive(Debug, Deserialize)]
struct TraceEvent {
    /// Logical operation, e.g. `fw_in`, `name_in`, `status_in`, `status_out`.
    op: String,
    /// Endpoint as a hex string (`"0x83"`, `"0x06"`, …) or `null` for non-USB
    /// bookkeeping ops (`settle_begin`, `drain_done`).
    ep: Option<String>,
    /// Request bytes written to the device, hex (empty for IN reads).
    req: String,
    /// Response bytes read from the device, hex (empty for OUT writes / errors).
    resp: String,
}

/// Parse a JSONL trace into its events (one JSON object per non-blank line).
fn load_trace(jsonl: &str) -> Vec<TraceEvent> {
    jsonl
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str::<TraceEvent>(l).expect("fixture line is valid JSON event"))
        .collect()
}

/// Decode a lowercase hex string (e.g. `"8405000000"`) into raw bytes.
fn hex_to_bytes(hex: &str) -> Vec<u8> {
    assert!(
        hex.len().is_multiple_of(2),
        "hex string has even length: {hex:?}"
    );
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("valid hex byte"))
        .collect()
}

/// The interrupt-IN (`ep 0x83`) response byte scripts for a trace, in order —
/// the real device replies used to drive the `FakeUsb` read queue. Skips reads
/// with no response (e.g. the init drain read, which times out).
fn int_in_replies(events: &[TraceEvent]) -> Vec<Vec<u8>> {
    events
        .iter()
        .filter(|e| e.ep.as_deref() == Some("0x83") && !e.resp.is_empty())
        .map(|e| hex_to_bytes(&e.resp))
        .collect()
}

/// The captured response bytes for the first event with the given `op`.
fn resp_for_op(events: &[TraceEvent], op: &str) -> Vec<u8> {
    let e = events
        .iter()
        .find(|e| e.op == op && !e.resp.is_empty())
        .unwrap_or_else(|| panic!("no `{op}` event with a response in trace"));
    hex_to_bytes(&e.resp)
}

/// The captured request bytes for the first event with the given `op`.
fn req_for_op(events: &[TraceEvent], op: &str) -> Vec<u8> {
    let e = events
        .iter()
        .find(|e| e.op == op && !e.req.is_empty())
        .unwrap_or_else(|| panic!("no `{op}` event with a request in trace"));
    hex_to_bytes(&e.req)
}

/// The `notready_polls_median` for a pps from `frame_cadence.summary.json` —
/// the real count of NotReady status polls before the device first signalled
/// Ready after a frame write.
fn cadence_notready_polls(pps: u32) -> usize {
    let summary: serde_json::Value =
        serde_json::from_str(FRAME_CADENCE_JSON).expect("cadence summary is valid JSON");
    summary["per_pps"][pps.to_string()]["notready_polls_median"]
        .as_u64()
        .unwrap_or_else(|| panic!("no notready_polls_median for pps {pps}")) as usize
}

// --- Test builders -----------------------------------------------------------

/// Build an open DAC over a fake transport (no real `rusb::Device`), the way
/// production `HeliosDac::Open` is shaped but hardware-free.
fn dac_over(fake: &FakeUsb) -> HeliosDac {
    HeliosDac::from_endpoints(fake.clone(), Some(5))
}

/// Wrap a fake transport in an open frame-swap backend. The fake is cloned so
/// the caller keeps a handle to inspect recorded writes.
fn backend_over(fake: &FakeUsb) -> HeliosBackend {
    HeliosBackend::from_dac(dac_over(fake))
}

fn one_blanked_point() -> Vec<LaserPoint> {
    // A blanked (zero-colour, zero-intensity) point, matching the capture rig,
    // which ran with no laser connected.
    vec![LaserPoint::new(0.0, 0.0, 0, 0, 0, 0)]
}

// =============================================================================
// (1) Firmware parse — real 5-byte `84 05 00 00 00` → fw == 5.  [init.jsonl]
// =============================================================================

#[test]
fn firmware_parses_real_five_byte_reply() {
    let events = load_trace(INIT_JSONL);

    // Ground truth: the init firmware probe read a 5-byte reply, not a padded
    // 32-byte buffer.
    let reply = resp_for_op(&events, "fw_in");
    assert_eq!(
        reply,
        vec![0x84, 0x05, 0x00, 0x00, 0x00],
        "captured fw_in bytes"
    );
    assert_eq!(reply.len(), 5, "device returns exactly 5 bytes, unpadded");

    let fake = FakeUsb::default();
    fake.script_int_in([Ok(reply)]);
    let dac = dac_over(&fake);

    // The production parse (`[0x84, b0..b3, ..]` → u32 LE) yields firmware 5.
    assert_eq!(dac.firmware_version().unwrap(), 5);

    // …and it issued the real firmware request the capture recorded (`04 00`).
    let out = fake.writes_to(ENDPOINT_INT_OUT);
    assert_eq!(out.len(), 1, "one interrupt-OUT command");
    assert_eq!(
        out[0],
        req_for_op(&events, "fw_out"),
        "GET_FIRMWARE request bytes"
    );
}

// =============================================================================
// (2) Name parse — real 32-byte reply with junk tail after the NUL →
//     "Helios 909127731"; discovery id → helios:helios-909127731. [identity.jsonl]
// =============================================================================

#[test]
fn name_parses_real_reply_with_uninitialised_junk_tail() {
    let events = load_trace(IDENTITY_JSONL);

    // Ground truth: name reply is a full 32 bytes — `85`, the ASCII name, a NUL,
    // then uninitialised firmware junk that must NOT leak into the parsed name.
    let reply = resp_for_op(&events, "name_in");
    assert_eq!(reply.len(), 32, "device returns a full 32-byte name packet");
    assert_eq!(reply[0], 0x85);
    assert!(
        reply[17..].iter().any(|&b| b != 0),
        "junk tail after the NUL is non-zero"
    );

    let fake = FakeUsb::default();
    fake.script_int_in([Ok(reply)]);
    let dac = dac_over(&fake);

    let name = dac.name().unwrap();
    assert_eq!(
        name, "Helios 909127731",
        "NUL-split name, junk tail discarded"
    );

    // The discovery id derives from that name via the shared slugifier.
    let id = format!("helios:{}", slugify_device_id(&name));
    assert_eq!(id, "helios:helios-909127731");
}

// =============================================================================
// (3) Status parse — real 2-byte `83 01` / `83 00`.
//     Ready from status_idle.sample.jsonl; NotReady from pps_sweep.jsonl.
// =============================================================================

#[test]
fn status_parses_real_two_byte_replies() {
    // Ready: idle device always answers `83 01`.
    let idle = load_trace(STATUS_IDLE_JSONL);
    let ready = resp_for_op(&idle, "status_in");
    assert_eq!(ready, vec![0x83, 0x01], "captured idle status = Ready");
    assert_eq!(ready.len(), 2, "status is exactly 2 bytes, unpadded");

    let fake = FakeUsb::default();
    fake.script_int_in([Ok(ready)]);
    let dac = dac_over(&fake);
    assert!(matches!(dac.status().unwrap(), DeviceStatus::Ready));

    // NotReady: mid-playback the sweep captured `83 00`.
    let sweep = load_trace(PPS_SWEEP_JSONL);
    let not_ready = int_in_replies(&sweep)
        .into_iter()
        .find(|r| r == &[0x83, 0x00])
        .expect("pps_sweep captured a NotReady status reply");

    let fake = FakeUsb::default();
    fake.script_int_in([Ok(not_ready)]);
    let dac = dac_over(&fake);
    assert!(matches!(dac.status().unwrap(), DeviceStatus::NotReady));
}

// =============================================================================
// (4) Frame-swap cadence — per-pps NotReady→Ready poll sequence driven through
//     `HeliosBackend::is_ready_for_frame()`. Cadence modelled as POLL COUNTS
//     (N × `83 00` then `83 01`), NO wall-clock sleeps. [frame_cadence.summary.json]
// =============================================================================

#[test]
fn frame_swap_cadence_matches_captured_notready_poll_counts() {
    for pps in [5000u32, 20000, 30000] {
        let notready_polls = cadence_notready_polls(pps);

        // Script the real cadence: exactly N NotReady status reads, then Ready.
        let mut replies: Vec<rusb::Result<Vec<u8>>> =
            (0..notready_polls).map(|_| Ok(vec![0x83, 0x00])).collect();
        replies.push(Ok(vec![0x83, 0x01]));

        let fake = FakeUsb::default();
        fake.script_int_in(replies);
        let mut backend = backend_over(&fake);

        // Every NotReady poll reports "not ready to swap"…
        for poll in 0..notready_polls {
            assert!(
                !backend.is_ready_for_frame(),
                "pps {pps}: poll {poll} should be NotReady (of {notready_polls})"
            );
        }
        // …and the very next poll flips to Ready — the frame-swap boundary.
        assert!(
            backend.is_ready_for_frame(),
            "pps {pps}: poll {notready_polls} should be the first Ready"
        );
    }
}

// =============================================================================
// (5) Back-pressure — status NotReady → `write_frame` returns `WouldBlock`
//     with zero bulk writes. [pps_sweep.jsonl]
// =============================================================================

#[test]
fn write_frame_backpressures_when_status_not_ready() {
    let sweep = load_trace(PPS_SWEEP_JSONL);
    let not_ready = int_in_replies(&sweep)
        .into_iter()
        .find(|r| r == &[0x83, 0x00])
        .expect("pps_sweep captured a NotReady status reply");

    let fake = FakeUsb::default();
    fake.script_int_in([Ok(not_ready)]); // pre-write status poll → NotReady
    let mut backend = backend_over(&fake);

    let outcome = backend.write_frame(30_000, &one_blanked_point()).unwrap();
    assert_eq!(
        outcome,
        WriteOutcome::WouldBlock,
        "NotReady must back-pressure the write"
    );
    assert!(
        fake.bulk_writes().is_empty(),
        "no frame bytes may reach the bulk endpoint while NotReady"
    );
}

// =============================================================================
// (6) Init drain + probe — drain finds 0 packets (first IN read Timeouts), then
//     the firmware probe succeeds on attempt 1. [init.jsonl]
// =============================================================================

#[test]
fn init_drain_finds_zero_packets_then_probe_succeeds_first_attempt() {
    let events = load_trace(INIT_JSONL);

    // Ground truth: init `drain_read` on `ep 0x83` timed out (drained 0 packets)
    // — there were no stale IN packets queued.
    assert!(
        events
            .iter()
            .any(|e| e.op == "drain_read" && e.ep.as_deref() == Some("0x83") && e.resp.is_empty()),
        "init trace records a drain read that returned nothing"
    );

    let fake = FakeUsb::default();

    // Model the drain: with an empty IN queue the transport times out, which is
    // exactly how the real drain loop learns 0 packets are pending and stops.
    let mut buf = [0u8; 32];
    assert!(
        matches!(
            fake.read_interrupt(
                ENDPOINT_INT_IN,
                &mut buf,
                std::time::Duration::from_millis(5)
            ),
            Err(rusb::Error::Timeout)
        ),
        "empty IN queue times out → drain finds 0 packets"
    );

    // Now the probe: the real firmware reply is queued and the OUT succeeds on
    // its first attempt, so `firmware_version()` parses fw 5 without retrying.
    fake.script_int_in([Ok(resp_for_op(&events, "fw_in"))]);
    let dac = HeliosDac::from_endpoints(fake.clone(), None);
    assert_eq!(
        dac.firmware_version().unwrap(),
        5,
        "probe parses fw on attempt 1"
    );
    assert_eq!(
        fake.writes_to(ENDPOINT_INT_OUT).len(),
        1,
        "exactly one firmware request — no retry needed"
    );
}
