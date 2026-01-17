//! DAC device discovery.
//!
//! Provides a DAC interface for discovering and connecting to laser DAC devices
//! from multiple manufacturers.

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use crate::backend::{Error, Result, StreamBackend};
use crate::types::{DacType, DiscoveredDac, EnabledDacTypes};

// Feature-gated imports from internal protocol modules

#[cfg(feature = "helios")]
use crate::backend::HeliosBackend;
#[cfg(feature = "helios")]
use crate::protocols::helios::{HeliosDac, HeliosDacController};

#[cfg(feature = "ether-dream")]
use crate::backend::EtherDreamBackend;
#[cfg(feature = "ether-dream")]
use crate::protocols::ether_dream::protocol::DacBroadcast as EtherDreamBroadcast;
#[cfg(feature = "ether-dream")]
use crate::protocols::ether_dream::recv_dac_broadcasts;

#[cfg(feature = "idn")]
use crate::backend::IdnBackend;
#[cfg(feature = "idn")]
use crate::protocols::idn::dac::ServerInfo as IdnServerInfo;
#[cfg(feature = "idn")]
use crate::protocols::idn::dac::ServiceInfo as IdnServiceInfo;
#[cfg(feature = "idn")]
use crate::protocols::idn::scan_for_servers;
#[cfg(all(feature = "idn", feature = "testutils"))]
use crate::protocols::idn::ServerScanner;
#[cfg(all(feature = "idn", feature = "testutils"))]
use std::net::SocketAddr;

#[cfg(feature = "lasercube-wifi")]
use crate::backend::LasercubeWifiBackend;
#[cfg(feature = "lasercube-wifi")]
use crate::protocols::lasercube_wifi::dac::Addressed as LasercubeAddressed;
#[cfg(feature = "lasercube-wifi")]
use crate::protocols::lasercube_wifi::discover_dacs as discover_lasercube_wifi;
#[cfg(feature = "lasercube-wifi")]
use crate::protocols::lasercube_wifi::protocol::DeviceInfo as LasercubeDeviceInfo;

#[cfg(feature = "lasercube-usb")]
use crate::backend::LasercubeUsbBackend;
#[cfg(feature = "lasercube-usb")]
use crate::protocols::lasercube_usb::rusb;
#[cfg(feature = "lasercube-usb")]
use crate::protocols::lasercube_usb::DacController as LasercubeUsbController;

// =============================================================================
// Custom Discovery Source
// =============================================================================

/// Trait for implementing custom DAC discovery sources.
///
/// Implement this trait to add support for third-party or closed-source DAC
/// hardware to the discovery system. Custom sources are polled alongside
/// built-in backends by [`crate::DacDiscoveryWorker`].
///
/// # Example
///
/// ```ignore
/// use laser_dac::{CustomDiscoverySource, DiscoveredDac, DacBackend, DacType};
///
/// struct MyCustomDiscovery {
///     // Your discovery state
/// }
///
/// impl CustomDiscoverySource for MyCustomDiscovery {
///     fn poll_devices(&mut self) -> Vec<DiscoveredDac> {
///         // Scan for your custom DAC hardware
///         vec![DiscoveredDac {
///             dac_type: DacType::Custom("MyDAC".to_string()),
///             id: "device-001".to_string(),
///             name: "My Custom DAC".to_string(),
///             address: Some("192.168.1.100".to_string()),
///             metadata: None,
///         }]
///     }
///
///     fn create_backend(&self, device: &DiscoveredDac) -> Option<Box<dyn StreamBackend>> {
///         // Return your backend implementation
///         // Some(Box::new(MyCustomBackend::new(device)))
///         None
///     }
/// }
/// ```
///
/// # Integration with DacDiscoveryWorker
///
/// Register your custom source with the discovery worker builder:
///
/// ```ignore
/// use laser_dac::DacDiscoveryWorker;
///
/// let discovery = DacDiscoveryWorker::builder()
///     .add_custom_source(MyCustomDiscovery::new())
///     .build();
///
/// // Poll for devices as usual
/// for device in discovery.poll_new_devices() {
///     println!("Found: {}", device.name());
/// }
/// ```
pub trait CustomDiscoverySource: Send + 'static {
    /// Poll for available devices.
    ///
    /// Called periodically by the discovery worker (default: every 2 seconds).
    /// Return all currently visible devices - the worker handles deduplication
    /// based on the device's `id` field.
    fn poll_devices(&mut self) -> Vec<DiscoveredDac>;

    /// Create a streaming backend for connecting to a discovered device.
    ///
    /// Called when the discovery worker wants to connect to a device that
    /// passed the device filter. Return `None` if the device is no longer
    /// available or connection failed.
    fn create_backend(&self, device: &DiscoveredDac) -> Option<Box<dyn StreamBackend>>;
}

// =============================================================================
// DiscoveredDevice
// =============================================================================

/// A discovered but not-yet-connected DAC device.
///
/// Use `DacDiscovery::connect()` to establish a connection and get a backend.
pub struct DiscoveredDevice {
    dac_type: DacType,
    ip_address: Option<IpAddr>,
    mac_address: Option<[u8; 6]>,
    hostname: Option<String>,
    usb_address: Option<String>,
    hardware_name: Option<String>,
    inner: DiscoveredDeviceInner,
}

impl DiscoveredDevice {
    /// Returns the device name (unique identifier).
    /// For network devices: IP address.
    /// For USB devices: hardware name or bus:address.
    pub fn name(&self) -> String {
        self.ip_address
            .map(|ip| ip.to_string())
            .or_else(|| self.hardware_name.clone())
            .or_else(|| self.usb_address.clone())
            .unwrap_or_else(|| "Unknown".into())
    }

    /// Returns the DAC type.
    pub fn dac_type(&self) -> DacType {
        self.dac_type.clone()
    }

    /// Returns a lightweight, cloneable info struct for this device.
    pub fn info(&self) -> DiscoveredDeviceInfo {
        DiscoveredDeviceInfo {
            dac_type: self.dac_type.clone(),
            ip_address: self.ip_address,
            mac_address: self.mac_address,
            hostname: self.hostname.clone(),
            usb_address: self.usb_address.clone(),
            hardware_name: self.hardware_name.clone(),
        }
    }
}

/// Lightweight info about a discovered device.
///
/// This struct is Clone-able and can be used for filtering and reporting
/// without consuming the original `DiscoveredDevice`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiscoveredDeviceInfo {
    /// The DAC type.
    pub dac_type: DacType,
    /// IP address for network devices, None for USB devices.
    pub ip_address: Option<IpAddr>,
    /// MAC address (Ether Dream only).
    pub mac_address: Option<[u8; 6]>,
    /// Hostname (IDN only).
    pub hostname: Option<String>,
    /// USB bus:address (LaserCube USB only).
    pub usb_address: Option<String>,
    /// Device name from hardware (Helios only).
    pub hardware_name: Option<String>,
}

impl DiscoveredDeviceInfo {
    /// Returns the device name (human-readable).
    /// For network devices: IP address.
    /// For USB devices: hardware name or bus:address.
    pub fn name(&self) -> String {
        self.ip_address
            .map(|ip| ip.to_string())
            .or_else(|| self.hardware_name.clone())
            .or_else(|| self.usb_address.clone())
            .unwrap_or_else(|| "Unknown".into())
    }

    /// Returns a stable, namespaced identifier for the device.
    ///
    /// The ID is prefixed with the protocol name to avoid cross-protocol collisions:
    /// - Ether Dream: `etherdream:<mac>` (survives IP changes)
    /// - IDN: `idn:<hostname>` (mDNS name, survives IP changes)
    /// - Helios: `helios:<hardware_name>` (USB serial if available)
    /// - LaserCube USB: `lasercube-usb:<serial|bus:addr>`
    /// - LaserCube WiFi: `lasercube-wifi:<ip>` (best available)
    ///
    /// This is used for device tracking/deduplication in the discovery worker.
    pub fn stable_id(&self) -> String {
        match &self.dac_type {
            DacType::EtherDream => {
                if let Some(mac) = self.mac_address {
                    return format!(
                        "etherdream:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
                    );
                }
                if let Some(ip) = self.ip_address {
                    return format!("etherdream:{}", ip);
                }
            }
            DacType::Idn => {
                if let Some(ref hostname) = self.hostname {
                    return format!("idn:{}", hostname);
                }
                if let Some(ip) = self.ip_address {
                    return format!("idn:{}", ip);
                }
            }
            DacType::Helios => {
                if let Some(ref hw_name) = self.hardware_name {
                    return format!("helios:{}", hw_name);
                }
                if let Some(ref usb_addr) = self.usb_address {
                    return format!("helios:{}", usb_addr);
                }
            }
            DacType::LasercubeUsb => {
                if let Some(ref hw_name) = self.hardware_name {
                    return format!("lasercube-usb:{}", hw_name);
                }
                if let Some(ref usb_addr) = self.usb_address {
                    return format!("lasercube-usb:{}", usb_addr);
                }
            }
            DacType::LasercubeWifi => {
                if let Some(ip) = self.ip_address {
                    return format!("lasercube-wifi:{}", ip);
                }
            }
            DacType::Custom(name) => {
                // For custom types, use the custom name as prefix
                if let Some(ip) = self.ip_address {
                    return format!("{}:{}", name.to_lowercase(), ip);
                }
                if let Some(ref hw_name) = self.hardware_name {
                    return format!("{}:{}", name.to_lowercase(), hw_name);
                }
            }
        }

        // Fallback for unknown configurations
        format!("unknown:{:?}", self.dac_type)
    }
}

impl From<&DiscoveredDac> for DiscoveredDeviceInfo {
    fn from(dac: &DiscoveredDac) -> Self {
        DiscoveredDeviceInfo {
            dac_type: dac.dac_type.clone(),
            ip_address: dac.address.as_ref().and_then(|a| a.parse().ok()),
            mac_address: None,
            hostname: None,
            usb_address: None,
            hardware_name: Some(dac.name.clone()),
        }
    }
}

/// Internal data needed for connection (opaque to callers).
enum DiscoveredDeviceInner {
    #[cfg(feature = "helios")]
    Helios(HeliosDac),
    #[cfg(feature = "ether-dream")]
    EtherDream {
        broadcast: EtherDreamBroadcast,
        ip: IpAddr,
    },
    #[cfg(feature = "idn")]
    Idn {
        server: IdnServerInfo,
        service: IdnServiceInfo,
    },
    #[cfg(feature = "lasercube-wifi")]
    LasercubeWifi {
        info: LasercubeDeviceInfo,
        source_addr: std::net::SocketAddr,
    },
    #[cfg(feature = "lasercube-usb")]
    LasercubeUsb(rusb::Device<rusb::Context>),
    /// Placeholder variant to ensure enum is not empty when no features are enabled
    #[cfg(not(any(
        feature = "helios",
        feature = "ether-dream",
        feature = "idn",
        feature = "lasercube-wifi",
        feature = "lasercube-usb"
    )))]
    _Placeholder,
}

// =============================================================================
// Per-DAC Discovery Implementations
// =============================================================================

/// Discovery for Helios USB DACs.
#[cfg(feature = "helios")]
pub struct HeliosDiscovery {
    controller: HeliosDacController,
}

#[cfg(feature = "helios")]
impl HeliosDiscovery {
    /// Create a new Helios discovery instance.
    ///
    /// Returns None if the USB controller fails to initialize.
    pub fn new() -> Option<Self> {
        HeliosDacController::new()
            .ok()
            .map(|controller| Self { controller })
    }

    /// Scan for available Helios devices.
    pub fn scan(&self) -> Vec<DiscoveredDevice> {
        let Ok(devices) = self.controller.list_devices() else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        for device in devices {
            // Only process idle (unopened) devices
            let HeliosDac::Idle(_) = &device else {
                continue;
            };

            // Try to open to get name
            let opened = match device.open() {
                Ok(o) => o,
                Err(_) => continue,
            };

            let hardware_name = opened.name().unwrap_or_else(|_| "Unknown Helios".into());
            discovered.push(DiscoveredDevice {
                dac_type: DacType::Helios,
                ip_address: None,
                mac_address: None,
                hostname: None,
                usb_address: None,
                hardware_name: Some(hardware_name),
                inner: DiscoveredDeviceInner::Helios(opened),
            });
        }
        discovered
    }

    /// Connect to a discovered Helios device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn StreamBackend>> {
        let DiscoveredDeviceInner::Helios(dac) = device.inner else {
            return Err(Error::invalid_config("Invalid device type for Helios"));
        };
        Ok(Box::new(HeliosBackend::from_dac(dac)))
    }
}

/// Discovery for Ether Dream network DACs.
#[cfg(feature = "ether-dream")]
pub struct EtherDreamDiscovery {
    timeout: Duration,
}

#[cfg(feature = "ether-dream")]
impl EtherDreamDiscovery {
    /// Create a new Ether Dream discovery instance.
    pub fn new() -> Self {
        Self {
            // Ether Dream DACs broadcast once per second, so we need
            // at least 1.5s to reliably catch a broadcast
            timeout: Duration::from_millis(1500),
        }
    }

    /// Scan for available Ether Dream devices.
    pub fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let Ok(mut rx) = recv_dac_broadcasts() else {
            return Vec::new();
        };

        if rx.set_timeout(Some(self.timeout)).is_err() {
            return Vec::new();
        }

        let mut discovered = Vec::new();
        let mut seen_macs = std::collections::HashSet::new();

        // Only try 3 iterations max - we just need one device
        for _ in 0..3 {
            let (broadcast, source_addr) = match rx.next_broadcast() {
                Ok(b) => b,
                Err(_) => break,
            };

            let ip = source_addr.ip();

            // Skip duplicate MACs - but keep polling to find other devices
            if seen_macs.contains(&broadcast.mac_address) {
                continue;
            }
            seen_macs.insert(broadcast.mac_address);

            discovered.push(DiscoveredDevice {
                dac_type: DacType::EtherDream,
                ip_address: Some(ip),
                mac_address: Some(broadcast.mac_address),
                hostname: None,
                usb_address: None,
                hardware_name: None,
                inner: DiscoveredDeviceInner::EtherDream { broadcast, ip },
            });
        }
        discovered
    }

    /// Connect to a discovered Ether Dream device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn StreamBackend>> {
        let DiscoveredDeviceInner::EtherDream { broadcast, ip } = device.inner else {
            return Err(Error::invalid_config("Invalid device type for EtherDream"));
        };

        let backend = EtherDreamBackend::new(broadcast, ip);
        Ok(Box::new(backend))
    }
}

#[cfg(feature = "ether-dream")]
impl Default for EtherDreamDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Discovery for IDN (ILDA Digital Network) DACs.
#[cfg(feature = "idn")]
pub struct IdnDiscovery {
    scan_timeout: Duration,
}

#[cfg(feature = "idn")]
impl IdnDiscovery {
    /// Create a new IDN discovery instance.
    pub fn new() -> Self {
        Self {
            scan_timeout: Duration::from_millis(500),
        }
    }

    /// Scan for available IDN devices.
    pub fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let Ok(servers) = scan_for_servers(self.scan_timeout) else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        for server in servers {
            let Some(service) = server.find_laser_projector().cloned() else {
                continue;
            };

            let ip_address = server.addresses.first().map(|addr| addr.ip());
            let hostname = server.hostname.clone();

            discovered.push(DiscoveredDevice {
                dac_type: DacType::Idn,
                ip_address,
                mac_address: None,
                hostname: Some(hostname),
                usb_address: None,
                hardware_name: None,
                inner: DiscoveredDeviceInner::Idn { server, service },
            });
        }
        discovered
    }

    /// Connect to a discovered IDN device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn StreamBackend>> {
        let DiscoveredDeviceInner::Idn { server, service } = device.inner else {
            return Err(Error::invalid_config("Invalid device type for IDN"));
        };

        Ok(Box::new(IdnBackend::new(server, service)))
    }

    /// Scan a specific address for IDN devices.
    ///
    /// This is useful for testing with mock servers on localhost where
    /// broadcast won't work.
    ///
    /// This method is only available with the `testutils` feature.
    #[cfg(feature = "testutils")]
    pub fn scan_address(&mut self, addr: SocketAddr) -> Vec<DiscoveredDevice> {
        let Ok(mut scanner) = ServerScanner::new(0) else {
            return Vec::new();
        };

        let Ok(servers) = scanner.scan_address(addr, self.scan_timeout) else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        for server in servers {
            let Some(service) = server.find_laser_projector().cloned() else {
                continue;
            };

            let ip_address = server.addresses.first().map(|addr| addr.ip());
            let hostname = server.hostname.clone();

            discovered.push(DiscoveredDevice {
                dac_type: DacType::Idn,
                ip_address,
                mac_address: None,
                hostname: Some(hostname),
                usb_address: None,
                hardware_name: None,
                inner: DiscoveredDeviceInner::Idn { server, service },
            });
        }
        discovered
    }
}

#[cfg(feature = "idn")]
impl Default for IdnDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Discovery for LaserCube WiFi DACs.
#[cfg(feature = "lasercube-wifi")]
pub struct LasercubeWifiDiscovery {
    timeout: Duration,
}

#[cfg(feature = "lasercube-wifi")]
impl LasercubeWifiDiscovery {
    /// Create a new LaserCube WiFi discovery instance.
    pub fn new() -> Self {
        Self {
            timeout: Duration::from_millis(100),
        }
    }

    /// Scan for available LaserCube WiFi devices.
    pub fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let Ok(mut discovery) = discover_lasercube_wifi() else {
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

            discovered.push(DiscoveredDevice {
                dac_type: DacType::LasercubeWifi,
                ip_address: Some(ip_address),
                mac_address: None,
                hostname: None,
                usb_address: None,
                hardware_name: None,
                inner: DiscoveredDeviceInner::LasercubeWifi {
                    info: device_info,
                    source_addr,
                },
            });
        }
        discovered
    }

    /// Connect to a discovered LaserCube WiFi device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn StreamBackend>> {
        let DiscoveredDeviceInner::LasercubeWifi { info, source_addr } = device.inner else {
            return Err(Error::invalid_config(
                "Invalid device type for LaserCube WiFi",
            ));
        };

        let addressed = LasercubeAddressed::from_discovery(&info, source_addr);
        Ok(Box::new(LasercubeWifiBackend::new(addressed)))
    }
}

#[cfg(feature = "lasercube-wifi")]
impl Default for LasercubeWifiDiscovery {
    fn default() -> Self {
        Self::new()
    }
}

/// Discovery for LaserCube USB DACs (LaserDock).
#[cfg(feature = "lasercube-usb")]
pub struct LasercubeUsbDiscovery {
    controller: LasercubeUsbController,
}

#[cfg(feature = "lasercube-usb")]
impl LasercubeUsbDiscovery {
    /// Create a new LaserCube USB discovery instance.
    ///
    /// Returns None if the USB controller fails to initialize.
    pub fn new() -> Option<Self> {
        LasercubeUsbController::new()
            .ok()
            .map(|controller| Self { controller })
    }

    /// Scan for available LaserCube USB devices.
    pub fn scan(&self) -> Vec<DiscoveredDevice> {
        let Ok(devices) = self.controller.list_devices() else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        for device in devices {
            let usb_address = format!("{}:{}", device.bus_number(), device.address());
            let serial = crate::protocols::lasercube_usb::get_serial_number(&device);

            discovered.push(DiscoveredDevice {
                dac_type: DacType::LasercubeUsb,
                ip_address: None,
                mac_address: None,
                hostname: None,
                usb_address: Some(usb_address),
                hardware_name: serial,
                inner: DiscoveredDeviceInner::LasercubeUsb(device),
            });
        }
        discovered
    }

    /// Connect to a discovered LaserCube USB device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn StreamBackend>> {
        let DiscoveredDeviceInner::LasercubeUsb(usb_device) = device.inner else {
            return Err(Error::invalid_config(
                "Invalid device type for LaserCube USB",
            ));
        };

        let backend = LasercubeUsbBackend::new(usb_device);
        Ok(Box::new(backend))
    }
}

// =============================================================================
// DAC Discovery
// =============================================================================

/// DAC discovery coordinator for all DAC types.
///
/// This provides a single entry point for discovering and connecting to any
/// supported DAC hardware.
pub struct DacDiscovery {
    #[cfg(feature = "helios")]
    helios: Option<HeliosDiscovery>,
    #[cfg(feature = "ether-dream")]
    etherdream: EtherDreamDiscovery,
    #[cfg(feature = "idn")]
    idn: IdnDiscovery,
    #[cfg(all(feature = "idn", feature = "testutils"))]
    idn_scan_addresses: Vec<SocketAddr>,
    #[cfg(feature = "lasercube-wifi")]
    lasercube_wifi: LasercubeWifiDiscovery,
    #[cfg(feature = "lasercube-usb")]
    lasercube_usb: Option<LasercubeUsbDiscovery>,
    enabled: EnabledDacTypes,
}

impl DacDiscovery {
    /// Create a new DAC discovery instance.
    ///
    /// This initializes USB controllers, so it should be called from the main thread.
    /// If a USB controller fails to initialize, that DAC type will be unavailable
    /// but other types will still work.
    pub fn new(enabled: EnabledDacTypes) -> Self {
        Self {
            #[cfg(feature = "helios")]
            helios: HeliosDiscovery::new(),
            #[cfg(feature = "ether-dream")]
            etherdream: EtherDreamDiscovery::new(),
            #[cfg(feature = "idn")]
            idn: IdnDiscovery::new(),
            #[cfg(all(feature = "idn", feature = "testutils"))]
            idn_scan_addresses: Vec::new(),
            #[cfg(feature = "lasercube-wifi")]
            lasercube_wifi: LasercubeWifiDiscovery::new(),
            #[cfg(feature = "lasercube-usb")]
            lasercube_usb: LasercubeUsbDiscovery::new(),
            enabled,
        }
    }

    /// Set specific addresses to scan for IDN servers.
    ///
    /// When set, the scanner will scan these specific addresses instead of
    /// using broadcast discovery. This is useful for testing with mock servers.
    ///
    /// This method is only available with the `testutils` feature.
    #[cfg(all(feature = "idn", feature = "testutils"))]
    pub fn set_idn_scan_addresses(&mut self, addresses: Vec<SocketAddr>) {
        self.idn_scan_addresses = addresses;
    }

    /// Update which DAC types to scan for.
    pub fn set_enabled(&mut self, enabled: EnabledDacTypes) {
        self.enabled = enabled;
    }

    /// Returns the currently enabled DAC types.
    pub fn enabled(&self) -> &EnabledDacTypes {
        &self.enabled
    }

    /// Scan for available DAC devices of all enabled types.
    ///
    /// Returns a list of discovered devices. Each device can be connected
    /// using `connect()`.
    pub fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let mut devices = Vec::new();

        // Helios
        #[cfg(feature = "helios")]
        if self.enabled.is_enabled(DacType::Helios) {
            if let Some(ref discovery) = self.helios {
                devices.extend(discovery.scan());
            }
        }

        // Ether Dream
        #[cfg(feature = "ether-dream")]
        if self.enabled.is_enabled(DacType::EtherDream) {
            devices.extend(self.etherdream.scan());
        }

        // IDN
        #[cfg(feature = "idn")]
        if self.enabled.is_enabled(DacType::Idn) {
            #[cfg(feature = "testutils")]
            {
                if self.idn_scan_addresses.is_empty() {
                    // Use broadcast discovery
                    devices.extend(self.idn.scan());
                } else {
                    // Scan specific addresses (for testing with mock servers)
                    for addr in &self.idn_scan_addresses {
                        devices.extend(self.idn.scan_address(*addr));
                    }
                }
            }
            #[cfg(not(feature = "testutils"))]
            {
                devices.extend(self.idn.scan());
            }
        }

        // LaserCube WiFi
        #[cfg(feature = "lasercube-wifi")]
        if self.enabled.is_enabled(DacType::LasercubeWifi) {
            devices.extend(self.lasercube_wifi.scan());
        }

        // LaserCube USB
        #[cfg(feature = "lasercube-usb")]
        if self.enabled.is_enabled(DacType::LasercubeUsb) {
            if let Some(ref discovery) = self.lasercube_usb {
                devices.extend(discovery.scan());
            }
        }

        devices
    }

    /// Connect to a discovered device and return a streaming backend.
    #[allow(unreachable_patterns)]
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn StreamBackend>> {
        match device.dac_type {
            #[cfg(feature = "helios")]
            DacType::Helios => self
                .helios
                .as_ref()
                .ok_or_else(|| Error::disconnected("Helios discovery not available"))?
                .connect(device),
            #[cfg(feature = "ether-dream")]
            DacType::EtherDream => self.etherdream.connect(device),
            #[cfg(feature = "idn")]
            DacType::Idn => self.idn.connect(device),
            #[cfg(feature = "lasercube-wifi")]
            DacType::LasercubeWifi => self.lasercube_wifi.connect(device),
            #[cfg(feature = "lasercube-usb")]
            DacType::LasercubeUsb => self
                .lasercube_usb
                .as_ref()
                .ok_or_else(|| Error::disconnected("LaserCube USB discovery not available"))?
                .connect(device),
            _ => Err(Error::invalid_config(format!(
                "DAC type {:?} not supported in this build",
                device.dac_type
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stable_id_etherdream_with_mac() {
        let info = DiscoveredDeviceInfo {
            dac_type: DacType::EtherDream,
            ip_address: Some("192.168.1.100".parse().unwrap()),
            mac_address: Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xab]),
            hostname: None,
            usb_address: None,
            hardware_name: None,
        };
        assert_eq!(info.stable_id(), "etherdream:01:23:45:67:89:ab");
    }

    #[test]
    fn test_stable_id_idn_with_hostname() {
        let info = DiscoveredDeviceInfo {
            dac_type: DacType::Idn,
            ip_address: Some("192.168.1.100".parse().unwrap()),
            mac_address: None,
            hostname: Some("laser-projector.local".to_string()),
            usb_address: None,
            hardware_name: None,
        };
        assert_eq!(info.stable_id(), "idn:laser-projector.local");
    }

    #[test]
    fn test_stable_id_helios_with_hardware_name() {
        let info = DiscoveredDeviceInfo {
            dac_type: DacType::Helios,
            ip_address: None,
            mac_address: None,
            hostname: None,
            usb_address: Some("1:5".to_string()),
            hardware_name: Some("Helios DAC".to_string()),
        };
        assert_eq!(info.stable_id(), "helios:Helios DAC");
    }

    #[test]
    fn test_stable_id_lasercube_usb_with_address() {
        let info = DiscoveredDeviceInfo {
            dac_type: DacType::LasercubeUsb,
            ip_address: None,
            mac_address: None,
            hostname: None,
            usb_address: Some("2:3".to_string()),
            hardware_name: None,
        };
        assert_eq!(info.stable_id(), "lasercube-usb:2:3");
    }

    #[test]
    fn test_stable_id_lasercube_wifi_with_ip() {
        let info = DiscoveredDeviceInfo {
            dac_type: DacType::LasercubeWifi,
            ip_address: Some("192.168.1.50".parse().unwrap()),
            mac_address: None,
            hostname: None,
            usb_address: None,
            hardware_name: None,
        };
        assert_eq!(info.stable_id(), "lasercube-wifi:192.168.1.50");
    }

    #[test]
    fn test_stable_id_custom_fallback() {
        let info = DiscoveredDeviceInfo {
            dac_type: DacType::Custom("MyDAC".to_string()),
            ip_address: None,
            mac_address: None,
            hostname: None,
            usb_address: None,
            hardware_name: None,
        };
        // Custom with no identifiers falls back to unknown format
        assert_eq!(info.stable_id(), "unknown:Custom(\"MyDAC\")");
    }

    #[test]
    fn test_stable_id_custom_with_ip() {
        let info = DiscoveredDeviceInfo {
            dac_type: DacType::Custom("MyDAC".to_string()),
            ip_address: Some("10.0.0.1".parse().unwrap()),
            mac_address: None,
            hostname: None,
            usb_address: None,
            hardware_name: None,
        };
        assert_eq!(info.stable_id(), "mydac:10.0.0.1");
    }
}
