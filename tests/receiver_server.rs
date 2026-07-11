//! Behavior/lifecycle coverage for the IDN receiver UDP server.
//!
//! These tests bind the real [`IdnServer`] to an ephemeral loopback port and
//! drive it with hand-crafted IDN packets from a client `UdpSocket`, then
//! assert on the [`ServerBehavior`] callback surface and on the raw protocol
//! responses. Everything is loopback-only and uses short timeouts so the
//! suite stays fast and hermetic.

#![cfg(feature = "receiver")]

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use laser_dac::protocols::idn::protocol::IDNVAL_RTACK_ERR_EXCLUDED;
use laser_dac::receiver::{
    ChunkType, IdnServer, ReceivedChunk, Relay, ServerBehavior, ServerConfig, ServerHandle,
    Service, IDNFLG_STATUS_MALFUNCTION, IDNFLG_STATUS_REALTIME,
};

// IDN command bytes (client-side; not re-exported by the receiver module).
const IDNCMD_PING_REQUEST: u8 = 0x08;
const IDNCMD_SCAN_REQUEST: u8 = 0x10;
const IDNCMD_SCAN_RESPONSE: u8 = 0x11;
const IDNCMD_SERVICEMAP_REQUEST: u8 = 0x12;
const IDNCMD_SERVICEMAP_RESPONSE: u8 = 0x13;
const IDNCMD_RT_CNLMSG: u8 = 0x40;
const IDNCMD_RT_CNLMSG_ACKREQ: u8 = 0x41;
const IDNCMD_RT_CNLMSG_CLOSE: u8 = 0x44;
const IDNCMD_RT_ACK: u8 = 0x47;

const IDNCMD_PING_RESPONSE: u8 = 0x09;

// =============================================================================
// Recording behavior
// =============================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
struct RecordedChunk {
    sequence: u16,
    channel_id: u8,
    chunk_type: ChunkType,
    has_config: bool,
    config_or_last_fragment: bool,
    is_last_fragment: bool,
    timestamp_us_u32: u32,
    duration_us: u32,
    point_count: usize,
}

#[derive(Default)]
struct SharedState {
    chunks: Vec<RecordedChunk>,
    packets: usize,
    connects: usize,
    disconnects: usize,
}

struct RecordingBehavior {
    state: Arc<Mutex<SharedState>>,
    status: u8,
    ack_code: u8,
    excluded: bool,
    silent: bool,
}

impl RecordingBehavior {
    fn new() -> (Self, Arc<Mutex<SharedState>>) {
        let state = Arc::new(Mutex::new(SharedState::default()));
        (
            Self {
                state: Arc::clone(&state),
                status: IDNFLG_STATUS_REALTIME,
                ack_code: 0x00,
                excluded: false,
                silent: false,
            },
            state,
        )
    }
}

impl ServerBehavior for RecordingBehavior {
    fn on_packet_received(&mut self, _raw: &[u8]) {
        self.state.lock().unwrap().packets += 1;
    }

    fn on_chunk_received(&mut self, chunk: ReceivedChunk<'_>) {
        self.state.lock().unwrap().chunks.push(RecordedChunk {
            sequence: chunk.sequence,
            channel_id: chunk.channel_id,
            chunk_type: chunk.chunk_type,
            has_config: chunk.has_config,
            config_or_last_fragment: chunk.config_or_last_fragment,
            is_last_fragment: chunk.is_last_fragment,
            timestamp_us_u32: chunk.timestamp_us_u32,
            duration_us: chunk.duration_us,
            point_count: chunk.points.len(),
        });
    }

    fn should_respond(&self, _command: u8) -> bool {
        !self.silent
    }

    fn get_status_byte(&self) -> u8 {
        self.status
    }

    fn get_ack_result_code(&self) -> u8 {
        self.ack_code
    }

    fn is_excluded(&self) -> bool {
        self.excluded
    }

    fn on_client_connected(&mut self, _addr: SocketAddr) {
        self.state.lock().unwrap().connects += 1;
    }

    fn on_client_disconnected(&mut self) {
        self.state.lock().unwrap().disconnects += 1;
    }
}

// =============================================================================
// Harness helpers
// =============================================================================

/// A test config that binds an ephemeral loopback port and reacts to the stop
/// flag quickly (short read timeout) while not timing clients out mid-test.
fn test_config(hostname: &str) -> ServerConfig {
    ServerConfig::new(hostname)
        .with_read_timeout(Duration::from_millis(20))
        .with_link_timeout(Duration::from_secs(30))
}

fn spawn(config: ServerConfig, behavior: RecordingBehavior) -> ServerHandle {
    IdnServer::new(config, behavior)
        .expect("server should bind ephemeral loopback port")
        .spawn()
}

/// A client socket with a bounded read timeout for response assertions.
fn client_socket() -> UdpSocket {
    let sock = UdpSocket::bind("127.0.0.1:0").expect("client should bind");
    sock.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    sock
}

fn recv_response(sock: &UdpSocket) -> Option<Vec<u8>> {
    let mut buf = [0u8; 2048];
    sock.recv_from(&mut buf)
        .ok()
        .map(|(n, _)| buf[..n].to_vec())
}

fn wait_for_chunks(state: &Arc<Mutex<SharedState>>, n: usize) -> Vec<RecordedChunk> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        {
            let s = state.lock().unwrap();
            if s.chunks.len() >= n {
                return s.chunks.clone();
            }
        }
        if Instant::now() >= deadline {
            return state.lock().unwrap().chunks.clone();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

// =============================================================================
// IDN packet construction (client side)
// =============================================================================

fn header(command: u8, sequence: u16) -> Vec<u8> {
    let mut v = Vec::with_capacity(4);
    v.push(command);
    v.push(0x00);
    v.extend_from_slice(&sequence.to_be_bytes());
    v
}

/// XYRGBI descriptor array (8 bytes/sample: X16, Y16, R, G, B, I).
fn xyrgbi_descriptors() -> [u16; 8] {
    [
        0x4200, 0x4010, 0x4210, 0x4010, 0x527E, 0x5214, 0x51CC, 0x5C10,
    ]
}

fn xyrgbi_sample(x: i16, y: i16, r: u8, g: u8, b: u8, intensity: u8) -> Vec<u8> {
    let mut sample = Vec::new();
    sample.extend_from_slice(&x.to_be_bytes());
    sample.extend_from_slice(&y.to_be_bytes());
    sample.extend_from_slice(&[r, g, b, intensity]);
    sample
}

#[allow(clippy::too_many_arguments)]
fn channel_message_packet(
    command: u8,
    sequence: u16,
    channel_id: u8,
    chunk_type: u8,
    config_or_last_fragment: bool,
    include_config: bool,
    timestamp_us_u32: u32,
    samples: &[u8],
) -> Vec<u8> {
    let descriptors = xyrgbi_descriptors();
    let config_size = if include_config {
        4 + descriptors.len() * 2
    } else {
        0
    };
    let total_size = 8 + config_size + 4 + samples.len();
    let content_id = 0x8000u16
        | if config_or_last_fragment {
            0x4000
        } else {
            0x0000
        }
        | (((channel_id as u16) & 0x3F) << 8)
        | chunk_type as u16;

    let mut packet = header(command, sequence);
    packet.extend_from_slice(&(total_size as u16).to_be_bytes());
    packet.extend_from_slice(&content_id.to_be_bytes());
    packet.extend_from_slice(&timestamp_us_u32.to_be_bytes());

    if include_config {
        packet.push((descriptors.len() / 2) as u8);
        packet.push(0x01); // routing flag
        packet.push(channel_id + 1); // service_id
        packet.push(0x02); // service_mode: discrete frame
        for descriptor in descriptors {
            packet.extend_from_slice(&descriptor.to_be_bytes());
        }
    }

    packet.extend_from_slice(&1000u32.to_be_bytes()); // sample chunk header: duration 1000us
    packet.extend_from_slice(samples);
    packet
}

// =============================================================================
// Tests: ServerConfig options
// =============================================================================

#[test]
fn server_binds_ephemeral_loopback_port() {
    let (behavior, _state) = RecordingBehavior::new();
    let handle = spawn(test_config("EphemeralPort"), behavior);

    let addr = handle.addr;
    assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
    assert_ne!(addr.port(), 0, "port 0 should be resolved to a real port");
}

#[test]
fn server_config_defaults_bind_loopback_ephemeral() {
    // The default ServerConfig binds 127.0.0.1:0 for hermetic testing.
    let config = ServerConfig::new("DefaultBind");
    assert_eq!(config.bind_address.ip(), Ipv4Addr::LOCALHOST);
    assert_eq!(config.bind_address.port(), 0);
}

#[test]
fn servicemap_response_reflects_configured_services_and_relays() {
    let (behavior, _state) = RecordingBehavior::new();
    let config = test_config("ServiceMap")
        .with_services(vec![
            Service::laser_projector(1, "LaserA"),
            Service::dmx512(2, "DmxB").with_relay(1),
        ])
        .with_relays(vec![Relay::new(1, "RelayOne")]);
    let handle = spawn(config, behavior);

    let sock = client_socket();
    sock.send_to(&header(IDNCMD_SERVICEMAP_REQUEST, 0x0001), handle.addr)
        .unwrap();

    let resp = recv_response(&sock).expect("should get servicemap response");
    assert_eq!(resp[0], IDNCMD_SERVICEMAP_RESPONSE);
    assert_eq!(resp[6], 1, "relay_count");
    assert_eq!(resp[7], 2, "service_count");
}

// =============================================================================
// Tests: realtime status flag
// =============================================================================

#[test]
fn scan_response_carries_behavior_status_byte() {
    let (behavior, _state) = RecordingBehavior::new(); // default status = REALTIME
    let handle = spawn(test_config("StatusRealtime"), behavior);

    let sock = client_socket();
    sock.send_to(&header(IDNCMD_SCAN_REQUEST, 0x1234), handle.addr)
        .unwrap();

    let resp = recv_response(&sock).expect("should get scan response");
    assert_eq!(resp[0], IDNCMD_SCAN_RESPONSE);
    assert_eq!(resp[2..4], [0x12, 0x34], "sequence echoed");
    assert_eq!(
        resp[6], IDNFLG_STATUS_REALTIME,
        "status byte should report REALTIME"
    );
}

#[test]
fn scan_response_reflects_custom_status_byte() {
    let (mut behavior, _state) = RecordingBehavior::new();
    behavior.status = IDNFLG_STATUS_REALTIME | IDNFLG_STATUS_MALFUNCTION;
    let handle = spawn(test_config("StatusMalfunction"), behavior);

    let sock = client_socket();
    sock.send_to(&header(IDNCMD_SCAN_REQUEST, 0x0007), handle.addr)
        .unwrap();

    let resp = recv_response(&sock).expect("should get scan response");
    assert_eq!(resp[6], IDNFLG_STATUS_REALTIME | IDNFLG_STATUS_MALFUNCTION);
}

#[test]
fn silent_behavior_suppresses_responses() {
    let (mut behavior, state) = RecordingBehavior::new();
    behavior.silent = true;
    let handle = spawn(test_config("Silent"), behavior);

    let sock = client_socket();
    sock.send_to(&header(IDNCMD_SCAN_REQUEST, 0x0001), handle.addr)
        .unwrap();

    assert!(
        recv_response(&sock).is_none(),
        "silent server must not respond"
    );
    // But it still observed the packet.
    let deadline = Instant::now() + Duration::from_secs(1);
    while state.lock().unwrap().packets == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(state.lock().unwrap().packets >= 1);
}

// =============================================================================
// Tests: ping echo
// =============================================================================

#[test]
fn ping_response_echoes_payload() {
    let (behavior, _state) = RecordingBehavior::new();
    let handle = spawn(test_config("Ping"), behavior);

    let sock = client_socket();
    let mut ping = header(IDNCMD_PING_REQUEST, 0xABCD);
    ping.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
    sock.send_to(&ping, handle.addr).unwrap();

    let resp = recv_response(&sock).expect("should get ping response");
    assert_eq!(resp[0], IDNCMD_PING_RESPONSE);
    assert_eq!(resp[2..4], [0xAB, 0xCD]);
    assert_eq!(&resp[4..8], &[0xDE, 0xAD, 0xBE, 0xEF]);
}

// =============================================================================
// Tests: chunk callback surface (config vs data)
// =============================================================================

#[test]
fn on_chunk_reports_config_and_data_metadata() {
    let (behavior, state) = RecordingBehavior::new();
    let handle = spawn(test_config("ChunkMeta"), behavior);

    let sock = client_socket();

    // Packet 1: complete discrete frame WITH channel config on channel 3.
    let config_packet = channel_message_packet(
        IDNCMD_RT_CNLMSG,
        100,
        3,
        0x02, // Frame
        true, // config_or_last_fragment (config present)
        true, // include_config
        1_000,
        &xyrgbi_sample(0, 0, 255, 0, 0, 255),
    );
    // Packet 2: data-only frame on the same channel, reusing cached config.
    let data_packet = channel_message_packet(
        IDNCMD_RT_CNLMSG,
        101,
        3,
        0x02,  // Frame
        false, // no config bit
        false, // no config header
        2_000,
        &xyrgbi_sample(100, -100, 0, 255, 0, 255),
    );

    sock.send_to(&config_packet, handle.addr).unwrap();
    sock.send_to(&data_packet, handle.addr).unwrap();

    let chunks = wait_for_chunks(&state, 2);
    assert_eq!(chunks.len(), 2, "both frames should surface as chunks");

    let first = &chunks[0];
    assert_eq!(first.sequence, 100);
    assert_eq!(first.channel_id, 3);
    assert_eq!(first.chunk_type, ChunkType::Frame);
    assert!(first.has_config, "first chunk carries channel config");
    assert!(first.config_or_last_fragment);
    assert!(
        first.is_last_fragment,
        "a complete Frame is a last fragment"
    );
    assert_eq!(first.timestamp_us_u32, 1_000);
    assert_eq!(first.duration_us, 1000);
    assert_eq!(first.point_count, 1);

    let second = &chunks[1];
    assert_eq!(second.sequence, 101);
    assert_eq!(second.channel_id, 3);
    assert_eq!(second.chunk_type, ChunkType::Frame);
    assert!(!second.has_config, "data chunk reuses cached config");
    assert!(!second.config_or_last_fragment);
    assert_eq!(second.timestamp_us_u32, 2_000);
    assert_eq!(second.point_count, 1);

    // A client that sent real-time channel messages should have been observed
    // connecting exactly once (both packets share the same source IP).
    assert_eq!(state.lock().unwrap().connects, 1);
}

// =============================================================================
// Tests: ACK / excluded / close
// =============================================================================

#[test]
fn ackreq_frame_receives_ack_with_configured_result_code() {
    let (mut behavior, _state) = RecordingBehavior::new();
    behavior.ack_code = 0x00;
    let handle = spawn(test_config("AckReq"), behavior);

    let sock = client_socket();
    let pkt = channel_message_packet(
        IDNCMD_RT_CNLMSG_ACKREQ,
        5,
        0,
        0x02,
        true,
        true,
        1_000,
        &xyrgbi_sample(0, 0, 255, 255, 255, 255),
    );
    sock.send_to(&pkt, handle.addr).unwrap();

    let resp = recv_response(&sock).expect("ACKREQ should get an ACK");
    assert_eq!(resp[0], IDNCMD_RT_ACK);
    assert_eq!(resp[5], 0x00, "result code = success");
}

#[test]
fn excluded_behavior_rejects_ackreq_with_excluded_code() {
    let (mut behavior, _state) = RecordingBehavior::new();
    behavior.excluded = true;
    let handle = spawn(test_config("Excluded"), behavior);

    let sock = client_socket();
    let pkt = channel_message_packet(
        IDNCMD_RT_CNLMSG_ACKREQ,
        6,
        0,
        0x02,
        true,
        true,
        1_000,
        &xyrgbi_sample(0, 0, 255, 0, 0, 255),
    );
    sock.send_to(&pkt, handle.addr).unwrap();

    let resp = recv_response(&sock).expect("excluded ACKREQ should still get an ACK");
    assert_eq!(resp[0], IDNCMD_RT_ACK);
    assert_eq!(
        resp[5], IDNVAL_RTACK_ERR_EXCLUDED,
        "excluded group should return the EXCLUDED result code"
    );
}

#[test]
fn close_command_triggers_client_disconnected_callback() {
    let (behavior, state) = RecordingBehavior::new();
    let handle = spawn(test_config("Close"), behavior);

    let sock = client_socket();

    // Connect by sending a data frame first.
    let frame = channel_message_packet(
        IDNCMD_RT_CNLMSG,
        1,
        0,
        0x02,
        true,
        true,
        1_000,
        &xyrgbi_sample(0, 0, 255, 0, 0, 255),
    );
    sock.send_to(&frame, handle.addr).unwrap();
    let _ = wait_for_chunks(&state, 1);

    // Now close.
    sock.send_to(&header(IDNCMD_RT_CNLMSG_CLOSE, 2), handle.addr)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(1);
    while state.lock().unwrap().disconnects == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        state.lock().unwrap().disconnects,
        1,
        "RT_CNLMSG_CLOSE should fire on_client_disconnected"
    );
}

// =============================================================================
// Tests: malformed / truncated input resilience
// =============================================================================

#[test]
fn malformed_packets_do_not_kill_the_server_loop() {
    let (behavior, state) = RecordingBehavior::new();
    let handle = spawn(test_config("Malformed"), behavior);

    let sock = client_socket();

    // 1) Sub-header packet (len < 4) — dropped early.
    sock.send_to(&[0x40], handle.addr).unwrap();
    // 2) RT_CNLMSG with only a header, no channel message body.
    sock.send_to(&header(IDNCMD_RT_CNLMSG, 1), handle.addr)
        .unwrap();
    // 3) Channel message declaring a huge size but truncated on the wire.
    let mut truncated = header(IDNCMD_RT_CNLMSG, 2);
    truncated.extend_from_slice(&0xFF00u16.to_be_bytes()); // total_size
    truncated.extend_from_slice(&0x8002u16.to_be_bytes()); // content_id
    truncated.extend_from_slice(&[0u8; 4]); // partial timestamp
    sock.send_to(&truncated, handle.addr).unwrap();
    // 4) Random garbage.
    sock.send_to(&[0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA], handle.addr)
        .unwrap();

    // The loop must still be alive: a subsequent valid frame surfaces a chunk.
    let good = channel_message_packet(
        IDNCMD_RT_CNLMSG,
        50,
        1,
        0x02,
        true,
        true,
        3_000,
        &xyrgbi_sample(10, 20, 255, 0, 0, 255),
    );
    sock.send_to(&good, handle.addr).unwrap();

    let chunks = wait_for_chunks(&state, 1);
    assert_eq!(chunks.len(), 1, "server survived malformed input");
    assert_eq!(chunks[0].sequence, 50);
    assert_eq!(chunks[0].point_count, 1);
}

// =============================================================================
// Tests: ServerHandle stop() / Drop
// =============================================================================

#[test]
fn stop_halts_the_server_and_further_requests_go_unanswered() {
    let (behavior, _state) = RecordingBehavior::new();
    let handle = spawn(test_config("Stop"), behavior);
    let addr = handle.addr;

    let sock = client_socket();

    // Alive: a scan gets answered.
    sock.send_to(&header(IDNCMD_SCAN_REQUEST, 1), addr).unwrap();
    assert!(
        recv_response(&sock).is_some(),
        "server should answer before stop()"
    );

    // Stop and let the loop observe the flag (read_timeout = 20ms).
    handle.stop();
    std::thread::sleep(Duration::from_millis(120));

    // Dead: further scans get no response.
    sock.send_to(&header(IDNCMD_SCAN_REQUEST, 2), addr).unwrap();
    assert!(
        recv_response(&sock).is_none(),
        "stopped server must not answer"
    );
}

#[test]
fn stop_is_idempotent() {
    let (behavior, _state) = RecordingBehavior::new();
    let handle = spawn(test_config("StopTwice"), behavior);
    handle.stop();
    handle.stop(); // must not panic
}

#[test]
fn dropping_handle_stops_and_joins_without_hanging() {
    let (behavior, _state) = RecordingBehavior::new();
    let handle = spawn(test_config("DropJoin"), behavior);
    let addr = handle.addr;

    // Drop should set the stop flag and join the worker thread. If Drop hung,
    // this test would never complete.
    drop(handle);

    // Port should be free again; the server is gone, so nothing answers.
    let sock = client_socket();
    sock.send_to(&header(IDNCMD_SCAN_REQUEST, 1), addr).unwrap();
    assert!(recv_response(&sock).is_none());
}
