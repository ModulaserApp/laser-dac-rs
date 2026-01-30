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
/// "points sent so far." When used in `ChunkRequest::start`, it represents:
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
///
/// # Buffer-Driven Timing
///
/// The streaming API uses pure buffer-driven timing:
/// - `target_buffer`: Target buffer level to maintain (default: 20ms)
/// - `min_buffer`: Minimum buffer before requesting urgent fill (default: 8ms)
///
/// The callback is invoked when `buffered < target_buffer`. The callback receives
/// a `ChunkRequest` with `min_points` and `target_points` calculated from these
/// durations and the current buffer state.
///
/// To reduce perceived latency, reduce `target_buffer`.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct StreamConfig {
    /// Points per second output rate.
    pub pps: u32,

    /// Target buffer level to maintain (default: 20ms).
    ///
    /// The callback's `target_points` is calculated to bring the buffer to this level.
    /// The callback is invoked when the buffer drops below this level.
    #[cfg_attr(feature = "serde", serde(with = "duration_millis"))]
    pub target_buffer: std::time::Duration,

    /// Minimum buffer before requesting urgent fill (default: 8ms).
    ///
    /// When buffer drops below this, `min_points` in `ChunkRequest` will be non-zero.
    #[cfg_attr(feature = "serde", serde(with = "duration_millis"))]
    pub min_buffer: std::time::Duration,

    /// What to do when the producer can't keep up.
    pub underrun: UnderrunPolicy,

    /// Maximum time to wait for queued points to drain on graceful shutdown (default: 1s).
    ///
    /// When the producer returns `ChunkResult::End`, the stream waits for buffered
    /// points to play out before returning. This timeout caps that wait to prevent
    /// blocking forever if the DAC stalls or queue depth is unknown.
    #[cfg_attr(feature = "serde", serde(with = "duration_millis"))]
    pub drain_timeout: std::time::Duration,
}

#[cfg(feature = "serde")]
mod duration_millis {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::Duration;

    pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Use u64 for both serialize and deserialize to ensure round-trip compatibility.
        // Clamp to u64::MAX for durations > ~584 million years (practically never hit).
        let millis = duration.as_millis().min(u64::MAX as u128) as u64;
        millis.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
    where
        D: Deserializer<'de>,
    {
        let millis = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(millis))
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        use std::time::Duration;
        Self {
            pps: 30_000,
            target_buffer: Duration::from_millis(20),
            min_buffer: Duration::from_millis(8),
            underrun: UnderrunPolicy::default(),
            drain_timeout: Duration::from_secs(1),
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

    /// Set the target buffer level to maintain (builder pattern).
    ///
    /// Default: 20ms. Higher values provide more safety margin against underruns.
    /// Lower values reduce perceived latency.
    pub fn with_target_buffer(mut self, duration: std::time::Duration) -> Self {
        self.target_buffer = duration;
        self
    }

    /// Set the minimum buffer level before urgent fill (builder pattern).
    ///
    /// Default: 8ms. When buffer drops below this, `min_points` will be non-zero.
    pub fn with_min_buffer(mut self, duration: std::time::Duration) -> Self {
        self.min_buffer = duration;
        self
    }

    /// Set the underrun policy (builder pattern).
    pub fn with_underrun(mut self, policy: UnderrunPolicy) -> Self {
        self.underrun = policy;
        self
    }

    /// Set the drain timeout for graceful shutdown (builder pattern).
    ///
    /// Default: 1 second. Set to `Duration::ZERO` to skip drain entirely.
    pub fn with_drain_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.drain_timeout = timeout;
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

/// A request to fill a buffer with points for streaming.
///
/// This is the streaming API with pure buffer-driven timing.
/// The callback receives a `ChunkRequest` describing buffer state and requirements,
/// and fills points into a library-owned buffer.
///
/// # Point Tiers
///
/// - `min_points`: Minimum points needed to avoid imminent underrun (ceiling rounded)
/// - `target_points`: Ideal number of points to reach target buffer level (clamped to buffer length)
/// - `buffer.len()` (passed separately): Maximum points the callback may write
///
/// # Rounding Rules
///
/// - `min_points`: Always **ceiling** (underrun prevention)
/// - `target_points`: **ceiling**, then clamped to buffer length
#[derive(Clone, Debug)]
pub struct ChunkRequest {
    /// Estimated playback time when this chunk starts.
    ///
    /// Calculated as: playhead + buffered_points
    /// Use this for audio synchronization.
    pub start: StreamInstant,

    /// Points per second (fixed for stream duration).
    pub pps: u32,

    /// Minimum points needed to avoid imminent underrun.
    ///
    /// Calculated with ceiling to prevent underrun: `ceil((min_buffer - buffered) * pps)`
    /// If 0, buffer is healthy.
    pub min_points: usize,

    /// Ideal number of points to reach target buffer level.
    ///
    /// Calculated as: `ceil((target_buffer - buffered) * pps)`, clamped to buffer length.
    pub target_points: usize,

    /// Current buffer level in points (for diagnostics/adaptive content).
    pub buffered_points: u64,

    /// Current buffer level as duration (for audio sync convenience).
    pub buffered: std::time::Duration,

    /// Raw device queue if available (best-effort, may differ from buffered_points).
    pub device_queued_points: Option<u64>,
}

/// Result returned by the fill callback indicating how the buffer was filled.
///
/// This enum allows the callback to communicate three distinct states:
/// - Successfully filled some number of points
/// - Temporarily unable to provide data (underrun policy applies)
/// - Stream should end gracefully
///
/// # `Filled(0)` Semantics
///
/// - If `target_points == 0`: Buffer is full, nothing needed. This is fine.
/// - If `target_points > 0`: We needed points but got none. Treated as `Starved`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkResult {
    /// Wrote n points to the buffer.
    ///
    /// `n` must be <= `buffer.len()`.
    /// Partial fills (`n < min_points`) are accepted without padding - useful when
    /// content is legitimately ending. Return `End` on the next call to signal completion.
    Filled(usize),

    /// No data available right now.
    ///
    /// Underrun policy is applied (repeat last chunk or blank).
    /// Stream continues; callback will be called again when buffer needs filling.
    Starved,

    /// Stream is finished. Shutdown sequence:
    /// 1. Stop calling callback
    /// 2. Let queued points drain (play out)
    /// 3. Blank/park the laser at last position
    /// 4. Return from stream() with `RunExit::ProducerEnded`
    End,
}

/// Current status of a stream.
#[derive(Clone, Debug)]
pub struct StreamStatus {
    /// Whether the device is connected.
    pub connected: bool,
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

    // ==========================================================================
    // StreamConfig Serde Tests
    // ==========================================================================

    #[cfg(feature = "serde")]
    #[test]
    fn test_stream_config_serde_roundtrip() {
        use std::time::Duration;

        let config = StreamConfig {
            pps: 45000,
            target_buffer: Duration::from_millis(50),
            min_buffer: Duration::from_millis(12),
            underrun: UnderrunPolicy::Park { x: 0.5, y: -0.3 },
            drain_timeout: Duration::from_secs(2),
        };

        // Round-trip through JSON
        let json = serde_json::to_string(&config).expect("serialize to JSON");
        let restored: StreamConfig =
            serde_json::from_str(&json).expect("deserialize from JSON");

        assert_eq!(restored.pps, config.pps);
        assert_eq!(restored.target_buffer, config.target_buffer);
        assert_eq!(restored.min_buffer, config.min_buffer);
        assert_eq!(restored.drain_timeout, config.drain_timeout);

        // Verify underrun policy
        match restored.underrun {
            UnderrunPolicy::Park { x, y } => {
                assert!((x - 0.5).abs() < f32::EPSILON);
                assert!((y - (-0.3)).abs() < f32::EPSILON);
            }
            _ => panic!("Expected Park policy"),
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn test_duration_millis_roundtrip_consistency() {
        use std::time::Duration;

        // Test various duration values round-trip correctly
        let test_durations = [
            Duration::from_millis(0),
            Duration::from_millis(1),
            Duration::from_millis(10),
            Duration::from_millis(100),
            Duration::from_millis(1000),
            Duration::from_millis(u64::MAX / 1000), // Large but valid
        ];

        for &duration in &test_durations {
            let config = StreamConfig {
                target_buffer: duration,
                ..StreamConfig::default()
            };

            let json = serde_json::to_string(&config).expect("serialize");
            let restored: StreamConfig = serde_json::from_str(&json).expect("deserialize");

            assert_eq!(
                restored.target_buffer, duration,
                "Duration {:?} did not round-trip correctly",
                duration
            );
        }
    }
}
