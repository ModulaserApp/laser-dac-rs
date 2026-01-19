//! DAC device discovery.
//!
//! Provides a DAC interface for discovering and connecting to laser DAC devices
//! from multiple manufacturers.

use std::any::Any;
use std::io;
use std::net::IpAddr;
use std::time::Duration;

use crate::backend::{Error, Result, StreamBackend};
use crate::types::{DacType, EnabledDacTypes};

// =============================================================================
// External Discoverer Support
// =============================================================================

/// Trait for external DAC discovery implementations.
///
/// External crates implement this to integrate their DAC discovery
/// with the unified `DacDiscovery` system.
///
/// # Example
///
/// ```ignore
/// use laser_dac::{
///     DacDiscovery, ExternalDiscoverer, ExternalDevice,
///     StreamBackend, DacType, EnabledDacTypes,
/// };
/// use std::any::Any;
///
/// struct MyClosedDacDiscoverer { /* ... */ }
///
/// impl ExternalDiscoverer for MyClosedDacDiscoverer {
///     fn dac_type(&self) -> DacType {
///         DacType::Custom("MyClosedDAC".into())
///     }
///
///     fn scan(&mut self) -> Vec<ExternalDevice> {
///         // Your discovery logic here
///         vec![]
///     }
///
///     fn connect(&mut self, opaque_data: Box<dyn Any + Send>) -> Result<Box<dyn StreamBackend>> {
///         // Your connection logic here
///         todo!()
///     }
/// }
/// ```
pub trait ExternalDiscoverer: Send {
    /// Returns the DAC type this discoverer handles.
    fn dac_type(&self) -> DacType;

    /// Scan for devices. Called during `DacDiscovery::scan()`.
    fn scan(&mut self) -> Vec<ExternalDevice>;

    /// Connect to a previously discovered device.
    /// The `opaque_data` is the same data returned in `ExternalDevice`.
    fn connect(&mut self, opaque_data: Box<dyn Any + Send>) -> Result<Box<dyn StreamBackend>>;
}

/// Device info returned by external discoverers.
///
/// This struct contains the common fields that `DacDiscovery` uses to create
/// a `DiscoveredDevice`. The `opaque_data` field stores protocol-specific
/// connection information that will be passed back to `connect()`.
pub struct ExternalDevice {
    /// IP address for network devices.
    pub ip_address: Option<IpAddr>,
    /// MAC address if available.
    pub mac_address: Option<[u8; 6]>,
    /// Hostname if available.
    pub hostname: Option<String>,
    /// USB address (e.g., "bus:device") for USB devices.
    pub usb_address: Option<String>,
    /// Hardware/device name if available.
    pub hardware_name: Option<String>,
    /// Opaque data passed back to `connect()`.
    /// Store whatever your protocol needs to establish a connection.
    pub opaque_data: Box<dyn Any + Send>,
}

impl ExternalDevice {
    /// Create a new external device with the given opaque data.
    ///
    /// All other fields default to `None`.
    pub fn new<T: Any + Send + 'static>(opaque_data: T) -> Self {
        Self {
            ip_address: None,
            mac_address: None,
            hostname: None,
            usb_address: None,
            hardware_name: None,
            opaque_data: Box::new(opaque_data),
        }
    }
}

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
    /// This is used for device tracking/deduplication.
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
    /// External discoverer device.
    External {
        /// Index into `DacDiscovery.external` for the discoverer that found this device.
        discoverer_index: usize,
        /// Opaque data passed back to `ExternalDiscoverer::connect()`.
        opaque_data: Box<dyn Any + Send>,
    },
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
    /// External discoverers registered by external crates.
    external: Vec<Box<dyn ExternalDiscoverer>>,
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
            external: Vec::new(),
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

    /// Register an external discoverer.
    ///
    /// External discoverers are called during `scan()` to find additional devices
    /// beyond the built-in DAC types. This allows external crates to integrate
    /// their own DAC discovery with the unified discovery system.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
    /// discovery.register(Box::new(MyClosedDacDiscoverer::new()));
    ///
    /// // Now scan() will include devices from the external discoverer
    /// let devices = discovery.scan();
    /// ```
    pub fn register(&mut self, discoverer: Box<dyn ExternalDiscoverer>) {
        self.external.push(discoverer);
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

        // External discoverers
        for (index, discoverer) in self.external.iter_mut().enumerate() {
            let dac_type = discoverer.dac_type();
            for ext_device in discoverer.scan() {
                devices.push(DiscoveredDevice {
                    dac_type: dac_type.clone(),
                    ip_address: ext_device.ip_address,
                    mac_address: ext_device.mac_address,
                    hostname: ext_device.hostname,
                    usb_address: ext_device.usb_address,
                    hardware_name: ext_device.hardware_name,
                    inner: DiscoveredDeviceInner::External {
                        discoverer_index: index,
                        opaque_data: ext_device.opaque_data,
                    },
                });
            }
        }

        devices
    }

    /// Connect to a discovered device and return a streaming backend.
    #[allow(unreachable_patterns)]
    pub fn connect(&mut self, device: DiscoveredDevice) -> Result<Box<dyn StreamBackend>> {
        // Handle external devices first (check inner variant)
        if let DiscoveredDeviceInner::External {
            discoverer_index,
            opaque_data,
        } = device.inner
        {
            return self
                .external
                .get_mut(discoverer_index)
                .ok_or_else(|| Error::invalid_config("External discoverer not found"))?
                .connect(opaque_data);
        }

        // Handle built-in DAC types
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

    // =========================================================================
    // External Discoverer Tests
    // =========================================================================

    use crate::types::{DacCapabilities, LaserPoint};
    use crate::WriteOutcome;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Mock connection info for testing.
    #[derive(Debug, Clone)]
    struct MockConnectionInfo {
        _device_id: u32,
    }

    /// Mock backend for testing external discoverers.
    struct MockBackend {
        connected: bool,
    }

    impl StreamBackend for MockBackend {
        fn dac_type(&self) -> DacType {
            DacType::Custom("MockDAC".into())
        }

        fn caps(&self) -> &DacCapabilities {
            static CAPS: DacCapabilities = DacCapabilities {
                pps_min: 1,
                pps_max: 100_000,
                max_points_per_chunk: 4096,
                prefers_constant_pps: false,
                can_estimate_queue: false,
                output_model: crate::types::OutputModel::NetworkFifo,
            };
            &CAPS
        }

        fn connect(&mut self) -> Result<()> {
            self.connected = true;
            Ok(())
        }

        fn disconnect(&mut self) -> Result<()> {
            self.connected = false;
            Ok(())
        }

        fn is_connected(&self) -> bool {
            self.connected
        }

        fn try_write_chunk(&mut self, _pps: u32, _points: &[LaserPoint]) -> Result<WriteOutcome> {
            Ok(WriteOutcome::Written)
        }

        fn stop(&mut self) -> Result<()> {
            Ok(())
        }

        fn set_shutter(&mut self, _open: bool) -> Result<()> {
            Ok(())
        }
    }

    /// Mock external discoverer for testing.
    struct MockExternalDiscoverer {
        scan_count: Arc<AtomicUsize>,
        connect_called: Arc<AtomicBool>,
        devices_to_return: Vec<(u32, Option<IpAddr>)>,
    }

    impl MockExternalDiscoverer {
        fn new(devices: Vec<(u32, Option<IpAddr>)>) -> Self {
            Self {
                scan_count: Arc::new(AtomicUsize::new(0)),
                connect_called: Arc::new(AtomicBool::new(false)),
                devices_to_return: devices,
            }
        }
    }

    impl ExternalDiscoverer for MockExternalDiscoverer {
        fn dac_type(&self) -> DacType {
            DacType::Custom("MockDAC".into())
        }

        fn scan(&mut self) -> Vec<ExternalDevice> {
            self.scan_count.fetch_add(1, Ordering::SeqCst);
            self.devices_to_return
                .iter()
                .map(|(id, ip)| {
                    let mut device = ExternalDevice::new(MockConnectionInfo { _device_id: *id });
                    device.ip_address = *ip;
                    device.hardware_name = Some(format!("Mock Device {}", id));
                    device
                })
                .collect()
        }

        fn connect(&mut self, opaque_data: Box<dyn Any + Send>) -> Result<Box<dyn StreamBackend>> {
            self.connect_called.store(true, Ordering::SeqCst);
            let _info = opaque_data
                .downcast::<MockConnectionInfo>()
                .map_err(|_| Error::invalid_config("wrong device type"))?;
            Ok(Box::new(MockBackend { connected: false }))
        }
    }

    #[test]
    fn test_external_discoverer_scan_is_called() {
        let discoverer = MockExternalDiscoverer::new(vec![(1, Some("10.0.0.1".parse().unwrap()))]);
        let scan_count = discoverer.scan_count.clone();

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer));

        assert_eq!(scan_count.load(Ordering::SeqCst), 0);
        let devices = discovery.scan();
        assert_eq!(scan_count.load(Ordering::SeqCst), 1);
        assert_eq!(devices.len(), 1);
    }

    #[test]
    fn test_external_discoverer_device_info() {
        let discoverer =
            MockExternalDiscoverer::new(vec![(42, Some("192.168.1.100".parse().unwrap()))]);

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer));

        let devices = discovery.scan();
        assert_eq!(devices.len(), 1);

        let device = &devices[0];
        assert_eq!(device.dac_type(), DacType::Custom("MockDAC".into()));
        assert_eq!(
            device.info().ip_address,
            Some("192.168.1.100".parse().unwrap())
        );
        assert_eq!(device.info().hardware_name, Some("Mock Device 42".into()));
    }

    #[test]
    fn test_external_discoverer_connect() {
        let discoverer = MockExternalDiscoverer::new(vec![(99, None)]);
        let connect_called = discoverer.connect_called.clone();

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer));

        let devices = discovery.scan();
        assert_eq!(devices.len(), 1);
        assert!(!connect_called.load(Ordering::SeqCst));

        let backend = discovery.connect(devices.into_iter().next().unwrap());
        assert!(backend.is_ok());
        assert!(connect_called.load(Ordering::SeqCst));

        let backend = backend.unwrap();
        assert_eq!(backend.dac_type(), DacType::Custom("MockDAC".into()));
    }

    #[test]
    fn test_external_discoverer_multiple_devices() {
        let discoverer = MockExternalDiscoverer::new(vec![
            (1, Some("10.0.0.1".parse().unwrap())),
            (2, Some("10.0.0.2".parse().unwrap())),
            (3, None),
        ]);

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer));

        let devices = discovery.scan();
        assert_eq!(devices.len(), 3);

        // Verify we can connect to any of them
        for device in devices {
            let backend = discovery.connect(device);
            assert!(backend.is_ok());
        }
    }

    #[test]
    fn test_multiple_external_discoverers() {
        let discoverer1 = MockExternalDiscoverer::new(vec![(1, None)]);
        let discoverer2 = MockExternalDiscoverer::new(vec![(2, None), (3, None)]);

        let mut discovery = DacDiscovery::new(EnabledDacTypes::none());
        discovery.register(Box::new(discoverer1));
        discovery.register(Box::new(discoverer2));

        let devices = discovery.scan();
        assert_eq!(devices.len(), 3);
    }
}
