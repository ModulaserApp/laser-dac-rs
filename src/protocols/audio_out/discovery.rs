//! Audio output device discovery.

use cpal::traits::{DeviceTrait, HostTrait};

use super::AudioOutBackend;
use crate::backend::{Result, StreamBackend};
use crate::error::Error;

/// Information about a discovered audio output device.
#[derive(Debug, Clone)]
pub struct AudioDeviceInfo {
    /// Device name from the audio system
    pub name: String,
    /// Default sample rate for the device
    pub sample_rate: u32,
}

/// Discovery for audio output devices.
pub struct AudioOutDiscovery {
    /// Cached device info from last scan
    devices: Vec<AudioDeviceInfo>,
}

impl AudioOutDiscovery {
    /// Create a new audio output discovery instance.
    pub fn new() -> Self {
        Self {
            devices: Vec::new(),
        }
    }

    /// Scan for available audio output devices.
    ///
    /// Returns information about each discovered device.
    pub fn scan(&mut self) -> Vec<AudioDeviceInfo> {
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

            let info = AudioDeviceInfo {
                name,
                sample_rate,
            };

            self.devices.push(info);
        }

        self.devices.clone()
    }

    /// Connect to a device by name.
    ///
    /// Returns an `AudioOutBackend` for the device.
    pub fn connect(&self, device_name: &str) -> Result<Box<dyn StreamBackend>> {
        let info = self
            .devices
            .iter()
            .find(|info| info.name == device_name)
            .ok_or_else(|| {
                Error::disconnected(format!("Audio device '{}' not found", device_name))
            })?;

        Ok(Box::new(AudioOutBackend::new(
            info.name.clone(),
            info.sample_rate,
        )))
    }

    /// Connect to a device by index.
    pub fn connect_by_index(&self, index: usize) -> Result<Box<dyn StreamBackend>> {
        let info = self.devices.get(index).ok_or_else(|| {
            Error::disconnected(format!(
                "Audio device index {} out of range (found {} devices)",
                index,
                self.devices.len()
            ))
        })?;

        Ok(Box::new(AudioOutBackend::new(
            info.name.clone(),
            info.sample_rate,
        )))
    }

    /// Get device info by index.
    pub fn get_info(&self, index: usize) -> Option<&AudioDeviceInfo> {
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

impl Default for AudioOutDiscovery {
    fn default() -> Self {
        Self::new()
    }
}
