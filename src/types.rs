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
/// Colors are 16-bit (0-65535) to support high-resolution DACs.
/// DACs with lower resolution (8-bit) will downscale automatically.
///
/// This allows each DAC to convert to its native format:
/// - Helios: 12-bit unsigned (0-4095), inverted
/// - EtherDream: 16-bit signed (-32768 to 32767)
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct LaserPoint {
    /// X coordinate, -1.0 to 1.0
    pub x: f32,
    /// Y coordinate, -1.0 to 1.0
    pub y: f32,
    /// Red channel (0-65535)
    pub r: u16,
    /// Green channel (0-65535)
    pub g: u16,
    /// Blue channel (0-65535)
    pub b: u16,
    /// Intensity (0-65535)
    pub intensity: u16,
}

impl LaserPoint {
    /// Creates a new laser point.
    pub fn new(x: f32, y: f32, r: u16, g: u16, b: u16, intensity: u16) -> Self {
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
            ..Default::default()
        }
    }
}

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
    /// Custom DAC implementation (for external/third-party backends).
    Custom(String),
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
    pub fn display_name(&self) -> &str {
        match self {
            DacType::Helios => "Helios",
            DacType::EtherDream => "Ether Dream",
            DacType::Idn => "IDN",
            DacType::LasercubeWifi => "LaserCube WiFi",
            DacType::LasercubeUsb => "LaserCube USB (Laserdock)",
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
    #[allow(dead_code)]
    pub fn iter(&self) -> impl Iterator<Item = DacType> + '_ {
        self.types.iter().cloned()
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

// =============================================================================
// Streaming Types
// =============================================================================

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
    /// Some DACs dislike per-chunk PPS changes.
    pub prefers_constant_pps: bool,
    /// Best-effort: can we estimate device queue depth/latency?
    pub can_estimate_queue: bool,
    /// The scheduler-relevant output model.
    pub output_model: OutputModel,
}

impl Default for DacCapabilities {
    fn default() -> Self {
        Self {
            pps_min: 1,
            pps_max: 100_000,
            max_points_per_chunk: 4096,
            prefers_constant_pps: false,
            can_estimate_queue: false,
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

/// Represents a point in stream time, anchored to estimated playback position.
///
/// `StreamInstant` represents the **estimated playback time** of points, not merely
/// "points sent so far." When used in `FillRequest::start`, it represents:
///
/// `start` = playhead + buffered
///
/// Where:
/// - `playhead` = stream_epoch + estimated_consumed_points
/// - `buffered` = points sent but not yet played
///
/// This allows callbacks to generate content for the exact time it will be displayed,
/// enabling accurate audio synchronization.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StreamInstant(pub u64);

impl StreamInstant {
    /// Create a new stream instant from a point count.
    pub fn new(points: u64) -> Self {
        Self(points)
    }

    /// Returns the number of points since stream start.
    pub fn points(&self) -> u64 {
        self.0
    }

    /// Convert this instant to seconds at the given points-per-second rate.
    pub fn as_seconds(&self, pps: u32) -> f64 {
        self.0 as f64 / pps as f64
    }

    /// Convert to seconds at the given PPS.
    ///
    /// This is an alias for `as_seconds()` for consistency with standard Rust
    /// duration naming conventions (e.g., `Duration::as_secs_f64()`).
    #[inline]
    pub fn as_secs_f64(&self, pps: u32) -> f64 {
        self.as_seconds(pps)
    }

    /// Create a stream instant from a duration in seconds at the given PPS.
    pub fn from_seconds(seconds: f64, pps: u32) -> Self {
        Self((seconds * pps as f64) as u64)
    }

    /// Add a number of points to this instant.
    pub fn add_points(&self, points: u64) -> Self {
        Self(self.0.saturating_add(points))
    }

    /// Subtract a number of points from this instant (saturating at 0).
    pub fn sub_points(&self, points: u64) -> Self {
        Self(self.0.saturating_sub(points))
    }
}

impl std::ops::Add<u64> for StreamInstant {
    type Output = Self;
    fn add(self, rhs: u64) -> Self::Output {
        self.add_points(rhs)
    }
}

impl std::ops::Sub<u64> for StreamInstant {
    type Output = Self;
    fn sub(self, rhs: u64) -> Self::Output {
        self.sub_points(rhs)
    }
}

impl std::ops::AddAssign<u64> for StreamInstant {
    fn add_assign(&mut self, rhs: u64) {
        self.0 = self.0.saturating_add(rhs);
    }
}

impl std::ops::SubAssign<u64> for StreamInstant {
    fn sub_assign(&mut self, rhs: u64) {
        self.0 = self.0.saturating_sub(rhs);
    }
}

/// Configuration for starting a stream.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StreamConfig {
    /// Points per second output rate.
    pub pps: u32,
    /// Exact chunk size to request/write. If `None`, the library chooses a default.
    pub chunk_points: Option<usize>,
    /// Target amount of queued data expressed in points.
    pub target_queue_points: usize,
    /// What to do when the producer can't keep up.
    pub underrun: UnderrunPolicy,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            pps: 30_000,
            chunk_points: None,
            target_queue_points: 3000,
            underrun: UnderrunPolicy::default(),
        }
    }
}

impl StreamConfig {
    /// Create a new stream configuration with the given PPS.
    pub fn new(pps: u32) -> Self {
        Self {
            pps,
            ..Default::default()
        }
    }

    /// Set the chunk size (builder pattern).
    pub fn with_chunk_points(mut self, chunk_points: usize) -> Self {
        self.chunk_points = Some(chunk_points);
        self
    }

    /// Set the target queue depth in points (builder pattern).
    pub fn with_target_queue_points(mut self, points: usize) -> Self {
        self.target_queue_points = points;
        self
    }

    /// Set the underrun policy (builder pattern).
    pub fn with_underrun(mut self, policy: UnderrunPolicy) -> Self {
        self.underrun = policy;
        self
    }
}

/// Policy for what to do when the producer can't keep up with the stream.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Default)]
pub enum UnderrunPolicy {
    /// Repeat the last chunk of points.
    RepeatLast,
    /// Output blanked points (laser off).
    #[default]
    Blank,
    /// Park the beam at a specific position with laser off.
    Park { x: f32, y: f32 },
    /// Stop the stream entirely on underrun.
    Stop,
}

/// A request from the stream for a chunk of points.
#[derive(Clone, Debug)]
pub struct ChunkRequest {
    /// The stream instant at which this chunk starts.
    pub start: StreamInstant,
    /// The points-per-second rate for this chunk.
    pub pps: u32,
    /// Number of points requested for this chunk.
    pub n_points: usize,
    /// How many points are currently scheduled ahead of `start`.
    pub scheduled_ahead_points: u64,
    /// Best-effort: points reported by the device as queued.
    pub device_queued_points: Option<u64>,
}

/// Current status of a stream.
#[derive(Clone, Debug)]
pub struct StreamStatus {
    /// Whether the device is connected.
    pub connected: bool,
    /// The resolved chunk size chosen for this stream.
    pub chunk_points: usize,
    /// Library-owned scheduled amount.
    pub scheduled_ahead_points: u64,
    /// Best-effort device/backend estimate.
    pub device_queued_points: Option<u64>,
    /// Optional statistics for diagnostics.
    pub stats: Option<StreamStats>,
}

/// Stream statistics for diagnostics and debugging.
#[derive(Clone, Debug, Default)]
pub struct StreamStats {
    /// Number of times the stream underran.
    pub underrun_count: u64,
    /// Number of chunks that arrived late.
    pub late_chunk_count: u64,
    /// Number of times the device reconnected.
    pub reconnect_count: u64,
    /// Total chunks written since stream start.
    pub chunks_written: u64,
    /// Total points written since stream start.
    pub points_written: u64,
}

/// How a callback-mode stream run ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunExit {
    /// Stream was stopped via `StreamControl::stop()`.
    Stopped,
    /// Producer returned `None` (graceful completion).
    ProducerEnded,
    /// Device disconnected. No auto-reconnect; new streams start disarmed.
    Disconnected,
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
