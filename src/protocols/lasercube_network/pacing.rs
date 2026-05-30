//! Pure pacing calculations for the LaserCube network transport.

use std::time::Duration;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct PacerInputs {
    pub queue_len: usize,
    pub free_estimate: usize,
    pub buffer_total: usize,
    pub remote_buffer_cutoff: usize,
    pub per_tick_packet_budget: usize,
}

pub fn send_budget(inputs: PacerInputs) -> usize {
    let device_target = inputs.remote_buffer_cutoff.min(inputs.buffer_total);
    let device_buffered = inputs.buffer_total.saturating_sub(inputs.free_estimate);
    let remote_budget = device_target.saturating_sub(device_buffered);
    inputs
        .queue_len
        .min(remote_budget)
        .min(inputs.per_tick_packet_budget)
}

pub fn packet_interval(packet_samples: usize, point_rate: u32) -> Duration {
    if packet_samples == 0 || point_rate == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs_f64(packet_samples as f64 / point_rate as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_send_at_or_above_remote_cutoff_even_when_buffer_has_space() {
        let budget = send_budget(PacerInputs {
            queue_len: 140,
            free_estimate: 4000,
            buffer_total: 6000,
            remote_buffer_cutoff: 1800,
            per_tick_packet_budget: 140,
        });
        assert_eq!(budget, 0);
    }

    #[test]
    fn limits_send_to_one_packet_budget() {
        let budget = send_budget(PacerInputs {
            queue_len: 1000,
            free_estimate: 6000,
            buffer_total: 6000,
            remote_buffer_cutoff: 1800,
            per_tick_packet_budget: 80,
        });
        assert_eq!(budget, 80);
    }

    #[test]
    fn one_packet_cadence_sustains_30k_for_80_and_140_profiles() {
        let packet_80 = packet_interval(80, 30_000);
        let packet_140 = packet_interval(140, 30_000);
        assert!(packet_80 <= Duration::from_micros(2667));
        assert!(packet_140 <= Duration::from_micros(4667));
        assert!((packet_interval(80, 30_000).as_secs_f64() - 80.0 / 30_000.0).abs() < 1e-9);
        assert!((packet_interval(140, 30_000).as_secs_f64() - 140.0 / 30_000.0).abs() < 1e-9);
    }
}
