//! Software-only buffer fullness estimator.
//!
//! Anchor-based: holds the last known fullness and the time it was set.
//! Reads compute `fullness_at_anchor − elapsed × pps` from a fixed anchor —
//! truncation does not accumulate across calls.

use std::time::{Duration, Instant};

use super::BufferEstimator;

/// Pure software estimator with no device telemetry.
pub struct SoftwareDecayEstimator {
    fullness_at_anchor: u64,
    anchor_time: Instant,
}

impl SoftwareDecayEstimator {
    pub fn new() -> Self {
        Self {
            fullness_at_anchor: 0,
            anchor_time: Instant::now(),
        }
    }

    /// Drop all tracking state (e.g. on reconnect or stop).
    pub fn reset(&mut self, now: Instant) {
        self.fullness_at_anchor = 0;
        self.anchor_time = now;
    }

    /// Record that `n` points were just sent at `pps`. Rebases the anchor while
    /// carrying the fractional consumption forward.
    ///
    /// Naively setting `anchor_time = now` and `fullness = floor(current) + n`
    /// discards the sub-point remainder of `elapsed × pps` on every call
    /// (~0.5 points of upward bias per rebase — hundreds of phantom points/sec
    /// at ~1 kHz write rates). Instead we subtract only the *integer* points
    /// consumed and advance the anchor by the exact time those points represent,
    /// leaving the fractional remainder in the residual `now − anchor_time` gap
    /// so it is accounted for on the next read/rebase.
    pub fn record_send(&mut self, now: Instant, n: u64, pps: u32) {
        if pps == 0 {
            // No decay clock: accumulate without moving the anchor.
            self.fullness_at_anchor = self.fullness_at_anchor.saturating_add(n);
            return;
        }
        let elapsed_secs = now
            .saturating_duration_since(self.anchor_time)
            .as_secs_f64();
        let consumed_int = (elapsed_secs * pps as f64) as u64; // floor
        self.fullness_at_anchor = self
            .fullness_at_anchor
            .saturating_sub(consumed_int)
            .saturating_add(n);
        // Advance the anchor by exactly the time the integer points consumed
        // represent, not all the way to `now` — the leftover fraction stays in
        // the (now − anchor) residual and is carried into future reads.
        let consumed_secs = consumed_int as f64 / pps as f64;
        self.anchor_time += Duration::from_secs_f64(consumed_secs);
    }

    fn read_at(&self, now: Instant, pps: u32) -> u64 {
        let elapsed_secs = now
            .saturating_duration_since(self.anchor_time)
            .as_secs_f64();
        let consumed = (elapsed_secs * pps as f64) as u64;
        self.fullness_at_anchor.saturating_sub(consumed)
    }
}

impl Default for SoftwareDecayEstimator {
    fn default() -> Self {
        Self::new()
    }
}

impl BufferEstimator for SoftwareDecayEstimator {
    fn estimated_fullness(&self, now: Instant, pps: u32) -> u64 {
        self.read_at(now, pps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn new_estimator_starts_empty() {
        let est = SoftwareDecayEstimator::new();
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 0);
    }

    #[test]
    fn record_send_increases_fullness() {
        let mut est = SoftwareDecayEstimator::new();
        let t = Instant::now();
        est.record_send(t, 1000, 30_000);
        assert_eq!(est.estimated_fullness(t, 30_000), 1000);
    }

    #[test]
    fn fullness_decays_over_time() {
        let mut est = SoftwareDecayEstimator::new();
        let t0 = Instant::now();
        est.record_send(t0, 3000, 30_000);

        // After 50ms at 30000 pps: 3000 − 1500 = 1500.
        let t1 = t0 + Duration::from_millis(50);
        assert_eq!(est.estimated_fullness(t1, 30_000), 1500);
    }

    #[test]
    fn fullness_clamps_to_zero() {
        let mut est = SoftwareDecayEstimator::new();
        let t0 = Instant::now();
        est.record_send(t0, 100, 30_000);

        let t_far = t0 + Duration::from_secs(10);
        assert_eq!(est.estimated_fullness(t_far, 30_000), 0);
    }

    #[test]
    fn record_send_rebases_from_decayed_current() {
        let mut est = SoftwareDecayEstimator::new();
        let t0 = Instant::now();
        est.record_send(t0, 3000, 30_000);

        // 50ms later, current = 1500. Add 500 more, then total = 2000.
        let t1 = t0 + Duration::from_millis(50);
        est.record_send(t1, 500, 30_000);
        assert_eq!(est.estimated_fullness(t1, 30_000), 2000);
    }

    #[test]
    fn anchor_reads_avoid_fractional_truncation_drift() {
        // Equivalent to the old scheduler's
        // `advance_scheduled_ahead_accumulates_fractional_consumption` test
        // (10 pts at 10 pps), but anchor-based.
        let mut est = SoftwareDecayEstimator::new();
        let t0 = Instant::now();
        est.record_send(t0, 10, 10);

        let t1 = t0 + Duration::from_millis(50);
        assert_eq!(est.estimated_fullness(t1, 10), 10); // 10 − 0.5 → 9 (floor)? actually 0.5 truncates to 0
                                                        // 0.05s × 10pps = 0.5 → truncates to 0 consumed → still 10.

        let t2 = t0 + Duration::from_millis(100);
        // 0.1 × 10 = 1.0 → 1 consumed → 9.
        assert_eq!(est.estimated_fullness(t2, 10), 9);
    }

    #[test]
    fn many_rebases_do_not_accumulate_fractional_bias() {
        // 1000 rebases at ~1.0167ms spacing (30.5 points/step at 30k pps) exercises
        // the fractional-carry path heavily. The floor-truncating rebase used to
        // bake in ~0.5 points of upward bias per call → ~500 phantom points here;
        // the exact-carry version must track a full-precision reference to ~1 pt.
        let pps = 30_000u32;
        let mut est = SoftwareDecayEstimator::new();
        let t0 = Instant::now();
        est.reset(t0);

        est.record_send(t0, 5_000, pps);
        let mut exact = 5_000.0f64;
        let mut last_t = t0;

        for i in 1..=1_000u64 {
            let t = t0 + Duration::from_nanos(i * 1_016_667);
            let dt = t.saturating_duration_since(last_t).as_secs_f64();
            exact -= dt * pps as f64; // true consumption since last rebase
            let send = 30u64; // slight net drain vs 30.5 consumed/step
            est.record_send(t, send, pps);
            exact += send as f64;
            last_t = t;
        }

        let est_val = est.estimated_fullness(last_t, pps) as f64;
        let drift = (est_val - exact).abs();
        assert!(
            drift <= 1.5,
            "estimator drifted {drift} points from exact ({est_val} vs {exact})"
        );
    }

    #[test]
    fn reset_clears_state() {
        let mut est = SoftwareDecayEstimator::new();
        let t0 = Instant::now();
        est.record_send(t0, 1000, 30_000);

        let t1 = t0 + Duration::from_millis(1);
        est.reset(t1);
        assert_eq!(est.estimated_fullness(t1, 30_000), 0);
    }

    #[test]
    fn zero_pps_no_decay() {
        let mut est = SoftwareDecayEstimator::new();
        let t0 = Instant::now();
        est.record_send(t0, 500, 0);

        let later = t0 + Duration::from_secs(5);
        assert_eq!(est.estimated_fullness(later, 0), 500);
    }
}
