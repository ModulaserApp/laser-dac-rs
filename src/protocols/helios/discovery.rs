//! Helios USB DAC discovery.

use std::any::Any;

use crate::backend::{BackendKind, Result};
use crate::device::DacType;
use crate::discovery::{
    downcast_connect_data, slugify_device_id, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer,
};
use crate::protocols::helios::{HeliosBackend, HeliosDac, HeliosDacController};

const PREFIX: &str = "helios";

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

/// Stable id derived from the device's own name (`CONTROL_GET_NAME`). Preferred
/// because it survives replug into a different USB port and does not swap
/// between two units the way a bus/port path can.
fn format_stable_id_from_name(name: &str) -> String {
    format!("{}:{}", PREFIX, slugify_device_id(name))
}

/// Fallback stable id keyed on the physical USB bus/port path, used when the
/// device name cannot be read (e.g. the unit is busy in another session).
fn format_stable_id_from_location(usb_location: &str) -> String {
    format!("{}:usb:{}", PREFIX, usb_location)
}

fn format_display_name(usb_location: &str) -> String {
    format!("Helios DAC ({usb_location})")
}

fn format_display_name_with_name(name: &str, usb_location: &str) -> String {
    format!("Helios DAC {name} ({usb_location})")
}

impl Discoverer for HeliosDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Helios
    }

    fn prefix(&self) -> &str {
        PREFIX
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let Ok(devices) = self.controller.list_devices() else {
            return Vec::new();
        };

        let mut discovered = Vec::new();
        for device in devices {
            let usb_location = device.usb_location();

            // Best-effort scan-time name read for a hardware-anchored identity.
            // Falls back to the port-path id when the device is busy/unreadable.
            let name = device
                .read_name()
                .ok()
                .map(|n| n.trim().to_string())
                .filter(|n| !n.is_empty() && !slugify_device_id(n).is_empty());

            let (stable_id, display_name) = match &name {
                Some(n) => (
                    format_stable_id_from_name(n),
                    format_display_name_with_name(n, &usb_location),
                ),
                None => (
                    format_stable_id_from_location(&usb_location),
                    format_display_name(&usb_location),
                ),
            };

            let mut info = DiscoveredDeviceInfo::new(DacType::Helios, stable_id, display_name)
                .with_usb_address(usb_location);
            if let Some(n) = name {
                info = info.with_hardware_name(n);
            }
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
    fn format_stable_id_from_location_uses_usb_location() {
        assert_eq!(format_stable_id_from_location("1:2.3"), "helios:usb:1:2.3");
    }

    #[test]
    fn format_stable_id_from_name_slugs_device_name() {
        assert_eq!(
            format_stable_id_from_name("Helios Left"),
            "helios:helios-left"
        );
    }

    #[test]
    fn format_display_name_includes_usb_location() {
        assert_eq!(format_display_name("1:2.3"), "Helios DAC (1:2.3)");
    }

    #[test]
    fn format_display_name_with_name_includes_both() {
        assert_eq!(
            format_display_name_with_name("Left", "1:2.3"),
            "Helios DAC Left (1:2.3)"
        );
    }
}
