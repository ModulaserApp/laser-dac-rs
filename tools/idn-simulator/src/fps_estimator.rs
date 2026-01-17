//! FFT-based frame rate estimation.
//!
//! Analyzes the incoming point stream to detect repeating patterns and estimate
//! the frame rate (cycle rate) of the content being displayed.

use rustfft::{num_complex::Complex, FftPlanner};
use std::collections::VecDeque;
use std::time::Instant;

/// Buffer size for FFT analysis (must be power of 2).
const FFT_SIZE: usize = 8192;

/// Minimum points needed before we attempt analysis.
const MIN_POINTS_FOR_ANALYSIS: usize = FFT_SIZE;

/// How often to recalculate the FPS estimate (seconds).
const UPDATE_INTERVAL_SECS: f64 = 1.0;

/// Threshold multiplier for detecting significant peaks above noise floor.
const PEAK_THRESHOLD_MULTIPLIER: f32 = 3.0;

/// Minimum frequency to consider (Hz) - filters out very slow drift.
/// Set to 10 Hz to avoid picking up DC offset and slow position drift.
const MIN_FREQUENCY_HZ: f32 = 10.0;

/// Maximum frequency to consider (Hz) - above this is unlikely to be a frame rate.
const MAX_FREQUENCY_HZ: f32 = 500.0;

/// Estimates frame rate by detecting periodicity in the point stream.
pub struct FpsEstimator {
    /// Rolling buffer of X positions.
    x_buffer: VecDeque<f32>,
    /// Rolling buffer of Y positions.
    y_buffer: VecDeque<f32>,
    /// Current sample rate (points per second).
    sample_rate: f32,
    /// Cached FPS estimate.
    last_estimate: Option<f32>,
    /// When we last computed the estimate.
    last_update: Instant,
    /// FFT planner (reused for efficiency).
    fft_planner: FftPlanner<f32>,
}

impl FpsEstimator {
    pub fn new() -> Self {
        Self {
            x_buffer: VecDeque::with_capacity(FFT_SIZE + 1024),
            y_buffer: VecDeque::with_capacity(FFT_SIZE + 1024),
            sample_rate: 30000.0, // Default, will be updated
            last_estimate: None,
            last_update: Instant::now(),
            fft_planner: FftPlanner::new(),
        }
    }

    /// Feed new points into the estimator.
    ///
    /// Call this for each chunk of points received.
    pub fn push_points(&mut self, points: &[(f32, f32)], pps: u32) {
        self.sample_rate = pps as f32;

        for &(x, y) in points {
            self.x_buffer.push_back(x);
            self.y_buffer.push_back(y);
        }

        // Keep buffer at reasonable size (FFT_SIZE + some margin)
        let max_size = FFT_SIZE + 2048;
        while self.x_buffer.len() > max_size {
            self.x_buffer.pop_front();
            self.y_buffer.pop_front();
        }
    }

    /// Get the current FPS estimate.
    ///
    /// Returns `None` if not enough data or no periodicity detected.
    /// The estimate is cached and only recomputed periodically.
    pub fn get_fps(&mut self) -> Option<f32> {
        // Check if we should recompute
        let elapsed = self.last_update.elapsed().as_secs_f64();
        if elapsed >= UPDATE_INTERVAL_SECS {
            self.last_estimate = self.compute_fps();
            self.last_update = Instant::now();
        }

        self.last_estimate
    }

    /// Reset the estimator (e.g., on client disconnect).
    pub fn reset(&mut self) {
        self.x_buffer.clear();
        self.y_buffer.clear();
        self.last_estimate = None;
        self.last_update = Instant::now();
    }

    /// Compute the FPS estimate using FFT.
    fn compute_fps(&mut self) -> Option<f32> {
        if self.x_buffer.len() < MIN_POINTS_FOR_ANALYSIS {
            return None;
        }

        // Extract the most recent FFT_SIZE points
        let start_idx = self.x_buffer.len().saturating_sub(FFT_SIZE);
        let x_slice: Vec<f32> = self.x_buffer.iter().skip(start_idx).copied().collect();
        let y_slice: Vec<f32> = self.y_buffer.iter().skip(start_idx).copied().collect();

        // Compute FFT for both X and Y
        let mag_x = self.compute_magnitude_spectrum(&x_slice);
        let mag_y = self.compute_magnitude_spectrum(&y_slice);

        // Combine magnitudes (use sum of squares for combined power)
        let combined: Vec<f32> = mag_x
            .iter()
            .zip(mag_y.iter())
            .map(|(x, y)| (x * x + y * y).sqrt())
            .collect();

        // Find the fundamental frequency
        self.find_fundamental_frequency(&combined)
    }

    /// Compute the magnitude spectrum of a signal using FFT.
    fn compute_magnitude_spectrum(&mut self, signal: &[f32]) -> Vec<f32> {
        let n = signal.len();
        let fft = self.fft_planner.plan_fft_forward(n);

        // Apply Hanning window and convert to complex
        let mut buffer: Vec<Complex<f32>> = signal
            .iter()
            .enumerate()
            .map(|(i, &x)| {
                let window = 0.5 * (1.0 - (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos());
                Complex::new(x * window, 0.0)
            })
            .collect();

        // Perform FFT
        fft.process(&mut buffer);

        // Compute magnitude (only need first half due to symmetry)
        buffer.iter().take(n / 2).map(|c| c.norm()).collect()
    }

    /// Find the fundamental frequency from the combined magnitude spectrum.
    fn find_fundamental_frequency(&self, magnitudes: &[f32]) -> Option<f32> {
        if magnitudes.is_empty() {
            return None;
        }

        // Calculate frequency resolution
        let freq_resolution = self.sample_rate / FFT_SIZE as f32;

        // Convert frequency limits to bin indices
        let min_bin = (MIN_FREQUENCY_HZ / freq_resolution).ceil() as usize;
        let max_bin = ((MAX_FREQUENCY_HZ / freq_resolution).floor() as usize).min(magnitudes.len());

        if min_bin >= max_bin {
            return None;
        }

        // Calculate noise floor (median of magnitudes in range)
        let mut sorted_mags: Vec<f32> = magnitudes[min_bin..max_bin].to_vec();
        sorted_mags.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let noise_floor = sorted_mags[sorted_mags.len() / 2];

        let threshold = noise_floor * PEAK_THRESHOLD_MULTIPLIER;

        // Find top 5 peaks for debugging
        let mut peaks: Vec<(usize, f32)> = Vec::new();
        for bin in min_bin..max_bin {
            let is_peak = (bin == min_bin || magnitudes[bin] > magnitudes[bin - 1])
                && (bin == max_bin - 1 || magnitudes[bin] > magnitudes[bin + 1]);
            if is_peak && magnitudes[bin] > threshold {
                peaks.push((bin, magnitudes[bin]));
            }
        }
        peaks.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        peaks.truncate(5);

        // Strategy: Use the STRONGEST peak as primary selection.
        // The strongest peak is most likely to be the actual frame rate.
        // Then check for sub-harmonics in case strongest is 2x/3x the fundamental.
        if peaks.is_empty() {
            return None;
        }

        let (strongest_bin, strongest_mag) = peaks[0];

        // Check for sub-harmonics: if the strongest peak is at frequency F,
        // check if there's a significant peak at F/2, F/3, etc.
        // This catches cases where harmonics are stronger than fundamental.
        for divisor in [2, 3, 4] {
            let candidate_bin = strongest_bin / divisor;
            if candidate_bin >= min_bin {
                // Look for a peak near the candidate bin (within 1 bin tolerance)
                for check_bin in
                    candidate_bin.saturating_sub(1)..=(candidate_bin + 1).min(max_bin - 1)
                {
                    if let Some(&(_, sub_mag)) = peaks.iter().find(|(b, _)| *b == check_bin) {
                        // Sub-harmonic should be at least 10% as strong as the main peak
                        if sub_mag > strongest_mag * 0.1 {
                            let frequency = check_bin as f32 * freq_resolution;
                            return Some(frequency);
                        }
                    }
                }
            }
        }

        // No sub-harmonic found, use the strongest peak
        let frequency = strongest_bin as f32 * freq_resolution;
        Some(frequency)
    }
}

impl Default for FpsEstimator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_buffer_returns_none() {
        let mut estimator = FpsEstimator::new();
        assert!(estimator.get_fps().is_none());
    }

    #[test]
    fn test_insufficient_data_returns_none() {
        let mut estimator = FpsEstimator::new();
        // Add fewer points than needed
        let points: Vec<(f32, f32)> = (0..100).map(|i| (i as f32 / 100.0, 0.0)).collect();
        estimator.push_points(&points, 30000);
        assert!(estimator.get_fps().is_none());
    }

    #[test]
    fn test_detects_simple_sine_wave() {
        let mut estimator = FpsEstimator::new();

        // Generate a 50 Hz sine wave at 30k samples/sec
        // Period = 30000 / 50 = 600 samples
        let frequency = 50.0;
        let sample_rate = 30000.0;
        let points: Vec<(f32, f32)> = (0..FFT_SIZE)
            .map(|i| {
                let t = i as f32 / sample_rate;
                let x = (2.0 * std::f32::consts::PI * frequency * t).sin();
                let y = (2.0 * std::f32::consts::PI * frequency * t).cos();
                (x, y)
            })
            .collect();

        estimator.push_points(&points, sample_rate as u32);

        // Force computation
        estimator.last_update = Instant::now() - std::time::Duration::from_secs(2);
        let fps = estimator.get_fps();

        assert!(fps.is_some(), "Should detect periodicity");
        let detected = fps.unwrap();
        // Allow some tolerance due to FFT bin resolution
        assert!(
            (detected - frequency).abs() < 5.0,
            "Expected ~{} Hz, got {} Hz",
            frequency,
            detected
        );
    }
}
