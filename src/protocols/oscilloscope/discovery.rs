//! Oscilloscope device discovery.

use std::any::Any;

use cpal::traits::{DeviceTrait, HostTrait};

use super::{capabilities, OscilloscopeBackend};
use crate::backend::{BackendKind, Result};
use crate::discovery::{
    downcast_connect_data, slugify_device_id, DiscoveredDevice, DiscoveredDeviceInfo, Discoverer,
};
use crate::types::DacType;

const PREFIX: &str = "oscilloscope";

/// Information about a discovered oscilloscope-capable audio device.
#[derive(Debug, Clone)]
pub struct OscilloscopeDeviceInfo {
    pub name: String,
    pub sample_rate: u32,
}

struct ConnectData {
    info: OscilloscopeDeviceInfo,
}

pub struct OscilloscopeDiscoverer;

impl OscilloscopeDiscoverer {
    pub fn new() -> Self {
        Self
    }
}

impl Default for OscilloscopeDiscoverer {
    fn default() -> Self {
        Self::new()
    }
}

fn format_stable_id(name: &str) -> String {
    format!("{}:{}", PREFIX, slugify_device_id(name))
}

fn enumerate() -> Vec<OscilloscopeDeviceInfo> {
    let host = cpal::default_host();
    let devices = match host.output_devices() {
        Ok(d) => d,
        Err(e) => {
            log::warn!("Failed to enumerate audio devices: {}", e);
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for device in devices {
        let name = match device.name() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let config = match device.default_output_config() {
            Ok(c) => c,
            Err(_) => continue,
        };
        if config.channels() < 2 {
            continue;
        }
        let sample_rate = config.sample_rate().0;
        out.push(OscilloscopeDeviceInfo { name, sample_rate });
    }
    out
}

impl Discoverer for OscilloscopeDiscoverer {
    fn dac_type(&self) -> DacType {
        DacType::Oscilloscope
    }

    fn prefix(&self) -> &str {
        PREFIX
    }

    fn scan(&mut self) -> Vec<DiscoveredDevice> {
        enumerate()
            .into_iter()
            .map(|info| {
                let stable_id = format_stable_id(&info.name);
                let caps = capabilities(info.sample_rate);
                let device_info =
                    DiscoveredDeviceInfo::new(DacType::Oscilloscope, stable_id, &info.name)
                        .with_hardware_name(&info.name);
                DiscoveredDevice::new(device_info, Box::new(ConnectData { info })).with_caps(caps)
            })
            .collect()
    }

    fn connect(&mut self, opaque: Box<dyn Any + Send>) -> Result<BackendKind> {
        let data = downcast_connect_data::<ConnectData>(opaque, "Oscilloscope")?;
        Ok(BackendKind::Fifo(Box::new(OscilloscopeBackend::new(
            data.info.name.clone(),
            data.info.sample_rate,
        ))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_stable_id_slugs_device_name() {
        assert_eq!(
            format_stable_id("Built-in Output"),
            "oscilloscope:built-in-output"
        );
        assert_eq!(
            format_stable_id("MOTU UltraLite"),
            "oscilloscope:motu-ultralite"
        );
    }
}
