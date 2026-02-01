//! Shared network interface enumeration utilities.
//!
//! Provides a cross-platform way to enumerate local IPv4 network interfaces
//! and compute subnet-directed broadcast addresses.

use std::io;
use std::net::Ipv4Addr;

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
/// Returns an empty vec on failure or on unsupported platforms (Windows),
/// allowing callers to fall back to limited broadcast (255.255.255.255).
pub fn get_local_interfaces() -> io::Result<Vec<NetworkInterface>> {
    get_local_interfaces_impl()
}

#[cfg(unix)]
fn get_local_interfaces_impl() -> io::Result<Vec<NetworkInterface>> {
    let mut interfaces = Vec::new();

    unsafe {
        let mut ifaddrs: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifaddrs) != 0 {
            return Err(io::Error::last_os_error());
        }

        let mut current = ifaddrs;
        while !current.is_null() {
            let ifa = &*current;
            current = ifa.ifa_next;

            if ifa.ifa_addr.is_null() || ifa.ifa_netmask.is_null() {
                continue;
            }

            let family = (*ifa.ifa_addr).sa_family as i32;
            if family != libc::AF_INET {
                continue;
            }

            let addr = ifa.ifa_addr as *const libc::sockaddr_in;
            let ip_bytes = (*addr).sin_addr.s_addr.to_ne_bytes();
            let ip = Ipv4Addr::new(ip_bytes[0], ip_bytes[1], ip_bytes[2], ip_bytes[3]);

            if ip.is_loopback() {
                continue;
            }

            let mask_addr = ifa.ifa_netmask as *const libc::sockaddr_in;
            let mask_bytes = (*mask_addr).sin_addr.s_addr.to_ne_bytes();
            let netmask =
                Ipv4Addr::new(mask_bytes[0], mask_bytes[1], mask_bytes[2], mask_bytes[3]);

            interfaces.push(NetworkInterface { ip, netmask });
        }

        libc::freeifaddrs(ifaddrs);
    }

    Ok(interfaces)
}

#[cfg(windows)]
fn get_local_interfaces_impl() -> io::Result<Vec<NetworkInterface>> {
    // On Windows, return empty vec to fall back to limited broadcast.
    // A more complete implementation would use GetAdaptersAddresses.
    Ok(vec![])
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
