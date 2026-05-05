//! AVB audio-output DAC discovery.

use std::any::Any;

use crate::backend::{AvbBackend, BackendKind, Result};
use crate::device::DacType;
use crate::discovery::{
    downcast_connect_data, slugify_device_id, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer,
};
use crate::protocols::avb::{discover_device_selectors, AvbSelector};

const PREFIX: &str = "avb";

struct ConnectData {
    selector: AvbSelector,
}

pub struct AvbDiscoverer;

impl AvbDiscoverer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for AvbDiscoverer {
    fn default() -> Self {
        Self::new()
    }
}

fn format_stable_id(name: &str, device_index: u16) -> String {
    format!("{}:{}:{}", PREFIX, slugify_device_id(name), device_index)
}

impl Discoverer for AvbDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Avb
    }

    fn prefix(&self) -> &str {
        PREFIX
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        let Ok(selectors) = discover_device_selectors() else {
            return Vec::new();
        };
        selectors
            .into_iter()
            .map(|selector| {
                let device_index = selector.duplicate_index;
                let stable_id = format_stable_id(&selector.name, device_index);
                let info = DiscoveredDeviceInfo::new(DacType::Avb, stable_id, &selector.name)
                    .with_hardware_name(&selector.name)
                    .with_device_index(device_index);
                DiscoveredDevice::new(info, Box::new(ConnectData { selector }))
            })
            .collect()
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "AVB")?;
        Ok(BackendKind::Fifo(Box::new(AvbBackend::from_selector(
            data.selector,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_stable_id_slugs_name_and_appends_index() {
        assert_eq!(format_stable_id("MOTU AVB Main", 0), "avb:motu-avb-main:0");
        assert_eq!(format_stable_id("MOTU AVB Main", 1), "avb:motu-avb-main:1");
    }

    #[test]
    fn format_stable_id_collapses_punctuation_and_case() {
        assert_eq!(
            format_stable_id("  Built-in Output! ", 0),
            "avb:built-in-output:0"
        );
    }
}
