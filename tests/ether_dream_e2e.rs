//! End-to-end tests for the Ether Dream protocol stack with a mock DAC server.
//!
//! These tests exercise the discover -> connect -> command -> stream ->
//! disconnect lifecycle using an in-process mock that speaks the Ether Dream
//! wire protocol (TCP command channel + UDP broadcast discovery).
//!
//! ## Port constraints (why some tests are serialized)
//!
//! The production connect path hardcodes the DAC command port (7765) and the
//! discovery listener hardcodes the broadcast port (7654):
//!
//!   * [`stream::connect`]/[`stream::connect_timeout`] always dial
//!     `SocketAddr::new(dac_ip, COMMUNICATION_PORT)` — the caller only supplies
//!     the IP, never the port.
//!   * [`recv_dac_broadcasts`] binds `0.0.0.0:BROADCAST_PORT`.
//!
//! Because those ports are fixed, the mock cannot use an ephemeral port for
//! anything that flows through the real connect/discovery code. Tests that bind
//! a fixed port therefore acquire a process-global lock ([`port_lock`]) so they
//! run one-at-a-time and never collide with the parallel test harness. We bind
//! on `127.0.0.1` (loopback) so nothing leaves the machine.
//!
//! What is NOT covered, and why:
//!   * A live over-the-wire UDP *broadcast* round trip (an actual 255.255.255.255
//!     datagram). It is not hermetic in CI (needs a broadcast-capable interface
//!     and can leak onto the LAN), so discovery is exercised by delivering the
//!     same bytes as a loopback unicast datagram to the bound `0.0.0.0:7654`
//!     socket — the receive/parse path is identical.

#![cfg(feature = "ether-dream")]

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use laser_dac::device::DacType;
use laser_dac::discovery::DacDiscovery;
use laser_dac::protocols::ether_dream::dac::stream::{
    self, CommunicationError, Nak, ResponseErrorKind,
};
use laser_dac::protocols::ether_dream::protocol::{
    self, DacBroadcast, DacResponse, DacStatus, ReadBytes, SizeBytes, WriteBytes,
};
use laser_dac::protocols::ether_dream::{recv_dac_broadcasts, EtherDreamBackend};
use laser_dac::types::EnabledDacTypes;
use laser_dac::{DacBackend, FifoBackend, LaserPoint, WriteOutcome};

const COMM_PORT: u16 = protocol::COMMUNICATION_PORT; // 7765
const BROADCAST_PORT: u16 = protocol::BROADCAST_PORT; // 7654

// Command start bytes (mirrors protocol::command::*::START_BYTE).
const CMD_PREPARE: u8 = 0x70;
const CMD_BEGIN: u8 = 0x62;
const CMD_UPDATE: u8 = 0x75;
const CMD_POINT_RATE: u8 = 0x74;
const CMD_DATA: u8 = 0x64;
const CMD_STOP: u8 = 0x73;
const CMD_ESTOP: u8 = 0x00;
const CMD_ESTOP_ALT: u8 = 0xff;
const CMD_CLEAR_ESTOP: u8 = 0x63;
const CMD_PING: u8 = 0x3f;

/// Serializes every test that binds a fixed port (7765 or 7654) so the parallel
/// harness never causes an `AddrInUse` collision. Poison-tolerant.
fn port_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// =============================================================================
// Protocol byte helpers (exercise the real WriteToBytes/ReadFromBytes impls)
// =============================================================================

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

fn broadcast_with(status: DacStatus, buffer_capacity: u16) -> DacBroadcast {
    DacBroadcast {
        mac_address: [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01],
        hw_revision: 1,
        sw_revision: 2,
        buffer_capacity,
        max_point_rate: 100_000,
        dac_status: status,
    }
}

fn encode_response(resp: &DacResponse) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(DacResponse::SIZE_BYTES);
    buf.write_bytes(*resp).expect("encode DacResponse");
    buf
}

fn encode_broadcast(bc: &DacBroadcast) -> Vec<u8> {
    let mut buf: Vec<u8> = Vec::with_capacity(DacBroadcast::SIZE_BYTES);
    buf.write_bytes(*bc).expect("encode DacBroadcast");
    buf
}

// =============================================================================
// Mock Ether Dream DAC (TCP command server)
// =============================================================================

/// Immutable behavior configuration for the mock server.
#[derive(Clone)]
struct MockConfig {
    /// Status echoed in the initial (post-connect) handshake response.
    handshake_status: DacStatus,
    /// Capacity used to clamp the simulated `buffer_fullness`.
    buffer_capacity: u16,
    /// Per-command response-code override (e.g. NAK on a `data` command).
    nak_for: HashMap<u8, u8>,
    /// Answer at most this many *post-handshake* commands, then close the socket.
    answer_limit: Option<usize>,
    /// For the Nth post-handshake command (0-based), send a truncated 2-byte
    /// reply and then close — simulates a malformed/short response.
    short_at: Option<usize>,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            handshake_status: ready_status(),
            buffer_capacity: 1800,
            nak_for: HashMap::new(),
            answer_limit: None,
            short_at: None,
        }
    }
}

struct MockShared {
    status: Mutex<DacStatus>,
    received: Mutex<Vec<u8>>,
    data_points_total: Mutex<u32>,
    config: MockConfig,
}

/// A running mock DAC. Bound to `127.0.0.1:7765`. Drop stops the server and
/// frees the port (join happens before the [`port_lock`] guard is released,
/// provided the guard is declared *before* the server — Rust drops in reverse).
struct MockServer {
    shared: Arc<MockShared>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    addr: SocketAddr,
}

impl MockServer {
    fn start(config: MockConfig) -> MockServer {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, COMM_PORT))
            .expect("bind mock DAC on 127.0.0.1:7765");
        let addr = listener.local_addr().expect("local addr");
        listener
            .set_nonblocking(true)
            .expect("listener nonblocking");

        let shared = Arc::new(MockShared {
            status: Mutex::new(config.handshake_status),
            received: Mutex::new(Vec::new()),
            data_points_total: Mutex::new(0),
            config,
        });
        let stop = Arc::new(AtomicBool::new(false));

        let thread = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            thread::spawn(move || accept_loop(listener, shared, stop))
        };

        MockServer {
            shared,
            stop,
            thread: Some(thread),
            addr,
        }
    }

    fn ip(&self) -> std::net::IpAddr {
        self.addr.ip()
    }

    fn received(&self) -> Vec<u8> {
        self.shared.received.lock().unwrap().clone()
    }

    fn data_points_total(&self) -> u32 {
        *self.shared.data_points_total.lock().unwrap()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn accept_loop(listener: TcpListener, shared: Arc<MockShared>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((sock, _peer)) => {
                sock.set_nonblocking(false).ok();
                sock.set_read_timeout(Some(Duration::from_millis(50))).ok();
                handle_connection(sock, &shared, &stop);
            }
            Err(ref e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => break,
        }
    }
}

fn handle_connection(mut sock: TcpStream, shared: &Arc<MockShared>, stop: &Arc<AtomicBool>) {
    // 1) Initial handshake: the real DAC greets a fresh connection with a status
    //    frame that the client reads as a response to a "ping" (0x3f).
    let handshake = DacResponse {
        response: DacResponse::ACK,
        command: CMD_PING,
        dac_status: shared.config.handshake_status,
    };
    if sock.write_all(&encode_response(&handshake)).is_err() {
        return;
    }

    let mut idx = 0usize;
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if let Some(limit) = shared.config.answer_limit {
            if idx >= limit {
                return; // Simulated mid-stream disconnect.
            }
        }

        let mut cmd = [0u8; 1];
        match sock.read(&mut cmd) {
            Ok(0) => return, // EOF: client closed.
            Ok(_) => {}
            Err(ref e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                continue; // Idle; loop back and re-check stop.
            }
            Err(_) => return,
        }

        let c = cmd[0];
        let n_points = match read_command_payload(&mut sock, c) {
            Some(n) => n,
            None => return, // Unknown command or truncated read.
        };

        // Record the command as received BEFORE sending the ACK. The client's
        // `submit()` returns as soon as it reads the ACK, so a test that reads
        // `received()` right after `submit()` would otherwise race this thread
        // and miss the last command. Recording first makes it deterministic.
        shared.received.lock().unwrap().push(c);

        // Advance the simulated device status.
        {
            let mut st = shared.status.lock().unwrap();
            match c {
                CMD_PREPARE => {
                    st.playback_state = DacStatus::PLAYBACK_PREPARED;
                    st.playback_flags = 0;
                }
                CMD_BEGIN => st.playback_state = DacStatus::PLAYBACK_PLAYING,
                CMD_DATA => {
                    let cap = shared.config.buffer_capacity as u32;
                    let new = (st.buffer_fullness as u32 + n_points as u32).min(cap);
                    st.buffer_fullness = new as u16;
                    st.point_count = st.point_count.wrapping_add(n_points as u32);
                    *shared.data_points_total.lock().unwrap() += n_points as u32;
                }
                CMD_STOP => {
                    st.playback_state = DacStatus::PLAYBACK_IDLE;
                    st.buffer_fullness = 0;
                }
                CMD_ESTOP | CMD_ESTOP_ALT => {
                    st.light_engine_state = DacStatus::LIGHT_ENGINE_EMERGENCY_STOP;
                    st.playback_flags = 0b0000_0100; // EMERGENCY_STOP
                }
                CMD_CLEAR_ESTOP => {
                    st.light_engine_state = DacStatus::LIGHT_ENGINE_READY;
                    st.playback_flags = 0;
                }
                _ => {}
            }
        }

        let status_snapshot = *shared.status.lock().unwrap();
        let code = shared
            .config
            .nak_for
            .get(&c)
            .copied()
            .unwrap_or(DacResponse::ACK);
        let resp = DacResponse {
            response: code,
            command: c,
            dac_status: status_snapshot,
        };

        if shared.config.short_at == Some(idx) {
            // Truncated reply then close: exercises the client's read_exact EOF path.
            let bytes = encode_response(&resp);
            let _ = sock.write_all(&bytes[..2]);
            return;
        }

        if sock.write_all(&encode_response(&resp)).is_err() {
            return;
        }
        idx += 1;
    }
}

/// Consume the fixed-size payload that follows a command byte. Returns the point
/// count for `data` commands (0 for all others), or `None` on an unknown command
/// or a truncated read.
fn read_command_payload(sock: &mut TcpStream, cmd: u8) -> Option<u16> {
    let extra = match cmd {
        CMD_PREPARE | CMD_STOP | CMD_ESTOP | CMD_ESTOP_ALT | CMD_CLEAR_ESTOP | CMD_PING => 0,
        CMD_BEGIN | CMD_UPDATE => 6, // u16 low_water_mark + u32 point_rate
        CMD_POINT_RATE => 4,         // u32 point_rate
        CMD_DATA => {
            let mut n = [0u8; 2];
            sock.read_exact(&mut n).ok()?;
            let n_points = u16::from_le_bytes(n);
            let mut pts = vec![0u8; n_points as usize * 18];
            sock.read_exact(&mut pts).ok()?;
            return Some(n_points);
        }
        _ => return None,
    };
    if extra > 0 {
        let mut buf = vec![0u8; extra];
        sock.read_exact(&mut buf).ok()?;
    }
    Some(0)
}

/// Convenience: connect a real [`stream::Stream`] to a mock server.
fn connect_stream(server: &MockServer, broadcast: &DacBroadcast) -> stream::Stream {
    stream::connect(broadcast, server.ip()).expect("stream should connect")
}

// =============================================================================
// Stream-layer tests (real connect path, port 7765)
// =============================================================================

#[test]
fn test_connect_handshake_and_initial_status() {
    let _guard = port_lock();

    let mut status = ready_status();
    status.buffer_fullness = 42;
    status.point_rate = 30_000;
    let server = MockServer::start(MockConfig {
        handshake_status: status,
        ..MockConfig::default()
    });

    let broadcast = broadcast_with(ready_status(), 1800);
    let stream = connect_stream(&server, &broadcast);

    // Handshake response status must have been folded into the DAC state.
    let dac = stream.dac();
    assert_eq!(dac.buffer_capacity, 1800, "capacity comes from broadcast");
    assert_eq!(dac.max_point_rate, 100_000);
    assert_eq!(dac.status.buffer_fullness, 42, "fullness from handshake");
    assert_eq!(dac.status.point_rate, 30_000);
    assert_eq!(
        dac.mac_address.0,
        [0xde, 0xad, 0xbe, 0xef, 0x00, 0x01],
        "mac comes from broadcast"
    );

    // No commands were sent yet.
    assert!(server.received().is_empty());

    drop(stream);
}

#[test]
fn test_prepare_begin_update_pointrate_roundtrip() {
    let _guard = port_lock();
    let server = MockServer::start(MockConfig::default());
    let broadcast = broadcast_with(ready_status(), 1800);
    let mut stream = connect_stream(&server, &broadcast);

    stream
        .queue_commands()
        .prepare_stream()
        .submit()
        .expect("prepare ok");
    stream
        .queue_commands()
        .point_rate(25_000)
        .submit()
        .expect("point_rate ok");
    stream
        .queue_commands()
        .begin(0, 30_000)
        .submit()
        .expect("begin ok");
    stream
        .queue_commands()
        .update(0, 40_000)
        .submit()
        .expect("update ok");

    // Multiple commands can be queued and submitted as one batch.
    stream
        .queue_commands()
        .prepare_stream()
        .begin(0, 30_000)
        .submit()
        .expect("batch ok");

    assert_eq!(
        server.received(),
        vec![
            CMD_PREPARE,
            CMD_POINT_RATE,
            CMD_BEGIN,
            CMD_UPDATE,
            CMD_PREPARE,
            CMD_BEGIN,
        ]
    );

    drop(stream);
}

#[test]
fn test_data_buffer_fullness_accounting() {
    let _guard = port_lock();
    let server = MockServer::start(MockConfig::default());
    let broadcast = broadcast_with(ready_status(), 1800);
    let mut stream = connect_stream(&server, &broadcast);

    let point = protocol::DacPoint {
        control: 0,
        x: 0,
        y: 0,
        r: 0,
        g: 0,
        b: 0,
        i: 0,
        u1: 0,
        u2: 0,
    };

    stream
        .queue_commands()
        .data(std::iter::repeat_n(point, 100))
        .submit()
        .expect("first data ok");
    assert_eq!(stream.dac().status.buffer_fullness, 100);

    stream
        .queue_commands()
        .data(std::iter::repeat_n(point, 250))
        .submit()
        .expect("second data ok");
    assert_eq!(
        stream.dac().status.buffer_fullness,
        350,
        "fullness accumulates across writes"
    );

    assert_eq!(server.data_points_total(), 350);
    assert_eq!(server.received(), vec![CMD_DATA, CMD_DATA]);

    drop(stream);
}

#[test]
fn test_nak_full_on_data_surfaces_response_error() {
    let _guard = port_lock();
    let mut nak_for = HashMap::new();
    nak_for.insert(CMD_DATA, DacResponse::NAK_FULL);
    let server = MockServer::start(MockConfig {
        nak_for,
        ..MockConfig::default()
    });
    let broadcast = broadcast_with(ready_status(), 1800);
    let mut stream = connect_stream(&server, &broadcast);

    let point = protocol::DacPoint {
        control: 0,
        x: 1,
        y: 2,
        r: 3,
        g: 4,
        b: 5,
        i: 6,
        u1: 0,
        u2: 0,
    };
    let err = stream
        .queue_commands()
        .data(std::iter::once(point))
        .submit()
        .expect_err("NAK-Full should surface as an error");

    match err {
        CommunicationError::Response(resp_err) => {
            assert!(matches!(resp_err.kind, ResponseErrorKind::Nak(Nak::Full)));
            assert_eq!(resp_err.response.response, DacResponse::NAK_FULL);
        }
        other => panic!("expected Response(Nak::Full), got {other:?}"),
    }

    drop(stream);
}

#[test]
fn test_stop_estop_clear_ping_roundtrip() {
    let _guard = port_lock();
    let server = MockServer::start(MockConfig::default());
    let broadcast = broadcast_with(ready_status(), 1800);
    let mut stream = connect_stream(&server, &broadcast);

    stream.queue_commands().ping().submit().expect("ping ok");
    stream
        .queue_commands()
        .emergency_stop()
        .submit()
        .expect("estop ok");
    // Light engine should now report emergency stop.
    assert_eq!(
        stream.dac().status.light_engine,
        laser_dac::protocols::ether_dream::dac::LightEngine::EmergencyStop
    );

    stream
        .queue_commands()
        .clear_emergency_stop()
        .submit()
        .expect("clear estop ok");
    assert_eq!(
        stream.dac().status.light_engine,
        laser_dac::protocols::ether_dream::dac::LightEngine::Ready
    );

    stream.queue_commands().stop().submit().expect("stop ok");

    assert_eq!(
        server.received(),
        vec![CMD_PING, CMD_ESTOP, CMD_CLEAR_ESTOP, CMD_STOP]
    );

    drop(stream);
}

#[test]
fn test_server_disconnect_after_handshake_surfaces_io_error() {
    let _guard = port_lock();
    // answer_limit = 0: greet, then immediately close the connection.
    let server = MockServer::start(MockConfig {
        answer_limit: Some(0),
        ..MockConfig::default()
    });
    let broadcast = broadcast_with(ready_status(), 1800);
    let mut stream = connect_stream(&server, &broadcast);

    let err = stream
        .queue_commands()
        .prepare_stream()
        .submit()
        .expect_err("submit after disconnect should fail");
    assert!(
        matches!(err, CommunicationError::Io(_)),
        "expected an IO error, got {err:?}"
    );

    drop(stream);
}

#[test]
fn test_short_response_surfaces_io_error() {
    let _guard = port_lock();
    // The first post-handshake command gets a truncated (2-byte) reply.
    let server = MockServer::start(MockConfig {
        short_at: Some(0),
        ..MockConfig::default()
    });
    let broadcast = broadcast_with(ready_status(), 1800);
    let mut stream = connect_stream(&server, &broadcast);

    let err = stream
        .queue_commands()
        .prepare_stream()
        .submit()
        .expect_err("short response should fail read_exact");
    assert!(
        matches!(err, CommunicationError::Io(_)),
        "expected an IO error from the truncated response, got {err:?}"
    );

    drop(stream);
}

#[test]
fn test_invalid_status_in_handshake_surfaces_protocol_error() {
    let _guard = port_lock();
    // A handshake status with an out-of-range light-engine state must fail to
    // parse in Status::from_protocol (via recv_response_buffered -> update_status).
    let mut bad = ready_status();
    bad.light_engine_state = 99;
    let server = MockServer::start(MockConfig {
        handshake_status: bad,
        ..MockConfig::default()
    });

    // The broadcast itself is valid, so connect gets past Addressed::from_broadcast
    // and fails only when it reads the malformed handshake response.
    let broadcast = broadcast_with(ready_status(), 1800);
    // `Stream` is not `Debug`, so match the Result explicitly rather than using
    // `expect_err`.
    match stream::connect(&broadcast, server.ip()) {
        Ok(_) => panic!("connect should have failed on a malformed status"),
        Err(CommunicationError::Protocol(_)) => {}
        Err(other) => panic!("expected a protocol error, got {other:?}"),
    }
}

// =============================================================================
// Backend-layer tests (EtherDreamBackend, real connect path, port 7765)
// =============================================================================

#[test]
fn test_backend_connect_disconnect_lifecycle() {
    let _guard = port_lock();
    let server = MockServer::start(MockConfig::default());
    let broadcast = broadcast_with(ready_status(), 1800);

    let mut backend = EtherDreamBackend::new(broadcast, server.ip());
    assert_eq!(backend.dac_type(), DacType::EtherDream);
    assert!(!backend.is_connected());

    backend.connect().expect("backend connect");
    assert!(backend.is_connected());

    // disconnect() sends a stop command as a courtesy, then drops the stream.
    backend.disconnect().expect("backend disconnect");
    assert!(!backend.is_connected());

    drop(server);
}

#[test]
fn test_backend_not_connected_returns_disconnected() {
    // No server, no connect: the FIFO write must report a disconnected error.
    let broadcast = broadcast_with(ready_status(), 1800);
    let mut backend = EtherDreamBackend::new(broadcast, Ipv4Addr::LOCALHOST.into());

    let pts = vec![LaserPoint::new(0.0, 0.0, 0, 0, 0, 0)];
    let err = backend
        .try_write_points(30_000, &pts)
        .expect_err("write without connect should fail");
    assert!(err.is_disconnected(), "expected Disconnected, got {err:?}");
}

#[test]
fn test_backend_full_buffer_reports_would_block() {
    let _guard = port_lock();
    // Ready light engine, but the buffer is reported completely full.
    let mut full = ready_status();
    full.buffer_fullness = 100;
    let server = MockServer::start(MockConfig {
        handshake_status: full,
        buffer_capacity: 100,
        ..MockConfig::default()
    });
    let broadcast = broadcast_with(full, 100);

    let mut backend = EtherDreamBackend::new(broadcast, server.ip());
    backend.connect().expect("connect");

    let pts = vec![LaserPoint::new(0.0, 0.0, 0, 0, 0, 0); 32];
    let outcome = backend
        .try_write_points(30_000, &pts)
        .expect("full buffer is backpressure, not an error");
    assert_eq!(outcome, WriteOutcome::WouldBlock);

    // Because it short-circuited on backpressure, no command was sent.
    assert!(server.received().is_empty());

    drop(server);
}

#[test]
fn test_backend_write_points_prepares_and_writes() {
    let _guard = port_lock();
    let server = MockServer::start(MockConfig::default());
    let broadcast = broadcast_with(ready_status(), 1800);

    let mut backend = EtherDreamBackend::new(broadcast, server.ip());
    backend.connect().expect("connect");

    // Enough points that the mock's fullness crosses the begin threshold (256).
    let pts = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535); 300];
    let outcome = backend
        .try_write_points(30_000, &pts)
        .expect("write should succeed");
    assert_eq!(outcome, WriteOutcome::Written);

    let received = server.received();
    assert!(
        received.contains(&CMD_PREPARE),
        "idle DAC should be prepared first: {received:?}"
    );
    assert!(
        received.contains(&CMD_DATA),
        "points should be written: {received:?}"
    );
    assert!(
        received.contains(&CMD_BEGIN),
        "playback should begin once buffered: {received:?}"
    );
    assert!(server.data_points_total() >= 256);

    drop(server);
}

#[test]
fn test_backend_server_disconnect_returns_error() {
    let _guard = port_lock();
    let server = MockServer::start(MockConfig {
        answer_limit: Some(0), // close right after handshake
        ..MockConfig::default()
    });
    let broadcast = broadcast_with(ready_status(), 1800);

    let mut backend = EtherDreamBackend::new(broadcast, server.ip());
    backend.connect().expect("connect");

    let pts = vec![LaserPoint::new(0.0, 0.0, 0, 0, 0, 0); 8];
    // The DAC has vanished; the first command submit fails. At the FIFO layer an
    // IO error is wrapped as Error::Backend (not Disconnected) — Disconnected is
    // reserved for the "not connected" guard exercised in the test above.
    let err = backend
        .try_write_points(30_000, &pts)
        .expect_err("write to a vanished DAC should fail");
    assert!(!err.is_would_block(), "a dead socket is not backpressure");

    drop(server);
}

// =============================================================================
// Discovery / broadcast tests
// =============================================================================

#[test]
fn test_dac_broadcast_byte_roundtrip() {
    // Pure protocol round trip — no sockets, exercises Write/ReadFromBytes for
    // DacBroadcast + the embedded DacStatus.
    let mut status = ready_status();
    status.buffer_fullness = 1234;
    status.point_rate = 42_000;
    status.point_count = 7;
    status.light_engine_state = DacStatus::LIGHT_ENGINE_WARMUP;
    status.playback_state = DacStatus::PLAYBACK_PLAYING;
    let bc = broadcast_with(status, 2000);

    let bytes = encode_broadcast(&bc);
    assert_eq!(bytes.len(), DacBroadcast::SIZE_BYTES);
    assert_eq!(DacBroadcast::SIZE_BYTES, 36);

    let mut slice = &bytes[..];
    let decoded = slice
        .read_bytes::<DacBroadcast>()
        .expect("decode broadcast");
    assert_eq!(decoded, bc);
}

#[test]
fn test_recv_dac_broadcasts_receives_datagram() {
    let _guard = port_lock();

    let mut rx = recv_dac_broadcasts().expect("bind broadcast listener on :7654");
    rx.set_timeout(Some(Duration::from_millis(500)))
        .expect("set timeout");

    let mut status = ready_status();
    status.buffer_fullness = 555;
    let bc = broadcast_with(status, 1800);
    let bytes = encode_broadcast(&bc);

    // Deliver the broadcast payload as a loopback unicast datagram (see module
    // docs for why we don't emit a real network broadcast).
    let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind sender");
    sender
        .send_to(&bytes, (Ipv4Addr::LOCALHOST, BROADCAST_PORT))
        .expect("send broadcast");

    let (got, src) = rx.next_broadcast().expect("should receive broadcast");
    assert_eq!(got, bc);
    assert_eq!(src.ip(), Ipv4Addr::LOCALHOST);
}

#[test]
fn test_discoverer_scan_and_connect() {
    let _guard = port_lock();

    let mut status = ready_status();
    status.buffer_fullness = 10;
    let bc = broadcast_with(status, 1800);
    let bytes = encode_broadcast(&bc);

    // Keep pumping broadcasts on 127.0.0.1:7654 while the discoverer scans.
    let stop = Arc::new(AtomicBool::new(false));
    let sender_stop = Arc::clone(&stop);
    let sender = thread::spawn(move || {
        let sock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind sender");
        while !sender_stop.load(Ordering::SeqCst) {
            let _ = sock.send_to(&bytes, (Ipv4Addr::LOCALHOST, BROADCAST_PORT));
            thread::sleep(Duration::from_millis(5));
        }
    });

    let mut enabled = EnabledDacTypes::none();
    enabled.enable(DacType::EtherDream);
    let mut discovery = DacDiscovery::new(enabled);

    let devices = discovery.scan();

    stop.store(true, Ordering::SeqCst);
    sender.join().expect("sender thread");

    assert!(!devices.is_empty(), "discoverer should find the mock DAC");
    let device = devices.into_iter().next().unwrap();
    assert_eq!(*device.dac_type(), DacType::EtherDream);
    let info = device.info();
    assert_eq!(info.ip_address, Some(Ipv4Addr::LOCALHOST.into()));
    assert!(
        info.stable_id().starts_with("etherdream:"),
        "stable id should be mac-based: {}",
        info.stable_id()
    );

    // The registry hands the opaque connect data back to the discoverer, which
    // must build an Ether Dream FIFO backend (no TCP connection is attempted).
    let backend = discovery.connect(device).expect("build backend");
    assert_eq!(backend.dac_type(), DacType::EtherDream);
}
