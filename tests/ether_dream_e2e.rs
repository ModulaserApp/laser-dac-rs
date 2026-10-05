//! End-to-end tests for the Ether Dream protocol stack against the simulator.
//!
//! The TCP tests run against [`SimServer`], an in-process Ether Dream that
//! listens on an ephemeral loopback port. They reach it through
//! [`stream::connect_to`] and [`EtherDreamBackend::with_address`], so they can
//! run in parallel. Every stream and backend test runs once per firmware
//! profile in [`FirmwareProfile::all`].
//!
//! The discovery tests still use the real broadcast port (7654), because
//! [`recv_dac_broadcasts`] binds it. They take a process-global lock
//! ([`port_lock`]) and deliver the broadcast as a loopback unicast datagram,
//! so nothing leaves the machine. A real 255.255.255.255 broadcast is not
//! exercised: it is not hermetic in CI and could leak onto the LAN. The
//! receive and parse path is identical.

#![cfg(all(feature = "ether-dream", feature = "testutils"))]

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use laser_dac::device::DacType;
use laser_dac::discovery::DacDiscovery;
use laser_dac::presentation::{Frame, FrameSessionConfig};
use laser_dac::protocols::ether_dream::dac::stream::{
    self, CommunicationError, Nak, ResponseErrorKind,
};
use laser_dac::protocols::ether_dream::dac::{LightEngine, Playback};
use laser_dac::protocols::ether_dream::protocol::{
    self, DacBroadcast, DacPoint, DacResponse, DacStatus, ReadBytes, SizeBytes, WriteBytes,
};
use laser_dac::protocols::ether_dream::sim::model::cmd;
use laser_dac::protocols::ether_dream::sim::{Faults, SimServer, SimServerConfig};
use laser_dac::protocols::ether_dream::{recv_dac_broadcasts, EtherDreamBackend, FirmwareProfile};
use laser_dac::types::EnabledDacTypes;
use laser_dac::{
    caps_for_dac_type, BackendKind, ChunkResult, Dac, DacBackend, DacInfo, FifoBackend, LaserPoint,
    StreamConfig, WriteOutcome,
};

const BROADCAST_PORT: u16 = protocol::BROADCAST_PORT; // 7654
const TIMEOUT: Duration = Duration::from_secs(2);

const CMD_PREPARE: u8 = b'p';
const CMD_BEGIN: u8 = b'b';
const CMD_UPDATE: u8 = b'u';
const CMD_POINT_RATE: u8 = b'q';
const CMD_DATA: u8 = b'd';
const CMD_STOP: u8 = b's';
const CMD_ESTOP: u8 = 0x00;
const CMD_CLEAR_ESTOP: u8 = b'c';
const CMD_PING: u8 = b'?';
const CMD_VERSION: u8 = b'v';

/// Serializes the tests that bind the fixed broadcast port. Poison-tolerant.
fn port_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// =============================================================================
// Helpers
// =============================================================================

fn start(profile: &FirmwareProfile) -> SimServer {
    SimServer::loopback(profile.clone()).expect("start simulator")
}

fn start_with(profile: &FirmwareProfile, faults: Faults) -> SimServer {
    SimServer::start(SimServerConfig::new(profile.clone()).with_faults(faults))
        .expect("start simulator")
}

fn connect_stream(srv: &SimServer) -> stream::Stream {
    match stream::connect_to(srv.addr(), Some(&srv.broadcast()), TIMEOUT) {
        Ok(s) => s,
        Err(e) => panic!("connect to simulator: {e:?}"),
    }
}

fn connect_backend(srv: &SimServer) -> EtherDreamBackend {
    let mut b = EtherDreamBackend::with_address(srv.addr(), Some(srv.broadcast()));
    b.connect().expect("backend connect");
    b
}

/// Run raw commands on the DAC, as another client would.
fn feed(srv: &SimServer, bytes: &[u8]) {
    srv.with_model(|m| {
        let now = m.now();
        m.feed(now, bytes);
    });
}

fn blank_points(n: usize) -> impl Iterator<Item = DacPoint> {
    std::iter::repeat_n(cmd::blank(), n)
}

fn laser_points(n: usize) -> Vec<LaserPoint> {
    vec![LaserPoint::new(0.0, 0.0, 0, 0, 0, 0); n]
}

fn ready_status() -> DacStatus {
    DacStatus {
        protocol: 0,
        light_engine_state: DacStatus::LIGHT_ENGINE_READY,
        playback_state: DacStatus::PLAYBACK_IDLE,
        source: DacStatus::SOURCE_NETWORK_STREAMING,
        light_engine_flags: 0,
        playback_flags: 0,
        source_flags: 0,
        buffer_fullness: 0,
        point_rate: 0,
        point_count: 0,
    }
}

fn encode_broadcast(bc: &DacBroadcast) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(DacBroadcast::SIZE_BYTES);
    buf.write_bytes(*bc).expect("encode DacBroadcast");
    buf
}

fn expect_nak(err: CommunicationError, nak: Nak) {
    match err {
        CommunicationError::Response(e) => {
            assert!(
                matches!(e.kind, ResponseErrorKind::Nak(n) if n == nak),
                "expected {nak:?}, got {:?}",
                e.kind
            );
        }
        other => panic!("expected a {nak:?} response, got {other:?}"),
    }
}

// =============================================================================
// Stream layer
// =============================================================================

#[test]
fn test_connect_handshake_and_initial_status() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        feed(&srv, &cmd::prepare());
        feed(&srv, &cmd::blank_data(42));

        let stream = connect_stream(&srv);
        let dac = stream.dac();
        assert_eq!(dac.buffer_capacity, p.buffer_capacity, "{}", p.name);
        assert_eq!(dac.max_point_rate, p.max_point_rate);
        assert_eq!(dac.mac_address.0, p.mac_address);
        assert_eq!(dac.status.buffer_fullness, 42, "fullness from the hello");
        assert_eq!(dac.status.playback, Playback::Prepared);
        // Only the commands fed above; connecting sends nothing.
        assert_eq!(srv.commands(), vec![CMD_PREPARE, CMD_DATA]);
    }
}

#[test]
fn test_connect_without_broadcast_uses_defaults() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let stream = match stream::connect_to(srv.addr(), None, TIMEOUT) {
            Ok(s) => s,
            Err(e) => panic!("{}: {e:?}", p.name),
        };
        assert_eq!(stream.dac().buffer_capacity, 1799);
        assert_eq!(stream.dac().max_point_rate, 100_000);
        assert_eq!(stream.dac().mac_address.0, [0; 6]);
        assert_eq!(stream.peer_addr().unwrap(), srv.addr());
    }
}

#[test]
fn test_query_version() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut stream = connect_stream(&srv);
        let v = stream.query_version().expect("version query");
        assert_eq!(v.as_deref(), p.version_string, "{}", p.name);
        assert_eq!(srv.commands(), vec![CMD_VERSION]);
        // The connection is still usable afterwards.
        stream.queue_commands().ping().submit().expect("ping");
    }
}

/// The rates are low so the 1500 queued points last over a second. The
/// simulator plays on the wall clock, and at tens of kpps a short host stall
/// would drain the ring, idle the DAC, zero its rate and make the stop NAK.
#[test]
fn test_prepare_begin_update_pointrate_roundtrip() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut stream = connect_stream(&srv);

        stream
            .queue_commands()
            .prepare_stream()
            .submit()
            .expect("prepare");
        stream
            .queue_commands()
            .data(blank_points(1500))
            .submit()
            .expect("data");
        stream
            .queue_commands()
            .point_rate(500)
            .submit()
            .expect("point_rate");
        stream
            .queue_commands()
            .begin(0, 1_000)
            .submit()
            .expect("begin");
        assert_eq!(stream.dac().status.playback, Playback::Playing);
        assert_eq!(stream.dac().status.point_rate, 1_000);
        stream
            .queue_commands()
            .update(0, 1_200)
            .submit()
            .expect("update");
        assert_eq!(stream.dac().status.playback, Playback::Playing);
        assert_eq!(stream.dac().status.point_rate, 1_200);
        stream.queue_commands().stop().submit().expect("stop");

        // Several commands can be queued and submitted as one batch.
        stream
            .queue_commands()
            .prepare_stream()
            .data(blank_points(1500))
            .begin(0, 1_000)
            .submit()
            .expect("batch");
        assert_eq!(stream.dac().status.playback, Playback::Playing);

        assert_eq!(
            srv.commands(),
            vec![
                CMD_PREPARE,
                CMD_DATA,
                CMD_POINT_RATE,
                CMD_BEGIN,
                CMD_UPDATE,
                CMD_STOP,
                CMD_PREPARE,
                CMD_DATA,
                CMD_BEGIN,
            ],
            "{}",
            p.name
        );
    }
}

#[test]
fn test_data_buffer_fullness_accounting() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut stream = connect_stream(&srv);
        stream.queue_commands().prepare_stream().submit().unwrap();

        stream
            .queue_commands()
            .data(blank_points(100))
            .submit()
            .expect("first data");
        assert_eq!(stream.dac().status.buffer_fullness, 100);
        stream
            .queue_commands()
            .data(blank_points(250))
            .submit()
            .expect("second data");
        assert_eq!(stream.dac().status.buffer_fullness, 350, "accumulates");
        assert_eq!(srv.with_model(|m| m.received_points().len()), 350);
    }
}

#[test]
fn test_overfull_data_is_nak_invalid_unless_firmware_naks_full() {
    let mut profiles: Vec<(FirmwareProfile, Nak)> = FirmwareProfile::all()
        .into_iter()
        .map(|p| (p, Nak::Invalid))
        .collect();
    let mut full = FirmwareProfile::ed1_j4cdac();
    full.naks_full = true;
    profiles.push((full, Nak::Full));

    for (p, nak) in profiles {
        let srv = start(&p);
        let mut stream = connect_stream(&srv);
        stream.queue_commands().prepare_stream().submit().unwrap();
        let err = stream
            .queue_commands()
            .data(blank_points(p.ring_points as usize + 1))
            .submit()
            .err()
            .unwrap_or_else(|| panic!("{}: overfull write must be NAKed", p.name));
        expect_nak(err, nak);
    }
}

/// Regression: a batch whose first command was NAKed returned before reading
/// the remaining responses, leaving them in the socket for the next submit.
#[test]
fn test_batch_reads_every_response_and_reports_first_nak() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut stream = connect_stream(&srv);
        // 's' while idle is NAK-Invalid; 'p' still runs.
        let err = stream
            .queue_commands()
            .stop()
            .prepare_stream()
            .submit()
            .err()
            .unwrap_or_else(|| panic!("{}: stop while idle must NAK", p.name));
        expect_nak(err, Nak::Invalid);
        assert_eq!(stream.dac().status.playback, Playback::Prepared);
        // The stream is still in sync.
        stream
            .queue_commands()
            .ping()
            .submit()
            .expect("ping after NAK");
        assert_eq!(stream.dac().status.playback, Playback::Prepared);
    }
}

/// Regression: rate 0 hangs Ether Dream firmware until a power cycle, and a
/// rate above the maximum is NAKed after one byte, so the argument bytes run
/// as commands. The stream refuses both before anything reaches the wire.
#[test]
fn test_unsafe_rates_are_rejected_before_sending() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut stream = connect_stream(&srv);
        stream.queue_commands().prepare_stream().submit().unwrap();
        let before = srv.commands();
        let over = p.max_point_rate + 1;
        let attempts: Vec<Result<(), CommunicationError>> = vec![
            stream.queue_commands().begin(0, 0).submit(),
            stream.queue_commands().begin(0, over).submit(),
            stream.queue_commands().update(0, 0).submit(),
            stream.queue_commands().update(0, over).submit(),
            stream.queue_commands().point_rate(0).submit(),
            stream
                .queue_commands()
                .data(blank_points(10))
                .point_rate(over)
                .submit(),
        ];
        for (i, result) in attempts.into_iter().enumerate() {
            match result {
                Err(CommunicationError::Io(e)) => {
                    assert_eq!(
                        e.kind(),
                        std::io::ErrorKind::InvalidInput,
                        "{} #{i}",
                        p.name
                    )
                }
                other => panic!("{} #{i}: expected InvalidInput, got {other:?}", p.name),
            }
        }
        assert_eq!(srv.commands(), before, "{}: nothing was sent", p.name);
        assert!(!srv.with_model(|m| m.is_hung()), "{}", p.name);
        // The stream is still in sync, and the boundary rates are accepted.
        // Begin at rate 1 so the ten points play for ten seconds: the update
        // then lands on a playing DAC. The firmware ACKs 'u' while idle too,
        // so an update after the ring drained would prove nothing.
        stream
            .queue_commands()
            .data(blank_points(10))
            .begin(0, 1)
            .submit()
            .expect("begin at rate 1");
        assert_eq!(stream.dac().status.playback, Playback::Playing);
        assert_eq!(stream.dac().status.point_rate, 1, "{}", p.name);
        stream
            .queue_commands()
            .update(0, p.max_point_rate)
            .submit()
            .expect("update to the maximum rate");
        assert_eq!(stream.dac().status.playback, Playback::Playing);
        assert_eq!(
            stream.dac().status.point_rate,
            p.max_point_rate,
            "{}",
            p.name
        );
        assert_eq!(stream.dac().status.light_engine, LightEngine::Ready);
    }
}

#[test]
fn test_stop_estop_clear_ping_roundtrip() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut stream = connect_stream(&srv);

        stream.queue_commands().ping().submit().expect("ping");
        stream
            .queue_commands()
            .emergency_stop()
            .submit()
            .expect("estop");
        assert_eq!(stream.dac().status.light_engine, LightEngine::EmergencyStop);

        stream
            .queue_commands()
            .clear_emergency_stop()
            .submit()
            .expect("clear estop");
        assert_eq!(stream.dac().status.light_engine, LightEngine::Ready);

        stream.queue_commands().prepare_stream().submit().unwrap();
        stream.queue_commands().stop().submit().expect("stop");
        assert_eq!(stream.dac().status.playback, Playback::Idle);

        assert_eq!(
            srv.commands(),
            vec![CMD_PING, CMD_ESTOP, CMD_CLEAR_ESTOP, CMD_PREPARE, CMD_STOP]
        );
    }
}

#[test]
fn test_server_disconnect_after_handshake_surfaces_io_error() {
    let mut faults = Faults::default();
    faults.drop_after_replies = Some(0);
    for p in FirmwareProfile::all() {
        let srv = start_with(&p, faults.clone());
        let mut stream = connect_stream(&srv);
        let err = stream
            .queue_commands()
            .prepare_stream()
            .submit()
            .expect_err("submit after disconnect should fail");
        assert!(matches!(err, CommunicationError::Io(_)), "{err:?}");
    }
}

#[test]
fn test_short_response_surfaces_io_error() {
    let mut faults = Faults::default();
    faults.truncate_reply = Some(0);
    for p in FirmwareProfile::all() {
        let srv = start_with(&p, faults.clone());
        let mut stream = connect_stream(&srv);
        let err = stream
            .queue_commands()
            .prepare_stream()
            .submit()
            .expect_err("truncated response should fail");
        assert!(matches!(err, CommunicationError::Io(_)), "{err:?}");
    }
}

#[test]
fn test_invalid_status_in_handshake_surfaces_protocol_error() {
    let mut bad = ready_status();
    bad.light_engine_state = 99;
    let mut faults = Faults::default();
    faults.hello_status = Some(bad);
    let srv = start_with(&FirmwareProfile::ed2_r331(), faults);
    // `Stream` is not `Debug`, so match explicitly.
    match stream::connect_to(srv.addr(), Some(&srv.broadcast()), TIMEOUT) {
        Ok(_) => panic!("connect should fail on a malformed status"),
        Err(CommunicationError::Protocol(_)) => {}
        Err(other) => panic!("expected a protocol error, got {other:?}"),
    }
}

// =============================================================================
// Backend layer
// =============================================================================

#[test]
fn test_backend_connect_disconnect_lifecycle() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut backend = EtherDreamBackend::with_address(srv.addr(), Some(srv.broadcast()));
        assert_eq!(backend.dac_type(), DacType::EtherDream);
        assert!(!backend.is_connected());

        backend.connect().expect("connect");
        assert!(backend.is_connected());
        assert_eq!(backend.firmware_version(), p.version_string);

        backend.disconnect().expect("disconnect");
        assert!(!backend.is_connected());
    }
}

#[test]
fn test_backend_not_connected_returns_disconnected() {
    let mut backend =
        EtherDreamBackend::with_address(SocketAddr::from((Ipv4Addr::LOCALHOST, 1)), None);
    let err = backend
        .try_write_points(30_000, &laser_points(1))
        .expect_err("write without connect should fail");
    assert!(err.is_disconnected(), "expected Disconnected, got {err:?}");
}

#[test]
fn test_backend_full_buffer_reports_would_block() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        feed(&srv, &cmd::prepare());
        feed(&srv, &cmd::blank_data(p.buffer_capacity as usize));
        let mut backend = connect_backend(&srv);
        let before = srv.commands().len();

        let outcome = backend
            .try_write_points(30_000, &laser_points(32))
            .expect("a full buffer is backpressure, not an error");
        assert_eq!(outcome, WriteOutcome::WouldBlock, "{}", p.name);
        // No data is sent. The prepared ring cannot drain on its own, so the
        // backend begins it instead of waiting forever.
        assert_eq!(srv.commands()[before..], [CMD_BEGIN], "{}", p.name);
    }
}

#[test]
fn test_backend_write_points_prepares_begins_and_writes() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut backend = connect_backend(&srv);

        // 400 points last 400 ms at 1 kpps, so a host stall cannot drain the
        // ring before the status checks below.
        let outcome = backend
            .try_write_points(1_000, &laser_points(400))
            .expect("write");
        assert_eq!(outcome, WriteOutcome::Written);
        assert_eq!(
            srv.commands(),
            vec![CMD_VERSION, CMD_PREPARE, CMD_DATA, CMD_BEGIN],
            "{}",
            p.name
        );
        assert_eq!(srv.status().playback_state, DacStatus::PLAYBACK_PLAYING);
        assert_eq!(srv.status().point_rate, 1_000);
    }
}

/// Every authored point reaches the wire once, in order, byte-for-byte equal
/// to `DacPoint::from` of that point.
#[test]
fn test_backend_write_points_encodes_each_point_once_on_the_wire() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut backend = connect_backend(&srv);

        let pts: Vec<LaserPoint> = (0..300)
            .map(|i| {
                let t = i as f32 / 300.0;
                LaserPoint::new(t * 2.0 - 1.0, 1.0 - t, i * 7, i * 11, i * 13, i * 17)
            })
            .collect();
        assert_eq!(
            backend.try_write_points(30_000, &pts).unwrap(),
            WriteOutcome::Written
        );
        let expected: Vec<DacPoint> = pts.iter().map(DacPoint::from).collect();
        assert_eq!(srv.with_model(|m| m.received_points().to_vec()), expected);
    }
}

#[test]
fn test_backend_server_disconnect_returns_error() {
    // Reply 0 answers the version query; reply 1 would answer the prepare.
    let mut faults = Faults::default();
    faults.drop_after_replies = Some(1);
    for p in FirmwareProfile::all() {
        let srv = start_with(&p, faults.clone());
        let mut backend = connect_backend(&srv);
        let err = backend
            .try_write_points(30_000, &laser_points(8))
            .expect_err("write to a vanished DAC should fail");
        assert!(!err.is_would_block(), "a dead socket is not backpressure");
    }
}

/// Keep a playing DAC fed for a while. Scheduling stalls on a loaded machine
/// can still underflow the simulated ring, so this checks that every chunk
/// reported as written was accepted, not that playback never stuttered.
#[test]
fn test_backend_streams_continuously() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let mut backend = connect_backend(&srv);
        let deadline = Instant::now() + Duration::from_millis(300);
        let mut written = 0u64;
        while Instant::now() < deadline {
            match backend.try_write_points(30_000, &laser_points(300)) {
                Ok(WriteOutcome::Written) => written += 300,
                Ok(WriteOutcome::WouldBlock) => thread::sleep(Duration::from_millis(2)),
                Err(e) => panic!("{}: {e}", p.name),
            }
        }
        assert!(written >= 1_800, "{}: wrote only {written}", p.name);
        let accepted = srv.with_model(|m| m.accepted_total());
        assert!(accepted >= written, "{}: {accepted} < {written}", p.name);
        assert!(srv.with_model(|m| m.played_total()) > 0, "{}", p.name);
    }
}

// =============================================================================
// Presentation layer: Dac::new + start_frame_session
// =============================================================================

fn frame_session_reaches_playing(p: &FirmwareProfile, pps: u32) {
    let srv = start(p);
    let backend = EtherDreamBackend::with_address(srv.addr(), Some(srv.broadcast()));
    let backend = BackendKind::fifo(Box::new(backend)).expect("fifo backend");
    let info = DacInfo::new(
        "etherdream:sim".to_string(),
        "Ether Dream simulator".to_string(),
        DacType::EtherDream,
        caps_for_dac_type(&DacType::EtherDream),
    );
    let dac = Dac::new(info, backend);
    let (session, info) = dac
        .start_frame_session(FrameSessionConfig::new(pps))
        .expect("start frame session");
    // Caps were refreshed from the advertised capacity after connect.
    assert_eq!(info.caps.max_points_per_chunk, p.buffer_capacity as usize);

    session.send_frame(Frame::new(laser_points(200)));
    let deadline = Instant::now() + Duration::from_secs(3);
    while srv.status().playback_state != DacStatus::PLAYBACK_PLAYING {
        assert!(
            Instant::now() < deadline,
            "{} at {pps} pps never began playback: {:?}",
            p.name,
            srv.status()
        );
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(srv.status().point_rate, pps);
    // Output keeps flowing. A stall on a loaded machine may underflow the
    // ring, so check progress rather than an uninterrupted Playing state.
    let played = srv.with_model(|m| m.played_total());
    thread::sleep(Duration::from_millis(200));
    assert!(
        srv.with_model(|m| m.played_total()) > played,
        "{} at {pps} pps stopped playing",
        p.name
    );

    session.control().stop().expect("stop");
    session.join().expect("join");
}

#[test]
fn test_frame_session_plays_on_every_profile() {
    for p in FirmwareProfile::all() {
        frame_session_reaches_playing(&p, 30_000);
    }
}

/// Regression: at low rates the presenter keeps fewer points queued than the
/// old fixed begin threshold, so playback never began.
#[test]
fn test_frame_session_begins_at_low_point_rate() {
    for p in FirmwareProfile::all() {
        frame_session_reaches_playing(&p, 1_000);
    }
}

/// Regression: a prepared stream below the begin threshold was reported to the
/// estimator as not draining. With a target buffer below the threshold the
/// stream then never asked for more points, and playback never began.
#[test]
fn test_stream_with_small_target_buffer_begins() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let backend = EtherDreamBackend::with_address(srv.addr(), Some(srv.broadcast()));
        let backend = BackendKind::fifo(Box::new(backend)).expect("fifo backend");
        let info = DacInfo::new(
            "etherdream:sim".to_string(),
            "Ether Dream simulator".to_string(),
            DacType::EtherDream,
            caps_for_dac_type(&DacType::EtherDream),
        );
        // 5 ms at 30 kpps is 150 points; the begin threshold is 300.
        let cfg = StreamConfig::new(30_000).with_target_buffer(Duration::from_millis(5));
        let (stream, _) = Dac::new(info, backend).start_stream(cfg).expect("start");
        let control = stream.control();
        let runner = thread::spawn(move || {
            stream.run(
                |req, buf| {
                    let n = req.target_points.min(buf.len());
                    for pt in &mut buf[..n] {
                        *pt = LaserPoint::blanked(0.0, 0.0);
                    }
                    ChunkResult::Filled(n)
                },
                |_| {},
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut began = false;
        while Instant::now() < deadline {
            if srv.commands().contains(&CMD_BEGIN) {
                began = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }
        control.stop().expect("stop");
        runner.join().expect("join").expect("run");
        assert!(began, "{} never began: {:?}", p.name, srv.status());
    }
}

fn sim_dac(srv: &SimServer) -> Dac {
    let backend = EtherDreamBackend::with_address(srv.addr(), Some(srv.broadcast()));
    let backend = BackendKind::fifo(Box::new(backend)).expect("fifo backend");
    let info = DacInfo::new(
        "etherdream:sim".to_string(),
        "Ether Dream simulator".to_string(),
        DacType::EtherDream,
        caps_for_dac_type(&DacType::EtherDream),
    );
    Dac::new(info, backend)
}

fn wait_for_playing(srv: &SimServer, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while srv.status().playback_state != DacStatus::PLAYBACK_PLAYING {
        assert!(Instant::now() < deadline, "{what} never began playback");
        thread::sleep(Duration::from_millis(5));
    }
}

/// After a session stopped via its control, the DAC must have been told to
/// stop exactly once. Closing TCP alone leaves it idle with stale points and
/// no record of a stop, which is what the hardware showed.
fn assert_stopped_cleanly(srv: &SimServer, what: &str) {
    let cmds = srv.commands();
    let began = cmds.iter().position(|&c| c == CMD_BEGIN).expect("began");
    let stops = cmds[began..].iter().filter(|&&c| c == CMD_STOP).count();
    assert_eq!(
        stops,
        1,
        "{what}: stops after playback: {:?}",
        &cmds[began..]
    );
    let st = srv.status();
    assert_eq!(
        st.playback_state,
        DacStatus::PLAYBACK_IDLE,
        "{what}: {st:?}"
    );
    let deadline = Instant::now() + TIMEOUT;
    while srv.active_connections() != 0 {
        assert!(Instant::now() < deadline, "{what}: connection left open");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Regression: ending `Stream::run` or a `FrameSession` through
/// `SessionControl::stop()` only closed the shutter and then dropped the
/// connection, leaving the DAC idle with leftover points and no stop.
#[test]
fn test_session_stop_sends_stop_to_the_dac() {
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let (stream, _) = sim_dac(&srv)
            .start_stream(StreamConfig::new(30_000))
            .expect("start");
        let control = stream.control();
        control.arm().expect("arm");
        let runner = thread::spawn(move || {
            stream.run(
                |req, buf| {
                    let n = req.target_points.min(buf.len());
                    for pt in &mut buf[..n] {
                        *pt = LaserPoint::blanked(0.0, 0.0);
                    }
                    ChunkResult::Filled(n)
                },
                |_| {},
            )
        });
        wait_for_playing(&srv, &format!("{} stream", p.name));
        control.stop().expect("stop");
        runner.join().expect("join").expect("run");
        assert_stopped_cleanly(&srv, &format!("{} stream", p.name));

        let srv = start(&p);
        let (session, _) = sim_dac(&srv)
            .start_frame_session(FrameSessionConfig::new(30_000))
            .expect("start frame session");
        session.send_frame(Frame::new(laser_points(200)));
        wait_for_playing(&srv, &format!("{} frame session", p.name));
        session.control().stop().expect("stop");
        session.join().expect("join");
        assert_stopped_cleanly(&srv, &format!("{} frame session", p.name));
    }
}

/// Regression: after lowering the rate, the adapter judged the buffered points
/// against the new rate and slept for about a second. The DAC kept draining
/// at the old rate, because the new rate only rides on the next data write,
/// and underflowed.
#[test]
fn test_lowering_the_rate_does_not_underflow() {
    // At 1 kpps the default target buffer holds about 80 ms, so a host stall
    // that long on a loaded test runner underflows legitimately. The bug
    // ran the ring dry within about 100 ms of the rate change on every
    // attempt, so a few retries remove the flake without weakening the check.
    const ATTEMPTS: usize = 3;
    let mut failure = None;
    for attempt in 1..=ATTEMPTS {
        match lower_rate_once() {
            Ok(()) => {
                failure = None;
                break;
            }
            Err(why) => {
                eprintln!("attempt {attempt}/{ATTEMPTS} failed: {why}");
                failure = Some(why);
            }
        }
    }
    if let Some(why) = failure {
        panic!("failed {ATTEMPTS} attempts, last: {why}");
    }
}

/// One pass for `test_lowering_the_rate_does_not_underflow`. Returns a
/// description of the problem instead of panicking so the caller can retry a
/// run spoiled by host load.
fn lower_rate_once() -> Result<(), String> {
    let srv = start(&FirmwareProfile::ed2_r331());
    let (stream, _) = sim_dac(&srv)
        .start_stream(StreamConfig::new(20_000))
        .expect("start");
    let control = stream.control();
    control.arm().expect("arm");
    let runner = thread::spawn(move || {
        stream.run(
            |req, buf| {
                let n = req.target_points.min(buf.len());
                for pt in &mut buf[..n] {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            },
            |_| {},
        )
    });
    wait_for_playing(&srv, "ed2 stream");
    thread::sleep(Duration::from_millis(300));
    let underflows = srv.with_model(|m| m.underflows());

    control.set_pps(1_000).expect("set pps");
    let deadline = Instant::now() + Duration::from_millis(500);
    let mut reached = true;
    while srv.status().point_rate != 1_000 {
        if Instant::now() >= deadline {
            reached = false;
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let status = srv.status();
    if reached {
        thread::sleep(Duration::from_millis(1_200));
    }
    let after = srv.with_model(|m| m.underflows());

    control.stop().expect("stop");
    runner.join().expect("join").expect("run");
    if !reached {
        return Err(format!("new rate never reached the DAC: {status:?}"));
    }
    if after != underflows {
        return Err(format!(
            "underflowed {} times after lowering the rate",
            after - underflows
        ));
    }
    Ok(())
}

/// Regression: the adapter aimed for `target_buffer * pps` with no regard
/// for the ring. It sizes a chunk from the deficit and waits until the whole
/// chunk fits, so once the target reaches about twice the ring the chunk only
/// fits after the ring has run dry. The backend's ceiling (80 % of the ring)
/// keeps the target reachable on every profile.
///
/// On ED1 and ED2, 28 points per second per ring slot puts the 80 ms default
/// above twice the ring, where an unclamped target always underflows, while
/// the ceiling still holds about 29 ms. ED3 and ED4 cannot reach twice the
/// ring at 100 kpps, so they run with a ceiling of about 60 ms and only check
/// that the clamped stream is clean.
#[test]
fn test_default_target_above_ring_capacity_streams_cleanly() {
    let default_target = StreamConfig::ETHER_DREAM_DEFAULT_TARGET_BUFFER;
    for p in FirmwareProfile::all() {
        let capacity = u32::from(p.buffer_capacity);
        let guards_starvation = p.name.starts_with("ed1") || p.name.starts_with("ed2");
        let pps = if guards_starvation {
            (28 * capacity).min(100_000)
        } else {
            (capacity * 4 / 5 * 1000 / 60).min(100_000)
        };
        let requested = default_target.as_secs_f64() * f64::from(pps);
        assert!(
            requested > f64::from(capacity * 4 / 5),
            "{}: {pps} pps does not exercise the ceiling",
            p.name
        );
        if guards_starvation {
            assert!(
                requested >= f64::from(2 * capacity),
                "{}: {pps} pps would not starve without the ceiling",
                p.name
            );
        }
        // This runs over real TCP with only ~29 ms of ring headroom at 100
        // kpps, so a host stall on a loaded test runner can starve the ring
        // legitimately. Without the ceiling the ring runs dry within ~40 ms
        // on every attempt, so a few retries remove the flake without
        // weakening the regression check.
        const ATTEMPTS: usize = 3;
        let mut failure = None;
        for attempt in 1..=ATTEMPTS {
            match stream_default_target_once(&p, pps) {
                Ok(()) => {
                    failure = None;
                    break;
                }
                Err(why) => {
                    eprintln!("{}: attempt {attempt}/{ATTEMPTS} failed: {why}", p.name);
                    failure = Some(why);
                }
            }
        }
        if let Some(why) = failure {
            panic!("{}: failed {ATTEMPTS} attempts, last: {why}", p.name);
        }
    }
}

/// One streaming pass for `test_default_target_above_ring_capacity_streams_cleanly`.
/// Returns a description of the first problem instead of panicking so the
/// caller can retry a run spoiled by host load.
fn stream_default_target_once(p: &FirmwareProfile, pps: u32) -> Result<(), String> {
    let srv = start(p);
    let (stream, _) = sim_dac(&srv)
        .start_stream(StreamConfig::new(pps))
        .expect("start");
    let control = stream.control();
    control.arm().expect("arm");
    let errors = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
    let sink = std::sync::Arc::clone(&errors);
    let runner = thread::spawn(move || {
        stream.run(
            |req, buf| {
                let n = req.target_points.min(buf.len());
                for pt in &mut buf[..n] {
                    *pt = LaserPoint::blanked(0.0, 0.0);
                }
                ChunkResult::Filled(n)
            },
            move |e| sink.lock().unwrap().push(e.to_string()),
        )
    });
    wait_for_playing(&srv, &format!("{} stream", p.name));
    let (played, underflows, naks) =
        srv.with_model(|m| (m.played_total(), m.underflows(), m.nak_replies()));
    // Sample the ring while streaming so a failure says when it ran dry.
    let watch = Instant::now();
    let mut lowest = u16::MAX;
    let mut first_underflow = None;
    while watch.elapsed() < Duration::from_secs(2) {
        let (fullness, now_underflows) =
            srv.with_model(|m| (m.status().buffer_fullness, m.underflows()));
        lowest = lowest.min(fullness);
        if now_underflows != underflows && first_underflow.is_none() {
            first_underflow = Some(watch.elapsed());
        }
        thread::sleep(Duration::from_millis(1));
    }
    let after = srv.with_model(|m| {
        (
            m.played_total(),
            m.underflows(),
            m.nak_replies(),
            m.overfull_writes(),
        )
    });
    control.stop().expect("stop");
    runner.join().expect("join").expect("run");

    if after.1 != underflows {
        return Err(format!(
            "underflowed at {first_underflow:?}, lowest fullness {lowest}"
        ));
    }
    if after.2 != naks {
        return Err("NAKed while streaming".into());
    }
    if after.3 != 0 {
        return Err(format!("{} overfull data writes", after.3));
    }
    let errors = errors.lock().unwrap();
    if !errors.is_empty() {
        return Err(format!("stream errors: {errors:?}"));
    }
    if srv.total_connections() != 1 {
        return Err(format!(
            "reconnected ({} connections)",
            srv.total_connections()
        ));
    }
    // Two seconds of output, allowing for a loaded test runner.
    let expected = u64::from(pps) * 3 / 2;
    if after.0 - played <= expected {
        return Err(format!("played only {} points", after.0 - played));
    }
    Ok(())
}

// =============================================================================
// Discovery / broadcast (real port 7654)
// =============================================================================

/// Every field gets a distinct non-zero value, checked against its offset in
/// the 36-byte frame, so swapped or misplaced fields cannot cancel out in the
/// roundtrip.
#[test]
fn test_dac_broadcast_byte_roundtrip() {
    let bc = DacBroadcast {
        mac_address: [0x02, 0xed, 0x11, 0x22, 0x33, 0x44],
        hw_revision: 0x0102,
        sw_revision: 0x0304,
        buffer_capacity: 0x0506,
        max_point_rate: 0x0708_090a,
        dac_status: DacStatus {
            protocol: 0x0b,
            light_engine_state: DacStatus::LIGHT_ENGINE_WARMUP,
            playback_state: DacStatus::PLAYBACK_PLAYING,
            source: DacStatus::SOURCE_INTERNAL_ABSTRACT_GENERATOR,
            light_engine_flags: 0x0c0d,
            playback_flags: 0x0e0f,
            source_flags: 0x1011,
            buffer_fullness: 0x1213,
            point_rate: 0x1415_1617,
            point_count: 0x1819_1a1b,
        },
    };
    let bytes = encode_broadcast(&bc);
    #[rustfmt::skip]
    let expected: [u8; 36] = [
        0x02, 0xed, 0x11, 0x22, 0x33, 0x44, // mac_address
        0x02, 0x01,                         // hw_revision
        0x04, 0x03,                         // sw_revision
        0x06, 0x05,                         // buffer_capacity
        0x0a, 0x09, 0x08, 0x07,             // max_point_rate
        0x0b,                               // protocol
        0x01,                               // light_engine_state
        0x02,                               // playback_state
        0x02,                               // source
        0x0d, 0x0c,                         // light_engine_flags
        0x0f, 0x0e,                         // playback_flags
        0x11, 0x10,                         // source_flags
        0x13, 0x12,                         // buffer_fullness
        0x17, 0x16, 0x15, 0x14,             // point_rate
        0x1b, 0x1a, 0x19, 0x18,             // point_count
    ];
    assert_eq!(bytes, expected);
    let decoded = (&bytes[..])
        .read_bytes::<DacBroadcast>()
        .expect("decode broadcast");
    assert_eq!(decoded, bc);

    // The simulator's own broadcast survives the roundtrip too.
    for p in FirmwareProfile::all() {
        let srv = start(&p);
        let bc = srv.broadcast();
        let bytes = encode_broadcast(&bc);
        assert_eq!(bytes.len(), DacBroadcast::SIZE_BYTES);
        assert_eq!(DacBroadcast::SIZE_BYTES, 36);
        let decoded = (&bytes[..])
            .read_bytes::<DacBroadcast>()
            .expect("decode broadcast");
        assert_eq!(decoded, bc);
        assert_eq!(decoded.buffer_capacity, p.buffer_capacity);
    }
    // Responses are 22 bytes on the wire.
    assert_eq!(DacResponse::SIZE_BYTES, 22);
}

#[test]
fn test_recv_dac_broadcasts_receives_simulator_broadcast() {
    let _guard = port_lock();

    let mut rx = recv_dac_broadcasts().expect("bind broadcast listener on :7654");
    rx.set_timeout(Some(Duration::from_millis(500)))
        .expect("set timeout");

    let profile = FirmwareProfile::ed2_r331();
    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, BROADCAST_PORT));
    let srv = SimServer::start(
        SimServerConfig::new(profile.clone()).with_broadcast(target, Duration::from_millis(5)),
    )
    .expect("start simulator");

    // A real DAC on the LAN, or another program bound to 7654 with
    // SO_REUSEPORT, can show up here too. Wait for the simulator's MAC.
    let deadline = Instant::now() + Duration::from_secs(3);
    let (got, src) = loop {
        match rx.next_broadcast() {
            Ok((b, src)) if b.mac_address == profile.mac_address => break (b, src),
            Ok(_) => {}
            Err(e) if Instant::now() >= deadline => panic!("no broadcast received: {e}"),
            Err(_) => {}
        }
        assert!(
            Instant::now() < deadline,
            "simulator broadcast not received"
        );
    };
    assert_eq!(got.mac_address, profile.mac_address);
    assert_eq!(got.buffer_capacity, profile.buffer_capacity);
    assert_eq!(src.ip(), Ipv4Addr::LOCALHOST);
    drop(srv);
}

#[test]
fn test_discoverer_scan_and_connect() {
    let _guard = port_lock();

    let target = SocketAddr::from((Ipv4Addr::LOCALHOST, BROADCAST_PORT));
    let srv = SimServer::start(
        SimServerConfig::new(FirmwareProfile::ed2_r331())
            .with_broadcast(target, Duration::from_millis(5)),
    )
    .expect("start simulator");

    let mut enabled = EnabledDacTypes::none();
    enabled.enable(DacType::EtherDream);
    let mut discovery = DacDiscovery::new(enabled);
    let devices = discovery.scan();
    drop(srv);

    // A real DAC on the LAN may be found too; pick the simulator.
    let device = devices
        .into_iter()
        .find(|d| d.info().stable_id() == "etherdream:02:ed:00:00:00:02")
        .expect("simulator found, with a MAC-based stable id");
    assert_eq!(*device.dac_type(), DacType::EtherDream);
    assert_eq!(device.info().ip_address, Some(Ipv4Addr::LOCALHOST.into()));

    // The discoverer builds an Ether Dream FIFO backend without connecting.
    let backend = discovery.connect(device).expect("build backend");
    assert_eq!(backend.dac_type(), DacType::EtherDream);
}
