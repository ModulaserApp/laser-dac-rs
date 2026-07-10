//! Oscilloscope streaming backend implementation.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig as CpalStreamConfig};
use crossbeam_queue::ArrayQueue;

use crate::resample::StreamingResampler;

use super::OscilloscopeConfig;
use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::{BufferEstimator, QueueDepthSource, RuntimeAuthorityEstimator};
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;

/// Approximate time constant for the mute ramp toward screen centre.
const MUTE_RAMP_MS: f32 = 3.0;

/// Shared state between the producer (backend) and consumer (audio callback).
struct RuntimeState {
    /// Lock-free queue of stereo sample pairs.
    queue: ArrayQueue<(f32, f32)>,
    /// Whether the shutter is open (unmuted).
    muted: AtomicBool,
    /// Whether the audio stream is alive.
    connected: AtomicBool,
    /// Device output sample rate in Hz (used to convert queue depth to
    /// pps-points and to size the mute ramp).
    sample_rate: u32,
    /// Last emitted output sample, as f32 bits. Held on underrun so the beam
    /// stays put instead of snapping to the screen centre, and ramped toward
    /// centre while muted. Written only by the audio callback.
    last_l_bits: AtomicU32,
    last_r_bits: AtomicU32,
}

impl RuntimeState {
    fn new(capacity: usize, sample_rate: u32) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
            muted: AtomicBool::new(true),
            connected: AtomicBool::new(false),
            sample_rate,
            last_l_bits: AtomicU32::new(0.0f32.to_bits()),
            last_r_bits: AtomicU32::new(0.0f32.to_bits()),
        }
    }

    fn remaining_capacity(&self) -> usize {
        self.queue.capacity().saturating_sub(self.queue.len())
    }

    fn has_capacity_for(&self, count: usize) -> bool {
        count == 0 || self.remaining_capacity() >= count
    }

    fn queued_points(&self) -> u64 {
        self.queue.len() as u64
    }

    fn clear_queue(&self) {
        while self.queue.pop().is_some() {}
    }

    fn last_output(&self) -> (f32, f32) {
        (
            f32::from_bits(self.last_l_bits.load(Ordering::Relaxed)),
            f32::from_bits(self.last_r_bits.load(Ordering::Relaxed)),
        )
    }

    fn set_last_output(&self, l: f32, r: f32) {
        self.last_l_bits.store(l.to_bits(), Ordering::Relaxed);
        self.last_r_bits.store(r.to_bits(), Ordering::Relaxed);
    }

    /// Compute the next output sample, advancing the held/ramped state.
    ///
    /// - Unmuted with a queued sample: pass it through.
    /// - Unmuted underrun (empty queue): hold the last output — no centre spike.
    /// - Muted: ramp the held value toward screen centre (0,0) over a few ms,
    ///   so muting glides rather than stepping. The queue is still drained so
    ///   the producer never stalls on `WouldBlock`.
    fn next_output(&self) -> (f32, f32) {
        let muted = self.muted.load(Ordering::Relaxed);
        let queued = self.queue.pop();
        let (last_l, last_r) = self.last_output();

        let (l, r) = if muted {
            let k = (1000.0 / (MUTE_RAMP_MS * self.sample_rate as f32)).clamp(0.0, 1.0);
            (last_l + (0.0 - last_l) * k, last_r + (0.0 - last_r) * k)
        } else {
            match queued {
                Some((l, r)) => (l, r),
                None => (last_l, last_r),
            }
        };

        self.set_last_output(l, r);
        (l, r)
    }
}

impl QueueDepthSource for RuntimeState {
    fn queued_points(&self) -> u64 {
        RuntimeState::queued_points(self)
    }
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

/// Handle to the audio thread.
struct AudioThread {
    handle: JoinHandle<()>,
    stop_flag: Arc<AtomicBool>,
}

/// Oscilloscope XY mode backend via audio output.
///
/// Maps `LaserPoint.x` → Left channel, `LaserPoint.y` → Right channel.
pub struct OscilloscopeBackend {
    /// Device name for connection
    device_name: String,
    /// Sample rate (also used as PPS)
    sample_rate: u32,
    /// Audio configuration
    config: OscilloscopeConfig,
    /// Device capabilities
    caps: DacCapabilities,
    /// Shared runtime state (created fresh on each connect)
    runtime: Option<Arc<RuntimeState>>,
    /// Audio thread handle
    audio_thread: Option<AudioThread>,
    /// Pre-allocated conversion buffer (avoids per-write heap allocation).
    sample_buffer: Vec<(f32, f32)>,
    /// Runtime-authoritative buffer estimator. Source is set on connect and
    /// cleared on disconnect; reports zero in between.
    estimator: RuntimeAuthorityEstimator,
    /// Stateful PPS→sample-rate resampler carrying phase across write chunks.
    /// Reset on connect/stop/disconnect; re-phased on PPS change.
    resampler: StreamingResampler<(f32, f32)>,
}

impl OscilloscopeBackend {
    /// Create a new oscilloscope backend for the given audio device.
    pub fn new(device_name: String, sample_rate: u32) -> Self {
        Self {
            device_name,
            sample_rate,
            config: OscilloscopeConfig::default(),
            caps: super::capabilities(sample_rate),
            runtime: None,
            audio_thread: None,
            sample_buffer: Vec::new(),
            estimator: RuntimeAuthorityEstimator::new(),
            resampler: StreamingResampler::new(1, 1),
        }
    }

    /// Set the oscilloscope output configuration.
    pub fn set_config(&mut self, config: OscilloscopeConfig) {
        self.config = config;
    }

    /// Convert a laser point to stereo audio samples.
    ///
    /// Unlike laser DACs, the oscilloscope beam is always visible — there is
    /// no "laser off" state.  Blanked points therefore still output the XY
    /// position so the beam tracks correctly; sending silence (0,0) would
    /// make it spike to the screen centre.
    fn point_to_samples(p: &LaserPoint, config: &OscilloscopeConfig) -> (f32, f32) {
        let mut l = p.x * config.gain + config.dc_offset;
        let mut r = p.y * config.gain + config.dc_offset;

        if config.clip {
            l = l.clamp(-1.0, 1.0);
            r = r.clamp(-1.0, 1.0);
        }

        (l, r)
    }

    /// Ring buffer capacity: ~100ms of audio at the sample rate.
    fn buffer_capacity(&self) -> usize {
        super::buffer_capacity(self.sample_rate)
    }

    /// Start the audio thread.
    fn start_audio_thread(&self, runtime: &Arc<RuntimeState>) -> Result<AudioThread> {
        let device_name = self.device_name.clone();
        let sample_rate = self.sample_rate;
        let runtime = Arc::clone(runtime);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = Arc::clone(&stop_flag);

        let handle = thread::Builder::new()
            .name(format!("oscilloscope-{}", device_name))
            .spawn(move || {
                if let Err(e) =
                    run_audio_thread(&device_name, sample_rate, &runtime, &stop_flag_clone)
                {
                    log::error!("Oscilloscope audio thread error: {}", e);
                }
                // Always mark disconnected when thread exits, regardless of reason
                runtime.connected.store(false, Ordering::Release);
            })
            .map_err(|e| {
                Error::backend(std::io::Error::other(format!(
                    "Failed to spawn oscilloscope thread: {}",
                    e
                )))
            })?;

        Ok(AudioThread { handle, stop_flag })
    }
}

/// Fill an F32 output buffer from the runtime queue.
///
/// Always consumes from the queue even when muted, to prevent the producer
/// from stalling on `WouldBlock`. Underrun holds the last position and mute
/// ramps toward centre (see [`RuntimeState::next_output`]).
fn fill_f32_output(data: &mut [f32], runtime: &RuntimeState) {
    for chunk in data.chunks_mut(2) {
        let (l, r) = runtime.next_output();
        chunk[0] = l;
        chunk[1] = r;
    }
}

/// Fill an I16 output buffer from the runtime queue.
fn fill_i16_output(data: &mut [i16], runtime: &RuntimeState) {
    for chunk in data.chunks_mut(2) {
        let (l, r) = runtime.next_output();
        chunk[0] = (l * i16::MAX as f32) as i16;
        chunk[1] = (r * i16::MAX as f32) as i16;
    }
}

/// Run the audio output on a dedicated thread.
fn run_audio_thread(
    device_name: &str,
    sample_rate: u32,
    runtime: &Arc<RuntimeState>,
    stop_flag: &AtomicBool,
) -> Result<()> {
    let host = cpal::default_host();

    // Find the device by name
    let device = host
        .output_devices()
        .map_err(|e| {
            Error::backend(std::io::Error::other(format!(
                "Failed to enumerate devices: {}",
                e
            )))
        })?
        .find(|d| d.name().map(|n| n == device_name).unwrap_or(false))
        .ok_or_else(|| Error::disconnected(format!("Audio device '{}' not found", device_name)))?;

    // Get supported config
    let supported_config = device
        .supported_output_configs()
        .map_err(|e| {
            Error::backend(std::io::Error::other(format!(
                "Failed to get audio configs: {}",
                e
            )))
        })?
        .find(|c| {
            c.channels() == 2
                && c.min_sample_rate().0 <= sample_rate
                && c.max_sample_rate().0 >= sample_rate
        })
        .ok_or_else(|| {
            Error::invalid_config(format!(
                "Audio device doesn't support stereo output at {} Hz",
                sample_rate
            ))
        })?;

    let sample_format = supported_config.sample_format();
    let config = CpalStreamConfig {
        channels: 2,
        sample_rate: cpal::SampleRate(sample_rate),
        buffer_size: cpal::BufferSize::Default,
    };

    // Clone runtime for the error callback so it can mark disconnected on fatal errors
    let runtime_err = Arc::clone(runtime);

    let err_callback = move |err: cpal::StreamError| {
        log::error!("Oscilloscope stream error: {}", err);
        if matches!(err, cpal::StreamError::DeviceNotAvailable) {
            runtime_err.connected.store(false, Ordering::Release);
        }
    };

    // Build stream based on sample format
    let stream = match sample_format {
        SampleFormat::F32 => {
            let rt = Arc::clone(runtime);
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _| fill_f32_output(data, &rt),
                    err_callback,
                    None,
                )
                .map_err(|e| {
                    Error::backend(std::io::Error::other(format!(
                        "Failed to build audio stream: {}",
                        e
                    )))
                })?
        }
        SampleFormat::I16 => {
            let rt = Arc::clone(runtime);
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [i16], _| fill_i16_output(data, &rt),
                    err_callback,
                    None,
                )
                .map_err(|e| {
                    Error::backend(std::io::Error::other(format!(
                        "Failed to build audio stream: {}",
                        e
                    )))
                })?
        }
        format => {
            return Err(Error::invalid_config(format!(
                "Unsupported audio sample format: {:?}",
                format
            )));
        }
    };

    // Start the stream
    stream.play().map_err(|e| {
        Error::backend(std::io::Error::other(format!(
            "Failed to start audio stream: {}",
            e
        )))
    })?;

    // Mark as connected
    runtime.connected.store(true, Ordering::Release);

    log::info!(
        "Oscilloscope started for '{}' at {} Hz",
        device_name,
        sample_rate
    );

    // Keep the stream alive until stop is requested
    while !stop_flag.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(10));
    }

    // Stream is dropped here, stopping audio output
    log::info!("Oscilloscope stopped for '{}'", device_name);

    Ok(())
}

impl DacBackend for OscilloscopeBackend {
    fn dac_type(&self) -> DacType {
        DacType::Oscilloscope
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        if self.is_connected() {
            return Ok(());
        }

        // Create fresh runtime state for this connection (no stale data)
        let runtime = Arc::new(RuntimeState::new(self.buffer_capacity(), self.sample_rate));

        // A fresh stream starts a fresh resample phase.
        self.resampler.reset();

        let audio_thread = self.start_audio_thread(&runtime)?;

        // Wait for the thread to connect (with timeout)
        let start = std::time::Instant::now();
        while !runtime.connected.load(Ordering::Acquire) {
            if start.elapsed() > Duration::from_secs(5) {
                // Stop the thread
                audio_thread.stop_flag.store(true, Ordering::Release);
                let _ = audio_thread.handle.join();
                return Err(Error::backend(std::io::Error::other(
                    "Timeout waiting for oscilloscope connection",
                )));
            }
            thread::sleep(Duration::from_millis(10));
        }

        self.estimator
            .set_source(Arc::clone(&runtime) as Arc<dyn QueueDepthSource>);
        self.runtime = Some(runtime);
        self.audio_thread = Some(audio_thread);

        log::info!(
            "Connected to oscilloscope '{}' at {} Hz",
            self.device_name,
            self.sample_rate
        );

        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(audio_thread) = self.audio_thread.take() {
            audio_thread.stop_flag.store(true, Ordering::Release);
            let _ = audio_thread.handle.join();
        }

        if let Some(runtime) = self.runtime.take() {
            runtime.clear_queue();
        }
        self.estimator.clear_source();
        self.resampler.reset();

        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.runtime
            .as_ref()
            .is_some_and(|rt| rt.connected.load(Ordering::Relaxed))
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(runtime) = &self.runtime {
            runtime.muted.store(true, Ordering::Release);
        }
        // Drop the carried resample phase so output resumes cleanly on re-arm.
        self.resampler.reset();
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        if let Some(runtime) = &self.runtime {
            runtime.muted.store(!open, Ordering::Release);
        }
        Ok(())
    }
}

impl FifoBackend for OscilloscopeBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let runtime = self
            .runtime
            .as_ref()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        if !runtime.connected.load(Ordering::Acquire) {
            return Err(Error::disconnected("Not connected"));
        }

        if points.is_empty() {
            return Ok(WriteOutcome::Written);
        }

        // Keep the resampler phased to the current PPS (a PPS change re-phases).
        self.resampler.set_rates(pps.max(1), self.sample_rate);

        // Reserve queue capacity for exactly what the resampler will emit for
        // this chunk, given its carried phase.
        let output_len = self.resampler.pending_output_count(points.len());
        if !runtime.has_capacity_for(output_len) {
            return Ok(WriteOutcome::WouldBlock);
        }

        // Convert points to stereo samples, then resample with carried phase.
        self.sample_buffer.clear();
        self.sample_buffer.extend(
            points
                .iter()
                .map(|p| Self::point_to_samples(p, &self.config)),
        );
        let samples = std::mem::take(&mut self.sample_buffer);
        self.resampler.process(&samples, |s| {
            let _ = runtime.queue.push(s);
        });
        self.sample_buffer = samples;

        Ok(WriteOutcome::Written)
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer_estimate::RuntimeAuthorityEstimator;
    use std::time::Instant;

    #[test]
    fn point_to_samples_maps_xy_with_gain_and_offset() {
        let config = OscilloscopeConfig {
            gain: 0.5,
            dc_offset: 0.1,
            clip: true,
        };
        let p = LaserPoint::new(1.0, -1.0, 0, 0, 0, 0);
        let (l, r) = OscilloscopeBackend::point_to_samples(&p, &config);
        assert!((l - 0.6).abs() < 1e-6);
        assert!((r - (-0.4)).abs() < 1e-6);
    }

    #[test]
    fn estimator_reports_pps_points_not_raw_samples() {
        // 600 device samples at 96kHz is ~187 pps-points at 30kpps.
        let runtime = Arc::new(RuntimeState::new(4096, 96_000));
        for _ in 0..600 {
            let _ = runtime.queue.push((0.0, 0.0));
        }
        let est = RuntimeAuthorityEstimator::with_source(
            Arc::clone(&runtime) as Arc<dyn QueueDepthSource>
        );
        assert_eq!(est.estimated_fullness(Instant::now(), 30_000), 187);
    }

    #[test]
    fn underrun_holds_last_position_not_center() {
        let runtime = RuntimeState::new(16, 48_000);
        runtime.muted.store(false, Ordering::Release);
        // Drain a real sample so `last_output` is set, then underrun.
        let _ = runtime.queue.push((0.7, -0.3));
        assert_eq!(runtime.next_output(), (0.7, -0.3));
        // Queue now empty: the beam holds its last position (no centre spike).
        assert_eq!(runtime.next_output(), (0.7, -0.3));
        assert_eq!(runtime.next_output(), (0.7, -0.3));
    }

    #[test]
    fn fill_f32_output_holds_last_on_underrun() {
        let runtime = RuntimeState::new(16, 48_000);
        runtime.muted.store(false, Ordering::Release);
        let _ = runtime.queue.push((0.4, 0.6));

        let mut data = vec![0.0f32; 6]; // 3 stereo frames, only 1 queued
        fill_f32_output(&mut data, &runtime);
        assert!((data[0] - 0.4).abs() < 1e-6 && (data[1] - 0.6).abs() < 1e-6);
        // Underrun frames hold the last position rather than snapping to 0.
        assert!((data[2] - 0.4).abs() < 1e-6 && (data[3] - 0.6).abs() < 1e-6);
        assert!((data[4] - 0.4).abs() < 1e-6 && (data[5] - 0.6).abs() < 1e-6);
    }

    #[test]
    fn mute_ramps_toward_center_monotonically() {
        let runtime = RuntimeState::new(16, 48_000);
        runtime.muted.store(false, Ordering::Release);
        let _ = runtime.queue.push((1.0, -1.0));
        assert_eq!(runtime.next_output(), (1.0, -1.0));

        // Now mute: output should glide toward centre, not step to it.
        runtime.muted.store(true, Ordering::Release);
        let (l1, r1) = runtime.next_output();
        assert!(
            l1 < 1.0 && l1 > 0.0,
            "left steps down but not to centre: {l1}"
        );
        assert!(
            r1 > -1.0 && r1 < 0.0,
            "right steps up but not to centre: {r1}"
        );

        let (l2, r2) = runtime.next_output();
        assert!(l2 < l1 && l2 > 0.0);
        assert!(r2 > r1 && r2 < 0.0);

        // After enough samples it settles near centre.
        for _ in 0..4000 {
            let _ = runtime.next_output();
        }
        let (l_end, r_end) = runtime.next_output();
        assert!(l_end.abs() < 0.01 && r_end.abs() < 0.01);
    }

    #[test]
    fn muted_still_drains_queue() {
        // Muting must keep consuming the queue so the producer never stalls.
        let runtime = RuntimeState::new(16, 48_000);
        runtime.muted.store(true, Ordering::Release);
        for _ in 0..8 {
            let _ = runtime.queue.push((0.5, 0.5));
        }
        assert_eq!(runtime.queued_points(), 8);
        let mut data = vec![0.0f32; 16]; // 8 stereo frames
        fill_f32_output(&mut data, &runtime);
        assert_eq!(runtime.queued_points(), 0);
    }
}
