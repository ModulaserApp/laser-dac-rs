//! AVB audio-output DAC discovery.

use std::any::Any;

use crate::backend::{AvbBackend, BackendKind, Result};
use crate::device::DacType;
use crate::discovery::{
    downcast_connect_data, slugify_device_id, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer,
};
use crate::protocols::avb::{discover_device_selectors, normalize_device_name, AvbSelector};
use std::collections::HashMap;

const PREFIX: &str = "avb";

struct ConnectData {
    selector: AvbSelector,
    /// Number of devices sharing this device's (normalized) name at scan time.
    /// The backend compares this against the live count on connect to detect a
    /// device-set change that would make the positional `duplicate_index` bind
    /// a different physical unit.
    scan_duplicate_count: usize,
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
        let selectors = match discover_device_selectors() {
            Ok(selectors) => selectors,
            Err(e) => {
                log::warn!("AVB: device discovery failed: {}", e);
                return Vec::new();
            }
        };

        // Count devices per normalized name so the backend can later detect a
        // device-set change that would shift the positional `duplicate_index`.
        let mut counts: HashMap<String, usize> = HashMap::new();
        for selector in &selectors {
            *counts
                .entry(normalize_device_name(&selector.name))
                .or_insert(0) += 1;
        }

        selectors
            .into_iter()
            .map(|selector| {
                let device_index = selector.duplicate_index;
                let scan_duplicate_count = *counts
                    .get(&normalize_device_name(&selector.name))
                    .unwrap_or(&1);
                let stable_id = format_stable_id(&selector.name, device_index);
                let info = DiscoveredDeviceInfo::new(DacType::Avb, stable_id, &selector.name)
                    .with_hardware_name(&selector.name)
                    .with_device_index(device_index);
                DiscoveredDevice::new(
                    info,
                    Box::new(ConnectData {
                        selector,
                        scan_duplicate_count,
                    }),
                )
            })
            .collect()
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "AVB")?;
        Ok(BackendKind::Fifo(Box::new(
            AvbBackend::from_selector_with_scan_count(data.selector, data.scan_duplicate_count),
        )))
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
