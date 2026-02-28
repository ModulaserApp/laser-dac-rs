//! AVB audio DAC backend.
//!
//! AVB output is implemented via system audio APIs: CoreAudio on macOS, ASIO on Windows.
//! v1 scope: fixed 6-channel mapping at 48 kHz.

pub mod backend;
pub mod error;

pub use backend::{discover_device_selectors, AvbBackend, AvbSelector};
pub use error::Error;

use crate::types::{DacCapabilities, OutputModel};

/// Returns default capabilities for AVB DAC output.
pub fn default_capabilities() -> DacCapabilities {
    DacCapabilities {
        pps_min: 48_000,
        pps_max: 48_000,
        max_points_per_chunk: 4096,
        output_model: OutputModel::NetworkFifo,
    }
}

/// True when the device name likely represents an AVB endpoint.
pub(crate) fn is_likely_avb_device_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    let tokens = ["avb", "motu", "digiface", "sollinger", "laseranimation"];
    tokens.iter().any(|token| name.contains(token))
}

/// Normalize a device name for deterministic comparison.
pub(crate) fn normalize_device_name(name: &str) -> String {
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_ascii_lowercase()
}

/// Slugify a device name for stable IDs.
#[cfg(test)]
pub(crate) fn slugify_device_name(name: &str) -> String {
    let normalized = normalize_device_name(name);
    let mut slug = String::with_capacity(normalized.len());
    let mut prev_dash = false;

    for ch in normalized.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }

    slug.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_device_name_collapses_whitespace_and_symbols() {
        assert_eq!(slugify_device_name("  MOTU  AVB (Main) "), "motu-avb-main");
    }

    #[test]
    fn likely_avb_name_token_match() {
        assert!(is_likely_avb_device_name("MOTU Pro AVB"));
        assert!(!is_likely_avb_device_name("Built-in Output"));
    }
}
