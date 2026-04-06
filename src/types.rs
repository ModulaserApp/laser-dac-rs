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

    // =========================================================================
    // Coordinate conversion helpers (shared across protocol backends)
    // =========================================================================

    /// Convert a coordinate from [-1.0, 1.0] to 12-bit unsigned (0-4095) with axis inversion.
    ///
    /// Used by Helios and LaserCube WiFi backends.
    #[inline]
    pub(crate) fn coord_to_u12_inverted(v: f32) -> u16 {
        ((1.0 - (v + 1.0) / 2.0).clamp(0.0, 1.0) * 4095.0).round() as u16
    }

    /// Convert a coordinate from [-1.0, 1.0] to 12-bit unsigned (0-4095).
    ///
    /// Used by LaserCube USB backend.
    #[inline]
    pub(crate) fn coord_to_u12(v: f32) -> u16 {
        (((v.clamp(-1.0, 1.0) + 1.0) / 2.0) * 4095.0).round() as u16
    }

    /// Convert a coordinate from [-1.0, 1.0] to signed 16-bit (-32767 to 32767) with inversion.
    ///
    /// Used by Ether Dream and IDN backends.
    #[inline]
    pub(crate) fn coord_to_i16_inverted(v: f32) -> i16 {
        (v.clamp(-1.0, 1.0) * -32767.0).round() as i16
    }

    /// Downscale a u16 color channel (0-65535) to u8 (0-255).
    #[inline]
    pub(crate) fn color_to_u8(v: u16) -> u8 {
        (v >> 8) as u8
    }

    /// Downscale a u16 color channel (0-65535) to 12-bit (0-4095).
    #[inline]
    pub(crate) fn color_to_u12(v: u16) -> u16 {
        v >> 4
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
/// - `target_buffer`: Target buffer level to maintain (default baseline: 20ms)
/// - `min_buffer`: Minimum buffer before requesting urgent fill (default baseline: 8ms)
///
/// The callback is invoked when `buffered < target_buffer`. The callback receives
/// a `ChunkRequest` with `min_points` and `target_points` calculated from these
/// durations and the current buffer state.
///
/// `Dac::start_stream()` may promote untouched defaults to safer network values
/// for `NetworkFifo` / `UdpTimed` backends.
///
/// To reduce perceived latency, reduce `target_buffer`.
#[derive(Debug)]
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

    /// What to do when the stream is idle (underrun or disarmed).
    pub idle_policy: IdlePolicy,

    /// Maximum time to wait for queued points to drain on graceful shutdown (default: 1s).
    ///
    /// When the producer returns `ChunkResult::End`, the stream waits for buffered
    /// points to play out before returning. This timeout caps that wait to prevent
    /// blocking forever if the DAC stalls or queue depth is unknown.
    #[cfg_attr(feature = "serde", serde(with = "duration_millis"))]
    pub drain_timeout: std::time::Duration,

    /// Initial color delay for scanner sync compensation (default: disabled).
    ///
    /// Delays RGB+intensity channels relative to XY coordinates by this duration,
    /// allowing galvo mirrors time to settle before the laser fires. The delay is
    /// implemented as a FIFO: output colors lag input colors by `ceil(color_delay * pps)` points.
    ///
    /// Can be changed at runtime via [`crate::StreamControl::set_color_delay`].
    ///
    /// Typical values: 50–200µs depending on scanner speed.
    /// `Duration::ZERO` disables the delay (default).
    #[cfg_attr(feature = "serde", serde(with = "duration_micros"))]
    pub color_delay: std::time::Duration,

    /// Duration of forced blanking after arming (default: 1ms).
    ///
    /// After the stream is armed, the first `ceil(startup_blank * pps)` points
    /// will have their color channels forced to zero, regardless of what the
    /// producer writes. This prevents the "flash on start" artifact where
    /// the laser fires before mirrors reach position.
    ///
    /// Note: when `color_delay` is also active, the delay line provides
    /// `color_delay` worth of natural startup blanking. This `startup_blank`
    /// setting adds blanking *beyond* that duration.
    ///
    /// Set to `Duration::ZERO` to disable explicit startup blanking.
    #[cfg_attr(feature = "serde", serde(with = "duration_micros"))]
    pub startup_blank: std::time::Duration,

    /// Reconnection configuration (default: disabled).
    ///
    /// Set via [`with_reconnect`](Self::with_reconnect) to enable automatic
    /// reconnection when the device disconnects.
    #[cfg_attr(feature = "serde", serde(skip))]
    pub reconnect: Option<ReconnectConfig>,
}

#[cfg(feature = "serde")]
macro_rules! duration_serde_module {
    ($mod_name:ident, $as_unit:ident, $from_unit:ident) => {
        mod $mod_name {
            use serde::{Deserialize, Deserializer, Serialize, Serializer};
            use std::time::Duration;

            pub fn serialize<S>(duration: &Duration, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                let value = duration.$as_unit().min(u64::MAX as u128) as u64;
                value.serialize(serializer)
            }

            pub fn deserialize<'de, D>(deserializer: D) -> Result<Duration, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = u64::deserialize(deserializer)?;
                Ok(Duration::$from_unit(value))
            }
        }
    };
}

#[cfg(feature = "serde")]
duration_serde_module!(duration_millis, as_millis, from_millis);
#[cfg(feature = "serde")]
duration_serde_module!(duration_micros, as_micros, from_micros);

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            pps: 30_000,
            target_buffer: Self::DEFAULT_TARGET_BUFFER,
            min_buffer: Self::DEFAULT_MIN_BUFFER,
            idle_policy: IdlePolicy::default(),
            drain_timeout: std::time::Duration::from_secs(1),
            color_delay: std::time::Duration::ZERO,
            startup_blank: std::time::Duration::from_millis(1),
            reconnect: None,
        }
    }
}

impl StreamConfig {
    /// Baseline default target buffer used by `StreamConfig::new()`.
    pub const DEFAULT_TARGET_BUFFER: std::time::Duration = std::time::Duration::from_millis(20);
    /// Baseline default minimum buffer used by `StreamConfig::new()`.
    pub const DEFAULT_MIN_BUFFER: std::time::Duration = std::time::Duration::from_millis(8);
    /// Safer default target buffer for network DACs when caller leaves defaults untouched.
    pub const NETWORK_DEFAULT_TARGET_BUFFER: std::time::Duration =
        std::time::Duration::from_millis(50);
    /// Safer default minimum buffer for network DACs when caller leaves defaults untouched.
    pub const NETWORK_DEFAULT_MIN_BUFFER: std::time::Duration =
        std::time::Duration::from_millis(20);

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

    /// Set the idle policy (builder pattern).
    ///
    /// Controls behavior when the stream is idle — either because the producer
    /// can't keep up (underrun) or the stream is disarmed. See [`IdlePolicy`].
    pub fn with_idle_policy(mut self, policy: IdlePolicy) -> Self {
        self.idle_policy = policy;
        self
    }

    /// Deprecated — use [`with_idle_policy`](Self::with_idle_policy) instead.
    #[deprecated(since = "0.8.0", note = "renamed to with_idle_policy")]
    pub fn with_underrun(self, policy: IdlePolicy) -> Self {
        self.with_idle_policy(policy)
    }

    /// Set the drain timeout for graceful shutdown (builder pattern).
    ///
    /// Default: 1 second. Set to `Duration::ZERO` to skip drain entirely.
    pub fn with_drain_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Set the color delay for scanner sync compensation (builder pattern).
    ///
    /// Default: `Duration::ZERO` (disabled). Typical values: 50–200µs.
    pub fn with_color_delay(mut self, delay: std::time::Duration) -> Self {
        self.color_delay = delay;
        self
    }

    /// Set the startup blanking duration after arming (builder pattern).
    ///
    /// Default: 1ms. Set to `Duration::ZERO` to disable.
    pub fn with_startup_blank(mut self, duration: std::time::Duration) -> Self {
        self.startup_blank = duration;
        self
    }

    /// Enable automatic reconnection (builder pattern).
    ///
    /// Requires the device to have been opened via [`open_device`](crate::open_device).
    pub fn with_reconnect(mut self, config: ReconnectConfig) -> Self {
        self.reconnect = Some(config);
        self
    }
}

/// Policy for what to output when the stream is idle (disarmed or underrun).
///
/// This governs both underrun recovery (producer can't keep up) and disarm
/// behavior (laser safety off). When disarmed, `RepeatLast` falls back to
/// `Blank` — repeating lit content on a disarmed stream is never correct.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[derive(Default)]
pub enum IdlePolicy {
    /// Repeat the last chunk of points (underrun only; falls back to `Blank` when disarmed).
    RepeatLast,
    /// Output blanked points at the origin (laser off, scanners park at 0,0).
    #[default]
    Blank,
    /// Park the beam at a specific position with laser off.
    Park { x: f32, y: f32 },
    /// Stop the stream entirely on underrun.
    Stop,
}

/// Deprecated alias — use [`IdlePolicy`] instead.
#[deprecated(since = "0.8.0", note = "renamed to IdlePolicy")]
pub type UnderrunPolicy = IdlePolicy;

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

    /// Points per second (current value, may change via `StreamControl::set_pps`).
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

// =============================================================================
// Reconnect Configuration
// =============================================================================

/// Configuration for automatic reconnection behavior.
///
/// Used with [`StreamConfig::with_reconnect`] or
/// [`FrameSessionConfig::with_reconnect`](crate::FrameSessionConfig::with_reconnect)
/// to enable transparent reconnection when the device disconnects.
///
/// # Example
///
/// ```
/// use laser_dac::ReconnectConfig;
/// use std::time::Duration;
///
/// let rc = ReconnectConfig::new()
///     .max_retries(5)
///     .backoff(Duration::from_secs(2))
///     .on_disconnect(|err| eprintln!("Lost connection: {}", err))
///     .on_reconnect(|info| println!("Reconnected to {}", info.name));
/// ```
type DisconnectCb = Box<dyn FnMut(&crate::Error) + Send + 'static>;
type ReconnectCb = Box<dyn FnMut(&DacInfo) + Send + 'static>;

pub struct ReconnectConfig {
    pub(crate) max_retries: Option<u32>,
    pub(crate) backoff: std::time::Duration,
    pub(crate) on_disconnect: Option<DisconnectCb>,
    pub(crate) on_reconnect: Option<ReconnectCb>,
}

impl ReconnectConfig {
    /// Create a new reconnect configuration with defaults.
    ///
    /// Defaults: infinite retries, 1s backoff, no callbacks.
    pub fn new() -> Self {
        Self {
            max_retries: None,
            backoff: std::time::Duration::from_secs(1),
            on_disconnect: None,
            on_reconnect: None,
        }
    }

    /// Set the maximum number of consecutive reconnect attempts.
    ///
    /// `None` (default) retries forever. `Some(0)` disables retries.
    pub fn max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = Some(max_retries);
        self
    }

    /// Set a fixed backoff duration between reconnect attempts.
    pub fn backoff(mut self, backoff: std::time::Duration) -> Self {
        self.backoff = backoff;
        self
    }

    /// Register a callback invoked when a disconnect is detected.
    pub fn on_disconnect<F>(mut self, f: F) -> Self
    where
        F: FnMut(&crate::Error) + Send + 'static,
    {
        self.on_disconnect = Some(Box::new(f));
        self
    }

    /// Register a callback invoked after a successful reconnect.
    pub fn on_reconnect<F>(mut self, f: F) -> Self
    where
        F: FnMut(&DacInfo) + Send + 'static,
    {
        self.on_reconnect = Some(Box::new(f));
        self
    }
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for ReconnectConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReconnectConfig")
            .field("max_retries", &self.max_retries)
            .field("backoff", &self.backoff)
            .field("on_disconnect", &self.on_disconnect.as_ref().map(|_| ".."))
            .field("on_reconnect", &self.on_reconnect.as_ref().map(|_| ".."))
            .finish()
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
            idle_policy: IdlePolicy::Park { x: 0.5, y: -0.3 },
            drain_timeout: Duration::from_secs(2),
            color_delay: Duration::from_micros(150),
            startup_blank: Duration::from_micros(800),
            reconnect: None,
        };

        // Round-trip through JSON
        let json = serde_json::to_string(&config).expect("serialize to JSON");
        let restored: StreamConfig = serde_json::from_str(&json).expect("deserialize from JSON");

        assert_eq!(restored.pps, config.pps);
        assert_eq!(restored.target_buffer, config.target_buffer);
        assert_eq!(restored.min_buffer, config.min_buffer);
        assert_eq!(restored.drain_timeout, config.drain_timeout);
        assert_eq!(restored.color_delay, config.color_delay);
        assert_eq!(restored.startup_blank, config.startup_blank);

        // Verify idle policy
        match restored.idle_policy {
            IdlePolicy::Park { x, y } => {
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
