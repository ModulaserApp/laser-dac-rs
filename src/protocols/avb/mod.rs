//! AVB audio DAC backend.
//!
//! AVB output is implemented via system audio APIs: CoreAudio on macOS,
//! ASIO on Windows by default (disable the `asio` default feature to use
//! WASAPI instead), and ALSA on Linux.
//! Supports 5-channel (XYRGB) and 6-channel (XYRGBI) mapping with auto-detected sample rate.

pub mod backend;
mod discovery;
pub mod error;

pub use backend::{discover_device_selectors, AvbBackend, AvbSelector};
pub use discovery::AvbDiscoverer;
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

/// Exact device names (matched case-insensitively) that are definitely *not*
/// laser DACs, even though they may expose enough channels to pass the
/// channel-count filter.
const BLACKLISTED_DEVICE_NAMES: &[&str] = &["studio display speakers"];

/// Returns `true` when the device name matches a known non-laser audio device.
pub(crate) fn is_blacklisted_device(name: &str) -> bool {
    let normalized = normalize_device_name(name);
    BLACKLISTED_DEVICE_NAMES
        .iter()
        .any(|blocked| normalized == *blocked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blacklist_matches_exact_name_case_insensitively() {
        assert!(is_blacklisted_device("Studio Display Speakers"));
        assert!(is_blacklisted_device("studio display speakers"));
        assert!(is_blacklisted_device("STUDIO DISPLAY SPEAKERS"));
    }

    #[test]
    fn blacklist_does_not_match_partial_or_unrelated() {
        assert!(!is_blacklisted_device("Apple Studio Display"));
        assert!(!is_blacklisted_device("Studio Display"));
        assert!(!is_blacklisted_device("Broadcom NetXtreme"));
        assert!(!is_blacklisted_device("MOTU AVB 24Ao"));
        assert!(!is_blacklisted_device("Built-in Output"));
    }
}
