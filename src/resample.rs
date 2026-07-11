//! Resampling helpers shared by the audio-clocked backends (AVB, oscilloscope).
//!
//! These backends must convert a point stream authored at the user's PPS into
//! the audio device's fixed sample rate. Doing that per write-chunk with a
//! fresh, endpoint-inclusive pass reintroduces a phase discontinuity at every
//! chunk boundary: the source interval spanning two chunks is always collapsed
//! into a single output step, producing a galvo-velocity transient (a ~50 Hz
//! banding artifact) and a C1 break from the clamped spline context.
//!
//! [`StreamingResampler`] fixes that by carrying the fractional source phase
//! and the trailing input samples across calls, so interpolation spans chunk
//! boundaries and the long-run output/input ratio is exactly `to_rate/from_rate`.

use std::collections::VecDeque;

/// Calculate the number of output samples when resampling `input_len` points
/// from `from_rate` to `to_rate` in a single stateless, endpoint-inclusive
/// pass. [`StreamingResampler::pending_output_count`] is the stateful
/// equivalent (accounting for carried phase) used by the backends; this
/// remains as a reference for tests.
#[cfg(test)]
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

/// A value that can be interpolated with a 4-point Catmull-Rom spline.
///
/// Implemented for the payload types the streaming resampler carries: scalar
/// `f32`, stereo `(f32, f32)` (oscilloscope), and the AVB backend's
/// `StreamPoint`.
pub(crate) trait CatmullInterp: Copy {
    fn catmull(s0: Self, s1: Self, s2: Self, s3: Self, t: f32) -> Self;
}

impl CatmullInterp for f32 {
    fn catmull(s0: Self, s1: Self, s2: Self, s3: Self, t: f32) -> Self {
        catmull_rom(s0, s1, s2, s3, t)
    }
}

impl CatmullInterp for (f32, f32) {
    fn catmull(s0: Self, s1: Self, s2: Self, s3: Self, t: f32) -> Self {
        (
            catmull_rom(s0.0, s1.0, s2.0, s3.0, t),
            catmull_rom(s0.1, s1.1, s2.1, s3.1, t),
        )
    }
}

/// Stateful streaming resampler from `from_rate` to `to_rate`.
///
/// Output sample `n` samples the input stream at source position
/// `src(n) = n * from_rate / to_rate` (in input-sample index units), measured
/// continuously across every [`process`](Self::process) call. Because the
/// phase is global, the long-run output count converges to exactly
/// `input_count * to_rate / from_rate` and there is no per-chunk endpoint snap.
///
/// The trailing input samples are retained between calls so the Catmull-Rom
/// context (`s0`, `s1`) is available for outputs that straddle a chunk
/// boundary. The look-ahead context (`s3`) is clamped to the newest available
/// input at the trailing edge of each chunk, so an output whose successor has
/// not yet arrived is emitted immediately (rather than held back) — introducing
/// only a sub-sample tangent difference relative to resampling the whole stream
/// at once, and never a position discontinuity.
///
/// Reset the phase (via [`reset`](Self::reset), or implicitly by changing rates
/// with [`set_rates`](Self::set_rates)) on connect/stop/reconnect so a new
/// stream starts cleanly.
pub(crate) struct StreamingResampler<T> {
    from_rate: u32,
    to_rate: u32,
    /// Retained input samples for absolute indices `[buf_start, n_in)`.
    buf: VecDeque<T>,
    /// Absolute input index of `buf.front()`.
    buf_start: u64,
    /// Total input samples consumed since the last reset.
    n_in: u64,
    /// Total output samples emitted since the last reset.
    n_out: u64,
}

impl<T: CatmullInterp> StreamingResampler<T> {
    pub(crate) fn new(from_rate: u32, to_rate: u32) -> Self {
        assert!(
            from_rate > 0 && to_rate > 0,
            "resampler rates must be nonzero"
        );
        Self {
            from_rate,
            to_rate,
            buf: VecDeque::new(),
            buf_start: 0,
            n_in: 0,
            n_out: 0,
        }
    }

    /// Clear all carried phase and history. Call on connect/stop/reconnect.
    pub(crate) fn reset(&mut self) {
        self.buf.clear();
        self.buf_start = 0;
        self.n_in = 0;
        self.n_out = 0;
    }

    /// Update the conversion rates. When either rate changes the phase is reset
    /// so the new ratio starts cleanly (a mid-stream PPS change re-phases).
    pub(crate) fn set_rates(&mut self, from_rate: u32, to_rate: u32) {
        assert!(
            from_rate > 0 && to_rate > 0,
            "resampler rates must be nonzero"
        );
        if from_rate != self.from_rate || to_rate != self.to_rate {
            self.from_rate = from_rate;
            self.to_rate = to_rate;
            self.reset();
        }
    }

    /// Total output samples that will have been emitted once `n_in` input
    /// samples have been consumed.
    fn total_emittable(&self, n_in: u64) -> u64 {
        if n_in == 0 {
            0
        } else {
            (n_in - 1) * self.to_rate as u64 / self.from_rate as u64 + 1
        }
    }

    /// Number of output samples [`process`](Self::process) will emit for an
    /// input chunk of `input_len` samples, given the current carried phase.
    /// Does not mutate — callers use this to reserve queue capacity first.
    pub(crate) fn pending_output_count(&self, input_len: usize) -> usize {
        (self.total_emittable(self.n_in + input_len as u64) - self.n_out) as usize
    }

    /// Read the input sample at absolute index `abs`, clamped to the available
    /// range `[0, last]`. Requires `buf_start <= clamp(abs)`, which the prune
    /// invariant guarantees for every index the emit loop asks for.
    fn at(&self, abs: i64, last: u64) -> T {
        let clamped = abs.clamp(0, last as i64) as u64;
        self.buf[(clamped - self.buf_start) as usize]
    }

    /// Drop input samples no longer needed for future output context.
    fn prune(&mut self) {
        // The earliest index any future output can reference is `idx - 1` for
        // the next output's `idx`. Keep at least one sample so the trailing
        // value survives as `s0`/`s1` context for the next chunk.
        let next_idx = self.n_out * self.from_rate as u64 / self.to_rate as u64;
        let target_start = next_idx.saturating_sub(1);
        while self.buf_start < target_start && self.buf.len() > 1 {
            self.buf.pop_front();
            self.buf_start += 1;
        }
    }

    /// Resample `input`, invoking `emit` once per output sample, in order.
    pub(crate) fn process(&mut self, input: &[T], mut emit: impl FnMut(T)) {
        self.buf.extend(input.iter().copied());
        self.n_in += input.len() as u64;
        if self.n_in == 0 {
            return;
        }

        let from = self.from_rate as u64;
        let to = self.to_rate as u64;
        let last = self.n_in - 1;
        let target = self.total_emittable(self.n_in);

        while self.n_out < target {
            let p = self.n_out * from;
            let idx = (p / to) as i64;
            let t = (p % to) as f32 / to as f32;
            let s0 = self.at(idx - 1, last);
            let s1 = self.at(idx, last);
            let s2 = self.at(idx + 1, last);
            let s3 = self.at(idx + 2, last);
            emit(T::catmull(s0, s1, s2, s3, t));
            self.n_out += 1;
        }

        self.prune();
    }
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

    /// Resample a whole signal in one call.
    fn resample_all(from: u32, to: u32, input: &[f32]) -> Vec<f32> {
        let mut r = StreamingResampler::new(from, to);
        let mut out = Vec::new();
        r.process(input, |s| out.push(s));
        out
    }

    /// Resample a signal split into fixed-size chunks.
    fn resample_chunked(from: u32, to: u32, input: &[f32], chunk: usize) -> Vec<f32> {
        let mut r = StreamingResampler::new(from, to);
        let mut out = Vec::new();
        for c in input.chunks(chunk) {
            // Reserve-then-emit mirrors the backend usage.
            let expected = r.pending_output_count(c.len());
            let before = out.len();
            r.process(c, |s| out.push(s));
            assert_eq!(
                out.len() - before,
                expected,
                "pending_output_count matches process"
            );
        }
        out
    }

    #[test]
    fn streaming_passthrough_is_exact() {
        let input: Vec<f32> = (0..64).map(|i| i as f32).collect();
        let out = resample_all(48_000, 48_000, &input);
        assert_eq!(out.len(), input.len());
        for (a, b) in out.iter().zip(input.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn chunked_matches_single_call_on_smooth_signal() {
        // A smooth signal keeps adjacent-sample deltas small, so the trailing
        // s3-clamp at each chunk boundary produces only a sub-sample difference.
        let input: Vec<f32> = (0..500).map(|i| (i as f32 * 0.05).sin() * 0.9).collect();
        let one = resample_all(30_000, 96_000, &input);
        for chunk in [1usize, 3, 7, 32, 128] {
            let many = resample_chunked(30_000, 96_000, &input, chunk);
            assert_eq!(one.len(), many.len(), "chunk={chunk} output length");
            let max_diff = one
                .iter()
                .zip(many.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0_f32, f32::max);
            assert!(
                max_diff < 0.01,
                "chunk={chunk} max_diff={max_diff} exceeds tolerance"
            );
        }
    }

    #[test]
    fn total_output_count_matches_rate_ratio() {
        // Over a long stream, the cumulative output count tracks the exact
        // rate ratio to within one sample.
        let len = 100_000usize;
        let input: Vec<f32> = (0..len).map(|i| (i as f32 * 0.001).sin()).collect();

        for (from, to) in [(30_000u32, 96_000u32), (96_000, 48_000), (30_000, 30_000)] {
            let many = resample_chunked(from, to, &input, 137);
            let ideal = (len as u64 - 1) * to as u64 / from as u64 + 1;
            let diff = (many.len() as i64 - ideal as i64).abs();
            assert!(
                diff <= 1,
                "from={from} to={to}: got {} expected ~{ideal} (diff {diff})",
                many.len()
            );
        }
    }

    #[test]
    fn reset_rephases_stream() {
        let input: Vec<f32> = (0..32).map(|i| i as f32).collect();
        let mut r = StreamingResampler::new(30_000, 96_000);
        let mut first = Vec::new();
        r.process(&input, |s| first.push(s));

        r.reset();
        let mut second = Vec::new();
        r.process(&input, |s| second.push(s));

        assert_eq!(first, second);
    }

    #[test]
    fn set_rates_resets_only_on_change() {
        let mut r = StreamingResampler::new(30_000, 96_000);
        r.process(&[0.0, 1.0, 2.0, 3.0], |_| {});
        let n_in_before = r.n_in;
        r.set_rates(30_000, 96_000); // no change → no reset
        assert_eq!(r.n_in, n_in_before);
        r.set_rates(48_000, 96_000); // change → reset
        assert_eq!(r.n_in, 0);
    }

    #[test]
    fn stereo_pair_resamples_both_channels() {
        let input: Vec<(f32, f32)> = (0..16).map(|i| (i as f32, -(i as f32))).collect();
        let mut r = StreamingResampler::new(48_000, 96_000);
        let mut out = Vec::new();
        r.process(&input, |s| out.push(s));
        assert!(out.len() > input.len());
        // First output is the first input exactly (t=0).
        assert!((out[0].0 - 0.0).abs() < 1e-6);
        assert!((out[0].1 - 0.0).abs() < 1e-6);
        // Right channel is the negation of the left throughout (linear ramp).
        for (l, rr) in &out {
            assert!((l + rr).abs() < 1e-4);
        }
    }
}
