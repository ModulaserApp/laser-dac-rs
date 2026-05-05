//! Stream and reconnection configuration types.
//!
//! - [`StreamConfig`] — buffer-driven timing config for `Dac::start_stream` /
//!   `start_frame_session`, with optional reconnection.
//! - [`IdlePolicy`] — what to output when the stream is idle (disarmed or
//!   underrun). [`UnderrunPolicy`] is a deprecated alias.
//! - [`ReconnectConfig`] — backoff and callback configuration for transparent
//!   reconnection after a device disconnect.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::device::DacInfo;

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

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

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
