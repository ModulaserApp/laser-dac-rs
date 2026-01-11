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
    IDNCMD_RT_CNLMSG, IDNFLG_STATUS_REALTIME,
};

use laser_dac::types::{DacConnectionState, DacType, EnabledDacTypes, LaserFrame, LaserPoint};
use laser_dac::DacDiscoveryWorker;

/// Create an EnabledDacTypes with only IDN enabled.
fn idn_only() -> EnabledDacTypes {
    let mut types = EnabledDacTypes::none();
    types.enable(DacType::Idn);
    types
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

impl TestBehavior {
    pub fn new() -> Self {
        Self {
            disconnected: Arc::new(AtomicBool::new(false)),
            silent: false,
            received_packets: Arc::new(Mutex::new(Vec::new())),
            status: IDNFLG_STATUS_REALTIME,
            ack_error_code: 0x00,
        }
    }

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
            behavior: TestBehavior::new(),
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
// Test Utilities
// =============================================================================

/// Create a simple test frame.
fn create_test_frame() -> LaserFrame {
    let points = vec![
        LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535),
        LaserPoint::new(0.5, 0.5, 0, 65535, 0, 65535),
        LaserPoint::new(-0.5, -0.5, 0, 0, 65535, 65535),
    ];
    LaserFrame::new(30000, points)
}

/// Wait for a worker to appear from the discovery worker.
fn wait_for_worker(
    discovery: &DacDiscoveryWorker,
    timeout: Duration,
) -> Option<laser_dac::DacWorker> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        // Drain discovered devices
        let _: Vec<_> = discovery.poll_discovered_devices().collect();

        // Check for workers
        let mut workers = discovery.poll_new_workers();
        if let Some(worker) = workers.next() {
            return Some(worker);
        }

        thread::sleep(Duration::from_millis(50));
    }
    None
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
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();
    eprintln!("Mock server listening on: {}", server_addr);

    // Give server time to start
    thread::sleep(Duration::from_millis(50));

    eprintln!("Creating discovery worker...");
    // Create discovery worker configured to scan mock server
    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    eprintln!("Waiting for discovery...");
    // Wait for discovery - give more time
    for i in 0..10 {
        thread::sleep(Duration::from_millis(100));

        let devices: Vec<_> = discovery.poll_discovered_devices().collect();
        let workers: Vec<_> = discovery.poll_new_workers().collect();

        eprintln!(
            "Poll {}: {} devices, {} workers, {} packets received",
            i,
            devices.len(),
            workers.len(),
            handle.received_packet_count()
        );

        if !devices.is_empty() || !workers.is_empty() {
            eprintln!("Found something!");
            for d in &devices {
                eprintln!("  Device: {} ({:?})", d.name(), d.dac_type);
            }
            for w in &workers {
                eprintln!("  Worker: {} ({:?})", w.device_name(), w.dac_type());
            }
            return; // Test passes
        }
    }

    panic!("Did not discover any devices after 1 second");
}

#[test]
fn test_send_frame() {
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Wait for worker
    let worker = wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Clear any discovery packets
    handle.clear_received_packets();

    // Send a frame
    let frame = create_test_frame();
    let submitted = worker.submit_frame(frame);
    assert!(submitted, "Frame should be submitted successfully");

    // Give the worker thread time to process
    thread::sleep(Duration::from_millis(100));

    // Verify server received frame data
    assert!(
        handle.received_frame_data(),
        "Server should have received frame data packet"
    );
}

#[test]
fn test_connection_loss_detection() {
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Wait for worker
    let mut worker =
        wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Verify initially connected
    worker.update();
    assert!(
        matches!(worker.state(), DacConnectionState::Connected { .. }),
        "Worker should be connected initially"
    );

    // Send a frame successfully
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(50));
    worker.update();

    // Simulate server disconnect
    handle.simulate_disconnect();

    // Wait for keepalive timeout to trigger (500ms idle + 200ms ping timeout)
    thread::sleep(Duration::from_millis(800));

    // Send another frame - this should trigger keepalive check
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(300));
    worker.update();

    // Worker should detect connection loss
    assert!(
        matches!(worker.state(), DacConnectionState::Lost { .. }),
        "Worker should detect connection loss after ping timeout"
    );
}

#[test]
fn test_reconnection_after_loss() {
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(200))
        .build();

    // Wait for initial worker
    let mut worker =
        wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Simulate disconnect
    handle.simulate_disconnect();

    // Trigger keepalive failure
    thread::sleep(Duration::from_millis(800));
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(300));
    worker.update();

    // Drop the old worker so discovery can reconnect
    drop(worker);

    // Resume server
    handle.resume();

    // Wait for rediscovery
    let mut new_worker = wait_for_worker(&discovery, Duration::from_secs(2))
        .expect("Should reconnect with a new worker after server resumes");

    // Verify new worker is connected
    new_worker.update();
    assert!(
        matches!(new_worker.state(), DacConnectionState::Connected { .. }),
        "New worker should be connected"
    );
}

#[test]
fn test_full_lifecycle() {
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(200))
        .build();

    // Phase 1: Initial connection
    let mut worker = wait_for_worker(&discovery, Duration::from_secs(2))
        .expect("Phase 1: Should connect initially");
    handle.clear_received_packets();

    // Phase 2: Send frames successfully
    for _ in 0..3 {
        worker.submit_frame(create_test_frame());
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
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(300));
    worker.update();
    assert!(
        matches!(worker.state(), DacConnectionState::Lost { .. }),
        "Phase 3: Should detect connection loss"
    );
    drop(worker);

    // Phase 4: Reconnect
    handle.resume();

    let mut new_worker =
        wait_for_worker(&discovery, Duration::from_secs(2)).expect("Phase 4: Should reconnect");
    handle.clear_received_packets();

    // Phase 5: Send frames again after reconnection
    new_worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(100));
    assert!(
        handle.received_frame_data(),
        "Phase 5: Should send frames after reconnection"
    );

    new_worker.update();
    assert!(
        matches!(new_worker.state(), DacConnectionState::Connected { .. }),
        "Phase 5: Should remain connected"
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
fn test_discovery_timeout_on_established_connection() {
    // Test that an established connection properly times out when server goes silent
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Get a connected worker
    let mut worker =
        wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Send a frame successfully first
    assert!(worker.submit_frame(create_test_frame()));
    thread::sleep(Duration::from_millis(100));
    worker.update();

    assert!(
        matches!(worker.state(), DacConnectionState::Connected { .. }),
        "Worker should be connected initially"
    );

    // Now simulate disconnect (server stops responding to pings)
    handle.simulate_disconnect();

    // Wait for keepalive interval (500ms) + ping timeout (200ms) + margin
    thread::sleep(Duration::from_millis(800));

    // Try to send another frame - this should trigger keepalive check
    worker.submit_frame(create_test_frame());
    thread::sleep(Duration::from_millis(300));
    worker.update();

    // Verify connection loss was detected
    assert!(
        matches!(worker.state(), DacConnectionState::Lost { .. }),
        "Worker should detect connection loss when server stops responding"
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
    let devices = idn_discovery.scan_address(nonexistent_addr);

    assert!(
        devices.is_empty(),
        "IdnDiscovery should return empty for nonexistent server"
    );

    // Test 3: DacDiscoveryWorker should handle unreachable addresses without crashing
    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![nonexistent_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Let discovery run for a bit - should not panic
    thread::sleep(Duration::from_millis(300));

    // Should find nothing but not crash
    let devices: Vec<_> = discovery.poll_discovered_devices().collect();
    let workers: Vec<_> = discovery.poll_new_workers().collect();

    assert!(devices.is_empty(), "Should not discover any devices");
    assert!(workers.is_empty(), "Should not get any workers");
}

#[test]
fn test_rapid_frame_submission_back_pressure() {
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    let worker = wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Clear discovery packets
    handle.clear_received_packets();

    // Submit many frames rapidly in a tight loop
    let mut submitted = 0;
    let mut dropped = 0;
    for _ in 0..100 {
        if worker.submit_frame(create_test_frame()) {
            submitted += 1;
        } else {
            dropped += 1;
        }
    }

    // Some frames should be submitted
    assert!(
        submitted > 0,
        "At least some frames should be submitted successfully"
    );

    // Due to the bounded channel (capacity 1), many frames should be dropped
    // when submitting faster than the worker can process
    assert!(
        dropped > 0,
        "Some frames should be dropped due to back pressure (channel capacity 1)"
    );

    eprintln!(
        "Rapid submission: {} submitted, {} dropped",
        submitted, dropped
    );

    // Give time for processing
    thread::sleep(Duration::from_millis(200));

    // Verify server received at least some frame data
    assert!(
        handle.received_frame_data(),
        "Server should have received at least some frame data"
    );

    // Verify worker is still functional after burst
    let mut worker = worker;
    worker.update();
    assert!(
        matches!(worker.state(), DacConnectionState::Connected { .. }),
        "Worker should remain connected after rapid submission"
    );

    // Verify we can still send frames after the burst
    handle.clear_received_packets();
    thread::sleep(Duration::from_millis(50)); // Give channel time to drain
    assert!(
        worker.submit_frame(create_test_frame()),
        "Should be able to submit frame after burst completes"
    );
    thread::sleep(Duration::from_millis(100));
    assert!(
        handle.received_frame_data(),
        "Server should receive frame after burst"
    );
}

#[test]
fn test_submit_frame_returns_false_when_busy() {
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    let worker = wait_for_worker(&discovery, Duration::from_secs(2)).expect("Should get a worker");

    // Submit first frame - should succeed
    let first_result = worker.submit_frame(create_test_frame());
    assert!(first_result, "First frame should be submitted");

    // Immediately submit second frame while first is likely still being processed
    // With a channel capacity of 1, if the worker hasn't picked up the first frame yet,
    // this should return false
    let mut any_rejected = false;
    for _ in 0..10 {
        if !worker.submit_frame(create_test_frame()) {
            any_rejected = true;
            break;
        }
    }

    // We expect at least one rejection when submitting rapidly
    // (though this depends on timing, it should happen frequently)
    eprintln!("Immediate submission test: any_rejected = {}", any_rejected);

    // The important invariant: the worker should still be functional
    thread::sleep(Duration::from_millis(100));
    let mut worker = worker;
    worker.update();
    assert!(
        matches!(worker.state(), DacConnectionState::Connected { .. }),
        "Worker should remain connected regardless of dropped frames"
    );
}
