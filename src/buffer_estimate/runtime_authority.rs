//! Buffer estimator backed by an external runtime that already tracks queue depth.
//!
//! Used by audio-driven backends (AVB, Oscilloscope) where the audio runtime
//! exposes the host-side ring queue length authoritatively.

use std::sync::Arc;
use std::time::Instant;

use super::BufferEstimator;

/// Anything that can report current queue depth.
///
/// The queue is measured in *device output samples* (at the device sample
/// rate), which is not the same unit the scheduler works in: the scheduler
/// compares fullness against `target_secs × pps` points. The estimator uses
/// [`sample_rate`](Self::sample_rate) to convert the sample-rate depth into
/// pps-points so the two are comparable.
pub trait QueueDepthSource: Send + Sync {
    /// Current queue depth in device output samples.
    fn queued_points(&self) -> u64;
    /// The device output sample rate in Hz.
    ///
    /// Defaults to `0`, which makes [`estimated_fullness`] pass
    /// [`queued_points`](Self::queued_points) through unscaled — the behavior
    /// before this conversion existed. Real backends override it with the
    /// detected device rate so the depth is converted into pps-points.
    ///
    /// [`estimated_fullness`]: RuntimeAuthorityEstimator::estimated_fullness
    fn sample_rate(&self) -> u32 {
        0
    }
}

/// Estimator that delegates to a [`QueueDepthSource`].
///
/// The optional source is set when the backend connects and cleared on
/// disconnect. With no source, the estimator reports zero — the same value
/// the underlying runtime would return when no queue exists yet.
#[derive(Default)]
pub struct RuntimeAuthorityEstimator {
    source: Option<Arc<dyn QueueDepthSource>>,
}

impl RuntimeAuthorityEstimator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_source(source: Arc<dyn QueueDepthSource>) -> Self {
        Self {
            source: Some(source),
        }
    }

    pub fn set_source(&mut self, source: Arc<dyn QueueDepthSource>) {
        self.source = Some(source);
    }

    pub fn clear_source(&mut self) {
        self.source = None;
    }
}

impl BufferEstimator for RuntimeAuthorityEstimator {
    fn estimated_fullness(&self, _now: Instant, pps: u32) -> u64 {
        self.source.as_ref().map_or(0, |s| {
            let queued_samples = s.queued_points();
            let sample_rate = s.sample_rate();
            if sample_rate == 0 {
                return queued_samples;
            }
            // Convert the device-sample-rate queue depth into pps-points so it
            // is comparable with the scheduler's `target_secs × pps` target.
            (queued_samples as u128 * pps as u128 / sample_rate as u128) as u64
        })
    }

    fn needs_clock(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct AtomicCounter {
        queued: AtomicU64,
        sample_rate: u32,
    }

    impl AtomicCounter {
        fn new(queued: u64, sample_rate: u32) -> Self {
            Self {
                queued: AtomicU64::new(queued),
                sample_rate,
            }
        }
    }

    impl QueueDepthSource for AtomicCounter {
        fn queued_points(&self) -> u64 {
            self.queued.load(Ordering::Relaxed)
        }
        fn sample_rate(&self) -> u32 {
            self.sample_rate
        }
    }

    #[test]
    fn defaults_to_zero_with_no_source() {
        let est = RuntimeAuthorityEstimator::new();
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 0);
    }

    #[test]
    fn converts_sample_rate_depth_to_pps_points() {
        // When pps matches the sample rate the depth passes through unchanged.
        let counter = Arc::new(AtomicCounter::new(0, 48_000));
        let est = RuntimeAuthorityEstimator::with_source(counter.clone());

        counter.queued.store(42, Ordering::Relaxed);
        assert_eq!(est.estimated_fullness(Instant::now(), 48_000), 42);

        counter.queued.store(0, Ordering::Relaxed);
        assert_eq!(est.estimated_fullness(Instant::now(), 48_000), 0);
    }

    #[test]
    fn depth_in_samples_reports_pps_points_not_raw_samples() {
        // 600 queued device samples at 96 kHz is only ~187 pps-points at 30kpps,
        // not 600 — the estimator must apply the pps/sample_rate ratio.
        let counter = Arc::new(AtomicCounter::new(600, 96_000));
        let est = RuntimeAuthorityEstimator::with_source(counter);
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 187);
    }

    #[test]
    fn default_sample_rate_passes_depth_through_unscaled() {
        // An implementor that does not override `sample_rate()` inherits the
        // `0` default, which must make the estimator report the raw queue depth
        // (the pre-conversion behavior) rather than dividing by zero.
        struct DepthOnly(u64);
        impl QueueDepthSource for DepthOnly {
            fn queued_points(&self) -> u64 {
                self.0
            }
        }

        let est = RuntimeAuthorityEstimator::with_source(Arc::new(DepthOnly(600)));
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 600);
    }

    #[test]
    fn clear_source_returns_zero() {
        let counter = Arc::new(AtomicCounter::new(7, 30_000));
        let mut est = RuntimeAuthorityEstimator::with_source(counter);
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 7);
        est.clear_source();
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 0);
    }
}
