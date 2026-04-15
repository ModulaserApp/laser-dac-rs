/// Calculate the number of output samples when resampling `input_len` points
/// from `from_rate` to `to_rate`.
pub(crate) fn resampled_len(input_len: usize, from_rate: u32, to_rate: u32) -> usize {
    if input_len == 0 {
        return 0;
    }
    (input_len as u64 * to_rate as u64).div_ceil(from_rate as u64) as usize
}

/// Evaluate a 4-point Catmull-Rom (Hermite) spline at fractional position `t`
/// between samples `s1` and `s2`, with predecessor `s0` and successor `s3`.
///
/// This gives C1-continuous interpolation (smooth slopes at every knot),
/// producing visually smoother curves than linear interpolation when
/// resampling laser paths between PPS and audio sample rate.
pub(crate) fn catmull_rom(s0: f32, s1: f32, s2: f32, s3: f32, t: f32) -> f32 {
    let c1 = 0.5 * (s2 - s0);
    let c2 = s0 - 2.5 * s1 + 2.0 * s2 - 0.5 * s3;
    let c3 = 0.5 * (s3 - s0) + 1.5 * (s1 - s2);
    ((c3 * t + c2) * t + c1) * t + s1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampled_len_upsample() {
        assert_eq!(resampled_len(3, 30_000, 48_000), 5);
    }

    #[test]
    fn resampled_len_downsample() {
        assert_eq!(resampled_len(5, 60_000, 48_000), 4);
    }

    #[test]
    fn resampled_len_passthrough() {
        assert_eq!(resampled_len(10, 48_000, 48_000), 10);
        assert_eq!(resampled_len(1, 48_000, 48_000), 1);
        assert_eq!(resampled_len(0, 48_000, 48_000), 0);
    }

    #[test]
    fn catmull_rom_passes_through_knots() {
        // At t=0, result should equal s1; at t=1, result should equal s2.
        let s0 = 0.0;
        let s1 = 1.0;
        let s2 = 2.0;
        let s3 = 3.0;
        assert!((catmull_rom(s0, s1, s2, s3, 0.0) - s1).abs() < 1e-6);
        assert!((catmull_rom(s0, s1, s2, s3, 1.0) - s2).abs() < 1e-6);
    }

    #[test]
    fn catmull_rom_midpoint_on_linear_data() {
        // For evenly spaced linear data, midpoint should be exact.
        assert!((catmull_rom(0.0, 1.0, 2.0, 3.0, 0.5) - 1.5).abs() < 1e-6);
    }

    #[test]
    fn catmull_rom_with_clamped_boundaries() {
        // Simulates boundary clamping where s0 == s1 and s3 == s2.
        let s1 = -1.0;
        let s2 = 1.0;
        let result = catmull_rom(s1, s1, s2, s2, 0.5);
        // Should still be between s1 and s2.
        assert!(result > s1 && result < s2);
    }
}
