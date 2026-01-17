//! A Rust implementation of the ILDA Digital Network (IDN) protocol.
//!
//! IDN is a network-based protocol for communicating with laser DACs over UDP.
//! Unlike Ether Dream (which broadcasts from DAC to host), IDN uses host-initiated
//! broadcast scanning to discover devices.
//!
//! ## Example
//!
//! ```no_run
//! use laser_dac::protocols::idn::{scan_for_servers, dac::stream};
//! use std::time::Duration;
//!
//! fn main() -> std::io::Result<()> {
//!     // Discover IDN servers on the network
//!     let servers = scan_for_servers(Duration::from_secs(1))?;
//!     println!("Found {} servers", servers.len());
//!
//!     if let Some(server) = servers.into_iter().next() {
//!         // Find a laser projector service
//!         if let Some(service) = server.find_laser_projector() {
//!             println!("Connecting to {} ({})", server.hostname, service.name);
//!             // Connect and stream...
//!         }
//!     }
//!     Ok(())
//! }
//! ```

pub mod backend;
pub mod dac;
pub mod error;
pub mod protocol;

use dac::{RelayInfo, ServerInfo, ServiceInfo};
use log::debug;
use protocol::{
    PacketHeader, ReadBytes, ScanResponse, ServiceMapEntry, ServiceMapResponseHeader, SizeBytes,
    WriteBytes, IDNCMD_SCAN_REQUEST, IDNCMD_SCAN_RESPONSE, IDNCMD_SERVICEMAP_REQUEST,
    IDNCMD_SERVICEMAP_RESPONSE, IDNMSK_PKTFLAGS_GROUP, IDN_PORT,
};
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

// Re-exports for convenience
pub use backend::IdnBackend;
pub use dac::{stream, Addressed, ServiceType};
pub use error::{CommunicationError, ProtocolError, ResponseError, Result};
pub use protocol::{AcknowledgeResponse, Point, PointExtended, PointXyrgbHighRes, PointXyrgbi};

/// Default client group for IDN communication.
pub const DEFAULT_CLIENT_GROUP: u8 = 0;

/// Scan for IDN servers on the network.
///
/// This function sends broadcast SCAN_REQUEST packets to all network interfaces
/// and collects responses from IDN servers. It then queries each server for its
/// service map to discover available laser projectors and other services.
///
/// # Arguments
///
/// * `timeout` - How long to wait for responses (typically 1-2 seconds)
///
/// # Returns
///
/// A vector of discovered servers with their services populated.
pub fn scan_for_servers(timeout: Duration) -> io::Result<Vec<ServerInfo>> {
    scan_for_servers_with_group(timeout, DEFAULT_CLIENT_GROUP)
}

/// Scan for IDN servers on the network with a specific client group.
///
/// Client groups (0-15) allow multiple clients to share an IDN network.
/// Most applications should use the default group 0.
pub fn scan_for_servers_with_group(
    timeout: Duration,
    client_group: u8,
) -> io::Result<Vec<ServerInfo>> {
    let mut scanner = ServerScanner::new(client_group)?;
    scanner.scan(timeout)
}

/// Scanner for discovering IDN servers on the network.
pub struct ServerScanner {
    /// UDP socket for sending requests and receiving responses
    socket: UdpSocket,
    /// Client group (0-15)
    client_group: u8,
    /// Sequence number counter
    sequence: u16,
    /// Receive buffer
    buffer: [u8; 1500],
}

impl ServerScanner {
    /// Create a new server scanner.
    pub fn new(client_group: u8) -> io::Result<Self> {
        // Bind to any address on an ephemeral port
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_broadcast(true)?;

        Ok(Self {
            socket,
            client_group: client_group & IDNMSK_PKTFLAGS_GROUP,
            sequence: 0,
            buffer: [0u8; 1500],
        })
    }

    /// Perform a scan for servers with the given timeout.
    pub fn scan(&mut self, timeout: Duration) -> io::Result<Vec<ServerInfo>> {
        let start = Instant::now();

        // Map of unit_id -> ServerInfo
        let mut servers: HashMap<[u8; 16], ServerInfo> = HashMap::new();
        // Map of address -> unit_id for tracking which server an address belongs to
        let mut addr_to_unit: HashMap<SocketAddr, [u8; 16]> = HashMap::new();

        // Send broadcast scan requests
        self.send_broadcast_scan()?;

        // Set socket timeout for receiving
        let recv_timeout = Duration::from_millis(100);
        self.socket.set_read_timeout(Some(recv_timeout))?;

        // Collect scan responses
        while start.elapsed() < timeout {
            match self.recv_scan_response() {
                Ok((response, src_addr)) => {
                    let addr = SocketAddr::V4(SocketAddrV4::new(src_addr, IDN_PORT));

                    // Check if we already know this server
                    if let Some(unit_id) = addr_to_unit.get(&addr) {
                        // Update existing server if needed
                        if let Some(server) = servers.get_mut(unit_id) {
                            if !server.addresses.contains(&addr) {
                                server.addresses.push(addr);
                            }
                        }
                    } else {
                        // New server or new address
                        let entry = servers.entry(response.unit_id).or_insert_with(|| {
                            ServerInfo::new(
                                response.unit_id,
                                response.hostname_str().to_string(),
                                (
                                    response.protocol_version >> 4,
                                    response.protocol_version & 0x0F,
                                ),
                                response.status,
                            )
                        });

                        if !entry.addresses.contains(&addr) {
                            entry.addresses.push(addr);
                        }
                        addr_to_unit.insert(addr, response.unit_id);
                    }
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => {}
            }
        }

        // Query service maps for each discovered server
        for server in servers.values_mut() {
            if let Some(&addr) = server.addresses.first() {
                let _ = self.query_service_map(server, addr);
            }
        }

        debug!("IDN: scan complete, found {} servers", servers.len());
        Ok(servers.into_values().collect())
    }

    /// Send broadcast scan request to all network interfaces.
    fn send_broadcast_scan(&mut self) -> io::Result<()> {
        // Build scan request packet
        let seq = self.next_sequence();
        let header = PacketHeader {
            command: IDNCMD_SCAN_REQUEST,
            flags: self.client_group,
            sequence: seq,
        };

        let mut packet = Vec::with_capacity(PacketHeader::SIZE_BYTES);
        packet.write_bytes(header)?;

        // Send to broadcast address
        let broadcast_addr = SocketAddrV4::new(Ipv4Addr::BROADCAST, IDN_PORT);
        self.socket.send_to(&packet, broadcast_addr)?;

        Ok(())
    }

    /// Receive and parse a scan response.
    fn recv_scan_response(&mut self) -> io::Result<(ScanResponse, Ipv4Addr)> {
        let (len, src_addr) = self.socket.recv_from(&mut self.buffer)?;

        if len < PacketHeader::SIZE_BYTES + ScanResponse::SIZE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("packet too small: {} bytes", len),
            ));
        }

        let mut cursor = &self.buffer[..len];

        // Parse packet header
        let header: PacketHeader = cursor.read_bytes()?;

        if header.command != IDNCMD_SCAN_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected command: 0x{:02x}", header.command),
            ));
        }

        // Parse scan response
        let response: ScanResponse = cursor.read_bytes()?;

        // Extract IPv4 address
        let src_ip = match src_addr {
            SocketAddr::V4(v4) => *v4.ip(),
            SocketAddr::V6(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "IPv6 not supported",
                ))
            }
        };

        Ok((response, src_ip))
    }

    /// Query the service map from a server.
    fn query_service_map(&mut self, server: &mut ServerInfo, addr: SocketAddr) -> io::Result<()> {
        // Build service map request
        let seq = self.next_sequence();
        let header = PacketHeader {
            command: IDNCMD_SERVICEMAP_REQUEST,
            flags: self.client_group,
            sequence: seq,
        };

        let mut packet = Vec::with_capacity(PacketHeader::SIZE_BYTES);
        packet.write_bytes(header)?;

        // Send request
        self.socket.send_to(&packet, addr)?;

        // Set short timeout for service map response
        self.socket
            .set_read_timeout(Some(Duration::from_millis(500)))?;

        // Receive response
        let (len, _) = self.socket.recv_from(&mut self.buffer)?;

        if len < PacketHeader::SIZE_BYTES + ServiceMapResponseHeader::SIZE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "service map response too small",
            ));
        }

        let mut cursor = &self.buffer[..len];

        // Parse packet header
        let header: PacketHeader = cursor.read_bytes()?;

        if header.command != IDNCMD_SERVICEMAP_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected command: 0x{:02x}", header.command),
            ));
        }

        // Parse service map response header
        let map_header: ServiceMapResponseHeader = cursor.read_bytes()?;

        // Parse relay entries
        for _ in 0..map_header.relay_entry_count {
            let entry: ServiceMapEntry = cursor.read_bytes()?;
            server.relays.push(RelayInfo::from_entry(&entry));
        }

        // Parse service entries
        for _ in 0..map_header.service_entry_count {
            let entry: ServiceMapEntry = cursor.read_bytes()?;
            server.services.push(ServiceInfo::from_entry(&entry));
        }

        Ok(())
    }

    /// Get the next sequence number.
    fn next_sequence(&mut self) -> u16 {
        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        seq
    }

    /// Scan a specific address for IDN servers.
    ///
    /// This is useful for testing with mock servers on localhost where
    /// broadcast won't work.
    ///
    /// # Arguments
    ///
    /// * `addr` - The specific address to scan
    /// * `timeout` - How long to wait for responses
    pub fn scan_address(
        &mut self,
        addr: SocketAddr,
        timeout: Duration,
    ) -> io::Result<Vec<ServerInfo>> {
        let start = Instant::now();

        let mut servers: HashMap<[u8; 16], ServerInfo> = HashMap::new();
        let mut addr_to_unit: HashMap<SocketAddr, [u8; 16]> = HashMap::new();

        // Send scan request to specific address
        self.send_scan_to(addr)?;

        // Set socket timeout for receiving
        let recv_timeout = Duration::from_millis(100);
        self.socket.set_read_timeout(Some(recv_timeout))?;

        // Collect scan responses
        while start.elapsed() < timeout {
            match self.recv_scan_response_with_port() {
                Ok((response, src_addr)) => {
                    if let Some(unit_id) = addr_to_unit.get(&src_addr) {
                        if let Some(server) = servers.get_mut(unit_id) {
                            if !server.addresses.contains(&src_addr) {
                                server.addresses.push(src_addr);
                            }
                        }
                    } else {
                        let entry = servers.entry(response.unit_id).or_insert_with(|| {
                            ServerInfo::new(
                                response.unit_id,
                                response.hostname_str().to_string(),
                                (
                                    response.protocol_version >> 4,
                                    response.protocol_version & 0x0F,
                                ),
                                response.status,
                            )
                        });

                        if !entry.addresses.contains(&src_addr) {
                            entry.addresses.push(src_addr);
                        }
                        addr_to_unit.insert(src_addr, response.unit_id);
                    }
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                Err(_) => {}
            }
        }

        // Query service maps
        for server in servers.values_mut() {
            if let Some(&addr) = server.addresses.first() {
                let _ = self.query_service_map(server, addr);
            }
        }

        debug!(
            "IDN: scan_address complete, found {} servers",
            servers.len()
        );
        Ok(servers.into_values().collect())
    }

    /// Receive and parse a scan response, returning the full source address.
    fn recv_scan_response_with_port(&mut self) -> io::Result<(ScanResponse, SocketAddr)> {
        let (len, src_addr) = self.socket.recv_from(&mut self.buffer)?;

        if len < PacketHeader::SIZE_BYTES + ScanResponse::SIZE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("packet too small: {} bytes", len),
            ));
        }

        let mut cursor = &self.buffer[..len];

        let header: PacketHeader = cursor.read_bytes()?;

        if header.command != IDNCMD_SCAN_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected command: 0x{:02x}", header.command),
            ));
        }

        let response: ScanResponse = cursor.read_bytes()?;

        Ok((response, src_addr))
    }

    /// Send scan request to a specific address.
    fn send_scan_to(&mut self, addr: SocketAddr) -> io::Result<()> {
        let seq = self.next_sequence();
        let header = PacketHeader {
            command: IDNCMD_SCAN_REQUEST,
            flags: self.client_group,
            sequence: seq,
        };

        let mut packet = Vec::with_capacity(PacketHeader::SIZE_BYTES);
        packet.write_bytes(header)?;

        self.socket.send_to(&packet, addr)?;
        Ok(())
    }
}
