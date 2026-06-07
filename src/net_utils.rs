//! Shared network interface enumeration utilities.
//!
//! Provides a cross-platform way to enumerate local IPv4 network interfaces
//! and compute subnet-directed broadcast addresses.

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::io;
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

/// A local IPv4 network interface with its IP address and subnet mask.
#[derive(Debug, Clone)]
pub struct NetworkInterface {
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
}

impl NetworkInterface {
    /// Compute the subnet-directed broadcast address (ip | !mask).
    pub fn broadcast_address(&self) -> Ipv4Addr {
        let ip = u32::from(self.ip);
        let mask = u32::from(self.netmask);
        Ipv4Addr::from(ip | !mask)
    }
}

/// Enumerate all local IPv4 network interfaces, excluding loopback.
///
/// Returns an empty vec on failure, allowing callers to fall back to
/// limited broadcast (255.255.255.255).
pub fn get_local_interfaces() -> io::Result<Vec<NetworkInterface>> {
    let interfaces = if_addrs::get_if_addrs()?
        .into_iter()
        .filter_map(|iface| {
            if iface.is_loopback() {
                return None;
            }
            match iface.addr {
                if_addrs::IfAddr::V4(v4) => Some(NetworkInterface {
                    ip: v4.ip,
                    netmask: v4.netmask,
                }),
                _ => None,
            }
        })
        .collect();

    Ok(interfaces)
}

/// Create a non-blocking UDP broadcast socket bound to a specific interface IP.
pub fn broadcast_socket_bound_to(ip: Ipv4Addr) -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.bind(&SockAddr::from(SocketAddrV4::new(ip, 0)))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_broadcast_address_slash_24() {
        let iface = NetworkInterface {
            ip: Ipv4Addr::new(192, 168, 1, 100),
            netmask: Ipv4Addr::new(255, 255, 255, 0),
        };
        assert_eq!(iface.broadcast_address(), Ipv4Addr::new(192, 168, 1, 255));
    }

    #[test]
    fn test_broadcast_address_slash_16() {
        let iface = NetworkInterface {
            ip: Ipv4Addr::new(10, 0, 5, 42),
            netmask: Ipv4Addr::new(255, 255, 0, 0),
        };
        assert_eq!(iface.broadcast_address(), Ipv4Addr::new(10, 0, 255, 255));
    }

    #[test]
    fn test_broadcast_address_slash_30() {
        let iface = NetworkInterface {
            ip: Ipv4Addr::new(172, 16, 0, 1),
            netmask: Ipv4Addr::new(255, 255, 255, 252),
        };
        assert_eq!(iface.broadcast_address(), Ipv4Addr::new(172, 16, 0, 3));
    }
}
