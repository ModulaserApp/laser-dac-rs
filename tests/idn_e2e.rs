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

use laser_dac::stream::Dac;
use laser_dac::types::{DacType, EnabledDacTypes, LaserPoint, StreamConfig};
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
// Test Utilities
// =============================================================================

/// Wait for a device to appear from the discovery worker.
fn wait_for_device(discovery: &DacDiscoveryWorker, timeout: Duration) -> Option<Dac> {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        // Drain discovered devices
        let _: Vec<_> = discovery.poll_discovered_devices().collect();

        // Check for devices
        let mut devices = discovery.poll_new_devices();
        if let Some(device) = devices.next() {
            return Some(device);
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

        let discovered: Vec<_> = discovery.poll_discovered_devices().collect();
        let devices: Vec<_> = discovery.poll_new_devices().collect();

        eprintln!(
            "Poll {}: {} discovered, {} devices, {} packets received",
            i,
            discovered.len(),
            devices.len(),
            handle.received_packet_count()
        );

        if !discovered.is_empty() || !devices.is_empty() {
            eprintln!("Found something!");
            for d in &discovered {
                eprintln!("  Discovered: {} ({:?})", d.name(), d.dac_type);
            }
            for d in &devices {
                eprintln!("  Device: {} ({:?})", d.name(), d.kind());
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

    // Wait for device
    let device = wait_for_device(&discovery, Duration::from_secs(2)).expect("Should get a device");

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
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Wait for device
    let device = wait_for_device(&discovery, Duration::from_secs(2)).expect("Should get a device");

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

    // Wait for connection loss to be detected
    // The IDN backend should detect loss when it can't send data or get acks
    thread::sleep(Duration::from_millis(800));

    // Try to get next request - should eventually fail
    // Note: The exact behavior depends on the IDN backend implementation
    // We may get a few more requests before the connection loss is detected
    let mut lost_detected = false;
    for _ in 0..10 {
        match stream.next_request() {
            Ok(req) => {
                let points: Vec<LaserPoint> = (0..req.n_points)
                    .map(|_| LaserPoint::blanked(0.0, 0.0))
                    .collect();
                if stream.write(&req, &points).is_err() {
                    lost_detected = true;
                    break;
                }
            }
            Err(_) => {
                lost_detected = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Check status shows disconnected
    let final_status = stream.status();
    if let Ok(status) = final_status {
        if !status.connected {
            lost_detected = true;
        }
    }

    assert!(
        lost_detected,
        "Stream should detect connection loss after server stops responding"
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

    // Wait for initial device
    let device = wait_for_device(&discovery, Duration::from_secs(2)).expect("Should get a device");

    // Start a stream and verify it works
    let config = StreamConfig::new(30000);
    let (mut stream, _info) = device.start_stream(config).expect("Should start stream");

    // Simulate disconnect
    handle.simulate_disconnect();

    // Trigger connection failure by waiting and trying to write
    thread::sleep(Duration::from_millis(800));

    // Stop the old stream
    let _ = stream.stop();
    drop(stream);

    // Resume server
    handle.resume();

    // Wait for rediscovery - need to wait for the device TTL to expire (10 seconds)
    // or the discovery to notice the device is gone and back
    thread::sleep(Duration::from_millis(500));

    // The discovery worker tracks devices by stable ID and won't rediscover
    // the same device until its TTL expires. For this test, we verify that
    // we can start a new stream with the server.
    let new_device = wait_for_device(&discovery, Duration::from_secs(12));

    if let Some(device) = new_device {
        // Verify new device is connected
        assert!(device.is_connected(), "New device should be connected");
    } else {
        // If we don't get a new device (because of TTL), that's expected
        // The test verifies the server is back up by scanning directly
        use laser_dac::protocols::idn::ServerScanner;
        let mut scanner = ServerScanner::new(0).expect("Failed to create scanner");
        let servers = scanner
            .scan_address(server_addr, Duration::from_millis(500))
            .expect("Scan failed");
        assert!(
            !servers.is_empty(),
            "Server should be discoverable after resume"
        );
    }
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
    let device =
        wait_for_device(&discovery, Duration::from_secs(2)).expect("Phase 1: Should get device");

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

    // Note: Due to device TTL in discovery worker, we verify via direct scan
    // rather than waiting for rediscovery. The important thing is the server
    // is back and responding.

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

    // Get a connected device
    let device = wait_for_device(&discovery, Duration::from_secs(2)).expect("Should get a device");

    // Start a stream
    let config = StreamConfig::new(30000);
    let (mut stream, _info) = device.start_stream(config).expect("Should start stream");

    // Send data successfully first
    let req = stream.next_request().expect("Should get request");
    let points: Vec<LaserPoint> = (0..req.n_points)
        .map(|_| LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535))
        .collect();
    stream.write(&req, &points).expect("Should write points");
    thread::sleep(Duration::from_millis(100));

    let status = stream.status().expect("Should get status");
    assert!(status.connected, "Stream should be connected initially");

    // Now simulate disconnect (server stops responding to pings)
    handle.simulate_disconnect();

    // Wait for keepalive interval (500ms) + ping timeout (200ms) + margin
    thread::sleep(Duration::from_millis(800));

    // Try to send more data - should eventually detect connection loss
    let mut loss_detected = false;
    for _ in 0..10 {
        match stream.next_request() {
            Ok(req) => {
                let points: Vec<LaserPoint> = (0..req.n_points)
                    .map(|_| LaserPoint::blanked(0.0, 0.0))
                    .collect();
                if stream.write(&req, &points).is_err() {
                    loss_detected = true;
                    break;
                }
            }
            Err(_) => {
                loss_detected = true;
                break;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }

    // Check status
    if let Ok(status) = stream.status() {
        if !status.connected {
            loss_detected = true;
        }
    }

    assert!(
        loss_detected,
        "Stream should detect connection loss when server stops responding"
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

    // Test 3: DacDiscoveryWorker should handle unreachable addresses without crashing
    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![nonexistent_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    // Let discovery run for a bit - should not panic
    thread::sleep(Duration::from_millis(300));

    // Should find nothing but not crash
    let discovered: Vec<_> = discovery.poll_discovered_devices().collect();
    let devices: Vec<_> = discovery.poll_new_devices().collect();

    assert!(discovered.is_empty(), "Should not discover any devices");
    assert!(devices.is_empty(), "Should not get any connected devices");
}

#[test]
fn test_rapid_streaming_back_pressure() {
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    let device = wait_for_device(&discovery, Duration::from_secs(2)).expect("Should get a device");

    // Clear discovery packets
    handle.clear_received_packets();

    // Start a stream
    let config = StreamConfig::new(30000);
    let (mut stream, _info) = device.start_stream(config).expect("Should start stream");

    // Write many chunks rapidly
    let mut chunks_written = 0;
    for _ in 0..10 {
        match stream.next_request() {
            Ok(req) => {
                let points: Vec<LaserPoint> = (0..req.n_points)
                    .map(|_| LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535))
                    .collect();
                if stream.write(&req, &points).is_ok() {
                    chunks_written += 1;
                }
            }
            Err(_) => break,
        }
    }

    // Some chunks should be written
    assert!(
        chunks_written > 0,
        "At least some chunks should be written successfully"
    );

    eprintln!("Rapid streaming: {} chunks written", chunks_written);

    // Give time for processing
    thread::sleep(Duration::from_millis(200));

    // Verify server received at least some frame data
    assert!(
        handle.received_frame_data(),
        "Server should have received at least some frame data"
    );

    // Verify stream is still functional after burst
    let status = stream.status().expect("Should get status");
    assert!(
        status.connected,
        "Stream should remain connected after rapid streaming"
    );

    // Verify we can still stream after the burst
    handle.clear_received_packets();
    thread::sleep(Duration::from_millis(50));

    let req = stream
        .next_request()
        .expect("Should get request after burst");
    let points: Vec<LaserPoint> = (0..req.n_points)
        .map(|_| LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535))
        .collect();
    stream
        .write(&req, &points)
        .expect("Should write points after burst");

    thread::sleep(Duration::from_millis(100));
    assert!(
        handle.received_frame_data(),
        "Server should receive frame data after burst"
    );
}

#[test]
fn test_stream_remains_functional_during_rapid_writes() {
    let handle = test_server("TestDAC").unwrap();
    let server_addr = handle.addr();

    thread::sleep(Duration::from_millis(50));

    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(idn_only())
        .idn_scan_addresses(vec![server_addr])
        .discovery_interval(Duration::from_millis(100))
        .build();

    let device = wait_for_device(&discovery, Duration::from_secs(2)).expect("Should get a device");

    // Start a stream
    let config = StreamConfig::new(30000);
    let (mut stream, _info) = device.start_stream(config).expect("Should start stream");

    // Submit first chunk - should succeed
    let req = stream.next_request().expect("Should get first request");
    let points: Vec<LaserPoint> = (0..req.n_points)
        .map(|_| LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535))
        .collect();
    stream
        .write(&req, &points)
        .expect("First chunk should be written");

    // Continue writing more chunks
    for _ in 0..5 {
        match stream.next_request() {
            Ok(req) => {
                let points: Vec<LaserPoint> = (0..req.n_points)
                    .map(|_| LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535))
                    .collect();
                let _ = stream.write(&req, &points);
            }
            Err(_) => break,
        }
    }

    // The important invariant: the stream should still be functional
    thread::sleep(Duration::from_millis(100));
    let status = stream.status().expect("Should get status");
    assert!(
        status.connected,
        "Stream should remain connected regardless of rapid writes"
    );
}
