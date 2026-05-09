//! Helios USB DAC discovery.

use std::any::Any;

use crate::backend::{BackendKind, Result};
use crate::device::DacType;
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
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

fn format_stable_id(usb_location: &str) -> String {
    format!("{}:usb:{}", PREFIX, usb_location)
}

fn format_display_name(usb_location: &str) -> String {
    format!("Helios DAC ({usb_location})")
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
            let stable_id = format_stable_id(&usb_location);
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
    fn format_stable_id_uses_usb_location() {
        assert_eq!(format_stable_id("1:2.3"), "helios:usb:1:2.3");
    }

    #[test]
    fn format_display_name_includes_usb_location() {
        assert_eq!(format_display_name("1:2.3"), "Helios DAC (1:2.3)");
    }
}
