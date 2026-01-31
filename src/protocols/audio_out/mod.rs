//! Audio output backend for oscilloscope XY mode.
//!
//! This module provides an audio output backend that maps laser points
//! to stereo audio channels:
//! - `LaserPoint.x` → Left channel
//! - `LaserPoint.y` → Right channel
//!
//! This enables oscilloscope XY visualization via a DC-coupled audio interface.
//!
//! # Requirements
//!
//! - **DC-coupled audio interface** is required for accurate DC representation.
//!   AC-coupled interfaces will high-pass filter the signal, causing drift.
//! - Common sample rates: 44100, 48000, 96000 Hz
//!
//! # Example
//!
//! ```ignore
//! use laser_dac::{list_devices, open_device, DacType, StreamConfig};
//!
//! // Find audio output device
//! let devices = list_devices()?;
//! let audio_device = devices.iter()
//!     .find(|d| d.kind == DacType::Audio)
//!     .expect("No audio device found");
//!
//! let device = open_device(&audio_device.id)?;
//! let sample_rate = device.caps().pps_max; // PPS = sample rate
//!
//! let (mut stream, _) = device.start_stream(StreamConfig::new(sample_rate))?;
//! ```

mod backend;
mod discovery;

pub use backend::AudioOutBackend;
pub use discovery::{AudioDeviceInfo, AudioOutDiscovery};

use crate::types::{DacCapabilities, OutputModel};

/// Configuration for audio output backend.
#[derive(Debug, Clone)]
pub struct AudioOutConfig {
    /// Gain multiplier applied to output (default 1.0).
    pub gain: f32,
    /// DC offset added to output (default 0.0).
    pub dc_offset: f32,
    /// Clip values to [-1, 1] before output (default true).
    pub clip: bool,
}

impl Default for AudioOutConfig {
    fn default() -> Self {
        Self {
            gain: 1.0,
            dc_offset: 0.0,
            clip: true,
        }
    }
}

/// Returns default capabilities for an audio output device at the given sample rate.
///
/// The key constraint is that `pps_min == pps_max == sample_rate`, enforcing
/// that the stream PPS matches the audio sample rate.
pub fn default_capabilities(sample_rate: u32) -> DacCapabilities {
    DacCapabilities {
        pps_min: sample_rate,
        pps_max: sample_rate,
        max_points_per_chunk: 4096,
        prefers_constant_pps: true,
        can_estimate_queue: true,
        output_model: OutputModel::NetworkFifo,
    }
}
