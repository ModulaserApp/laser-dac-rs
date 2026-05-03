//! LaserCube WiFi DAC discovery.

use std::any::Any;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use crate::backend::{BackendKind, LasercubeWifiBackend, Result};
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
use crate::protocols::lasercube_wifi::dac::Addressed;
use crate::protocols::lasercube_wifi::discover_dacs;
use crate::protocols::lasercube_wifi::protocol::DeviceInfo;
use crate::types::DacType;

const PREFIX: &str = "lasercube-wifi";

struct ConnectData {
    info: DeviceInfo,
    source_addr: SocketAddr,
}

pub struct LasercubeWifiDiscoverer {
    timeout: Duration,
}

impl LasercubeWifiDiscoverer {
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_millis(100),
        }
    }
}

impl Default for LasercubeWifiDiscoverer {
    fn default() -> Self {
        Self::new()
    }
}

impl Discoverer for LasercubeWifiDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::LasercubeWifi
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

        let mut discovered = Vec::new();
        for _ in 0..10 {
            let (device_info, source_addr) = match discovery.next_device() {
                Ok(d) => d,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => break,
                Err(_) => continue,
            };
            let ip_address = source_addr.ip();
            let stable_id = format!("{}:{}", PREFIX, ip_address);
            let info = DiscoveredDeviceInfo::new(
                DacType::LasercubeWifi,
                stable_id,
                ip_address.to_string(),
            )
            .with_ip(ip_address);
            discovered.push(DiscoveredDevice::new(
                info,
                Box::new(ConnectData {
                    info: device_info,
                    source_addr,
                }),
            ));
        }
        discovered
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "LaserCube WiFi")?;
        let addressed = Addressed::from_discovery(&data.info, data.source_addr);
        Ok(BackendKind::Fifo(Box::new(LasercubeWifiBackend::new(
            addressed,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lasercube_wifi_stable_id_uses_ip() {
        let info = DiscoveredDeviceInfo::new(
            DacType::LasercubeWifi,
            format!("{}:192.168.1.50", PREFIX),
            "192.168.1.50",
        )
        .with_ip("192.168.1.50".parse().unwrap());
        assert_eq!(info.stable_id(), "lasercube-wifi:192.168.1.50");
    }
}
