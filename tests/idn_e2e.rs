//! End-to-end tests for IDN protocol with mock server.
//!
//! These tests verify the full discovery -> connect -> stream -> disconnect -> reconnect
//! lifecycle using a mock UDP server that speaks the IDN protocol.

#![cfg(all(feature = "idn", feature = "testutils"))]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use idn_mock_server::{
    MockIdnServer, MockRelay, MockService, ServerBehavior, ServerConfig, ServerHandle,
    IDNCMD_RT_CNLMSG, IDNCMD_RT_CNLMSG_CLOSE_ACKREQ, IDNCMD_UNIT_PARAMS_REQUEST,
    IDNCMD_SERVICE_PARAMS_REQUEST, IDNFLG_STATUS_REALTIME,
};

use laser_dac::types::{DacType, EnabledDacTypes, LaserPoint, StreamConfig};
use laser_dac::{caps_for_dac_type, Dac, DacInfo};

/// Create an EnabledDacTypes with only IDN enabled.
fn idn_only() -> EnabledDacTypes {
    let mut types = EnabledDacTypes::none();
    types.enable(DacType::Idn);
    types
}

fn connect_dac(
    discovery: &mut laser_dac::DacDiscovery,
    device: laser_dac::DiscoveredDevice,
) -> Dac {
    let info = device.info();
    let dac_type = device.dac_type();
    let backend = discovery.connect(device).expect("Should connect");
    let dac_info = DacInfo::new(
        info.stable_id(),
        info.name(),
        dac_type.clone(),
        caps_for_dac_type(&dac_type),
    );
    Dac::new(dac_info, backend)
}

// =============================================================================
// Test Behavior Implementation
// =============================================================================

/// Test-specific behavior for the mock server.
pub struct TestBehavior {
    pub disconnected: Arc<AtomicBool>,
    pub silent: bool,
    pub received_packets: Arc<Mutex<Vec<Vec<u8>>>>,
    pub status: u8,
    pub ack_error_code: u8,
}

impl Default for TestBehavior {
    fn default() -> Self {
        Self {
            disconnected: Arc::new(AtomicBool::new(false)),
            silent: false,
            received_packets: Arc::new(Mutex::new(Vec::new())),
            status: IDNFLG_STATUS_REALTIME,
            ack_error_code: 0x00,
        }
    }
}

impl TestBehavior {
    pub fn silent(mut self) -> Self {
        self.silent = true;
        self
    }

    pub fn with_status(mut self, status: u8) -> Self {
        self.status = status;
        self
    }
}

impl ServerBehavior for TestBehavior {
    fn on_packet_received(&mut self, raw_data: &[u8]) {
        self.received_packets
            .lock()
            .unwrap()
            .push(raw_data.to_vec());
    }

    fn on_frame_received(&mut self, _raw_data: &[u8]) {
        // Frame data is already captured in on_packet_received
    }

    fn should_respond(&self, _command: u8) -> bool {
        !self.silent && !self.disconnected.load(Ordering::SeqCst)
    }

    fn get_status_byte(&self) -> u8 {
        self.status
    }

    fn get_ack_result_code(&self) -> u8 {
        self.ack_error_code
    }
}

// =============================================================================
// Test Server Handle
// =============================================================================

/// Handle to a running mock IDN server with test utilities.
pub struct TestServerHandle {
    inner: ServerHandle,
    pub disconnected: Arc<AtomicBool>,
    pub received_packets: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl TestServerHandle {
    /// Get the server's address.
    pub fn addr(&self) -> SocketAddr {
        self.inner.addr
    }

    /// Simulate a disconnection (server stops responding).
    pub fn simulate_disconnect(&self) {
        self.disconnected.store(true, Ordering::SeqCst);
    }

    /// Resume normal operation after simulated disconnect.
    pub fn resume(&self) {
        self.disconnected.store(false, Ordering::SeqCst);
    }

    /// Get the number of packets received by the server.
    pub fn received_packet_count(&self) -> usize {
        self.received_packets.lock().unwrap().len()
    }

    /// Clear received packets.
    pub fn clear_received_packets(&self) {
        self.received_packets.lock().unwrap().clear();
    }

    /// Check if server received any frame data packets.
    pub fn received_frame_data(&self) -> bool {
        let packets = self.received_packets.lock().unwrap();
        packets
            .iter()
            .any(|p| !p.is_empty() && p[0] == IDNCMD_RT_CNLMSG)
    }
}

// =============================================================================
// Test Server Builder
// =============================================================================

/// Builder for test mock servers.
pub struct TestServerBuilder {
    config: ServerConfig,
    behavior: TestBehavior,
}

impl TestServerBuilder {
    /// Create a new builder with the given hostname.
    pub fn new(hostname: &str) -> Self {
        Self {
            config: ServerConfig::new(hostname),
            behavior: TestBehavior::default(),
        }
    }

    /// Set a custom unit ID.
    pub fn unit_id(mut self, unit_id: [u8; 16]) -> Self {
        self.config = self.config.with_unit_id(unit_id);
        self
    }

    /// Set the protocol version (major.minor packed into single byte).
    pub fn protocol_version(mut self, major: u8, minor: u8) -> Self {
        self.config = self.config.with_protocol_version(major, minor);
        self
    }

    /// Set the status byte.
    pub fn status(mut self, status: u8) -> Self {
        self.behavior = self.behavior.with_status(status);
        self
    }

    /// Set the services this server provides.
    pub fn services(mut self, services: Vec<MockService>) -> Self {
        self.config = self.config.with_services(services);
        self
    }

    /// Set the relays this server provides.
    pub fn relays(mut self, relays: Vec<MockRelay>) -> Self {
        self.config = self.config.with_relays(relays);
        self
    }

    /// Enable silent mode (server receives but never responds).
    pub fn silent(mut self) -> Self {
        self.behavior = self.behavior.silent();
        self
    }

    /// Build and start the server.
    pub fn build(self) -> std::io::Result<TestServerHandle> {
        let disconnected = Arc::clone(&self.behavior.disconnected);
        let received_packets = Arc::clone(&self.behavior.received_packets);

        let server = MockIdnServer::new(self.config, self.behavior)?;
        let inner = server.spawn();

        Ok(TestServerHandle {
            inner,
            disconnected,
            received_packets,
        })
    }
}

/// Create a simple test server with default configuration.
pub fn test_server(hostname: &str) -> std::io::Result<TestServerHandle> {
    TestServerBuilder::new(hostname).build()
}

// =============================================================================
// Tests
// =============================================================================

#[test]
fn test_scanner_direct() {
    use laser_dac::protocols::idn::ServerScanner;

    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();
    eprintln!("Mock server listening on: {}", server_addr);

    // Give server time to start
    thread::sleep(Duration::from_millis(50));

    // Directly test the scanner
    let mut scanner = ServerScanner::new(0).expect("Failed to create scanner");
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .expect("Scan failed");

    eprintln!("Found {} servers", servers.len());
    for s in &servers {
        eprintln!("  Server: {} at {:?}", s.hostname, s.addresses);
    }

    assert!(!servers.is_empty(), "Scanner should find the mock server");
}

#[test]
fn test_idn_discovery_direct() {
    use laser_dac::discovery::IdnDiscovery;

    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();
    eprintln!("Mock server listening on: {}", server_addr);

    thread::sleep(Duration::from_millis(50));

    let mut idn_discovery = IdnDiscovery::new();
    let devices = idn_discovery.scan_address(server_addr);

    eprintln!("Found {} devices", devices.len());
    for d in &devices {
        eprintln!("  Device: {} ({:?})", d.name(), d.dac_type());
    }

    assert!(
        !devices.is_empty(),
        "IdnDiscovery should find the mock server"
    );
}

#[test]
fn test_discover_and_connect() {
    use laser_dac::discovery::DacDiscovery;

    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();
    eprintln!("Mock server listening on: {}", server_addr);

    // Give server time to start
    thread::sleep(Duration::from_millis(50));

    eprintln!("Creating discovery...");
    let mut discovery = DacDiscovery::new(idn_only());
    discovery.set_idn_scan_addresses(vec![server_addr]);

    eprintln!("Waiting for discovery...");
    for i in 0..10 {
        thread::sleep(Duration::from_millis(100));

        let discovered = discovery.scan();
        eprintln!(
            "Poll {}: {} discovered, {} packets received",
            i,
            discovered.len(),
            handle.received_packet_count()
        );

        if !discovered.is_empty() {
            eprintln!("Found something!");
            for d in &discovered {
                eprintln!("  Discovered: {} ({:?})", d.name(), d.dac_type());
            }
            return;
        }
    }

    panic!("Did not discover any devices after 1 second");
}

#[test]
fn test_send_frame() {
    use laser_dac::discovery::DacDiscovery;

    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let mut discovery = DacDiscovery::new(idn_only());
    discovery.set_idn_scan_addresses(vec![server_addr]);

    let device = discovery
        .scan()
        .into_iter()
        .next()
        .map(|d| connect_dac(&mut discovery, d))
        .expect("Should get a device");

    // Clear any discovery packets
    handle.clear_received_packets();

    // Start a stream and send points
    let config = StreamConfig::new(30000);
    let (mut stream, _info) = device.start_stream(config).expect("Should start stream");

    // Get a request and write points
    let req = stream.next_request().expect("Should get request");
    let points: Vec<LaserPoint> = (0..req.n_points)
        .map(|_| LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535))
        .collect();
    stream.write(&req, &points).expect("Should write points");

    // Give time for processing
    thread::sleep(Duration::from_millis(100));

    // Verify server received frame data
    assert!(
        handle.received_frame_data(),
        "Server should have received frame data packet"
    );
}

#[test]
fn test_connection_loss_detection() {
    use laser_dac::discovery::DacDiscovery;

    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let mut discovery = DacDiscovery::new(idn_only());
    discovery.set_idn_scan_addresses(vec![server_addr]);

    let device = discovery
        .scan()
        .into_iter()
        .next()
        .map(|d| connect_dac(&mut discovery, d))
        .expect("Should get a device");

    // Start a stream and send some data
    let config = StreamConfig::new(30000);
    let (mut stream, _info) = device.start_stream(config).expect("Should start stream");

    // Send data successfully
    let req = stream.next_request().expect("Should get request");
    let points: Vec<LaserPoint> = (0..req.n_points)
        .map(|_| LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535))
        .collect();
    stream.write(&req, &points).expect("Should write points");
    thread::sleep(Duration::from_millis(50));

    // Verify stream is connected
    let status = stream.status().expect("Should get status");
    assert!(status.connected, "Stream should be connected initially");

    // Simulate server disconnect
    handle.simulate_disconnect();
    thread::sleep(Duration::from_millis(800));

    // IDN uses fire-and-forget UDP, so writes succeed even when the server
    // stops responding. The stream remains "connected" from its perspective.
    // Verify that writes continue to succeed (no false errors).
    for _ in 0..5 {
        match stream.next_request() {
            Ok(req) => {
                let points: Vec<LaserPoint> = (0..req.n_points)
                    .map(|_| LaserPoint::blanked(0.0, 0.0))
                    .collect();
                stream
                    .write(&req, &points)
                    .expect("UDP writes should succeed even without server");
            }
            Err(e) => {
                panic!("next_request should not fail for UDP stream: {e}");
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Stream still reports connected since UDP has no delivery confirmation
    let final_status = stream.status().expect("Should get status");
    assert!(
        final_status.connected,
        "UDP stream should still report connected (fire-and-forget)"
    );
}

#[test]
fn test_full_lifecycle() {
    use laser_dac::discovery::DacDiscovery;

    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let mut discovery = DacDiscovery::new(idn_only());
    discovery.set_idn_scan_addresses(vec![server_addr]);

    // Phase 1: Initial connection
    let device = discovery
        .scan()
        .into_iter()
        .next()
        .map(|d| connect_dac(&mut discovery, d))
        .expect("Phase 1: Should get device");

    let config = StreamConfig::new(30000);
    let (mut stream, _info) = device
        .start_stream(config)
        .expect("Phase 1: Should start stream");
    handle.clear_received_packets();

    // Phase 2: Send frames successfully
    for _ in 0..3 {
        let req = stream.next_request().expect("Should get request");
        let points: Vec<LaserPoint> = (0..req.n_points)
            .map(|_| LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535))
            .collect();
        stream.write(&req, &points).expect("Should write points");
        thread::sleep(Duration::from_millis(30));
    }
    thread::sleep(Duration::from_millis(100));
    assert!(
        handle.received_frame_data(),
        "Phase 2: Server should receive frame data"
    );

    // Phase 3: Simulate disconnect
    handle.simulate_disconnect();
    thread::sleep(Duration::from_millis(800));

    // Check for connection loss
    let status = stream.status().unwrap();
    let initially_connected = status.connected;

    // Try to write - may fail if connection is detected as lost
    let loss_detected = if !initially_connected {
        true
    } else {
        // Try to continue writing to trigger loss detection
        let mut detected = false;
        for _ in 0..5 {
            match stream.next_request() {
                Ok(req) => {
                    let points: Vec<LaserPoint> = (0..req.n_points)
                        .map(|_| LaserPoint::blanked(0.0, 0.0))
                        .collect();
                    if stream.write(&req, &points).is_err() {
                        detected = true;
                        break;
                    }
                }
                Err(_) => {
                    detected = true;
                    break;
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
        detected
    };

    // Stop the old stream
    let _ = stream.stop();
    drop(stream);

    // Phase 4: Reconnect - verify server is back
    handle.resume();
    thread::sleep(Duration::from_millis(500));

    // Verify server is discoverable again
    use laser_dac::protocols::idn::ServerScanner;
    let mut scanner = ServerScanner::new(0).expect("Failed to create scanner");
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .expect("Scan failed");
    assert!(
        !servers.is_empty(),
        "Phase 4: Server should be discoverable after resume"
    );

    eprintln!(
        "Full lifecycle test completed. Loss detected: {}",
        loss_detected
    );
}

#[test]
fn test_parsed_server_metadata() {
    use laser_dac::protocols::idn::{ServerScanner, ServiceType};

    // Create a server with specific metadata
    let unit_id: [u8; 16] = [
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
        0x10,
    ];

    let handle = TestServerBuilder::new("MetadataTest")
        .unit_id(unit_id)
        .protocol_version(2, 5)
        .status(0x42)
        .services(vec![
            MockService::laser_projector(7, "MainLaser").with_dsid()
        ])
        .build()
        .unwrap();

    let expected_hostname = "MetadataTest";
    let expected_version = (2, 5);
    let expected_status = 0x42;
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    // Scan and get ServerInfo
    let mut scanner = ServerScanner::new(0).unwrap();
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .unwrap();

    assert_eq!(servers.len(), 1, "Should find exactly one server");

    let server_info = &servers[0];

    // Assert ServerInfo fields
    assert_eq!(
        server_info.unit_id, unit_id,
        "unit_id should match configured value"
    );
    assert_eq!(
        server_info.hostname, expected_hostname,
        "hostname should match configured value"
    );
    assert_eq!(
        server_info.protocol_version, expected_version,
        "protocol_version should match configured value"
    );
    assert_eq!(
        server_info.status, expected_status,
        "status should match configured value"
    );
    assert!(
        !server_info.addresses.is_empty(),
        "addresses should not be empty"
    );
    assert_eq!(
        server_info.addresses[0], server_addr,
        "address should match mock server address"
    );

    // Assert ServiceInfo fields
    assert_eq!(
        server_info.services.len(),
        1,
        "Should have exactly one service"
    );
    let service = &server_info.services[0];
    assert_eq!(service.service_id, 7, "service_id should match");
    assert!(
        matches!(service.service_type, ServiceType::LaserProjector),
        "service_type should be LaserProjector"
    );
    assert_eq!(service.name, "MainLaser", "service name should match");
    assert_eq!(service.flags, 0x01, "flags should have DSID bit set");
    assert_eq!(service.relay_number, 0, "relay_number should be 0 (root)");
}

#[test]
fn test_parsed_multiple_services() {
    use laser_dac::protocols::idn::{ServerScanner, ServiceType};

    let handle = TestServerBuilder::new("MultiService")
        .services(vec![
            MockService::laser_projector(1, "Laser1").with_dsid(),
            MockService::laser_projector(2, "Laser2"),
            MockService::dmx512(3, "DMXOut"),
        ])
        .build()
        .unwrap();

    let server_addr = handle.addr();
    thread::sleep(Duration::from_millis(50));

    let mut scanner = ServerScanner::new(0).unwrap();
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .unwrap();

    assert_eq!(servers.len(), 1);
    let server_info = &servers[0];

    // Should have all 3 services
    assert_eq!(server_info.services.len(), 3, "Should have 3 services");

    // Check first service (laser projector with DSID)
    let svc1 = &server_info.services[0];
    assert_eq!(svc1.service_id, 1);
    assert!(matches!(svc1.service_type, ServiceType::LaserProjector));
    assert_eq!(svc1.name, "Laser1");
    assert_eq!(svc1.flags, 0x01); // DSID flag

    // Check second service (laser projector without DSID)
    let svc2 = &server_info.services[1];
    assert_eq!(svc2.service_id, 2);
    assert!(matches!(svc2.service_type, ServiceType::LaserProjector));
    assert_eq!(svc2.name, "Laser2");
    assert_eq!(svc2.flags, 0x00);

    // Check third service (DMX512)
    let svc3 = &server_info.services[2];
    assert_eq!(svc3.service_id, 3);
    assert!(matches!(svc3.service_type, ServiceType::Dmx512));
    assert_eq!(svc3.name, "DMXOut");
}

#[test]
fn test_parsed_relays_and_services() {
    use laser_dac::protocols::idn::{ServerScanner, ServiceType};

    let handle = TestServerBuilder::new("RelayTest")
        .relays(vec![
            MockRelay::new(1, "Relay1"),
            MockRelay::new(2, "Relay2"),
        ])
        .services(vec![
            MockService::laser_projector(1, "RootLaser"),
            MockService::laser_projector(2, "RelayedLaser").with_relay(1),
        ])
        .build()
        .unwrap();

    let server_addr = handle.addr();
    thread::sleep(Duration::from_millis(50));

    let mut scanner = ServerScanner::new(0).unwrap();
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(500))
        .unwrap();

    assert_eq!(servers.len(), 1);
    let server_info = &servers[0];

    // Check relays
    assert_eq!(server_info.relays.len(), 2, "Should have 2 relays");

    let relay1 = &server_info.relays[0];
    assert_eq!(relay1.relay_number, 1, "relay1 number should be 1");
    assert_eq!(relay1.name, "Relay1", "relay1 name should match");

    let relay2 = &server_info.relays[1];
    assert_eq!(relay2.relay_number, 2, "relay2 number should be 2");
    assert_eq!(relay2.name, "Relay2", "relay2 name should match");

    // Check services
    assert_eq!(server_info.services.len(), 2, "Should have 2 services");

    let svc1 = &server_info.services[0];
    assert_eq!(svc1.service_id, 1);
    assert!(matches!(svc1.service_type, ServiceType::LaserProjector));
    assert_eq!(svc1.name, "RootLaser");
    assert_eq!(
        svc1.relay_number, 0,
        "RootLaser should be on root (relay 0)"
    );

    let svc2 = &server_info.services[1];
    assert_eq!(svc2.service_id, 2);
    assert!(matches!(svc2.service_type, ServiceType::LaserProjector));
    assert_eq!(svc2.name, "RelayedLaser");
    assert_eq!(svc2.relay_number, 1, "RelayedLaser should be on relay 1");
}

#[test]
fn test_discovered_device_info_metadata() {
    use laser_dac::discovery::IdnDiscovery;
    use std::net::IpAddr;

    let handle = TestServerBuilder::new("DiscoveryTest")
        .protocol_version(1, 2)
        .services(vec![MockService::laser_projector(5, "TestLaser")])
        .build()
        .unwrap();

    let server_addr = handle.addr();
    thread::sleep(Duration::from_millis(50));

    let mut idn_discovery = IdnDiscovery::new();
    let devices = idn_discovery.scan_address(server_addr);

    assert_eq!(devices.len(), 1, "Should discover one device");

    let device = &devices[0];
    let info = device.info();

    // DiscoveredDeviceInfo should expose relevant metadata
    assert_eq!(info.dac_type, DacType::Idn);

    // IP address should be set for IDN devices
    assert!(
        info.ip_address.is_some(),
        "IDN device should have IP address"
    );
    assert_eq!(
        info.ip_address,
        Some(IpAddr::V4("127.0.0.1".parse().unwrap())),
        "IP should be localhost"
    );

    // Hostname should be set from scan response
    assert!(info.hostname.is_some(), "IDN device should have hostname");
    assert_eq!(
        info.hostname.as_deref(),
        Some("DiscoveryTest"),
        "hostname should match mock server configuration"
    );

    // name() returns IP for network devices (unique identifier)
    assert_eq!(info.name(), "127.0.0.1");
}

// =============================================================================
// Timeout, Connection, and Back Pressure Tests
// =============================================================================

#[test]
fn test_server_never_responds_timeout() {
    use laser_dac::discovery::IdnDiscovery;
    use laser_dac::protocols::idn::ServerScanner;

    // Create a silent mock server that receives but never responds
    let handle = TestServerBuilder::new("SilentServer")
        .silent()
        .build()
        .unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    // Test 1: ServerScanner should return empty results after timeout
    let mut scanner = ServerScanner::new(0).expect("Failed to create scanner");
    let servers = scanner
        .scan_address(server_addr, Duration::from_millis(200))
        .expect("Scan should not error, just return empty");

    assert!(
        servers.is_empty(),
        "Scanner should return empty results for silent server"
    );

    // Verify server received the scan request
    assert!(
        handle.received_packet_count() > 0,
        "Silent server should still receive packets"
    );

    // Test 2: IdnDiscovery should also return empty results
    handle.clear_received_packets();
    let mut idn_discovery = IdnDiscovery::new();
    let devices = idn_discovery.scan_address(server_addr);

    assert!(
        devices.is_empty(),
        "IdnDiscovery should return empty for silent server"
    );
    assert!(
        handle.received_packet_count() > 0,
        "Silent server should receive IdnDiscovery packets"
    );
}

#[test]
fn test_connection_to_nonexistent_server() {
    use laser_dac::discovery::IdnDiscovery;
    use laser_dac::protocols::idn::ServerScanner;

    // Use a high port that definitely has no server listening
    let nonexistent_addr: SocketAddr = "127.0.0.1:65432".parse().unwrap();

    // Test 1: ServerScanner should gracefully handle no server
    let mut scanner = ServerScanner::new(0).expect("Failed to create scanner");
    let servers = scanner
        .scan_address(nonexistent_addr, Duration::from_millis(200))
        .expect("Scan should not error, just return empty");

    assert!(
        servers.is_empty(),
        "Scanner should return empty results when no server exists"
    );

    // Test 2: IdnDiscovery should also gracefully handle no server
    let mut idn_discovery = IdnDiscovery::new();
    let discovered = idn_discovery.scan_address(nonexistent_addr);

    assert!(
        discovered.is_empty(),
        "IdnDiscovery should return empty for nonexistent server"
    );
}

// =============================================================================
// Packet parsing helper for low-level E2E tests
// =============================================================================

/// Parsed fields from a raw IDN packet for test assertions.
#[derive(Debug)]
#[allow(dead_code)]
struct ParsedPacket {
    command: u8,
    flags: u8,
    sequence: u16,
    /// Timestamp from ChannelMessageHeader (offset 8..12), if present
    timestamp: Option<u32>,
    /// Content ID from ChannelMessageHeader (offset 6..8), if present
    content_id: Option<u16>,
    /// Number of sample bytes after all headers
    sample_byte_count: Option<usize>,
}

impl ParsedPacket {
    /// Parse a raw IDN packet captured by the mock server.
    fn from_bytes(data: &[u8]) -> Self {
        let command = data[0];
        let flags = data[1];
        let sequence = u16::from_be_bytes([data[2], data[3]]);

        let mut timestamp = None;
        let mut content_id = None;
        let mut sample_byte_count = None;

        // If this is a channel message, parse the ChannelMessageHeader
        if (command == 0x40 || command == 0x41 || command == 0x45) && data.len() >= 12 {
            // ChannelMessageHeader starts at offset 4:
            //   total_size: u16 (BE) at [4..6]
            //   content_id: u16 (BE) at [6..8]
            //   timestamp:  u32 (BE) at [8..12]
            content_id = Some(u16::from_be_bytes([data[6], data[7]]));
            timestamp = Some(u32::from_be_bytes([data[8], data[9], data[10], data[11]]));

            // Calculate sample bytes: total packet length minus all headers.
            // The total_size field in ChannelMessageHeader tells us everything
            // from the ChannelMessageHeader start to end of payload.
            let total_size = u16::from_be_bytes([data[4], data[5]]) as usize;
            // Packet is: PacketHeader(4) + ChannelMsg payload(total_size)
            // ChannelMsg payload includes: ChannelMessageHeader(8) + config (optional) + chunk header + samples
            // We can compute sample bytes from the raw data length minus the 4-byte packet header
            // and the total_size tells us the channel message size
            if data.len() >= 4 + total_size {
                // The data after PacketHeader(4) is total_size bytes
                // Within that: ChannelMessageHeader is 8 bytes, then config+chunk+samples
                // For simplicity, count everything after PacketHeader + ChannelMessageHeader
                // that could be samples by scanning for the SampleChunkHeader
                sample_byte_count = Some(data.len() - 4); // approximate: everything after packet header
            }
        }

        Self {
            command,
            flags,
            sequence,
            timestamp,
            content_id,
            sample_byte_count,
        }
    }

    /// Count the number of sample points of a given size in the packet.
    /// Searches for data after all headers by using the known header sizes.
    fn count_samples(data: &[u8], bytes_per_sample: usize) -> usize {
        if data.len() < 12 {
            return 0;
        }

        let total_size = u16::from_be_bytes([data[4], data[5]]) as usize;
        let channel_msg_payload = &data[4..4 + total_size.min(data.len() - 4)];

        // ChannelMessageHeader is 8 bytes within the payload
        if channel_msg_payload.len() <= 8 {
            return 0;
        }

        let content_id = u16::from_be_bytes([data[6], data[7]]);
        let has_config = content_id & 0x4000 != 0; // IDNFLG_CONTENTID_CONFIG_LSTFRG

        let mut offset = 8; // past ChannelMessageHeader

        if has_config {
            // ChannelConfigHeader is 4 bytes + descriptors
            if channel_msg_payload.len() <= offset {
                return 0;
            }
            let word_count = channel_msg_payload[offset] as usize;
            let config_size = 4 + word_count * 4; // 4 header bytes + word_count * 4 descriptor bytes
            offset += config_size;
        }

        // SampleChunkHeader is 4 bytes
        offset += 4;

        if offset > channel_msg_payload.len() {
            return 0;
        }

        let sample_data_len = channel_msg_payload.len() - offset;
        sample_data_len / bytes_per_sample
    }
}

// =============================================================================
// Helper: Create a stream connected to a mock server (low-level API)
// =============================================================================

/// Connect to a mock server using the low-level stream API.
fn connect_stream(
    handle: &TestServerHandle,
) -> laser_dac::protocols::idn::dac::stream::Stream {
    let addr = handle.addr();

    // Discover the server first to get real ServerInfo
    let mut scanner =
        laser_dac::protocols::idn::ServerScanner::new(0).expect("Failed to create scanner");
    let servers = scanner
        .scan_address(addr, Duration::from_millis(500))
        .expect("Scan failed");
    assert!(!servers.is_empty(), "Should find mock server");

    let server = &servers[0];
    let service_id = server.services.first().map(|s| s.service_id).unwrap_or(1);

    laser_dac::protocols::idn::dac::stream::connect(server, service_id)
        .expect("Should connect stream")
}

// =============================================================================
// Low-level E2E tests
// =============================================================================

#[test]
fn test_keepalive_sends_void_channel_message() {
    use laser_dac::protocols::idn::PointXyrgbi;

    let handle = test_server("KeepaliveTest").unwrap();
    thread::sleep(Duration::from_millis(50));

    let mut stream = connect_stream(&handle);

    // Send one frame to establish session
    let points: Vec<PointXyrgbi> = (0..20)
        .map(|_| PointXyrgbi::new(0, 0, 255, 0, 0, 255))
        .collect();
    stream.write_frame(&points).expect("Should write frame");
    thread::sleep(Duration::from_millis(50));

    // Clear packets and send keepalive
    handle.clear_received_packets();
    thread::sleep(Duration::from_millis(600)); // exceed KEEPALIVE_INTERVAL
    stream.send_keepalive().expect("Should send keepalive");
    thread::sleep(Duration::from_millis(50));

    // Inspect the keepalive packet
    let packets = handle.received_packets.lock().unwrap();
    assert!(!packets.is_empty(), "Should have captured keepalive packet");

    let last = packets.last().unwrap();
    let parsed = ParsedPacket::from_bytes(last);

    // Should be RT_CNLMSG (0x40), NOT PING_REQUEST (0x08)
    assert_eq!(
        parsed.command, 0x40,
        "keepalive should use RT_CNLMSG (0x40), not PING_REQUEST"
    );

    // Content ID lower bits should be VOID (0x00)
    let cid = parsed.content_id.expect("Should have content_id");
    assert_eq!(
        cid & 0x00FF,
        0x00,
        "keepalive content_id chunk type should be VOID (0x00)"
    );

    // Prevent Drop from sending close on the test socket
    std::mem::forget(stream);
}

#[test]
fn test_small_frame_padded_to_minimum() {
    use laser_dac::protocols::idn::PointXyrgbi;

    let handle = test_server("PaddingTest").unwrap();
    thread::sleep(Duration::from_millis(50));

    let mut stream = connect_stream(&handle);

    // Send a frame with only 5 points
    let points: Vec<PointXyrgbi> = (0..5)
        .map(|i| PointXyrgbi::new(i as i16 * 100, 0, 255, 0, 0, 255))
        .collect();

    handle.clear_received_packets();
    stream.write_frame(&points).expect("Should write frame");
    thread::sleep(Duration::from_millis(50));

    // Find the frame packet (command 0x40)
    let packets = handle.received_packets.lock().unwrap();
    let frame_packet = packets
        .iter()
        .find(|p| !p.is_empty() && p[0] == IDNCMD_RT_CNLMSG)
        .expect("Should find frame packet");

    // Count samples in the packet (PointXyrgbi = 8 bytes per sample)
    let sample_count = ParsedPacket::count_samples(frame_packet, 8);
    assert!(
        sample_count >= 20,
        "Frame should be padded to at least 20 samples, got {}",
        sample_count
    );

    std::mem::forget(stream);
}

#[test]
fn test_first_frame_has_nonzero_timestamp() {
    use laser_dac::protocols::idn::PointXyrgbi;

    let handle = test_server("TimestampTest").unwrap();
    thread::sleep(Duration::from_millis(50));

    let mut stream = connect_stream(&handle);

    // Wait a bit so connect_time.elapsed() is non-trivial
    thread::sleep(Duration::from_millis(50));

    let points: Vec<PointXyrgbi> = (0..20)
        .map(|_| PointXyrgbi::new(0, 0, 255, 0, 0, 255))
        .collect();

    handle.clear_received_packets();
    stream.write_frame(&points).expect("Should write frame");
    thread::sleep(Duration::from_millis(50));

    let packets = handle.received_packets.lock().unwrap();
    let frame_packet = packets
        .iter()
        .find(|p| !p.is_empty() && p[0] == IDNCMD_RT_CNLMSG)
        .expect("Should find frame packet");

    let parsed = ParsedPacket::from_bytes(frame_packet);
    let ts = parsed.timestamp.expect("Should have timestamp");
    assert!(
        ts > 0,
        "First frame timestamp should be non-zero (connect_time based), got {}",
        ts
    );

    std::mem::forget(stream);
}

#[test]
fn test_close_with_ack() {
    use laser_dac::protocols::idn::PointXyrgbi;

    let handle = test_server("CloseAckTest").unwrap();
    thread::sleep(Duration::from_millis(50));

    let mut stream = connect_stream(&handle);

    // Send one frame to establish the session
    let points: Vec<PointXyrgbi> = (0..20)
        .map(|_| PointXyrgbi::new(0, 0, 255, 0, 0, 255))
        .collect();
    stream.write_frame(&points).expect("Should write frame");
    thread::sleep(Duration::from_millis(50));

    handle.clear_received_packets();

    // Close with ack
    let result = stream.close_with_ack(Duration::from_millis(500));
    assert!(result.is_ok(), "close_with_ack should succeed");

    thread::sleep(Duration::from_millis(50));

    // Verify that the server received a CLOSE_ACKREQ packet (0x45)
    let packets = handle.received_packets.lock().unwrap();
    let has_close_ackreq = packets
        .iter()
        .any(|p| !p.is_empty() && p[0] == IDNCMD_RT_CNLMSG_CLOSE_ACKREQ);
    assert!(
        has_close_ackreq,
        "Server should have received CLOSE_ACKREQ (0x45) packet"
    );

    std::mem::forget(stream);
}

#[test]
fn test_get_parameter_uses_correct_command() {
    use laser_dac::protocols::idn::PointXyrgbi;

    let handle = test_server("ParamTest").unwrap();
    thread::sleep(Duration::from_millis(50));

    let mut stream = connect_stream(&handle);

    // Send one frame first to establish session
    let points: Vec<PointXyrgbi> = (0..20)
        .map(|_| PointXyrgbi::new(0, 0, 255, 0, 0, 255))
        .collect();
    stream.write_frame(&points).expect("Should write frame");
    thread::sleep(Duration::from_millis(50));

    // Test 1: service_id=0 should use UNIT_PARAMS_REQUEST (0x22)
    handle.clear_received_packets();
    let _ = stream.get_parameter(0, 0x0001, Duration::from_millis(500));
    thread::sleep(Duration::from_millis(50));

    {
        let packets = handle.received_packets.lock().unwrap();
        let unit_param_packet = packets
            .iter()
            .find(|p| !p.is_empty() && p[0] == IDNCMD_UNIT_PARAMS_REQUEST);
        assert!(
            unit_param_packet.is_some(),
            "service_id=0 should send UNIT_PARAMS_REQUEST (0x22)"
        );
    }

    // Test 2: service_id=1 should use SERVICE_PARAMS_REQUEST (0x20)
    handle.clear_received_packets();
    let _ = stream.get_parameter(1, 0x0001, Duration::from_millis(500));
    thread::sleep(Duration::from_millis(50));

    {
        let packets = handle.received_packets.lock().unwrap();
        let service_param_packet = packets
            .iter()
            .find(|p| !p.is_empty() && p[0] == IDNCMD_SERVICE_PARAMS_REQUEST);
        assert!(
            service_param_packet.is_some(),
            "service_id=1 should send SERVICE_PARAMS_REQUEST (0x20)"
        );
    }

    std::mem::forget(stream);
}
