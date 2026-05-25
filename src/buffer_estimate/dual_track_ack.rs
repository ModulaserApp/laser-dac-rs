//! Dual-track buffer estimator for UDP-acked DACs.
//!
//! Tracks two independent estimates of device buffer fullness — one anchored on
//! the last successful send, one on the last ACK — and reports the conservative
//! maximum. Prevents buffer overruns when ACKs are delayed or lost.
//!
//! Used by UDP-acked network DAC transports.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::BufferEstimator;

/// Safety margin subtracted from available space to prevent overruns.
pub const LATENCY_POINT_ADJUSTMENT: u16 = 300;

/// Entries in the message-time map older than this are considered stale.
const STALE_ENTRY_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum free points (after safety margin) required before sending.
const MIN_SENDABLE_POINTS: u16 = 140;

/// Dual-estimate buffer tracker.
///
/// Maintains two independent estimates:
///
/// 1. **Sent-track**: anchored on the last send, decayed at the playback rate.
/// 2. **Ack-track**: anchored on the last ACK-reported fullness, decayed at
///    the playback rate.
///
/// The conservative estimate is `max(sent, ack)`. ACKs are correlated to
/// previously-sent messages by `message_number` for precise sent-track
/// anchoring.
pub struct DualTrackAckEstimator {
    capacity: u16,
    point_rate: u32,
    last_data_sent_time: Option<Instant>,
    last_data_sent_buffer_size: u16,
    last_ack_time: Option<Instant>,
    last_reported_buffer_fullness: u16,
    message_times: HashMap<u8, Instant>,
}

impl DualTrackAckEstimator {
    /// Create a new estimator with the given device capacity and initial rate.
    pub fn new(capacity: u16, point_rate: u32) -> Self {
        Self {
            capacity,
            point_rate,
            last_data_sent_time: None,
            last_data_sent_buffer_size: 0,
            last_ack_time: None,
            last_reported_buffer_fullness: 0,
            message_times: HashMap::new(),
        }
    }

    pub fn capacity(&self) -> u16 {
        self.capacity
    }

    pub fn point_rate(&self) -> u32 {
        self.point_rate
    }

    /// Update the playback rate used for time-based decay calculations.
    pub fn set_point_rate(&mut self, rate: u32) {
        self.point_rate = rate;
    }

    /// Reset all tracking state (e.g. after clearing the device buffer).
    pub fn reset(&mut self) {
        self.last_data_sent_time = None;
        self.last_data_sent_buffer_size = 0;
        self.last_ack_time = None;
        self.last_reported_buffer_fullness = 0;
        self.message_times.clear();
    }

    fn estimate_fullness_by_time_sent(&self, now: Instant) -> u16 {
        let Some(sent_time) = self.last_data_sent_time else {
            return 0;
        };
        let elapsed = now.saturating_duration_since(sent_time);
        let consumed = (elapsed.as_secs_f64() * self.point_rate as f64) as u16;
        self.last_data_sent_buffer_size.saturating_sub(consumed)
    }

    fn estimate_fullness_by_time_acked(&self, now: Instant) -> u16 {
        let Some(ack_time) = self.last_ack_time else {
            return 0;
        };
        let elapsed = now.saturating_duration_since(ack_time);
        let consumed = (elapsed.as_secs_f64() * self.point_rate as f64) as u16;
        self.last_reported_buffer_fullness.saturating_sub(consumed)
    }

    /// Conservative estimate of buffer fullness: max of both tracks.
    pub fn estimated_buffer_fullness(&self, now: Instant) -> u16 {
        let by_sent = self.estimate_fullness_by_time_sent(now);
        let by_acked = self.estimate_fullness_by_time_acked(now);
        by_sent.max(by_acked)
    }

    /// Maximum number of points that can safely be added right now.
    pub fn max_points_to_add(&self, now: Instant) -> u16 {
        let fullness = self.estimated_buffer_fullness(now);
        self.capacity
            .saturating_sub(fullness)
            .saturating_sub(LATENCY_POINT_ADJUSTMENT)
    }

    /// Whether it's safe to send a packet right now.
    pub fn can_send(&self, now: Instant) -> bool {
        self.max_points_to_add(now) >= MIN_SENDABLE_POINTS
    }

    /// Record that a packet was sent.
    pub fn record_send(&mut self, now: Instant, message_number: u8, points_sent: u16) {
        let current_fullness = self.estimated_buffer_fullness(now);
        self.last_data_sent_time = Some(now);
        self.last_data_sent_buffer_size = current_fullness
            .saturating_add(points_sent)
            .min(self.capacity);
        self.message_times.insert(message_number, now);
    }

    /// Record an ACK from the device, correlating to the original send if possible.
    pub fn record_ack(&mut self, now: Instant, message_number: u8, free_space: u16) {
        let reported_fullness = self.capacity.saturating_sub(free_space);

        if let Some(&send_time) = self.message_times.get(&message_number) {
            if self.last_data_sent_time.is_none_or(|t| send_time >= t) {
                self.last_data_sent_time = Some(send_time);
                self.last_data_sent_buffer_size = reported_fullness;
            }
            self.message_times.remove(&message_number);
        }

        self.last_ack_time = Some(now);
        self.last_reported_buffer_fullness = reported_fullness;

        self.cleanup_stale_entries(now);
    }

    fn cleanup_stale_entries(&mut self, now: Instant) {
        self.message_times
            .retain(|_, &mut time| now.saturating_duration_since(time) < STALE_ENTRY_TIMEOUT);
    }
}

impl BufferEstimator for DualTrackAckEstimator {
    fn estimated_fullness(&self, now: Instant, _pps: u32) -> u64 {
        // Drain rate is held internally via `set_point_rate` — the trait pps
        // argument is ignored.
        self.estimated_buffer_fullness(now) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn new_estimator_has_zero_fullness() {
        let est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        assert_eq!(est.estimated_buffer_fullness(t), 0);
        assert_eq!(est.max_points_to_add(t), 6000 - 300);
    }

    #[test]
    fn reset_clears_state() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 140);
        assert!(est.estimated_buffer_fullness(t) > 0);

        est.reset();
        assert_eq!(est.estimated_buffer_fullness(t), 0);
    }

    #[test]
    fn send_increases_fullness() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 140);
        assert_eq!(est.estimated_buffer_fullness(t), 140);
    }

    #[test]
    fn fullness_decays_over_time() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 3000);
        assert_eq!(est.estimated_buffer_fullness(t), 3000);

        let later = t + Duration::from_millis(50);
        assert_eq!(est.estimated_buffer_fullness(later), 1500);
    }

    #[test]
    fn fullness_never_goes_negative() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 100);

        let later = t + Duration::from_secs(10);
        assert_eq!(est.estimated_buffer_fullness(later), 0);
    }

    #[test]
    fn multiple_sends_accumulate() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 140);
        est.record_send(t, 1, 140);
        assert_eq!(est.estimated_buffer_fullness(t), 280);
    }

    #[test]
    fn ack_updates_fullness() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_ack(t, 0, 1000);
        assert_eq!(est.estimated_buffer_fullness(t), 5000);
    }

    #[test]
    fn ack_track_decays_over_time() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_ack(t, 0, 1000);

        let later = t + Duration::from_millis(100);
        assert_eq!(est.estimated_buffer_fullness(later), 2000);
    }

    #[test]
    fn returns_max_of_both_tracks() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 140);
        est.record_ack(t, 42, 5900);
        assert_eq!(est.estimated_buffer_fullness(t), 140);
    }

    #[test]
    fn ack_track_can_dominate() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 100);
        est.record_ack(t, 99, 2000);
        assert_eq!(est.estimated_buffer_fullness(t), 4000);
    }

    #[test]
    fn ack_correlates_to_send_time() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t0 = now();
        est.record_send(t0, 5, 140);

        let t1 = t0 + Duration::from_millis(20);
        est.record_ack(t1, 5, 4000);
        // Sent-track anchored at t0 with fullness=2000.
        // At t1: 2000 − 600 = 1400; ack-track at t1 = 2000.
        // max = 2000.
        assert_eq!(est.estimated_buffer_fullness(t1), 2000);
    }

    #[test]
    fn unknown_message_still_updates_ack_track() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_ack(t, 255, 5000);
        assert_eq!(est.estimated_buffer_fullness(t), 1000);
    }

    #[test]
    fn ack_only_updates_sent_track_if_newer() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t0 = now();
        let t1 = t0 + Duration::from_millis(10);
        let t2 = t0 + Duration::from_millis(20);
        let t3 = t0 + Duration::from_millis(30);

        est.record_send(t0, 1, 140);
        est.record_send(t1, 2, 140);

        est.record_ack(t2, 2, 5000);
        est.record_ack(t3, 1, 5500);

        // Sent-track still anchored at t1.
        let sent_est = est.estimate_fullness_by_time_sent(t3);
        assert_eq!(sent_est, 400);
    }

    #[test]
    fn stale_entries_removed() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t0 = now();
        est.record_send(t0, 1, 140);
        est.record_send(t0, 2, 140);

        let t1 = t0 + Duration::from_secs(11);
        est.record_ack(t1, 99, 6000);
        assert!(est.message_times.is_empty());
    }

    #[test]
    fn recent_entries_kept() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t0 = now();
        est.record_send(t0, 1, 140);

        let t1 = t0 + Duration::from_secs(1);
        est.record_send(t1, 2, 140);
        est.record_ack(t1, 99, 6000);

        assert_eq!(est.message_times.len(), 2);
    }

    #[test]
    fn can_send_when_empty() {
        let est = DualTrackAckEstimator::new(6000, 30_000);
        assert!(est.can_send(now()));
    }

    #[test]
    fn cannot_send_when_full() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 5561);
        assert!(!est.can_send(t));
    }

    #[test]
    fn can_send_after_drain() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 5561);
        assert!(!est.can_send(t));

        let later = t + Duration::from_millis(100);
        assert!(est.can_send(later));
    }

    #[test]
    fn zero_rate_no_decay() {
        let mut est = DualTrackAckEstimator::new(6000, 0);
        let t = now();
        est.record_send(t, 0, 500);
        let later = t + Duration::from_secs(5);
        assert_eq!(est.estimated_buffer_fullness(later), 500);
    }

    #[test]
    fn zero_rate_no_panic() {
        let mut est = DualTrackAckEstimator::new(6000, 0);
        let t = now();
        est.record_send(t, 0, 100);
        est.record_ack(t, 0, 5900);
        let _ = est.can_send(t);
        let _ = est.max_points_to_add(t);
        let _ = est.estimated_buffer_fullness(t);
    }

    #[test]
    fn wrapping_message_numbers() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 254, 140);
        est.record_send(t, 255, 140);
        est.record_send(t, 0, 140);

        let t1 = t + Duration::from_millis(5);
        est.record_ack(t1, 255, 5500);

        assert!(!est.message_times.contains_key(&255));
        assert!(est.message_times.contains_key(&254));
        assert!(est.message_times.contains_key(&0));
    }

    #[test]
    fn set_point_rate_changes_decay() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 3000);

        let later = t + Duration::from_millis(50);
        assert_eq!(est.estimated_buffer_fullness(later), 1500);

        est.set_point_rate(60_000);
        assert_eq!(est.estimated_buffer_fullness(later), 0);
    }

    #[test]
    fn rate_decrease_reduces_writable_headroom() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 3000);

        let later = t + Duration::from_millis(50);
        assert_eq!(est.max_points_to_add(later), 4200);

        est.set_point_rate(10_000);
        assert_eq!(est.max_points_to_add(later), 3200);
    }

    #[test]
    fn send_fullness_is_clamped_to_capacity() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 5900);
        est.record_send(t, 1, 500);
        assert_eq!(est.estimated_buffer_fullness(t), 6000);
    }

    #[test]
    fn trait_estimated_fullness_matches_concrete() {
        let mut est = DualTrackAckEstimator::new(6000, 30_000);
        let t = now();
        est.record_send(t, 0, 1234);
        let trait_obj: &dyn BufferEstimator = &est;
        assert_eq!(trait_obj.estimated_fullness(t, 30_000), 1234);
    }
}
