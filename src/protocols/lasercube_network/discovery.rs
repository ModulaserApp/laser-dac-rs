//! LaserCube network discovery.

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use std::any::Any;
use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use crate::backend::{BackendKind, Result};
use crate::device::DacType;
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};

use super::backend::LaserCubeNetworkBackend;
use super::command;
use super::profiles::ConnectionProfile;
use super::protocol::{ALIVE_PORT, CMD_ALIVE, CMD_PORT};
use super::status::LaserCubeNetworkStatus;
use super::transport::AddressedDevice;

const PREFIX: &str = "lasercube-network";
const DEFAULT_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
struct ConnectData {
    addressed: AddressedDevice,
}

pub struct LaserCubeNetworkDiscoverer {
    timeout: Duration,
}

impl LaserCubeNetworkDiscoverer {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_millis(100),
        }
    }
}

impl Default for LaserCubeNetworkDiscoverer {
    fn default() -> Self {
        Self::new()
    }
}

pub struct DiscoverDacs {
    socket: UdpSocket,
    buffer: [u8; 1500],
    seen_ips: HashSet<IpAddr>,
}

pub fn discover_dacs() -> io::Result<DiscoverDacs> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.set_reuse_address(true)?;
    socket.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))?;
    socket.set_read_timeout(Some(DEFAULT_DISCOVERY_TIMEOUT))?;
    let udp_socket: UdpSocket = socket.into();
    send_discovery_broadcasts(&udp_socket)?;
    Ok(DiscoverDacs {
        socket: udp_socket,
        buffer: [0; 1500],
        seen_ips: HashSet::new(),
    })
}

fn send_discovery_broadcasts(socket: &UdpSocket) -> io::Result<()> {
    if let Ok(interfaces) = crate::net_utils::get_local_interfaces() {
        for iface in &interfaces {
            let cmd_addr = SocketAddrV4::new(iface.broadcast_address(), CMD_PORT);
            let alive_addr = SocketAddrV4::new(iface.broadcast_address(), ALIVE_PORT);
            for _ in 0..2 {
                let _ = socket.send_to(&command::get_full_info(), cmd_addr);
                let _ = socket.send_to(&[CMD_ALIVE], alive_addr);
            }
        }
    }
    for _ in 0..2 {
        let _ = socket.send_to(
            &command::get_full_info(),
            SocketAddrV4::new(Ipv4Addr::BROADCAST, CMD_PORT),
        );
        let _ = socket.send_to(
            &[CMD_ALIVE],
            SocketAddrV4::new(Ipv4Addr::BROADCAST, ALIVE_PORT),
        );
    }
    Ok(())
}

impl DiscoverDacs {
    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }

    pub fn next_device(&mut self) -> io::Result<AddressedDevice> {
        loop {
            let (len, source_addr) = self.socket.recv_from(&mut self.buffer)?;
            if is_alive_response(&self.buffer[..len]) {
                if !self.seen_ips.contains(&source_addr.ip()) {
                    let _ = self.socket.send_to(
                        &command::get_full_info(),
                        SocketAddr::new(source_addr.ip(), CMD_PORT),
                    );
                }
                continue;
            }
            if self.seen_ips.contains(&source_addr.ip()) {
                continue;
            }
            let status = match LaserCubeNetworkStatus::parse(&self.buffer[..len], source_addr.ip())
            {
                Ok(status) => status,
                Err(_) => continue,
            };
            self.seen_ips.insert(source_addr.ip());
            let buffer_total = status.buffer_max as usize;
            let profile = ConnectionProfile::for_connection(status.connection_type, buffer_total);
            return Ok(AddressedDevice {
                source_addr,
                status,
                profile,
            });
        }
    }
}

impl Iterator for DiscoverDacs {
    type Item = io::Result<AddressedDevice>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_device())
    }
}

fn is_alive_response(buffer: &[u8]) -> bool {
    buffer == [CMD_ALIVE, 0x00]
}

fn format_stable_id(ip: IpAddr) -> String {
    format!("{}:{}", PREFIX, ip)
}

impl Discoverer for LaserCubeNetworkDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::LaserCubeNetwork
    }

    fn prefix(&self) -> &str {
        PREFIX
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let Ok(mut discovery) = discover_dacs() else {
            return Vec::new();
        };
        if discovery.set_timeout(Some(self.timeout)).is_err() {
            return Vec::new();
        }

        let mut out = Vec::new();
        for _ in 0..10 {
            let addressed = match discovery.next_device() {
                Ok(d) => d,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => break,
                Err(_) => continue,
            };
            let ip = addressed.source_addr.ip();
            let stable_id = format_stable_id(ip);
            let name = if addressed.status.model_name.is_empty() {
                format!("LaserCube {}", ip)
            } else {
                format!("{} {}", addressed.status.model_name, ip)
            };
            let caps = super::capabilities_for_profile(addressed.profile);
            let info = DiscoveredDeviceInfo::new(DacType::LaserCubeNetwork, stable_id, name)
                .with_ip(ip)
                .with_hardware_name(addressed.status.serial_number.clone());
            out.push(
                DiscoveredDevice::new(info, Box::new(ConnectData { addressed })).with_caps(caps),
            );
        }
        out
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "LaserCube Network")?;
        Ok(BackendKind::Fifo(Box::new(LaserCubeNetworkBackend::new(
            data.addressed.clone(),
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_id_keeps_existing_lasercube_network_prefix() {
        let ip: IpAddr = "192.168.1.50".parse().unwrap();
        assert_eq!(format_stable_id(ip), "lasercube-network:192.168.1.50");
    }

    #[test]
    fn detects_exact_alive_response() {
        assert!(is_alive_response(&[0x27, 0x00]));
        assert!(!is_alive_response(&[0x27]));
        assert!(!is_alive_response(&[0x27, 0x01]));
    }
}
