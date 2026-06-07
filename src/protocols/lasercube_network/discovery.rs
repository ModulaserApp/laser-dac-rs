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
    /// Per-interface sockets used for limited broadcasts (255.255.255.255).
    /// A limited broadcast from a socket bound to 0.0.0.0 only egresses on the
    /// interface the routing table picks, which on multi-homed machines (VPNs,
    /// virtual adapters) is often the wrong one. Binding to each interface IP
    /// forces the broadcast out of every interface. Kept alive so replies
    /// addressed to them can be drained.
    interface_sockets: Vec<UdpSocket>,
    buffer: [u8; 1500],
    seen_ips: HashSet<IpAddr>,
}

#[derive(Clone, Copy)]
enum ReceiveSocket {
    Main,
    Interface(usize),
}

pub fn discover_dacs() -> io::Result<DiscoverDacs> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.set_reuse_address(true)?;
    socket.bind(&SockAddr::from(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)))?;
    socket.set_read_timeout(Some(DEFAULT_DISCOVERY_TIMEOUT))?;
    let udp_socket: UdpSocket = socket.into();
    let interfaces = match crate::net_utils::get_local_interfaces() {
        Ok(interfaces) => interfaces,
        Err(e) => {
            log::warn!("discovery: failed to enumerate interfaces: {e}");
            Vec::new()
        }
    };
    let interface_sockets = make_interface_sockets(&interfaces);
    send_discovery_broadcasts(&udp_socket, &interfaces, &interface_sockets);
    Ok(DiscoverDacs {
        socket: udp_socket,
        interface_sockets,
        buffer: [0; 1500],
        seen_ips: HashSet::new(),
    })
}

/// Create one non-blocking broadcast socket per local interface, bound to the
/// interface IP so limited broadcasts egress on that specific interface.
fn make_interface_sockets(interfaces: &[crate::net_utils::NetworkInterface]) -> Vec<UdpSocket> {
    interfaces
        .iter()
        .filter_map(
            |iface| match crate::net_utils::broadcast_socket_bound_to(iface.ip) {
                Ok(socket) => Some(socket),
                Err(e) => {
                    log::warn!(
                        "discovery: failed to create broadcast socket on {}: {e}",
                        iface.ip
                    );
                    None
                }
            },
        )
        .collect()
}

fn send_discovery_broadcasts(
    socket: &UdpSocket,
    interfaces: &[crate::net_utils::NetworkInterface],
    interface_sockets: &[UdpSocket],
) {
    for iface in interfaces {
        log::debug!(
            "discovery: interface {} netmask {} -> directed broadcast {}",
            iface.ip,
            iface.netmask,
            iface.broadcast_address()
        );
        let cmd_addr = SocketAddrV4::new(iface.broadcast_address(), CMD_PORT);
        let alive_addr = SocketAddrV4::new(iface.broadcast_address(), ALIVE_PORT);
        for _ in 0..2 {
            if let Err(e) = socket.send_to(&command::get_full_info(), cmd_addr) {
                log::warn!("discovery: directed broadcast to {cmd_addr} failed: {e}");
            }
            if let Err(e) = socket.send_to(&[CMD_ALIVE], alive_addr) {
                log::warn!("discovery: directed broadcast to {alive_addr} failed: {e}");
            }
        }
    }
    if interfaces.is_empty() {
        log::warn!("discovery: no usable network interfaces found");
    }
    for _ in 0..2 {
        if let Err(e) = socket.send_to(
            &command::get_full_info(),
            SocketAddrV4::new(Ipv4Addr::BROADCAST, CMD_PORT),
        ) {
            log::warn!("discovery: limited broadcast (cmd) failed: {e}");
        }
        if let Err(e) = socket.send_to(
            &[CMD_ALIVE],
            SocketAddrV4::new(Ipv4Addr::BROADCAST, ALIVE_PORT),
        ) {
            log::warn!("discovery: limited broadcast (alive) failed: {e}");
        }
    }
    // Limited broadcast per interface: covers devices reachable on an
    // interface whose configured netmask doesn't match the device's subnet
    // (e.g. a LaserCube in AP mode). Replies arrive on the per-interface
    // socket and are drained in `next_device`.
    for iface_socket in interface_sockets {
        for _ in 0..2 {
            if let Err(e) = iface_socket.send_to(
                &command::get_full_info(),
                SocketAddrV4::new(Ipv4Addr::BROADCAST, CMD_PORT),
            ) {
                log::warn!(
                    "discovery: per-interface limited full-info broadcast from {:?} failed: {e}",
                    iface_socket.local_addr()
                );
            }
            if let Err(e) = iface_socket.send_to(
                &[CMD_ALIVE],
                SocketAddrV4::new(Ipv4Addr::BROADCAST, ALIVE_PORT),
            ) {
                log::warn!(
                    "discovery: per-interface limited broadcast from {:?} failed: {e}",
                    iface_socket.local_addr()
                );
            }
        }
    }
}

impl DiscoverDacs {
    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)
    }

    fn recv_interface(&mut self) -> Option<(usize, SocketAddr, ReceiveSocket)> {
        for (index, iface_socket) in self.interface_sockets.iter().enumerate() {
            match iface_socket.recv_from(&mut self.buffer) {
                Ok((len, source_addr)) => {
                    log::trace!(
                        "discovery: {len} bytes from {source_addr} on interface socket {:?}",
                        iface_socket.local_addr()
                    );
                    return Some((len, source_addr, ReceiveSocket::Interface(index)));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    log::warn!(
                        "discovery: failed to receive from interface socket {:?}: {e}",
                        iface_socket.local_addr()
                    );
                }
            }
        }
        None
    }

    fn recv_any(&mut self) -> io::Result<(usize, SocketAddr, ReceiveSocket)> {
        if let Some(received) = self.recv_interface() {
            return Ok(received);
        }

        let received = self.socket.recv_from(&mut self.buffer)?;
        log::trace!(
            "discovery: {} bytes from {} on main socket",
            received.0,
            received.1
        );
        Ok((received.0, received.1, ReceiveSocket::Main))
    }

    fn send_full_info(&self, receive_socket: ReceiveSocket, ip: IpAddr) {
        let addr = SocketAddr::new(ip, CMD_PORT);
        let result = match receive_socket {
            ReceiveSocket::Main => self.socket.send_to(&command::get_full_info(), addr),
            ReceiveSocket::Interface(index) => {
                self.interface_sockets[index].send_to(&command::get_full_info(), addr)
            }
        };
        if let Err(e) = result {
            log::warn!("discovery: unicast full-info request to {addr} failed: {e}");
        }
    }

    pub fn next_device(&mut self) -> io::Result<AddressedDevice> {
        loop {
            let (len, source_addr, receive_socket) = match self.recv_any() {
                Ok(received) => received,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if let Some(received) = self.recv_interface() {
                        received
                    } else {
                        return Err(e);
                    }
                }
                Err(e) => return Err(e),
            };
            if is_alive_response(&self.buffer[..len]) {
                if !self.seen_ips.contains(&source_addr.ip()) {
                    log::debug!(
                        "discovery: alive response from {}, sending unicast full-info request",
                        source_addr.ip()
                    );
                    self.send_full_info(receive_socket, source_addr.ip());
                }
                continue;
            }
            if self.seen_ips.contains(&source_addr.ip()) {
                continue;
            }
            let status = match LaserCubeNetworkStatus::parse(&self.buffer[..len], source_addr.ip())
            {
                Ok(status) => status,
                Err(e) => {
                    log::debug!(
                        "discovery: discarding {len}-byte packet from {source_addr}: {e:?}"
                    );
                    continue;
                }
            };
            log::debug!(
                "discovery: found device at {source_addr} (model: {:?})",
                status.model_name
            );
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
            let caps = super::capabilities_for_status(addressed.profile, &addressed.status);
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
