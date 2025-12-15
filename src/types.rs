//! DAC types for laser output.
//!
//! Provides DAC-agnostic types for laser frames and points,
//! as well as device enumeration types.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

/// A DAC-agnostic laser point with full-precision f32 coordinates.
///
/// Coordinates are normalized:
/// - x: -1.0 (left) to 1.0 (right)
/// - y: -1.0 (bottom) to 1.0 (top)
///
/// This allows each DAC to convert to its native format:
/// - Helios: 12-bit unsigned (0-4095), inverted
/// - EtherDream: 16-bit signed (-32768 to 32767)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LaserPoint {
    /// X coordinate, -1.0 to 1.0
    pub x: f32,
    /// Y coordinate, -1.0 to 1.0
    pub y: f32,
    /// Red channel (0-255)
    pub r: u8,
    /// Green channel (0-255)
    pub g: u8,
    /// Blue channel (0-255)
    pub b: u8,
    /// Intensity (0-255)
    pub intensity: u8,
}

impl LaserPoint {
    /// Creates a new laser point.
    pub fn new(x: f32, y: f32, r: u8, g: u8, b: u8, intensity: u8) -> Self {
        Self {
            x,
            y,
            r,
            g,
            b,
            intensity,
        }
    }

    /// Creates a blanked point (laser off) at the given position.
    pub fn blanked(x: f32, y: f32) -> Self {
        Self {
            x,
            y,
            r: 0,
            g: 0,
            b: 0,
            intensity: 0,
        }
    }
}

/// A DAC-agnostic laser frame with full-precision coordinates.
#[derive(Debug, Clone, PartialEq)]
pub struct LaserFrame {
    /// Points per second output rate
    pub pps: u32,
    /// Points in this frame
    pub points: Vec<LaserPoint>,
}

impl LaserFrame {
    /// Creates a new laser frame.
    pub fn new(pps: u32, points: Vec<LaserPoint>) -> Self {
        Self { pps, points }
    }
}

/// Types of laser DAC hardware supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum DacType {
    /// Helios laser DAC (USB connection).
    Helios,
    /// Ether Dream laser DAC (network connection).
    EtherDream,
    /// IDN laser DAC (ILDA Digital Network, network connection).
    Idn,
    /// LaserCube WiFi laser DAC (network connection).
    LasercubeWifi,
    /// LaserCube USB laser DAC (USB connection, also known as LaserDock).
    LasercubeUsb,
}

impl DacType {
    /// Returns all available DAC types.
    pub fn all() -> &'static [DacType] {
        &[
            DacType::Helios,
            DacType::EtherDream,
            DacType::Idn,
            DacType::LasercubeWifi,
            DacType::LasercubeUsb,
        ]
    }

    /// Returns the display name for this DAC type.
    pub fn display_name(&self) -> &'static str {
        match self {
            DacType::Helios => "Helios",
            DacType::EtherDream => "Ether Dream",
            DacType::Idn => "IDN",
            DacType::LasercubeWifi => "LaserCube WiFi",
            DacType::LasercubeUsb => "LaserCube USB (Laserdock)",
        }
    }

    /// Returns a description of this DAC type.
    pub fn description(&self) -> &'static str {
        match self {
            DacType::Helios => "USB laser DAC",
            DacType::EtherDream => "Network laser DAC",
            DacType::Idn => "ILDA Digital Network laser DAC",
            DacType::LasercubeWifi => "WiFi laser DAC",
            DacType::LasercubeUsb => "USB laser DAC",
        }
    }
}

impl fmt::Display for DacType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// Set of enabled DAC types for discovery.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnabledDacTypes {
    types: HashSet<DacType>,
}

impl EnabledDacTypes {
    /// Creates a new set with all DAC types enabled.
    pub fn all() -> Self {
        Self {
            types: DacType::all().iter().copied().collect(),
        }
    }

    /// Creates an empty set (no DAC types enabled).
    #[allow(dead_code)]
    pub fn none() -> Self {
        Self {
            types: HashSet::new(),
        }
    }

    /// Returns true if the given DAC type is enabled.
    pub fn is_enabled(&self, dac_type: DacType) -> bool {
        self.types.contains(&dac_type)
    }

    /// Enables or disables a DAC type.
    pub fn set_enabled(&mut self, dac_type: DacType, enabled: bool) {
        if enabled {
            self.types.insert(dac_type);
        } else {
            self.types.remove(&dac_type);
        }
    }

    /// Returns an iterator over enabled DAC types.
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = DacType> + '_ {
        self.types.iter().copied()
    }

    /// Returns true if no DAC types are enabled.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

impl Default for EnabledDacTypes {
    fn default() -> Self {
        Self::all()
    }
}

/// Information about a discovered DAC device.
/// The name is the unique identifier for the device.
#[derive(Debug, Clone)]
pub struct DacDevice {
    pub name: String,
    pub dac_type: DacType,
}

impl DacDevice {
    pub fn new(name: String, dac_type: DacType) -> Self {
        Self { name, dac_type }
    }
}

/// Connection state for a single DAC device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DacConnectionState {
    /// Successfully connected and ready to receive frames.
    Connected { name: String },
    /// Connection was lost, waiting for reconnect.
    Lost { name: String, error: Option<String> },
}

/// Information about a discovered DAC device.
#[derive(Debug, Clone)]
pub struct DiscoveredDac {
    /// The type of DAC.
    pub dac_type: DacType,
    /// Unique identifier for this DAC.
    pub id: String,
    /// Human-readable name for this DAC.
    pub name: String,
    /// Network address (if applicable).
    pub address: Option<String>,
    /// Additional metadata about the DAC.
    pub metadata: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==========================================================================
    // LaserPoint Tests
    // ==========================================================================

    #[test]
    fn test_laser_point_blanked_sets_all_colors_to_zero() {
        // blanked() should set all color channels to 0 while preserving position
        let point = LaserPoint::blanked(0.25, 0.75);
        assert_eq!(point.x, 0.25);
        assert_eq!(point.y, 0.75);
        assert_eq!(point.r, 0);
        assert_eq!(point.g, 0);
        assert_eq!(point.b, 0);
        assert_eq!(point.intensity, 0);
    }

    // ==========================================================================
    // DacType Tests
    // ==========================================================================

    #[test]
    fn test_dac_type_all_returns_all_five_types() {
        let all_types = DacType::all();
        assert_eq!(all_types.len(), 5);
        assert!(all_types.contains(&DacType::Helios));
        assert!(all_types.contains(&DacType::EtherDream));
        assert!(all_types.contains(&DacType::Idn));
        assert!(all_types.contains(&DacType::LasercubeWifi));
        assert!(all_types.contains(&DacType::LasercubeUsb));
    }

    #[test]
    fn test_dac_type_display_uses_display_name() {
        // Display trait should delegate to display_name
        assert_eq!(
            format!("{}", DacType::Helios),
            DacType::Helios.display_name()
        );
        assert_eq!(
            format!("{}", DacType::EtherDream),
            DacType::EtherDream.display_name()
        );
    }

    #[test]
    fn test_dac_type_can_be_used_in_hashset() {
        use std::collections::HashSet;

        let mut set = HashSet::new();
        set.insert(DacType::Helios);
        set.insert(DacType::Helios); // Duplicate should not increase count

        assert_eq!(set.len(), 1);
    }

    // ==========================================================================
    // EnabledDacTypes Tests
    // ==========================================================================

    #[test]
    fn test_enabled_dac_types_all_enables_everything() {
        let enabled = EnabledDacTypes::all();
        for dac_type in DacType::all() {
            assert!(
                enabled.is_enabled(*dac_type),
                "{:?} should be enabled",
                dac_type
            );
        }
        assert!(!enabled.is_empty());
    }

    #[test]
    fn test_enabled_dac_types_none_disables_everything() {
        let enabled = EnabledDacTypes::none();
        for dac_type in DacType::all() {
            assert!(
                !enabled.is_enabled(*dac_type),
                "{:?} should be disabled",
                dac_type
            );
        }
        assert!(enabled.is_empty());
    }

    #[test]
    fn test_enabled_dac_types_set_enabled_toggles_correctly() {
        let mut enabled = EnabledDacTypes::none();

        // Enable one
        enabled.set_enabled(DacType::Helios, true);
        assert!(enabled.is_enabled(DacType::Helios));
        assert!(!enabled.is_enabled(DacType::EtherDream));

        // Enable another
        enabled.set_enabled(DacType::EtherDream, true);
        assert!(enabled.is_enabled(DacType::Helios));
        assert!(enabled.is_enabled(DacType::EtherDream));

        // Disable first
        enabled.set_enabled(DacType::Helios, false);
        assert!(!enabled.is_enabled(DacType::Helios));
        assert!(enabled.is_enabled(DacType::EtherDream));
    }

    #[test]
    fn test_enabled_dac_types_iter_only_returns_enabled() {
        let mut enabled = EnabledDacTypes::none();
        enabled.set_enabled(DacType::Helios, true);
        enabled.set_enabled(DacType::Idn, true);

        let types: Vec<DacType> = enabled.iter().collect();
        assert_eq!(types.len(), 2);
        assert!(types.contains(&DacType::Helios));
        assert!(types.contains(&DacType::Idn));
        assert!(!types.contains(&DacType::EtherDream));
    }

    #[test]
    fn test_enabled_dac_types_default_enables_all() {
        let enabled = EnabledDacTypes::default();
        // Default should be same as all()
        for dac_type in DacType::all() {
            assert!(enabled.is_enabled(*dac_type));
        }
    }

    #[test]
    fn test_enabled_dac_types_idempotent_operations() {
        let mut enabled = EnabledDacTypes::none();

        // Enabling twice should have same effect as once
        enabled.set_enabled(DacType::Helios, true);
        enabled.set_enabled(DacType::Helios, true);
        assert!(enabled.is_enabled(DacType::Helios));

        // Disabling twice should have same effect as once
        enabled.set_enabled(DacType::Helios, false);
        enabled.set_enabled(DacType::Helios, false);
        assert!(!enabled.is_enabled(DacType::Helios));
    }

    // ==========================================================================
    // DacConnectionState Tests
    // ==========================================================================

    #[test]
    fn test_dac_connection_state_equality() {
        let s1 = DacConnectionState::Connected {
            name: "DAC1".to_string(),
        };
        let s2 = DacConnectionState::Connected {
            name: "DAC1".to_string(),
        };
        let s3 = DacConnectionState::Connected {
            name: "DAC2".to_string(),
        };
        let s4 = DacConnectionState::Lost {
            name: "DAC1".to_string(),
            error: None,
        };

        assert_eq!(s1, s2);
        assert_ne!(s1, s3); // Different name
        assert_ne!(s1, s4); // Different variant
    }
}
