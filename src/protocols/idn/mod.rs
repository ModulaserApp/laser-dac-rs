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
use socket2::{Domain, Protocol, Socket, Type};
use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

// Re-exports for convenience
pub use backend::IdnBackend;
pub use dac::{stream, Addressed, ServiceType};
pub use error::{CommunicationError, ProtocolError, ResponseError, Result};
pub use protocol::{AcknowledgeResponse, Point, PointExtended, PointXyrgbHighRes, PointXyrgbi};

use crate::types::{DacCapabilities, OutputModel};

/// Returns the default capabilities for IDN DACs.
///
/// These are conservative defaults for unknown devices. IDN has no protocol-defined
/// pps_min (rate is derived from sampleCount/chunkDuration).
pub fn default_capabilities() -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max: 100_000,
        max_points_per_chunk: 4096,
        prefers_constant_pps: false,
        can_estimate_queue: false,
        output_model: OutputModel::UdpTimed,
    }
}

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

/// A broadcast endpoint bound to a specific network interface.
struct BroadcastEndpoint {
    /// UDP socket bound to the interface IP, non-blocking, broadcast enabled
    socket: UdpSocket,
    /// Subnet-directed broadcast address for this interface
    broadcast_addr: SocketAddrV4,
}

/// Scanner for discovering IDN servers on the network.
///
/// Creates one broadcast socket per network interface for subnet-directed
/// broadcasts, plus a unicast socket for limited broadcast fallback and
/// service map queries.
pub struct ServerScanner {
    /// One broadcast endpoint per network interface
    endpoints: Vec<BroadcastEndpoint>,
    /// Fallback socket for limited broadcast + service map queries
    unicast_socket: UdpSocket,
    /// Client group (0-15)
    client_group: u8,
    /// Sequence number counter
    sequence: u16,
    /// Receive buffer
    buffer: [u8; 1500],
}

impl ServerScanner {
    /// Create a new server scanner.
    ///
    /// Enumerates network interfaces and creates a broadcast socket per interface.
    /// Falls back to limited broadcast if interface enumeration fails.
    pub fn new(client_group: u8) -> io::Result<Self> {
        let client_group = client_group & IDNMSK_PKTFLAGS_GROUP;

        // Create per-interface broadcast endpoints
        let mut endpoints = Vec::new();
        if let Ok(interfaces) = crate::net_utils::get_local_interfaces() {
            for iface in &interfaces {
                match Self::create_broadcast_socket(iface.ip) {
                    Ok(socket) => {
                        endpoints.push(BroadcastEndpoint {
                            socket,
                            broadcast_addr: SocketAddrV4::new(iface.broadcast_address(), IDN_PORT),
                        });
                    }
                    Err(e) => {
                        debug!(
                            "IDN: failed to create broadcast socket for {}: {}",
                            iface.ip, e
                        );
                    }
                }
            }
        }

        // Create unicast/fallback socket bound to 0.0.0.0 (blocking by default)
        let unicast_socket = Self::create_unicast_socket()?;

        Ok(Self {
            endpoints,
            unicast_socket,
            client_group,
            sequence: 0,
            buffer: [0u8; 1500],
        })
    }

    /// Create a non-blocking UDP socket with broadcast enabled, bound to the given IP.
    fn create_broadcast_socket(bind_ip: Ipv4Addr) -> io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_broadcast(true)?;
        socket.set_nonblocking(true)?;
        let bind_addr = SocketAddrV4::new(bind_ip, 0);
        socket.bind(&socket2::SockAddr::from(bind_addr))?;
        Ok(UdpSocket::from(socket))
    }

    /// Create a blocking UDP socket with broadcast enabled, bound to 0.0.0.0.
    fn create_unicast_socket() -> io::Result<UdpSocket> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_broadcast(true)?;
        let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
        socket.bind(&socket2::SockAddr::from(bind_addr))?;
        Ok(UdpSocket::from(socket))
    }

    /// Perform a scan for servers with the given timeout.
    pub fn scan(&mut self, timeout: Duration) -> io::Result<Vec<ServerInfo>> {
        let start = Instant::now();

        let mut servers: HashMap<[u8; 16], ServerInfo> = HashMap::new();
        let mut addr_to_unit: HashMap<SocketAddr, [u8; 16]> = HashMap::new();

        // Send broadcast scan requests on all interfaces
        self.send_broadcast_scan()?;

        // Set unicast socket to non-blocking for the polling loop
        self.unicast_socket.set_nonblocking(true)?;

        // Non-blocking polling loop across all sockets
        while start.elapsed() < timeout {
            let mut received_any = false;

            // Poll each per-interface endpoint
            for i in 0..self.endpoints.len() {
                let mut buf = [0u8; 1500];
                match self.endpoints[i].socket.recv_from(&mut buf) {
                    Ok((len, src_addr)) => {
                        if let Some((response, src_ip)) =
                            Self::process_scan_response(&buf[..len], &src_addr)
                        {
                            Self::record_server(&mut servers, &mut addr_to_unit, response, src_ip);
                        }
                        received_any = true;
                    }
                    Err(e)
                        if e.kind() == io::ErrorKind::WouldBlock
                            || e.kind() == io::ErrorKind::TimedOut =>
                    {
                        // No data on this socket
                    }
                    Err(_) => {}
                }
            }

            // Poll unicast/fallback socket
            match self.unicast_socket.recv_from(&mut self.buffer) {
                Ok((len, src_addr)) => {
                    if let Some((response, src_ip)) =
                        Self::process_scan_response(&self.buffer[..len], &src_addr)
                    {
                        Self::record_server(&mut servers, &mut addr_to_unit, response, src_ip);
                    }
                    received_any = true;
                }
                Err(e)
                    if e.kind() == io::ErrorKind::WouldBlock
                        || e.kind() == io::ErrorKind::TimedOut =>
                {
                    // No data
                }
                Err(_) => {}
            }

            if !received_any {
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        // Switch unicast socket to blocking for service map queries
        self.unicast_socket.set_nonblocking(false)?;

        // Query service maps for each discovered server
        for server in servers.values_mut() {
            if let Some(&addr) = server.addresses.first() {
                let _ = self.query_service_map(server, addr);
            }
        }

        debug!("IDN: scan complete, found {} servers", servers.len());
        Ok(servers.into_values().collect())
    }

    /// Record a scan response into the servers map.
    fn record_server(
        servers: &mut HashMap<[u8; 16], ServerInfo>,
        addr_to_unit: &mut HashMap<SocketAddr, [u8; 16]>,
        response: ScanResponse,
        src_ip: Ipv4Addr,
    ) {
        let addr = SocketAddr::V4(SocketAddrV4::new(src_ip, IDN_PORT));

        if let Some(unit_id) = addr_to_unit.get(&addr) {
            if let Some(server) = servers.get_mut(unit_id) {
                if !server.addresses.contains(&addr) {
                    server.addresses.push(addr);
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

            if !entry.addresses.contains(&addr) {
                entry.addresses.push(addr);
            }
            addr_to_unit.insert(addr, response.unit_id);
        }
    }

    /// Parse a scan response from a raw packet buffer.
    fn process_scan_response(
        data: &[u8],
        src_addr: &SocketAddr,
    ) -> Option<(ScanResponse, Ipv4Addr)> {
        if data.len() < PacketHeader::SIZE_BYTES + ScanResponse::SIZE_BYTES {
            return None;
        }

        let mut cursor = data;
        let header: PacketHeader = cursor.read_bytes().ok()?;

        if header.command != IDNCMD_SCAN_RESPONSE {
            return None;
        }

        let response: ScanResponse = cursor.read_bytes().ok()?;

        let src_ip = match src_addr {
            SocketAddr::V4(v4) => *v4.ip(),
            SocketAddr::V6(_) => return None,
        };

        Some((response, src_ip))
    }

    /// Send broadcast scan request on all interfaces + fallback.
    fn send_broadcast_scan(&mut self) -> io::Result<()> {
        let seq = self.next_sequence();
        let header = PacketHeader {
            command: IDNCMD_SCAN_REQUEST,
            flags: self.client_group,
            sequence: seq,
        };

        let mut packet = Vec::with_capacity(PacketHeader::SIZE_BYTES);
        packet.write_bytes(header)?;

        // Send on each per-interface socket to its subnet-directed broadcast
        for endpoint in &self.endpoints {
            let _ = endpoint.socket.send_to(&packet, endpoint.broadcast_addr);
        }

        // Fallback: send limited broadcast on unicast socket
        let broadcast_addr = SocketAddrV4::new(Ipv4Addr::BROADCAST, IDN_PORT);
        let _ = self.unicast_socket.send_to(&packet, broadcast_addr);

        Ok(())
    }

    /// Query the service map from a server.
    fn query_service_map(&mut self, server: &mut ServerInfo, addr: SocketAddr) -> io::Result<()> {
        let seq = self.next_sequence();
        let header = PacketHeader {
            command: IDNCMD_SERVICEMAP_REQUEST,
            flags: self.client_group,
            sequence: seq,
        };

        let mut packet = Vec::with_capacity(PacketHeader::SIZE_BYTES);
        packet.write_bytes(header)?;

        self.unicast_socket.send_to(&packet, addr)?;

        self.unicast_socket
            .set_read_timeout(Some(Duration::from_millis(500)))?;

        let (len, _) = self.unicast_socket.recv_from(&mut self.buffer)?;

        if len < PacketHeader::SIZE_BYTES + ServiceMapResponseHeader::SIZE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "service map response too small",
            ));
        }

        let mut cursor = &self.buffer[..len];

        let header: PacketHeader = cursor.read_bytes()?;

        if header.command != IDNCMD_SERVICEMAP_RESPONSE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected command: 0x{:02x}", header.command),
            ));
        }

        let map_header: ServiceMapResponseHeader = cursor.read_bytes()?;

        let header_extra =
            (map_header.struct_size as usize).saturating_sub(ServiceMapResponseHeader::SIZE_BYTES);
        if header_extra > 0 && cursor.len() >= header_extra {
            cursor = &cursor[header_extra..];
        }

        let entry_size = map_header.entry_size as usize;

        for _ in 0..map_header.relay_entry_count {
            if cursor.len() < entry_size {
                break;
            }
            let read_bytes = entry_size.min(ServiceMapEntry::SIZE_BYTES);
            let mut entry_buf = [0u8; ServiceMapEntry::SIZE_BYTES];
            entry_buf[..read_bytes].copy_from_slice(&cursor[..read_bytes]);
            let entry: ServiceMapEntry = (&entry_buf[..]).read_bytes()?;
            server.relays.push(RelayInfo::from_entry(&entry));
            cursor = &cursor[entry_size..];
        }

        for _ in 0..map_header.service_entry_count {
            if cursor.len() < entry_size {
                break;
            }
            let read_bytes = entry_size.min(ServiceMapEntry::SIZE_BYTES);
            let mut entry_buf = [0u8; ServiceMapEntry::SIZE_BYTES];
            entry_buf[..read_bytes].copy_from_slice(&cursor[..read_bytes]);
            let entry: ServiceMapEntry = (&entry_buf[..]).read_bytes()?;
            server.services.push(ServiceInfo::from_entry(&entry));
            cursor = &cursor[entry_size..];
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
        self.unicast_socket.set_read_timeout(Some(recv_timeout))?;

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
        let (len, src_addr) = self.unicast_socket.recv_from(&mut self.buffer)?;

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

        let _extra = (response.struct_size as usize).saturating_sub(ScanResponse::SIZE_BYTES);

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

        self.unicast_socket.send_to(&packet, addr)?;
        Ok(())
    }
}
