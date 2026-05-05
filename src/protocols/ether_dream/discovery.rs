//! Ether Dream network DAC discovery.

use std::any::Any;
use std::net::IpAddr;
use std::time::Duration;

use crate::backend::{BackendKind, EtherDreamBackend, Result};
use crate::device::DacType;
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
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

impl Discoverer for EtherDreamDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::EtherDream
    }

    fn prefix(&self) -> &str {
        PREFIX
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let Ok(mut rx) = recv_dac_broadcasts() else {
            return Vec::new();
        };
        if rx.set_timeout(Some(self.timeout)).is_err() {
            return Vec::new();
        }

        let mut discovered = Vec::new();
        let mut seen_macs = std::collections::HashSet::new();

        for _ in 0..3 {
            let (broadcast, source_addr) = match rx.next_broadcast() {
                Ok(b) => b,
                Err(_) => break,
            };

            let ip = source_addr.ip();
            let device_mac = broadcast.mac_address;
            if !seen_macs.insert(device_mac) {
                continue;
            }

            let stable_id = format!("{}:{}", PREFIX, format_mac(device_mac));
            let info = DiscoveredDeviceInfo::new(DacType::EtherDream, stable_id, ip.to_string())
                .with_ip(ip)
                .with_mac(device_mac);
            discovered.push(DiscoveredDevice::new(
                info,
                Box::new(ConnectData { broadcast, ip }),
            ));
        }
        discovered
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "EtherDream")?;
        Ok(BackendKind::Fifo(Box::new(EtherDreamBackend::new(
            data.broadcast,
            data.ip,
        ))))
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
}
