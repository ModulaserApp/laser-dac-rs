//! Helios USB DAC discovery.

use std::any::Any;

use crate::backend::{BackendKind, Result};
use crate::device::DacType;
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
use crate::protocols::helios::{HeliosBackend, HeliosDac, HeliosDacController};

const PREFIX: &str = "helios";
#[cfg(target_os = "macos")]
const MACOS_SCAN_ENV: &str = "LASER_DAC_HELIOS_ENABLE_MACOS_SCAN";

struct ConnectData {
    dac: HeliosDac,
}

pub struct HeliosDiscoverer {
    controller: HeliosDacController,
}

impl HeliosDiscoverer {
    /// Returns `None` if the USB controller fails to initialize.
    pub fn new() -> Option<Self> {
        HeliosDacController::new()
            .ok()
            .map(|controller| Self { controller })
    }
}

fn format_stable_id(hardware_name: Option<&str>, usb_location: &str) -> String {
    match hardware_name {
        Some(name) => format!("{}:{}", PREFIX, name),
        None => format!("{}:usb:{}", PREFIX, usb_location),
    }
}

fn format_display_name(usb_location: &str) -> String {
    format!("Helios DAC ({usb_location})")
}

#[cfg(target_os = "macos")]
fn scan_enabled() -> bool {
    matches!(
        std::env::var(MACOS_SCAN_ENV).as_deref(),
        Ok("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

#[cfg(not(target_os = "macos"))]
fn scan_enabled() -> bool {
    true
}

impl Discoverer for HeliosDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Helios
    }

    fn prefix(&self) -> &str {
        PREFIX
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        if !scan_enabled() {
            log::warn!(
                "Helios USB discovery disabled on macOS; set {}=1 to opt in",
                MACOS_SCAN_ENV
            );
            return Vec::new();
        }

        let Ok(devices) = self.controller.list_devices() else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        for device in devices {
            let usb_location = device.usb_location();
            let stable_id = format_stable_id(None, &usb_location);
            let display_name = format_display_name(&usb_location);

            let info = DiscoveredDeviceInfo::new(DacType::Helios, stable_id, display_name)
                .with_usb_address(usb_location);
            discovered.push(DiscoveredDevice::new(
                info,
                Box::new(ConnectData { dac: device }),
            ));
        }
        discovered
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "Helios")?;
        Ok(BackendKind::FrameSwap(Box::new(HeliosBackend::from_dac(
            data.dac,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_stable_id_prefixes_hardware_name() {
        assert_eq!(
            format_stable_id(Some("Helios DAC"), "1:2.3"),
            "helios:Helios DAC"
        );
        assert_eq!(
            format_stable_id(Some("Unknown Helios"), "1:2.3"),
            "helios:Unknown Helios"
        );
    }

    #[test]
    fn format_stable_id_falls_back_to_usb_location() {
        assert_eq!(format_stable_id(None, "1:2.3"), "helios:usb:1:2.3");
    }

    #[test]
    fn format_display_name_includes_usb_location() {
        assert_eq!(format_display_name("1:2.3"), "Helios DAC (1:2.3)");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn scan_is_enabled_by_default_off_macos() {
        assert!(scan_enabled());
    }
}
