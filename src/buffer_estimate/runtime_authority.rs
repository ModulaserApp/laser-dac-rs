//! Buffer estimator backed by an external runtime that already tracks queue depth.
//!
//! Used by audio-driven backends (AVB, Oscilloscope) where the audio runtime
//! exposes the host-side ring queue length authoritatively.

use std::sync::Arc;
use std::time::Instant;

use super::BufferEstimator;

/// Anything that can report current queue depth in points.
pub trait QueueDepthSource: Send + Sync {
    fn queued_points(&self) -> u64;
}

/// Estimator that delegates to a [`QueueDepthSource`].
///
/// The optional source is set when the backend connects and cleared on
/// disconnect. With no source, the estimator reports zero — the same value
/// the underlying runtime would return when no queue exists yet.
pub struct RuntimeAuthorityEstimator {
    source: Option<Arc<dyn QueueDepthSource>>,
}

impl RuntimeAuthorityEstimator {
    pub fn new() -> Self {
        Self { source: None }
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

impl Default for RuntimeAuthorityEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferEstimator for RuntimeAuthorityEstimator {
    fn estimated_fullness(&self, _now: Instant, _pps: u32) -> u64 {
        self.source.as_ref().map_or(0, |s| s.queued_points())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct AtomicCounter(AtomicU64);

    impl QueueDepthSource for AtomicCounter {
        fn queued_points(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }
    }

    #[test]
    fn defaults_to_zero_with_no_source() {
        let est = RuntimeAuthorityEstimator::new();
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 0);
    }

    #[test]
    fn returns_source_value() {
        let counter = Arc::new(AtomicCounter(AtomicU64::new(0)));
        let est = RuntimeAuthorityEstimator::with_source(counter.clone());

        counter.0.store(42, Ordering::Relaxed);
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 42);

        counter.0.store(0, Ordering::Relaxed);
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 0);
    }

    #[test]
    fn clear_source_returns_zero() {
        let counter = Arc::new(AtomicCounter(AtomicU64::new(7)));
        let mut est = RuntimeAuthorityEstimator::with_source(counter);
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 7);
        est.clear_source();
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 0);
    }
}
