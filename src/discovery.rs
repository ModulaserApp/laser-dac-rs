//! DAC device discovery.
//!
//! Provides a unified interface for discovering and connecting to laser DAC devices
//! from multiple manufacturers.
//!
//! # Example
//!
//! ```ignore
//! use laser_dac::{UnifiedDiscovery, EnabledDacTypes, DacWorker};
//!
//! // Create discovery with all DAC types enabled
//! let mut discovery = UnifiedDiscovery::new(EnabledDacTypes::all());
//!
//! // Scan for devices
//! let devices = discovery.scan();
//! for device in devices {
//!     println!("Found: {} ({})", device.name(), device.dac_type());
//!
//!     // Connect when ready
//!     match discovery.connect(device) {
//!         Ok(backend) => {
//!             let worker = DacWorker::new(device.name().to_string(), device.dac_type(), backend);
//!             // Use worker...
//!         }
//!         Err(e) => eprintln!("Failed to connect: {}", e),
//!     }
//! }
//! ```

use std::io;
use std::net::IpAddr;
use std::time::Duration;

use crate::backend::DacBackend;
use crate::error::{Error, Result};
use crate::types::{DacType, EnabledDacTypes};

// Feature-gated imports from internal protocol modules

#[cfg(feature = "helios")]
use crate::backend::HeliosBackend;
#[cfg(feature = "helios")]
use crate::protocols::helios::{HeliosDac, HeliosDacController};

#[cfg(feature = "ether-dream")]
use crate::backend::EtherDreamBackend;
#[cfg(feature = "ether-dream")]
use crate::protocols::ether_dream::dac;
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
/// Use `UnifiedDiscovery::connect()` to establish a connection and get a backend.
pub struct DiscoveredDevice {
    name: String,
    dac_type: DacType,
    inner: DiscoveredDeviceInner,
}

impl DiscoveredDevice {
    /// Returns the device name (unique identifier).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the DAC type.
    pub fn dac_type(&self) -> DacType {
        self.dac_type
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
        match HeliosDacController::new() {
            Ok(controller) => Some(Self { controller }),
            Err(_) => None,
        }
    }

    /// Scan for available Helios devices.
    pub fn scan(&self) -> Vec<DiscoveredDevice> {
        let devices = match self.controller.list_devices() {
            Ok(devs) => devs,
            Err(_) => return Vec::new(),
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

            let name = opened
                .name()
                .unwrap_or_else(|_| "Unknown Helios".to_string());
            discovered.push(DiscoveredDevice {
                name,
                dac_type: DacType::Helios,
                inner: DiscoveredDeviceInner::Helios(opened),
            });
        }
        discovered
    }

    /// Connect to a discovered Helios device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn DacBackend>> {
        let DiscoveredDeviceInner::Helios(dac) = device.inner else {
            return Err(Error::msg("Invalid device type for Helios"));
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
            timeout: Duration::from_millis(100),
        }
    }

    /// Scan for available Ether Dream devices.
    pub fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let mut rx = match recv_dac_broadcasts() {
            Ok(rx) => rx,
            Err(_) => return Vec::new(),
        };

        if rx.set_timeout(Some(self.timeout)).is_err() {
            return Vec::new();
        }

        let mut discovered = Vec::new();
        for _ in 0..10 {
            let (broadcast, source_addr) = match rx.next_broadcast() {
                Ok(b) => b,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::TimedOut => break,
                Err(_) => continue,
            };

            let mac = dac::MacAddress(broadcast.mac_address);
            let name = mac.to_string();
            let ip = source_addr.ip();

            discovered.push(DiscoveredDevice {
                name,
                dac_type: DacType::EtherDream,
                inner: DiscoveredDeviceInner::EtherDream { broadcast, ip },
            });
        }
        discovered
    }

    /// Connect to a discovered Ether Dream device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn DacBackend>> {
        let DiscoveredDeviceInner::EtherDream { broadcast, ip } = device.inner else {
            return Err(Error::msg("Invalid device type for EtherDream"));
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
        let servers = match scan_for_servers(self.scan_timeout) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let mut discovered = Vec::new();
        for server in servers {
            // Get service first before moving server
            let service = match server.find_laser_projector() {
                Some(service) => service.clone(),
                None => continue,
            };

            let name = format!("IDN:{}", server.hostname);

            discovered.push(DiscoveredDevice {
                name,
                dac_type: DacType::Idn,
                inner: DiscoveredDeviceInner::Idn { server, service },
            });
        }
        discovered
    }

    /// Connect to a discovered IDN device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn DacBackend>> {
        let DiscoveredDeviceInner::Idn { server, service } = device.inner else {
            return Err(Error::msg("Invalid device type for IDN"));
        };

        Ok(Box::new(IdnBackend::new(server, service)))
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
        let mut discovery = match discover_lasercube_wifi() {
            Ok(d) => d,
            Err(_) => return Vec::new(),
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

            let name = format!("LaserCube:{}", device_info.serial_number);

            discovered.push(DiscoveredDevice {
                name,
                dac_type: DacType::LasercubeWifi,
                inner: DiscoveredDeviceInner::LasercubeWifi {
                    info: device_info,
                    source_addr,
                },
            });
        }
        discovered
    }

    /// Connect to a discovered LaserCube WiFi device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn DacBackend>> {
        let DiscoveredDeviceInner::LasercubeWifi { info, source_addr } = device.inner else {
            return Err(Error::msg("Invalid device type for LaserCube WiFi"));
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
        match LasercubeUsbController::new() {
            Ok(controller) => Some(Self { controller }),
            Err(_) => None,
        }
    }

    /// Scan for available LaserCube USB devices.
    pub fn scan(&self) -> Vec<DiscoveredDevice> {
        let devices = match self.controller.list_devices() {
            Ok(devs) => devs,
            Err(_) => return Vec::new(),
        };

        let mut discovered = Vec::new();
        for device in devices {
            let name = format!("LaserCube USB:{}:{}", device.bus_number(), device.address());

            discovered.push(DiscoveredDevice {
                name,
                dac_type: DacType::LasercubeUsb,
                inner: DiscoveredDeviceInner::LasercubeUsb(device),
            });
        }
        discovered
    }

    /// Connect to a discovered LaserCube USB device.
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn DacBackend>> {
        let DiscoveredDeviceInner::LasercubeUsb(usb_device) = device.inner else {
            return Err(Error::msg("Invalid device type for LaserCube USB"));
        };

        let backend = LasercubeUsbBackend::new(usb_device);
        Ok(Box::new(backend))
    }
}

// =============================================================================
// Unified Discovery
// =============================================================================

/// Unified discovery coordinator for all DAC types.
///
/// This provides a single entry point for discovering and connecting to any
/// supported DAC hardware.
pub struct UnifiedDiscovery {
    #[cfg(feature = "helios")]
    helios: Option<HeliosDiscovery>,
    #[cfg(feature = "ether-dream")]
    etherdream: EtherDreamDiscovery,
    #[cfg(feature = "idn")]
    idn: IdnDiscovery,
    #[cfg(feature = "lasercube-wifi")]
    lasercube_wifi: LasercubeWifiDiscovery,
    #[cfg(feature = "lasercube-usb")]
    lasercube_usb: Option<LasercubeUsbDiscovery>,
    enabled: EnabledDacTypes,
}

impl UnifiedDiscovery {
    /// Create a new unified discovery instance.
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
            #[cfg(feature = "lasercube-wifi")]
            lasercube_wifi: LasercubeWifiDiscovery::new(),
            #[cfg(feature = "lasercube-usb")]
            lasercube_usb: LasercubeUsbDiscovery::new(),
            enabled,
        }
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
            devices.extend(self.idn.scan());
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

    /// Connect to a discovered device.
    ///
    /// Returns a boxed backend that can be used with `DacWorker`.
    #[allow(unreachable_patterns)]
    pub fn connect(&self, device: DiscoveredDevice) -> Result<Box<dyn DacBackend>> {
        match device.dac_type {
            #[cfg(feature = "helios")]
            DacType::Helios => self
                .helios
                .as_ref()
                .ok_or_else(|| Error::msg("Helios discovery not available"))?
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
                .ok_or_else(|| Error::msg("LaserCube USB discovery not available"))?
                .connect(device),
            _ => Err(Error::msg(format!(
                "DAC type {:?} not supported in this build",
                device.dac_type
            ))),
        }
    }
}
