//! Helios USB DAC discovery.

use std::any::Any;

use crate::backend::{BackendKind, Result};
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
use crate::protocols::helios::{HeliosBackend, HeliosDac, HeliosDacController};
use crate::types::DacType;

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
            let HeliosDac::Idle(_) = &device else {
                continue;
            };
            let opened = match device.open() {
                Ok(o) => o,
                Err(_) => continue,
            };
            let hardware_name = opened.name().unwrap_or_else(|_| "Unknown Helios".into());
            let stable_id = format!("{}:{}", PREFIX, hardware_name);

            let info = DiscoveredDeviceInfo::new(DacType::Helios, stable_id, &hardware_name)
                .with_hardware_name(hardware_name);
            discovered.push(DiscoveredDevice::new(
                info,
                Box::new(ConnectData { dac: opened }),
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
    fn helios_stable_id_with_hardware_name() {
        let info = DiscoveredDeviceInfo::new(DacType::Helios, "helios:Helios DAC", "Helios DAC")
            .with_usb_address("1:5")
            .with_hardware_name("Helios DAC");
        assert_eq!(info.stable_id(), "helios:Helios DAC");
    }
}
