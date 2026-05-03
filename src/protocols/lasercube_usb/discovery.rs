//! LaserCube USB (LaserDock) DAC discovery.

use std::any::Any;

use crate::backend::{BackendKind, LasercubeUsbBackend, Result};
use crate::discovery::{downcast_connect_data, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer};
use crate::protocols::lasercube_usb::{rusb, DacController};
use crate::types::DacType;

const PREFIX: &str = "lasercube-usb";

struct ConnectData {
    device: rusb::Device<rusb::Context>,
}

pub struct LasercubeUsbDiscoverer {
    controller: DacController,
}

impl LasercubeUsbDiscoverer {
    /// Returns `None` if the USB controller fails to initialize.
    pub fn new() -> Option<Self> {
        DacController::new()
            .ok()
            .map(|controller| Self { controller })
    }
}

impl Discoverer for LasercubeUsbDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::LasercubeUsb
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
            let usb_address = format!("{}:{}", device.bus_number(), device.address());
            let serial = crate::protocols::lasercube_usb::get_serial_number(&device);

            let (stable_id, name) = match &serial {
                Some(s) => (format!("{}:{}", PREFIX, s), s.clone()),
                None => (format!("{}:{}", PREFIX, usb_address), usb_address.clone()),
            };

            let mut info = DiscoveredDeviceInfo::new(DacType::LasercubeUsb, stable_id, name)
                .with_usb_address(usb_address);
            if let Some(serial) = serial {
                info = info.with_hardware_name(serial);
            }
            discovered.push(DiscoveredDevice::new(
                info,
                Box::new(ConnectData { device }),
            ));
        }
        discovered
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "LaserCube USB")?;
        Ok(BackendKind::Fifo(Box::new(LasercubeUsbBackend::new(
            data.device,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lasercube_usb_stable_id_uses_serial_when_present() {
        let info = DiscoveredDeviceInfo::new(DacType::LasercubeUsb, "lasercube-usb:2:3", "2:3")
            .with_usb_address("2:3");
        assert_eq!(info.stable_id(), "lasercube-usb:2:3");
    }
}
