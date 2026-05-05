//! DAC identity, capabilities, and discovery filtering.
//!
//! Holds the types that describe a DAC as a *kind of device*:
//!
//! - [`DacType`] — the kind of DAC hardware (Helios, EtherDream, IDN, …)
//! - [`DacCapabilities`] + [`OutputModel`] — scheduler-relevant capabilities
//! - [`caps_for_dac_type`] — default capabilities per [`DacType`]
//! - [`DacInfo`] — identity record for a discovered DAC before connection
//! - [`DacDevice`], [`DacConnectionState`] — legacy identity / connection state
//! - [`EnabledDacTypes`] — discovery filter selecting which kinds to scan

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;

/// Types of laser DAC hardware supported.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
    /// Oscilloscope XY output via stereo audio interface.
    /// Maps LaserPoint.x → Left channel, LaserPoint.y → Right channel.
    #[cfg(feature = "oscilloscope")]
    Oscilloscope,
    /// AVB audio device backend.
    Avb,
    /// Custom DAC implementation (for external/third-party backends).
    Custom(String),
}

impl DacType {
    /// Returns all available DAC types.
    #[cfg(not(feature = "oscilloscope"))]
    pub fn all() -> &'static [DacType] {
        &[
            DacType::Helios,
            DacType::EtherDream,
            DacType::Idn,
            DacType::LasercubeWifi,
            DacType::LasercubeUsb,
            DacType::Avb,
        ]
    }

    /// Returns all available DAC types.
    #[cfg(feature = "oscilloscope")]
    pub fn all() -> &'static [DacType] {
        &[
            DacType::Helios,
            DacType::EtherDream,
            DacType::Idn,
            DacType::LasercubeWifi,
            DacType::LasercubeUsb,
            DacType::Avb,
            DacType::Oscilloscope,
        ]
    }

    /// Returns the display name for this DAC type.
    pub fn display_name(&self) -> &str {
        match self {
            DacType::Helios => "Helios",
            DacType::EtherDream => "Ether Dream",
            DacType::Idn => "IDN",
            DacType::LasercubeWifi => "LaserCube WiFi",
            DacType::LasercubeUsb => "LaserCube USB (Laserdock)",
            #[cfg(feature = "oscilloscope")]
            DacType::Oscilloscope => "Oscilloscope",
            DacType::Avb => "AVB Audio Device",
            DacType::Custom(name) => name,
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
            #[cfg(feature = "oscilloscope")]
            DacType::Oscilloscope => "Oscilloscope XY output via stereo audio",
            DacType::Avb => "AVB audio network output",
            DacType::Custom(_) => "Custom DAC",
        }
    }
}

impl fmt::Display for DacType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// Set of enabled DAC types for discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EnabledDacTypes {
    types: HashSet<DacType>,
}

impl EnabledDacTypes {
    /// Creates a new set with all DAC types enabled.
    pub fn all() -> Self {
        Self {
            types: DacType::all().iter().cloned().collect(),
        }
    }

    /// Creates an empty set (no DAC types enabled).
    pub fn none() -> Self {
        Self {
            types: HashSet::new(),
        }
    }

    /// Returns true if the given DAC type is enabled.
    pub fn is_enabled(&self, dac_type: DacType) -> bool {
        self.types.contains(&dac_type)
    }

    /// Enables a DAC type for discovery.
    ///
    /// Returns `&mut Self` to allow method chaining.
    ///
    /// # Examples
    ///
    /// ```
    /// use laser_dac::{EnabledDacTypes, DacType};
    ///
    /// let mut enabled = EnabledDacTypes::none();
    /// enabled.enable(DacType::Helios).enable(DacType::EtherDream);
    ///
    /// assert!(enabled.is_enabled(DacType::Helios));
    /// assert!(enabled.is_enabled(DacType::EtherDream));
    /// ```
    pub fn enable(&mut self, dac_type: DacType) -> &mut Self {
        self.types.insert(dac_type);
        self
    }

    /// Disables a DAC type for discovery.
    ///
    /// Returns `&mut Self` to allow method chaining.
    ///
    /// # Examples
    ///
    /// ```
    /// use laser_dac::{EnabledDacTypes, DacType};
    ///
    /// let mut enabled = EnabledDacTypes::all();
    /// enabled.disable(DacType::Helios).disable(DacType::EtherDream);
    ///
    /// assert!(!enabled.is_enabled(DacType::Helios));
    /// assert!(!enabled.is_enabled(DacType::EtherDream));
    /// ```
    pub fn disable(&mut self, dac_type: DacType) -> &mut Self {
        self.types.remove(&dac_type);
        self
    }

    /// Returns an iterator over enabled DAC types.
    pub fn iter(&self) -> impl Iterator<Item = DacType> + '_ {
        self.types.iter().cloned()
    }

    /// Returns a copy with the given DAC type removed.
    ///
    /// Useful in conjunction with `DacDiscovery::register` to replace a
    /// built-in discoverer with a custom-configured one (e.g., IDN with
    /// specific scan addresses for testing).
    pub fn without(mut self, dac_type: DacType) -> Self {
        self.types.remove(&dac_type);
        self
    }

    /// Returns true if no DAC types are enabled.
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }
}

impl Default for EnabledDacTypes {
    fn default() -> Self {
        Self::all()
    }
}

impl std::iter::FromIterator<DacType> for EnabledDacTypes {
    fn from_iter<I: IntoIterator<Item = DacType>>(iter: I) -> Self {
        Self {
            types: iter.into_iter().collect(),
        }
    }
}

impl Extend<DacType> for EnabledDacTypes {
    fn extend<I: IntoIterator<Item = DacType>>(&mut self, iter: I) {
        self.types.extend(iter);
    }
}

/// Information about a discovered DAC device.
/// The name is the unique identifier for the device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
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
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum DacConnectionState {
    /// Successfully connected and ready to receive frames.
    Connected { name: String },
    /// Worker stopped normally (callback returned None or stop() was called).
    Stopped { name: String },
    /// Connection was lost due to an error.
    Lost { name: String, error: Option<String> },
}

/// DAC capabilities that inform the stream scheduler about safe chunk sizes and behaviors.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DacCapabilities {
    /// Minimum points-per-second (hardware/protocol limit where known).
    ///
    /// A value of 1 means no known protocol constraint. Helios (7) and
    /// Ether Dream (1) have true hardware minimums. Note that very low PPS
    /// increases point dwell time and can produce flickery output.
    pub pps_min: u32,
    /// Maximum supported points-per-second (hardware limit).
    pub pps_max: u32,
    /// Maximum number of points allowed per chunk submission.
    pub max_points_per_chunk: usize,
    /// The scheduler-relevant output model.
    pub output_model: OutputModel,
}

impl Default for DacCapabilities {
    fn default() -> Self {
        Self {
            pps_min: 1,
            pps_max: 100_000,
            max_points_per_chunk: 4096,
            output_model: OutputModel::NetworkFifo,
        }
    }
}

/// Get default capabilities for a DAC type.
///
/// This delegates to each protocol's `default_capabilities()` function.
/// For optimal performance, backends should query actual device capabilities
/// at runtime where the protocol supports it (e.g., LaserCube's `max_dac_rate`
/// and ringbuffer queries).
pub fn caps_for_dac_type(dac_type: &DacType) -> DacCapabilities {
    match dac_type {
        #[cfg(feature = "helios")]
        DacType::Helios => crate::protocols::helios::default_capabilities(),
        #[cfg(not(feature = "helios"))]
        DacType::Helios => DacCapabilities::default(),

        #[cfg(feature = "ether-dream")]
        DacType::EtherDream => crate::protocols::ether_dream::default_capabilities(),
        #[cfg(not(feature = "ether-dream"))]
        DacType::EtherDream => DacCapabilities::default(),

        #[cfg(feature = "idn")]
        DacType::Idn => crate::protocols::idn::default_capabilities(),
        #[cfg(not(feature = "idn"))]
        DacType::Idn => DacCapabilities::default(),

        #[cfg(feature = "lasercube-wifi")]
        DacType::LasercubeWifi => crate::protocols::lasercube_wifi::default_capabilities(),
        #[cfg(not(feature = "lasercube-wifi"))]
        DacType::LasercubeWifi => DacCapabilities::default(),

        #[cfg(feature = "lasercube-usb")]
        DacType::LasercubeUsb => crate::protocols::lasercube_usb::default_capabilities(),
        #[cfg(not(feature = "lasercube-usb"))]
        DacType::LasercubeUsb => DacCapabilities::default(),

        #[cfg(feature = "oscilloscope")]
        DacType::Oscilloscope => DacCapabilities::default(), // Caps depend on sample rate, use backend's actual caps

        #[cfg(feature = "avb")]
        DacType::Avb => crate::protocols::avb::default_capabilities(),
        #[cfg(not(feature = "avb"))]
        DacType::Avb => DacCapabilities::default(),

        DacType::Custom(_) => DacCapabilities::default(),
    }
}

/// The scheduler-relevant output model for a DAC.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum OutputModel {
    /// Frame swap / limited queue depth (e.g., Helios-style double-buffering).
    UsbFrameSwap,
    /// FIFO-ish buffer where "top up" is natural (e.g., Ether Dream-style).
    NetworkFifo,
    /// Timed UDP chunks where OS send may not reflect hardware pacing.
    UdpTimed,
}

/// Information about a discovered DAC before connection.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct DacInfo {
    /// Stable, unique identifier used for (re)selecting DACs.
    pub id: String,
    /// Human-readable name for the DAC.
    pub name: String,
    /// The type of DAC hardware.
    pub kind: DacType,
    /// DAC capabilities.
    pub caps: DacCapabilities,
}

impl DacInfo {
    /// Create a new DAC info.
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        kind: DacType,
        caps: DacCapabilities,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            kind,
            caps,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==========================================================================
    // DacType Tests
    // ==========================================================================

    #[test]
    fn test_dac_type_all_returns_all_builtin_types() {
        let all_types = DacType::all();
        #[cfg(not(feature = "oscilloscope"))]
        assert_eq!(all_types.len(), 6);
        #[cfg(feature = "oscilloscope")]
        assert_eq!(all_types.len(), 7);
        assert!(all_types.contains(&DacType::Helios));
        assert!(all_types.contains(&DacType::EtherDream));
        assert!(all_types.contains(&DacType::Idn));
        assert!(all_types.contains(&DacType::LasercubeWifi));
        assert!(all_types.contains(&DacType::LasercubeUsb));
        assert!(all_types.contains(&DacType::Avb));
        #[cfg(feature = "oscilloscope")]
        assert!(all_types.contains(&DacType::Oscilloscope));
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
                enabled.is_enabled(dac_type.clone()),
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
                !enabled.is_enabled(dac_type.clone()),
                "{:?} should be disabled",
                dac_type
            );
        }
        assert!(enabled.is_empty());
    }

    #[test]
    fn test_enabled_dac_types_enable_disable_toggles_correctly() {
        let mut enabled = EnabledDacTypes::none();

        // Enable one
        enabled.enable(DacType::Helios);
        assert!(enabled.is_enabled(DacType::Helios));
        assert!(!enabled.is_enabled(DacType::EtherDream));

        // Enable another
        enabled.enable(DacType::EtherDream);
        assert!(enabled.is_enabled(DacType::Helios));
        assert!(enabled.is_enabled(DacType::EtherDream));

        // Disable first
        enabled.disable(DacType::Helios);
        assert!(!enabled.is_enabled(DacType::Helios));
        assert!(enabled.is_enabled(DacType::EtherDream));
    }

    #[test]
    fn test_enabled_dac_types_iter_only_returns_enabled() {
        let mut enabled = EnabledDacTypes::none();
        enabled.enable(DacType::Helios);
        enabled.enable(DacType::Idn);

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
            assert!(enabled.is_enabled(dac_type.clone()));
        }
    }

    #[test]
    fn test_enabled_dac_types_idempotent_operations() {
        let mut enabled = EnabledDacTypes::none();

        // Enabling twice should have same effect as once
        enabled.enable(DacType::Helios);
        enabled.enable(DacType::Helios);
        assert!(enabled.is_enabled(DacType::Helios));

        // Disabling twice should have same effect as once
        enabled.disable(DacType::Helios);
        enabled.disable(DacType::Helios);
        assert!(!enabled.is_enabled(DacType::Helios));
    }

    #[test]
    fn test_enabled_dac_types_chaining() {
        let mut enabled = EnabledDacTypes::none();
        enabled
            .enable(DacType::Helios)
            .enable(DacType::EtherDream)
            .disable(DacType::Helios);

        assert!(!enabled.is_enabled(DacType::Helios));
        assert!(enabled.is_enabled(DacType::EtherDream));
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
