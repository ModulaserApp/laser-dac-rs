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
    /// Passive listener on the well-known alive port (45456). The active
    /// sockets above bind ephemeral ports, so they only see replies addressed
    /// to our requests' source endpoints. Firmware that announces itself
    /// unsolicited sends to the well-known port instead; this socket catches
    /// those. Bound with reuse flags so sharing the port is possible when the
    /// other listener and platform allow it. `None` if binding failed.
    passive_socket: Option<UdpSocket>,
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
    let passive_socket = match make_passive_alive_socket() {
        Ok(socket) => Some(socket),
        Err(e) => {
            // Expected when another client holds the port without reuse flags.
            log::debug!("discovery: could not bind passive listener on port {ALIVE_PORT}: {e}");
            None
        }
    };
    send_discovery_broadcasts(
        &udp_socket,
        passive_socket.as_ref(),
        &interfaces,
        &interface_sockets,
    );
    Ok(DiscoverDacs {
        socket: udp_socket,
        interface_sockets,
        passive_socket,
        buffer: [0; 1500],
        seen_ips: HashSet::new(),
    })
}

/// Create the passive listener on the well-known alive port. See the
/// `passive_socket` field on [`DiscoverDacs`].
fn make_passive_alive_socket() -> io::Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_broadcast(true)?;
    socket.set_reuse_address(true)?;
    // On macOS/BSD, sharing a UDP port across processes requires SO_REUSEPORT.
    #[cfg(unix)]
    socket.set_reuse_port(true)?;
    socket.bind(&SockAddr::from(SocketAddrV4::new(
        Ipv4Addr::UNSPECIFIED,
        ALIVE_PORT,
    )))?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
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
                    // Expected on some interfaces (e.g. VPN/virtual adapters).
                    log::debug!(
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
    passive_socket: Option<&UdpSocket>,
    interfaces: &[crate::net_utils::NetworkInterface],
    interface_sockets: &[UdpSocket],
) {
    let alive_socket = passive_socket.unwrap_or(socket);
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
            // Send failures are expected on some interfaces (e.g. VPN/virtual
            // adapters), so log at debug to avoid per-scan noise.
            if let Err(e) = socket.send_to(&command::get_full_info(), cmd_addr) {
                log::debug!("discovery: directed broadcast to {cmd_addr} failed: {e}");
            }
            if let Err(e) = alive_socket.send_to(&[CMD_ALIVE], alive_addr) {
                log::debug!("discovery: directed broadcast to {alive_addr} failed: {e}");
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
        if let Err(e) = alive_socket.send_to(
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
                log::debug!(
                    "discovery: per-interface limited full-info broadcast from {:?} failed: {e}",
                    iface_socket.local_addr()
                );
            }
            if let Err(e) = iface_socket.send_to(
                &[CMD_ALIVE],
                SocketAddrV4::new(Ipv4Addr::BROADCAST, ALIVE_PORT),
            ) {
                log::debug!(
                    "discovery: per-interface limited broadcast from {:?} failed: {e}",
                    iface_socket.local_addr()
                );
            }
        }
    }
}

impl DiscoverDacs {
    /// Construct a discovery driver around a single, caller-controlled main
    /// socket, with no interface/passive sockets. Used by tests to drive
    /// [`Self::next_device`] deterministically over loopback.
    #[cfg(test)]
    fn for_test(socket: UdpSocket) -> Self {
        Self {
            socket,
            interface_sockets: Vec::new(),
            passive_socket: None,
            buffer: [0; 1500],
            seen_ips: HashSet::new(),
        }
    }

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
                    // Expected noise on some platforms, e.g. Windows reports
                    // ICMP port-unreachable from earlier broadcasts as recv
                    // errors (ECONNRESET).
                    log::debug!(
                        "discovery: failed to receive from interface socket {:?}: {e}",
                        iface_socket.local_addr()
                    );
                }
            }
        }
        self.recv_passive()
    }

    /// Drain the passive alive-port listener. Packets here include our own
    /// looped-back broadcast requests (a bare `[CMD_ALIVE]`, which fails the
    /// `is_alive_response` check) and unsolicited device announcements.
    /// Follow-up requests are sent from the main socket, so replies are tagged
    /// `ReceiveSocket::Main`.
    fn recv_passive(&mut self) -> Option<(usize, SocketAddr, ReceiveSocket)> {
        let passive_socket = self.passive_socket.as_ref()?;
        loop {
            match passive_socket.recv_from(&mut self.buffer) {
                Ok((len, source_addr)) => {
                    if self.is_local_ip(source_addr.ip()) {
                        continue;
                    }
                    log::trace!("discovery: {len} bytes from {source_addr} on passive socket");
                    return Some((len, source_addr, ReceiveSocket::Main));
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return None;
                }
                Err(e) => {
                    log::debug!("discovery: failed to receive from passive socket: {e}");
                    return None;
                }
            }
        }
    }

    /// Whether `ip` is one of this host's own addresses (our broadcasts are
    /// looped back to the passive socket).
    fn is_local_ip(&self, ip: IpAddr) -> bool {
        self.interface_sockets.iter().any(|socket| {
            socket
                .local_addr()
                .is_ok_and(|local_addr| local_addr.ip() == ip)
        })
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
                Err(e) if e.kind() == io::ErrorKind::ConnectionReset => {
                    // Windows reports ICMP port-unreachable from earlier
                    // broadcasts as recv errors. Ignore and keep draining.
                    log::debug!("discovery: ignoring connection reset from main socket: {e}");
                    continue;
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

/// Stable id from the hardware serial number, which survives DHCP lease
/// changes. Falls back to the IP form when the serial is unavailable (empty or
/// all-zero). The IP is retained separately as an identifier hint on the
/// discovered device info.
fn stable_id_for(status: &LaserCubeNetworkStatus) -> String {
    let serial = status.serial_number.trim();
    if !serial.is_empty() && serial.bytes().any(|b| b != b'0') {
        format!("{}:{}", PREFIX, serial)
    } else {
        format_stable_id_ip(status.ip)
    }
}

fn format_stable_id_ip(ip: IpAddr) -> String {
    format!("{}:{}", PREFIX, ip)
}

/// Human-readable device name, preferring the advertised model name and always
/// disambiguating by IP.
fn device_name(status: &LaserCubeNetworkStatus, ip: IpAddr) -> String {
    if status.model_name.is_empty() {
        format!("LaserCube {}", ip)
    } else {
        format!("{} {}", status.model_name, ip)
    }
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
            let stable_id = stable_id_for(&addressed.status);
            let name = device_name(&addressed.status, ip);
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
    use super::super::profiles::ConnectionType;
    use super::super::protocol::CMD_GET_FULL_INFO;
    use super::*;

    /// Build a valid 64-byte full-info response advertising `model` and the
    /// given raw connection-type byte.
    fn full_info_packet(model: &str, connection_type: u8) -> Vec<u8> {
        let mut d = vec![0u8; 64];
        d[0] = CMD_GET_FULL_INFO; // 0x77
        d[3] = 1; // firmware major
        d[4] = 24; // firmware minor
        d[10..14].copy_from_slice(&30_000u32.to_le_bytes()); // point_rate
        d[14..18].copy_from_slice(&30_000u32.to_le_bytes()); // point_rate_max
        d[19..21].copy_from_slice(&3000u16.to_le_bytes()); // buffer_free
        d[21..23].copy_from_slice(&6000u16.to_le_bytes()); // buffer_max
        d[25] = connection_type;
        d[37] = 10; // model number
        d[38..38 + model.len()].copy_from_slice(model.as_bytes());
        d
    }

    fn loopback_pair() -> (UdpSocket, UdpSocket, SocketAddr) {
        let main = UdpSocket::bind("127.0.0.1:0").unwrap();
        main.set_read_timeout(Some(Duration::from_millis(250)))
            .unwrap();
        let main_addr = main.local_addr().unwrap();
        let device = UdpSocket::bind("127.0.0.1:0").unwrap();
        (main, device, main_addr)
    }

    #[test]
    fn stable_id_uses_serial_when_available() {
        let mut status = LaserCubeNetworkStatus::minimal("192.168.1.50".parse().unwrap());
        status.serial_number = "010203040506".to_string();
        assert_eq!(stable_id_for(&status), "lasercube-network:010203040506");
    }

    #[test]
    fn stable_id_falls_back_to_ip_without_serial() {
        let mut status = LaserCubeNetworkStatus::minimal("192.168.1.50".parse().unwrap());
        status.serial_number = String::new();
        assert_eq!(stable_id_for(&status), "lasercube-network:192.168.1.50");
        // An all-zero serial is not a usable identity; fall back to IP.
        status.serial_number = "000000000000".to_string();
        assert_eq!(stable_id_for(&status), "lasercube-network:192.168.1.50");
    }

    #[test]
    fn stable_id_ip_form_keeps_existing_prefix() {
        let ip: IpAddr = "192.168.1.50".parse().unwrap();
        assert_eq!(format_stable_id_ip(ip), "lasercube-network:192.168.1.50");
    }

    #[test]
    fn detects_exact_alive_response() {
        assert!(is_alive_response(&[0x27, 0x00]));
        assert!(!is_alive_response(&[0x27]));
        assert!(!is_alive_response(&[0x27, 0x01]));
    }

    #[test]
    fn device_name_prefers_model_and_disambiguates_by_ip() {
        let ip: IpAddr = "10.0.0.5".parse().unwrap();
        let mut status = LaserCubeNetworkStatus::minimal(ip);
        status.model_name = "Ultra Mk2".to_string();
        assert_eq!(device_name(&status, ip), "Ultra Mk2 10.0.0.5");
        status.model_name.clear();
        assert_eq!(device_name(&status, ip), "LaserCube 10.0.0.5");
    }

    #[test]
    fn next_device_parses_valid_full_info_response() {
        let (main, device, main_addr) = loopback_pair();
        let device_ip = device.local_addr().unwrap().ip();
        device
            .send_to(&full_info_packet("Ultra Mk2", 1), main_addr)
            .unwrap();

        let mut discovery = DiscoverDacs::for_test(main);
        let addressed = discovery.next_device().unwrap();

        assert_eq!(addressed.ip(), device_ip);
        assert_eq!(addressed.status.model_name, "Ultra Mk2");
        assert_eq!(addressed.status.connection_type, ConnectionType::WifiServer);
        assert_eq!(addressed.status.buffer_max, 6000);
        // Profile is derived from the advertised connection type + buffer.
        assert_eq!(
            addressed.profile.connection_type,
            ConnectionType::WifiServer
        );
    }

    #[test]
    fn next_device_dedups_by_source_ip() {
        let (main, device, main_addr) = loopback_pair();
        device
            .send_to(&full_info_packet("A", 0), main_addr)
            .unwrap();
        device
            .send_to(&full_info_packet("A", 0), main_addr)
            .unwrap();

        let mut discovery = DiscoverDacs::for_test(main);
        assert!(discovery.next_device().is_ok());
        // Second announcement from the same IP is suppressed, so the driver
        // drains until the read timeout fires.
        let err = discovery.next_device().unwrap_err();
        assert!(matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn next_device_discards_truncated_and_wrong_magic_packets() {
        let (main, device, main_addr) = loopback_pair();
        // Truncated full-info (wrong length).
        device.send_to(&[0x77, 0x00, 0x00], main_addr).unwrap();
        // Correct length but wrong leading command byte.
        let mut wrong_magic = full_info_packet("X", 0);
        wrong_magic[0] = 0x99;
        device.send_to(&wrong_magic, main_addr).unwrap();

        let mut discovery = DiscoverDacs::for_test(main);
        let err = discovery.next_device().unwrap_err();
        assert!(matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn next_device_handles_alive_response_without_yielding_device() {
        let (main, device, main_addr) = loopback_pair();
        device.send_to(&[CMD_ALIVE, 0x00], main_addr).unwrap();

        let mut discovery = DiscoverDacs::for_test(main);
        // An alive response triggers a unicast full-info request and continues;
        // with no follow-up it drains to a timeout.
        let err = discovery.next_device().unwrap_err();
        assert!(matches!(
            err.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        ));
    }

    #[test]
    fn scan_runs_and_returns_without_devices() {
        // Exercises the live discovery setup (socket binding, broadcasts, and
        // the timeout-terminated scan loop). No LaserCube is expected in the
        // test environment, so this must complete quickly and not panic.
        let mut discoverer = LaserCubeNetworkDiscoverer::new();
        let _devices = discoverer.scan();
    }
}
