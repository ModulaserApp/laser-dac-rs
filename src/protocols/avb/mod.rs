//! AVB audio DAC backend.
//!
//! AVB output is implemented via system audio APIs: CoreAudio on macOS, ASIO on Windows.
//! Supports 5-channel (XYRGB) and 6-channel (XYRGBI) mapping with auto-detected sample rate.

pub mod backend;
pub mod error;

pub use backend::{discover_device_selectors, AvbBackend, AvbSelector};
pub use error::Error;

use crate::types::{DacCapabilities, OutputModel};

/// Returns default capabilities for AVB DAC output.
///
/// PPS is unconstrained because the backend resamples from the user's PPS
/// to the auto-detected audio device sample rate.
pub fn default_capabilities() -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max: 100_000,
        max_points_per_chunk: 4096,
        output_model: OutputModel::NetworkFifo,
    }
}

/// Normalize a device name for deterministic comparison.
pub(crate) fn normalize_device_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_ascii_lowercase()
}
