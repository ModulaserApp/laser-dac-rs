//! IDN (ILDA Digital Network) DAC discovery.

use std::any::Any;
#[cfg(feature = "testutils")]
use std::net::SocketAddr;
use std::time::Duration;

use crate::backend::{BackendKind, IdnBackend, Result};
use crate::device::DacType;
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
use crate::protocols::idn::dac::{ServerInfo, ServiceInfo};
use crate::protocols::idn::scan_for_servers;
#[cfg(feature = "testutils")]
use crate::protocols::idn::ServerScanner;

const PREFIX: &str = "idn";

struct ConnectData {
    server: ServerInfo,
    service: ServiceInfo,
}

pub struct IdnDiscoverer {
    scan_timeout: Duration,
    /// When non-empty (testutils only), scan these specific addresses
    /// instead of using mDNS broadcast discovery.
    #[cfg(feature = "testutils")]
    scan_addresses: Vec<SocketAddr>,
}

impl IdnDiscoverer {
    pub fn new() -> Self {
        Self {
            scan_timeout: Duration::from_millis(500),
            #[cfg(feature = "testutils")]
            scan_addresses: Vec::new(),
        }
    }

    /// Build a discoverer that scans specific addresses instead of using
    /// broadcast discovery — useful for testing against mock servers on
    /// localhost. Only available with the `testutils` feature.
    #[cfg(feature = "testutils")]
    pub fn with_scan_addresses(addresses: Vec<SocketAddr>) -> Self {
        Self {
            scan_addresses: addresses,
            ..Self::new()
        }
    }

    /// Scan a single address. Only available with the `testutils` feature.
    #[cfg(feature = "testutils")]
    pub fn scan_address(&mut self, addr: SocketAddr) -> Vec<DiscoveredDevice> {
        let Ok(mut scanner) = ServerScanner::new(0) else {
            return Vec::new();
        };
        let Ok(servers) = scanner.scan_address(addr, self.scan_timeout) else {
            return Vec::new();
        };
        servers_to_devices(servers)
    }
}

impl Default for IdnDiscoverer {
    fn default() -> Self {
        Self::new()
    }
}

/// Format the unit ID as a lowercase hex string.
fn unit_id_hex(unit_id: &[u8; 16]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(32);
    for b in unit_id {
        let _ = write!(s, "{:02x}", b);
    }
    s
}

/// Build the stable id for a service.
///
/// The id is derived from the hardware unit ID (stable across IP/port
/// changes) rather than the hostname — factory-default hostnames collide on
/// identical fixtures. The service ID is suffixed so a multi-output server
/// yields a distinct, stable id per laser-projector service.
fn format_stable_id(unit_id: &[u8; 16], service_id: u8) -> String {
    format!("{}:{}:{}", PREFIX, unit_id_hex(unit_id), service_id)
}

fn servers_to_devices(servers: Vec<ServerInfo>) -> Vec<DiscoveredDevice> {
    let mut devices = Vec::new();
    for server in servers {
        let ip_address = server.addresses.first().map(|addr| addr.ip());
        let hostname = server.hostname.clone();

        // Emit one device per laser-projector service (a multi-output IDN
        // server surfaces as several devices, not just the first).
        let laser_services: Vec<ServiceInfo> = server
            .services
            .iter()
            .filter(|s| s.is_laser_projector())
            .cloned()
            .collect();
        let multi_service = laser_services.len() > 1;

        for service in laser_services {
            let stable_id = format_stable_id(&server.unit_id, service.service_id);
            let base_name = ip_address
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| hostname.clone());
            // Disambiguate the display name only when a server exposes more
            // than one laser service.
            let name = if multi_service {
                format!("{} ({})", base_name, service.name)
            } else {
                base_name
            };

            let mut info = DiscoveredDeviceInfo::new(DacType::Idn, stable_id, name)
                .with_hostname(hostname.clone());
            if let Some(ip) = ip_address {
                info = info.with_ip(ip);
            }
            devices.push(DiscoveredDevice::new(
                info,
                Box::new(ConnectData {
                    server: server.clone(),
                    service,
                }),
            ));
        }
    }
    devices
}

impl Discoverer for IdnDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Idn
    }

    fn prefix(&self) -> &str {
        PREFIX
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        #[cfg(feature = "testutils")]
        if !self.scan_addresses.is_empty() {
            let Ok(mut scanner) = ServerScanner::new(0) else {
                return Vec::new();
            };
            let mut out = Vec::new();
            for addr in &self.scan_addresses {
                let Ok(servers) = scanner.scan_address(*addr, self.scan_timeout) else {
                    continue;
                };
                out.extend(servers_to_devices(servers));
            }
            return out;
        }

        let Ok(servers) = scan_for_servers(self.scan_timeout) else {
            return Vec::new();
        };
        servers_to_devices(servers)
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "IDN")?;
        Ok(BackendKind::Fifo(Box::new(IdnBackend::new(
            data.server,
            data.service,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_stable_id_uses_unit_id_hex_and_service() {
        let unit_id: [u8; 16] = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        assert_eq!(
            format_stable_id(&unit_id, 1),
            "idn:0102030405060708090a0b0c0d0e0f10:1"
        );
        // Same unit, different service → distinct id.
        assert_eq!(
            format_stable_id(&unit_id, 2),
            "idn:0102030405060708090a0b0c0d0e0f10:2"
        );
    }

    #[test]
    fn stable_id_distinguishes_identical_hostnames() {
        // Two servers with the same factory-default hostname but different
        // unit IDs must not collide.
        let a = format_stable_id(&[0xaa; 16], 1);
        let b = format_stable_id(&[0xbb; 16], 1);
        assert_ne!(a, b);
    }

    #[test]
    fn servers_to_devices_emits_one_per_laser_service() {
        use crate::protocols::idn::dac::{ServiceInfo, ServiceType};

        let mut server = ServerInfo::new([0x11; 16], "dual".to_string(), (1, 0), 0);
        server
            .addresses
            .push("10.0.0.5:7255".parse::<std::net::SocketAddr>().unwrap());
        server.services.push(ServiceInfo {
            service_id: 1,
            service_type: ServiceType::LaserProjector,
            name: "Left".to_string(),
            flags: 0,
            relay_number: 0,
        });
        server.services.push(ServiceInfo {
            service_id: 2,
            service_type: ServiceType::LaserProjector,
            name: "Right".to_string(),
            flags: 0,
            relay_number: 0,
        });
        // A non-laser service is ignored.
        server.services.push(ServiceInfo {
            service_id: 3,
            service_type: ServiceType::Dmx512,
            name: "DMX".to_string(),
            flags: 0,
            relay_number: 0,
        });

        let devices = servers_to_devices(vec![server]);
        assert_eq!(devices.len(), 2, "one device per laser-projector service");
        let ids: Vec<String> = devices
            .iter()
            .map(|d| d.info().stable_id().to_string())
            .collect();
        assert!(ids.contains(&"idn:11111111111111111111111111111111:1".to_string()));
        assert!(ids.contains(&"idn:11111111111111111111111111111111:2".to_string()));
    }
}
