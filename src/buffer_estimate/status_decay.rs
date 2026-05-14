//! Status-driven buffer fullness estimator.
//!
//! Anchored on periodic authoritative status reports (e.g. Ether Dream's
//! buffer-fullness ACKs). Between reports the anchor decays at the current
//! point rate; on every send the anchor rebases like the software estimator.

use std::time::Instant;

use super::BufferEstimator;

/// Status-driven anchor estimator.
pub struct StatusDecayEstimator {
    fullness_at_anchor: u64,
    anchor_time: Option<Instant>,
}

impl StatusDecayEstimator {
    pub fn new() -> Self {
        Self {
            fullness_at_anchor: 0,
            anchor_time: None,
        }
    }

    pub fn reset(&mut self) {
        self.fullness_at_anchor = 0;
        self.anchor_time = None;
    }

    /// Authoritative status report from the device.
    pub fn record_status(&mut self, now: Instant, fullness: u64) {
        self.fullness_at_anchor = fullness;
        self.anchor_time = Some(now);
    }

    /// Record that `n` points were just sent at `pps`. Rebases like
    /// [`SoftwareDecayEstimator::record_send`](super::SoftwareDecayEstimator::record_send),
    /// so the anchor stays valid even between status reports.
    pub fn record_send(&mut self, now: Instant, n: u64, pps: u32) {
        let current = self.read_at(now, pps);
        self.fullness_at_anchor = current.saturating_add(n);
        self.anchor_time = Some(now);
    }

    fn read_at(&self, now: Instant, pps: u32) -> u64 {
        match self.anchor_time {
            Some(t) => {
                let elapsed = now.saturating_duration_since(t).as_secs_f64();
                let consumed = (elapsed * pps as f64) as u64;
                self.fullness_at_anchor.saturating_sub(consumed)
            }
            None => 0,
        }
    }
}

impl Default for StatusDecayEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferEstimator for StatusDecayEstimator {
    fn estimated_fullness(&self, now: Instant, pps: u32) -> u64 {
        self.read_at(now, pps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn untouched_estimator_reports_zero() {
        let est = StatusDecayEstimator::new();
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 0);
    }

    #[test]
    fn status_anchors_fullness() {
        let mut est = StatusDecayEstimator::new();
        let t0 = Instant::now();
        est.record_status(t0, 4000);
        assert_eq!(est.estimated_fullness(t0, 30_000), 4000);
    }

    #[test]
    fn status_decays_over_time() {
        let mut est = StatusDecayEstimator::new();
        let t0 = Instant::now();
        est.record_status(t0, 3000);

        let t1 = t0 + Duration::from_millis(50);
        // 0.05s × 30000pps = 1500 → 3000 − 1500 = 1500.
        assert_eq!(est.estimated_fullness(t1, 30_000), 1500);
    }

    #[test]
    fn record_send_advances_anchor() {
        let mut est = StatusDecayEstimator::new();
        let t0 = Instant::now();
        est.record_status(t0, 1000);

        let t1 = t0 + Duration::from_millis(50);
        // current = 1000 − 1500 → 0; +500 from send → 500.
        est.record_send(t1, 500, 30_000);
        assert_eq!(est.estimated_fullness(t1, 30_000), 500);
    }

    #[test]
    fn reset_clears_anchor() {
        let mut est = StatusDecayEstimator::new();
        est.record_status(Instant::now(), 1234);
        est.reset();
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 0);
    }
}
