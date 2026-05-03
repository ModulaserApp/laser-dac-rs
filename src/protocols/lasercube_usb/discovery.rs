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

/// Stable id is the serial number when the device exposes one, otherwise the
/// USB bus:address (which is not stable across reboots but is the best we can
/// do).
fn format_stable_id(serial: Option<&str>, usb_address: &str) -> String {
    match serial {
        Some(s) => format!("{}:{}", PREFIX, s),
        None => format!("{}:{}", PREFIX, usb_address),
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

            let stable_id = format_stable_id(serial.as_deref(), &usb_address);
            let name = serial.clone().unwrap_or_else(|| usb_address.clone());

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
    fn format_stable_id_prefers_serial_when_present() {
        assert_eq!(
            format_stable_id(Some("LC-12345"), "2:3"),
            "lasercube-usb:LC-12345"
        );
    }

    #[test]
    fn format_stable_id_falls_back_to_usb_address() {
        assert_eq!(format_stable_id(None, "2:3"), "lasercube-usb:2:3");
    }
}
