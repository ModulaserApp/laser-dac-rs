use std::time::Instant;

/// Decay a software queue estimate based on elapsed wall-clock time.
pub(crate) fn advance_scheduled_ahead(
    scheduled_ahead: &mut u64,
    fractional_consumed: &mut f64,
    last_iteration: &mut Instant,
    now: Instant,
    pps: f64,
) {
    let elapsed = now.duration_since(*last_iteration);
    let consumed_f64 = elapsed.as_secs_f64() * pps + *fractional_consumed;
    let points_consumed = consumed_f64 as u64;
    *fractional_consumed = consumed_f64 - points_consumed as f64;
    *scheduled_ahead = scheduled_ahead.saturating_sub(points_consumed);
    *last_iteration = now;
}

/// Combine software and hardware queue estimates conservatively.
pub(crate) fn conservative_buffered_points(software: u64, hardware: Option<u64>) -> u64 {
    hardware.map_or(software, |device_queue| device_queue.min(software))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn advance_scheduled_ahead_accumulates_fractional_consumption() {
        let mut scheduled_ahead = 10u64;
        let mut fractional_consumed = 0.0;
        let mut last_iteration = Instant::now();
        let pps = 10.0;

        let t1 = last_iteration + Duration::from_millis(50);
        advance_scheduled_ahead(
            &mut scheduled_ahead,
            &mut fractional_consumed,
            &mut last_iteration,
            t1,
            pps,
        );
        assert_eq!(scheduled_ahead, 10);
        assert!((fractional_consumed - 0.5).abs() < f64::EPSILON);

        let t2 = last_iteration + Duration::from_millis(50);
        advance_scheduled_ahead(
            &mut scheduled_ahead,
            &mut fractional_consumed,
            &mut last_iteration,
            t2,
            pps,
        );
        assert_eq!(scheduled_ahead, 9);
        assert!(fractional_consumed.abs() < f64::EPSILON);
    }

    #[test]
    fn conservative_buffered_points_prefers_lower_estimate() {
        assert_eq!(conservative_buffered_points(500, None), 500);
        assert_eq!(conservative_buffered_points(500, Some(300)), 300);
        assert_eq!(conservative_buffered_points(500, Some(800)), 500);
    }
}
