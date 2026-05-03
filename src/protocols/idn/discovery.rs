//! IDN (ILDA Digital Network) DAC discovery.

use std::any::Any;
#[cfg(feature = "testutils")]
use std::net::SocketAddr;
use std::time::Duration;

use crate::backend::{BackendKind, IdnBackend, Result};
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
use crate::protocols::idn::dac::{ServerInfo, ServiceInfo};
use crate::protocols::idn::scan_for_servers;
#[cfg(feature = "testutils")]
use crate::protocols::idn::ServerScanner;
use crate::types::DacType;

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
            scan_timeout: Duration::from_millis(500),
            scan_addresses: addresses,
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

fn servers_to_devices(servers: Vec<ServerInfo>) -> Vec<DiscoveredDevice> {
    servers
        .into_iter()
        .filter_map(|server| {
            let service = server.find_laser_projector().cloned()?;
            let ip_address = server.addresses.first().map(|addr| addr.ip());
            let hostname = server.hostname.clone();

            let stable_id = format!("{}:{}", PREFIX, hostname);
            let name = ip_address
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| hostname.clone());

            let mut info =
                DiscoveredDeviceInfo::new(DacType::Idn, stable_id, name).with_hostname(hostname);
            if let Some(ip) = ip_address {
                info = info.with_ip(ip);
            }
            Some(DiscoveredDevice::new(
                info,
                Box::new(ConnectData { server, service }),
            ))
        })
        .collect()
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
    fn idn_stable_id_uses_hostname() {
        let info = DiscoveredDeviceInfo::new(
            DacType::Idn,
            format!("{}:laser-projector.local", PREFIX),
            "192.168.1.100",
        )
        .with_ip("192.168.1.100".parse().unwrap())
        .with_hostname("laser-projector.local");
        assert_eq!(info.stable_id(), "idn:laser-projector.local");
    }
}
