//! Oscilloscope XY mode backend via audio output.
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
//! ```no_run
//! use laser_dac::{list_devices, open_device, DacType, StreamConfig};
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Find oscilloscope device
//!     let devices = list_devices()?;
//!     let osc_device = devices.iter()
//!         .find(|d| d.kind == DacType::Oscilloscope)
//!         .expect("No oscilloscope device found");
//!
//!     let device = open_device(&osc_device.id)?;
//!     let (mut stream, _) = device.start_stream(StreamConfig::new(30_000))?;
//!     Ok(())
//! }
//! ```

mod backend;
mod discovery;

pub use backend::OscilloscopeBackend;
pub use discovery::{OscilloscopeDeviceInfo, OscilloscopeDiscoverer};

use crate::types::{DacCapabilities, OutputModel};

/// Configuration for oscilloscope backend.
#[derive(Debug, Clone)]
pub struct OscilloscopeConfig {
    /// Gain multiplier applied to output (default 1.0).
    pub gain: f32,
    /// DC offset added to output (default 0.0).
    pub dc_offset: f32,
    /// Clip values to [-1, 1] before output (default true).
    pub clip: bool,
}

impl Default for OscilloscopeConfig {
    fn default() -> Self {
        Self {
            gain: 1.0,
            dc_offset: 0.0,
            clip: true,
        }
    }
}

/// Returns capabilities for an oscilloscope device at the given sample rate.
///
/// PPS is unconstrained because the backend resamples from the user's PPS
/// to the audio device sample rate, just like the AVB backend.
///
/// `max_points_per_chunk` is set to the ring-buffer capacity (~100ms of
/// audio) so the scheduler can submit up to a full buffer's worth at once.
/// Unlike laser DACs there is no hardware packet/frame limit — the ring
/// buffer is the only constraint.
pub fn capabilities(sample_rate: u32) -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max: 100_000,
        max_points_per_chunk: buffer_capacity(sample_rate),
        output_model: OutputModel::NetworkFifo,
    }
}

/// Ring buffer capacity: ~100ms of audio at the given sample rate (min 4096).
fn buffer_capacity(sample_rate: u32) -> usize {
    (sample_rate as usize / 10).max(4096)
}
