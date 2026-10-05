//! Ether Dream network DAC discovery.

use std::any::Any;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use crate::backend::{BackendKind, EtherDreamBackend, Result};
use crate::device::DacType;
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
use crate::protocols::ether_dream::backend::caps_for;
use crate::protocols::ether_dream::protocol::DacBroadcast;
use crate::protocols::ether_dream::recv_dac_broadcasts;

const PREFIX: &str = "etherdream";

struct ConnectData {
    broadcast: DacBroadcast,
    ip: IpAddr,
}

pub struct EtherDreamDiscoverer {
    timeout: Duration,
}

impl EtherDreamDiscoverer {
    pub fn new() -> Self {
        Self {
            // Ether Dream DACs broadcast once per second; allow ~1.5s to
            // reliably catch a broadcast.
            timeout: Duration::from_millis(1500),
        }
    }
}

impl Default for EtherDreamDiscoverer {
    fn default() -> Self {
        Self::new()
    }
}

fn format_mac(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// A scan result for one broadcast. Caps come from what the DAC advertises,
/// exactly as the backend computes them after connecting.
fn discovered_device(broadcast: DacBroadcast, ip: IpAddr) -> DiscoveredDevice {
    let device_mac = broadcast.mac_address;
    let stable_id = format!("{}:{}", PREFIX, format_mac(device_mac));
    let info = DiscoveredDeviceInfo::new(DacType::EtherDream, stable_id, ip.to_string())
        .with_ip(ip)
        .with_mac(device_mac);
    let caps = caps_for(broadcast.buffer_capacity, broadcast.max_point_rate);
    DiscoveredDevice::new(info, Box::new(ConnectData { broadcast, ip })).with_caps(caps)
}

impl Discoverer for EtherDreamDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::EtherDream
    }

    fn prefix(&self) -> &str {
        PREFIX
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let mut rx = match recv_dac_broadcasts() {
            Ok(rx) => rx,
            Err(e) => {
                log::warn!("Ether Dream discovery: failed to bind UDP broadcast socket: {e}");
                return Vec::new();
            }
        };
        // Short per-recv timeout so the loop can poll until the overall deadline
        // rather than blocking on a single broadcast.
        if let Err(e) = rx.set_timeout(Some(Duration::from_millis(200))) {
            log::warn!("Ether Dream discovery: failed to set socket timeout: {e}");
            return Vec::new();
        }

        let mut discovered = Vec::new();
        let mut seen_macs = std::collections::HashSet::new();

        // Keep reading datagrams until the scan window closes; each DAC
        // broadcasts ~once per second, so a fixed small cap could miss devices.
        let deadline = Instant::now() + self.timeout;
        while Instant::now() < deadline {
            let (broadcast, source_addr) = match rx.next_broadcast() {
                Ok(b) => b,
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    // No datagram this interval; keep polling until the deadline.
                    continue;
                }
                Err(e) => {
                    // A malformed/short datagram shouldn't abort the whole scan.
                    log::warn!("Ether Dream discovery: recv error: {e}");
                    continue;
                }
            };

            if !seen_macs.insert(broadcast.mac_address) {
                continue;
            }
            discovered.push(discovered_device(broadcast, source_addr.ip()));
        }
        discovered
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "EtherDream")?;
        BackendKind::fifo(Box::new(EtherDreamBackend::new(data.broadcast, data.ip)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn etherdream_stable_id_uses_mac() {
        let mac = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab];
        assert_eq!(format_mac(mac), "01:23:45:67:89:ab");
        let info = DiscoveredDeviceInfo::new(
            DacType::EtherDream,
            format!("{}:{}", PREFIX, format_mac(mac)),
            "192.168.1.100",
        )
        .with_ip("192.168.1.100".parse().unwrap())
        .with_mac(mac);
        assert_eq!(info.stable_id(), "etherdream:01:23:45:67:89:ab");
    }

    /// Regression: scan results reported the 1799-point default instead of
    /// the capacity and rate the DAC advertised.
    #[test]
    fn scan_caps_come_from_the_broadcast() {
        let broadcast = DacBroadcast {
            mac_address: [0x01, 0x23, 0x45, 0x67, 0x89, 0xab],
            hw_revision: 2,
            sw_revision: 2,
            buffer_capacity: 3899,
            max_point_rate: 50_000,
            dac_status: crate::protocols::ether_dream::protocol::DacStatus {
                protocol: 0,
                light_engine_state: 0,
                playback_state: 0,
                source: 0,
                light_engine_flags: 0,
                playback_flags: 0,
                source_flags: 0,
                buffer_fullness: 0,
                point_rate: 0,
                point_count: 0,
            },
        };
        let ip: IpAddr = "192.168.1.100".parse().unwrap();
        let device = discovered_device(broadcast, ip);
        assert_eq!(device.caps().max_points_per_chunk, 3899);
        assert_eq!(device.caps().pps_max, 50_000);
        assert_eq!(device.dac_info().caps.max_points_per_chunk, 3899);
        assert_eq!(device.info().stable_id(), "etherdream:01:23:45:67:89:ab");
    }
}
