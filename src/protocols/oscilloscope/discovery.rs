//! Oscilloscope device discovery.

use cpal::traits::{DeviceTrait, HostTrait};

use super::OscilloscopeBackend;
use crate::backend::{BackendKind, Result};
use crate::error::Error;

/// Information about a discovered oscilloscope-capable audio device.
#[derive(Debug, Clone)]
pub struct OscilloscopeDeviceInfo {
    /// Device name from the audio system
    pub name: String,
    /// Default sample rate for the device
    pub sample_rate: u32,
}

/// Discovery for oscilloscope-capable audio devices.
pub struct OscilloscopeDiscovery {
    /// Cached device info from last scan
    devices: Vec<OscilloscopeDeviceInfo>,
}

impl OscilloscopeDiscovery {
    /// Create a new oscilloscope discovery instance.
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    /// Scan for available oscilloscope-capable audio devices.
    ///
    /// Returns information about each discovered device.
    pub fn scan(&mut self) -> Vec<OscilloscopeDeviceInfo> {
        self.devices.clear();

        let host = cpal::default_host();

        let devices = match host.output_devices() {
            Ok(d) => d,
            Err(e) => {
                log::warn!("Failed to enumerate audio devices: {}", e);
                return Vec::new();
            }
        };

        for device in devices {
            let name = match device.name() {
                Ok(n) => n,
                Err(_) => continue,
            };

            // Get the default output config to determine sample rate
            let config = match device.default_output_config() {
                Ok(c) => c,
                Err(_) => continue,
            };

            // Only include stereo-capable devices
            if config.channels() < 2 {
                continue;
            }

            let sample_rate = config.sample_rate().0;

            let info = OscilloscopeDeviceInfo { name, sample_rate };

            self.devices.push(info);
        }

        self.devices.clone()
    }

    /// Connect to a device by name.
    ///
    /// Returns a `BackendKind::Fifo` wrapping an [`OscilloscopeBackend`].
    pub fn connect(&self, device_name: &str) -> Result<BackendKind> {
        let info = self
            .devices
            .iter()
            .find(|info| info.name == device_name)
            .ok_or_else(|| {
                Error::disconnected(format!("Audio device '{}' not found", device_name))
            })?;

        Ok(BackendKind::Fifo(Box::new(OscilloscopeBackend::new(
            info.name.clone(),
            info.sample_rate,
        ))))
    }

    /// Connect to a device by index.
    pub fn connect_by_index(&self, index: usize) -> Result<BackendKind> {
        let info = self.devices.get(index).ok_or_else(|| {
            Error::disconnected(format!(
                "Audio device index {} out of range (found {} devices)",
                index,
                self.devices.len()
            ))
        })?;

        Ok(BackendKind::Fifo(Box::new(OscilloscopeBackend::new(
            info.name.clone(),
            info.sample_rate,
        ))))
    }

    /// Get device info by index.
    pub fn get_info(&self, index: usize) -> Option<&OscilloscopeDeviceInfo> {
        self.devices.get(index)
    }

    /// Get the number of discovered devices.
    pub fn len(&self) -> usize {
        self.devices.len()
    }

    /// Check if no devices were discovered.
    pub fn is_empty(&self) -> bool {
        self.devices.is_empty()
    }
}

impl Default for OscilloscopeDiscovery {
    fn default() -> Self {
        Self::new()
    }
}
