//! A library for communication with LaserCube WiFi DACs.
//!
//! This module provides a Rust interface for discovering and controlling LaserCube WiFi
//! laser DACs over the network using UDP.
//!
//! # Example
//!
//! ```no_run
//! use laser_dac::protocols::lasercube_wifi::{discover_dacs, dac};
//! use std::time::Duration;
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Discover LaserCube devices on the network
//!     let mut discovery = discover_dacs()?;
//!     discovery.set_timeout(Some(Duration::from_secs(2)))?;
//!
//!     // Find the first device
//!     let (dac_info, source_addr) = discovery.next().ok_or("no DAC found")??;
//!     println!("Found DAC at {}", source_addr);
//!
//!     // Connect to the DAC
//!     let addressed = dac::Addressed::from_discovery(&dac_info, source_addr);
//!     let mut stream = dac::stream::connect(&addressed)?;
//!
//!     // Stream some points...
//!     // stream.write_frame(&points, 30000)?;
//!
//!     Ok(())
//! }
//! ```

pub mod backend;
pub mod dac;
pub mod error;
pub mod protocol;

pub use backend::LasercubeWifiBackend;

use crate::types::{DacCapabilities, OutputModel};
use protocol::{command, DeviceInfo, CMD_PORT};

/// Returns the default capabilities for LaserCube WiFi DACs.
pub fn default_capabilities() -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max: 30_000,
        max_points_per_chunk: 6000,
        output_model: OutputModel::UdpTimed,
    }
}
use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;
use std::{io, net};

/// Default discovery timeout.
const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(1);

/// An iterator that discovers LaserCube DACs on the network.
///
/// This broadcasts discovery packets to all network interfaces and yields
/// device information as responses are received.
pub struct DiscoverDacs {
    socket: UdpSocket,
    buffer: [u8; 1500],
    seen_ips: HashSet<Ipv4Addr>,
}

/// Discover LaserCube DACs on the local network.
///
/// This function broadcasts a discovery packet to all network interfaces and
/// returns an iterator that yields discovered devices.
///
/// # Example
///
/// ```no_run
/// use laser_dac::protocols::lasercube_wifi::discover_dacs;
/// use std::time::Duration;
///
/// let mut discovery = discover_dacs().expect("failed to start discovery");
/// discovery.set_timeout(Some(Duration::from_secs(2))).unwrap();
///
/// for result in discovery {
///     match result {
///         Ok((info, addr)) => println!("Found DAC v{} at {}", info.version, addr),
///         Err(e) => eprintln!("Error: {}", e),
///     }
/// }
/// ```
pub fn discover_dacs() -> io::Result<DiscoverDacs> {
    // Create a UDP socket with broadcast enabled
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.set_reuse_address(true)?;

    // Bind to any address
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
    socket.bind(&SockAddr::from(bind_addr))?;

    // Set default timeout
    socket.set_read_timeout(Some(DEFAULT_DISCOVERY_TIMEOUT))?;

    let udp_socket: UdpSocket = socket.into();

    // Send discovery broadcast
    send_discovery_broadcast(&udp_socket)?;

    Ok(DiscoverDacs {
        socket: udp_socket,
        buffer: [0u8; 1500],
        seen_ips: HashSet::new(),
    })
}

/// Send discovery broadcast packets to all network interfaces.
fn send_discovery_broadcast(socket: &UdpSocket) -> io::Result<()> {
    let discovery_cmd = command::get_full_info();

    // Try to get local interfaces and broadcast on each subnet
    if let Ok(interfaces) = crate::net_utils::get_local_interfaces() {
        for iface in &interfaces {
            let broadcast_addr = SocketAddrV4::new(iface.broadcast_address(), CMD_PORT);

            // Send discovery packet twice for reliability
            for _ in 0..2 {
                let _ = socket.send_to(&discovery_cmd, broadcast_addr);
            }
        }
    }

    // Also try the standard broadcast address
    let standard_broadcast = SocketAddrV4::new(Ipv4Addr::BROADCAST, CMD_PORT);
    for _ in 0..2 {
        let _ = socket.send_to(&discovery_cmd, standard_broadcast);
    }

    Ok(())
}

impl DiscoverDacs {
    /// Set the timeout for receiving discovery responses.
    ///
    /// Pass `None` for no timeout (blocking indefinitely).
    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }

    /// Attempt to receive the next discovery response.
    ///
    /// Returns `Ok((DeviceInfo, SocketAddr))` on success.
    /// Returns an `io::Error` with `ErrorKind::WouldBlock` or `ErrorKind::TimedOut`
    /// when no more responses are available within the timeout period.
    pub fn next_device(&mut self) -> io::Result<(DeviceInfo, net::SocketAddr)> {
        loop {
            let (len, src_addr) = self.socket.recv_from(&mut self.buffer)?;

            // Deduplicate by IP address
            if let SocketAddr::V4(addr_v4) = src_addr {
                if self.seen_ips.contains(addr_v4.ip()) {
                    continue;
                }
                self.seen_ips.insert(*addr_v4.ip());
            }

            // Parse the device info
            match DeviceInfo::from_discovery_response(&self.buffer[..len]) {
                Ok(info) => return Ok((info, src_addr)),
                Err(_) => continue, // Skip invalid responses
            }
        }
    }

    /// Send another discovery broadcast.
    ///
    /// This can be useful to re-scan the network without creating a new `DiscoverDacs`.
    pub fn rescan(&mut self) -> io::Result<()> {
        self.seen_ips.clear();
        send_discovery_broadcast(&self.socket)
    }
}

impl Iterator for DiscoverDacs {
    type Item = io::Result<(DeviceInfo, net::SocketAddr)>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_device())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_point_signed_conversion() {
        use protocol::Point;

        // Test center point (0 maps to 2048)
        let p = Point::from_signed(0, 0, 1000, 2000, 3000);
        assert_eq!(p.x, 2048);
        assert_eq!(p.y, 2048);

        let (x, y) = p.to_signed();
        assert_eq!(x, 0);
        assert_eq!(y, 0);

        // Test max positive (2047 maps to 4095, clamped)
        let p = Point::from_signed(2047, 2047, 0, 0, 0);
        assert_eq!(p.x, 4095);
        assert_eq!(p.y, 4095);
        let (x, y) = p.to_signed();
        assert_eq!(x, 2047);
        assert_eq!(y, 2047);

        // Test max negative (-2048 maps to 0)
        let p = Point::from_signed(-2048, -2048, 0, 0, 0);
        assert_eq!(p.x, 0);
        assert_eq!(p.y, 0);
        let (x, y) = p.to_signed();
        assert_eq!(x, -2048);
        assert_eq!(y, -2048);
    }

    #[test]
    fn test_blank_point() {
        use protocol::Point;

        let blank = Point::blank();
        assert_eq!(blank.x, Point::CENTER);
        assert_eq!(blank.y, Point::CENTER);
        assert_eq!(blank.r, 0);
        assert_eq!(blank.g, 0);
        assert_eq!(blank.b, 0);
    }
}
