//! Dual-estimate buffer tracking for LaserCube WiFi DACs.
//!
//! Uses two independent estimates of buffer fullness — one based on when data was last sent,
//! and one based on the last ACK — taking the maximum (conservative) value. This prevents
//! buffer overruns even when ACKs are delayed or lost.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Safety margin subtracted from available space to prevent overruns.
const LATENCY_POINT_ADJUSTMENT: u16 = 300;

/// Entries in the message-time map older than this are considered stale and removed.
const STALE_ENTRY_TIMEOUT: Duration = Duration::from_secs(10);

/// Minimum number of free points (after safety margin) required before sending.
const MIN_SENDABLE_POINTS: u16 = 140;

/// Dual-estimate buffer tracker for LaserCube WiFi DACs.
///
/// Maintains two independent estimates of device buffer fullness:
/// 1. **Sent-track**: Based on the last send time and points sent, decayed by the playback rate.
/// 2. **Ack-track**: Based on the last ACK-reported fullness, decayed by the playback rate.
///
/// The maximum of both estimates is used as the conservative fullness value.
/// ACKs are correlated to specific sent messages via `message_number` for precise anchoring.
pub struct BufferEstimator {
    capacity: u16,
    point_rate: u32,
    // Sent-track state
    last_data_sent_time: Option<Instant>,
    last_data_sent_buffer_size: u16,
    // Ack-track state
    last_ack_time: Option<Instant>,
    last_reported_buffer_fullness: u16,
    // ACK correlation: message_number -> time sent
    message_times: HashMap<u8, Instant>,
}

impl BufferEstimator {
    /// Create a new estimator with the given device buffer capacity and playback rate.
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

    /// Estimate buffer fullness based on the sent-track (last send time + decay).
    fn estimate_fullness_by_time_sent(&self, now: Instant) -> u16 {
        let Some(sent_time) = self.last_data_sent_time else {
            return 0;
        };
        let elapsed = now.duration_since(sent_time);
        let consumed = (elapsed.as_secs_f64() * self.point_rate as f64) as u16;
        self.last_data_sent_buffer_size.saturating_sub(consumed)
    }

    /// Estimate buffer fullness based on the ack-track (last ACK + decay).
    fn estimate_fullness_by_time_acked(&self, now: Instant) -> u16 {
        let Some(ack_time) = self.last_ack_time else {
            return 0;
        };
        let elapsed = now.duration_since(ack_time);
        let consumed = (elapsed.as_secs_f64() * self.point_rate as f64) as u16;
        self.last_reported_buffer_fullness.saturating_sub(consumed)
    }

    /// Conservative estimate of buffer fullness: the maximum of both tracks.
    pub fn estimated_buffer_fullness(&self, now: Instant) -> u16 {
        let by_sent = self.estimate_fullness_by_time_sent(now);
        let by_acked = self.estimate_fullness_by_time_acked(now);
        by_sent.max(by_acked)
    }

    /// Maximum number of points that can safely be added to the buffer right now.
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

    /// Record that a packet was sent with the given message number and point count.
    pub fn record_send(&mut self, message_number: u8, points_sent: u16, now: Instant) {
        let current_fullness = self.estimated_buffer_fullness(now);
        self.last_data_sent_time = Some(now);
        self.last_data_sent_buffer_size = current_fullness + points_sent;
        self.message_times.insert(message_number, now);
    }

    /// Record an ACK from the device, correlating it to the original send if possible.
    pub fn record_ack(&mut self, message_number: u8, free_space: u16, now: Instant) {
        let reported_fullness = self.capacity.saturating_sub(free_space);

        // Try to correlate this ACK to a specific send time
        if let Some(&send_time) = self.message_times.get(&message_number) {
            // Only update sent-track if this is newer than our current anchor
            if self.last_data_sent_time.is_none_or(|t| send_time >= t) {
                self.last_data_sent_time = Some(send_time);
                self.last_data_sent_buffer_size = reported_fullness;
            }
            self.message_times.remove(&message_number);
        }

        // Always update ack-track
        self.last_ack_time = Some(now);
        self.last_reported_buffer_fullness = reported_fullness;

        self.cleanup_stale_entries(now);
    }

    /// Remove message-time entries older than `STALE_ENTRY_TIMEOUT`.
    fn cleanup_stale_entries(&mut self, now: Instant) {
        self.message_times
            .retain(|_, &mut time| now.duration_since(time) < STALE_ENTRY_TIMEOUT);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    // --- Construction & reset ---

    #[test]
    fn new_estimator_has_zero_fullness() {
        let est = BufferEstimator::new(6000, 30000);
        let t = now();
        assert_eq!(est.estimated_buffer_fullness(t), 0);
        assert_eq!(est.max_points_to_add(t), 6000 - 300);
    }

    #[test]
    fn reset_clears_state() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();
        est.record_send(0, 140, t);
        assert!(est.estimated_buffer_fullness(t) > 0);

        est.reset();
        assert_eq!(est.estimated_buffer_fullness(t), 0);
    }

    // --- Sent-track ---

    #[test]
    fn send_increases_fullness() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();
        est.record_send(0, 140, t);
        assert_eq!(est.estimated_buffer_fullness(t), 140);
    }

    #[test]
    fn fullness_decays_over_time() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();
        est.record_send(0, 3000, t);
        assert_eq!(est.estimated_buffer_fullness(t), 3000);

        // After 50ms at 30000 pps: consumed = 0.05 * 30000 = 1500
        let later = t + Duration::from_millis(50);
        assert_eq!(est.estimated_buffer_fullness(later), 1500);
    }

    #[test]
    fn fullness_never_goes_negative() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();
        est.record_send(0, 100, t);

        // Way in the future — should clamp to 0
        let later = t + Duration::from_secs(10);
        assert_eq!(est.estimated_buffer_fullness(later), 0);
    }

    #[test]
    fn multiple_sends_accumulate() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();
        est.record_send(0, 140, t);
        est.record_send(1, 140, t);
        assert_eq!(est.estimated_buffer_fullness(t), 280);
    }

    // --- Ack-track ---

    #[test]
    fn ack_updates_fullness() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();

        // ACK says 1000 free out of 6000 => fullness = 5000
        est.record_ack(0, 1000, t);
        // Ack track: 5000, sent track: 0 => max = 5000
        assert_eq!(est.estimated_buffer_fullness(t), 5000);
    }

    #[test]
    fn ack_track_decays_over_time() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();

        est.record_ack(0, 1000, t); // fullness = 5000
                                    // After 100ms at 30000 pps: consumed = 3000
        let later = t + Duration::from_millis(100);
        assert_eq!(est.estimated_buffer_fullness(later), 2000);
    }

    // --- Dual-track (max of both) ---

    #[test]
    fn returns_max_of_both_tracks() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();

        // Sent-track: 140 points
        est.record_send(0, 140, t);

        // ACK says only 100 fullness (free_space = 5900)
        est.record_ack(42, 5900, t);

        // max(140, 100) = 140
        assert_eq!(est.estimated_buffer_fullness(t), 140);
    }

    #[test]
    fn ack_track_can_dominate() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();

        // Sent-track: 100 points
        est.record_send(0, 100, t);

        // ACK says 4000 fullness (free_space = 2000) from unknown msg
        est.record_ack(99, 2000, t);

        // max(100, 4000) = 4000
        assert_eq!(est.estimated_buffer_fullness(t), 4000);
    }

    // --- ACK correlation ---

    #[test]
    fn ack_correlates_to_send_time() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t0 = now();

        // Send at t0
        est.record_send(5, 140, t0);

        // ACK arrives 20ms later, reports 4000 free
        let t1 = t0 + Duration::from_millis(20);
        est.record_ack(5, 4000, t1);

        // Sent-track should now be anchored at t0 with fullness=2000
        // At t1 (20ms later): 2000 - (0.02 * 30000) = 2000 - 600 = 1400
        // Ack-track at t1: 2000 (just set, 0 elapsed)
        // max(1400, 2000) = 2000
        assert_eq!(est.estimated_buffer_fullness(t1), 2000);
    }

    #[test]
    fn unknown_message_still_updates_ack_track() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();

        // ACK for an unknown message number
        est.record_ack(255, 5000, t);

        // Should still see the ack-track fullness
        assert_eq!(est.estimated_buffer_fullness(t), 1000);
    }

    #[test]
    fn ack_only_updates_sent_track_if_newer() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t0 = now();
        let t1 = t0 + Duration::from_millis(10);
        let t2 = t0 + Duration::from_millis(20);
        let t3 = t0 + Duration::from_millis(30);

        // Send msg 1 at t0, msg 2 at t1
        est.record_send(1, 140, t0);
        est.record_send(2, 140, t1);

        // ACK for msg 2 (newer) at t2
        est.record_ack(2, 5000, t2);
        // Sent-track anchored at t1 with fullness 1000

        // ACK for msg 1 (older) at t3 — should NOT update sent-track
        est.record_ack(1, 5500, t3);

        // Sent-track still anchored at t1 (from msg 2)
        // At t3 (20ms after t1): 1000 - (0.02 * 30000) = 1000 - 600 = 400
        let sent_est = est.estimate_fullness_by_time_sent(t3);
        assert_eq!(sent_est, 400);
    }

    // --- Stale cleanup ---

    #[test]
    fn stale_entries_removed() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t0 = now();

        est.record_send(1, 140, t0);
        est.record_send(2, 140, t0);

        // 11 seconds later, entries should be stale
        let t1 = t0 + Duration::from_secs(11);
        est.record_ack(99, 6000, t1); // triggers cleanup

        assert!(est.message_times.is_empty());
    }

    #[test]
    fn recent_entries_kept() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t0 = now();

        est.record_send(1, 140, t0);

        // Only 1 second later — not stale
        let t1 = t0 + Duration::from_secs(1);
        est.record_send(2, 140, t1);
        est.record_ack(99, 6000, t1); // triggers cleanup

        // msg 1 (1s old) and msg 2 (0s old) should both be kept
        assert_eq!(est.message_times.len(), 2);
    }

    // --- Send decisions ---

    #[test]
    fn can_send_when_empty() {
        let est = BufferEstimator::new(6000, 30000);
        assert!(est.can_send(now()));
    }

    #[test]
    fn cannot_send_when_full() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();

        // Fill buffer to capacity minus safety margin minus one less than min sendable
        // capacity=6000, safety=300, so available = 6000 - fullness - 300
        // We need available < 140, so fullness > 6000 - 300 - 140 = 5560
        est.record_send(0, 5561, t);
        assert!(!est.can_send(t));
    }

    #[test]
    fn can_send_after_drain() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();

        est.record_send(0, 5561, t);
        assert!(!est.can_send(t));

        // After enough time, buffer drains and we can send again
        // Need fullness <= 5560 => consumed >= 1 => elapsed >= 1/30000 ~= 0.033ms
        // But actually need fullness to drop enough: 5561 - consumed <= 5560
        // After 100ms: consumed = 3000, fullness = 2561
        let later = t + Duration::from_millis(100);
        assert!(est.can_send(later));
    }

    // --- Edge cases ---

    #[test]
    fn zero_rate_no_decay() {
        let mut est = BufferEstimator::new(6000, 0);
        let t = now();

        est.record_send(0, 500, t);
        let later = t + Duration::from_secs(5);
        // With zero rate, nothing is consumed
        assert_eq!(est.estimated_buffer_fullness(later), 500);
    }

    #[test]
    fn zero_rate_no_panic() {
        let mut est = BufferEstimator::new(6000, 0);
        let t = now();
        est.record_send(0, 100, t);
        est.record_ack(0, 5900, t);
        let _ = est.can_send(t);
        let _ = est.max_points_to_add(t);
        let _ = est.estimated_buffer_fullness(t);
    }

    #[test]
    fn wrapping_message_numbers() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();

        // Use message number near wrap boundary
        est.record_send(254, 140, t);
        est.record_send(255, 140, t);
        est.record_send(0, 140, t); // wrapped

        // ACK for 255
        let t1 = t + Duration::from_millis(5);
        est.record_ack(255, 5500, t1);

        // msg 255 should be removed, 254 and 0 still present
        assert!(!est.message_times.contains_key(&255));
        assert!(est.message_times.contains_key(&254));
        assert!(est.message_times.contains_key(&0));
    }

    #[test]
    fn set_point_rate_changes_decay() {
        let mut est = BufferEstimator::new(6000, 30000);
        let t = now();
        est.record_send(0, 3000, t);

        // At 30000 pps, after 50ms: consumed = 1500, fullness = 1500
        let later = t + Duration::from_millis(50);
        assert_eq!(est.estimated_buffer_fullness(later), 1500);

        // Change to 60000 pps — future estimates use new rate
        est.set_point_rate(60000);
        // At 60000 pps, after 50ms from t: consumed = 3000, fullness = 0
        assert_eq!(est.estimated_buffer_fullness(later), 0);
    }
}
