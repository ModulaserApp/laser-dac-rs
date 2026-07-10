//! Oscilloscope streaming backend implementation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig as CpalStreamConfig};
use crossbeam_queue::ArrayQueue;

use crate::resample::{catmull_rom, resampled_len};

use super::OscilloscopeConfig;
use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::{BufferEstimator, QueueDepthSource, RuntimeAuthorityEstimator};
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;

/// Shared state between the producer (backend) and consumer (audio callback).
struct RuntimeState {
    /// Lock-free queue of stereo sample pairs.
    queue: ArrayQueue<(f32, f32)>,
    /// Whether the shutter is open (unmuted).
    muted: AtomicBool,
    /// Whether the audio stream is alive.
    connected: AtomicBool,
}

impl RuntimeState {
    fn new(capacity: usize) -> Self {
        Self {
            queue: ArrayQueue::new(capacity),
            muted: AtomicBool::new(true),
            connected: AtomicBool::new(false),
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
}

impl QueueDepthSource for RuntimeState {
    fn queued_points(&self) -> u64 {
        RuntimeState::queued_points(self)
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
    /// Audio engine seam. Abstracts cpal so the backend can be tested with a
    /// mock engine that has no real audio device.
    engine: Arc<dyn AudioEngine>,
}

impl OscilloscopeBackend {
    /// Create a new oscilloscope backend for the given audio device.
    pub fn new(device_name: String, sample_rate: u32) -> Self {
        Self::with_engine(device_name, sample_rate, Arc::new(CpalAudioEngine))
    }

    fn with_engine(device_name: String, sample_rate: u32, engine: Arc<dyn AudioEngine>) -> Self {
        Self {
            device_name,
            sample_rate,
            config: OscilloscopeConfig::default(),
            caps: super::capabilities(sample_rate),
            runtime: None,
            audio_thread: None,
            sample_buffer: Vec::new(),
            estimator: RuntimeAuthorityEstimator::new(),
            engine,
        }
    }

    #[cfg(test)]
    fn with_engine_for_test(
        device_name: String,
        sample_rate: u32,
        engine: Arc<dyn AudioEngine>,
    ) -> Self {
        Self::with_engine(device_name, sample_rate, engine)
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
        let engine = Arc::clone(&self.engine);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = Arc::clone(&stop_flag);

        let handle = thread::Builder::new()
            .name(format!("oscilloscope-{}", device_name))
            .spawn(move || {
                if let Err(e) = run_audio_thread(
                    &engine,
                    &device_name,
                    sample_rate,
                    &runtime,
                    &stop_flag_clone,
                ) {
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

/// A running audio stream. Dropping it stops audio output.
///
/// The concrete type (a cpal stream in production) stays on the audio thread
/// and is never sent across threads, so no `Send` bound is required.
trait RunningAudioStream {}

/// Seam that abstracts the cpal audio host so the backend can be tested with a
/// mock engine that has no real audio device.
trait AudioEngine: Send + Sync {
    /// Open and start an output stream for `device_name` at `sample_rate`,
    /// wiring the audio callback to drain `runtime`'s queue.
    fn open_stream(
        &self,
        device_name: &str,
        sample_rate: u32,
        runtime: Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>>;
}

struct CpalRunningStream {
    _stream: cpal::Stream,
}

impl RunningAudioStream for CpalRunningStream {}

/// Production [`AudioEngine`] backed by cpal.
struct CpalAudioEngine;

impl AudioEngine for CpalAudioEngine {
    fn open_stream(
        &self,
        device_name: &str,
        sample_rate: u32,
        runtime: Arc<RuntimeState>,
    ) -> Result<Box<dyn RunningAudioStream>> {
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
            .ok_or_else(|| {
                Error::disconnected(format!("Audio device '{}' not found", device_name))
            })?;

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
        let runtime_err = Arc::clone(&runtime);

        let err_callback = move |err: cpal::StreamError| {
            log::error!("Oscilloscope stream error: {}", err);
            if matches!(err, cpal::StreamError::DeviceNotAvailable) {
                runtime_err.connected.store(false, Ordering::Release);
            }
        };

        // Build stream based on sample format
        let stream = match sample_format {
            SampleFormat::F32 => {
                let rt = Arc::clone(&runtime);
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
                let rt = Arc::clone(&runtime);
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

        Ok(Box::new(CpalRunningStream { _stream: stream }))
    }
}

/// Fill an F32 output buffer from the runtime queue.
///
/// Always consumes from the queue even when muted, to prevent the producer
/// from stalling on `WouldBlock`.
fn fill_f32_output(data: &mut [f32], runtime: &RuntimeState) {
    let muted = runtime.muted.load(Ordering::Relaxed);

    for chunk in data.chunks_mut(2) {
        if let Some((l, r)) = runtime.queue.pop() {
            if muted {
                chunk[0] = 0.0;
                chunk[1] = 0.0;
            } else {
                chunk[0] = l;
                chunk[1] = r;
            }
        } else {
            chunk[0] = 0.0;
            chunk[1] = 0.0;
        }
    }
}

/// Fill an I16 output buffer from the runtime queue.
fn fill_i16_output(data: &mut [i16], runtime: &RuntimeState) {
    let muted = runtime.muted.load(Ordering::Relaxed);

    for chunk in data.chunks_mut(2) {
        if let Some((l, r)) = runtime.queue.pop() {
            if muted {
                chunk[0] = 0;
                chunk[1] = 0;
            } else {
                chunk[0] = (l * i16::MAX as f32) as i16;
                chunk[1] = (r * i16::MAX as f32) as i16;
            }
        } else {
            chunk[0] = 0;
            chunk[1] = 0;
        }
    }
}

/// Run the audio output on a dedicated thread.
fn run_audio_thread(
    engine: &Arc<dyn AudioEngine>,
    device_name: &str,
    sample_rate: u32,
    runtime: &Arc<RuntimeState>,
    stop_flag: &AtomicBool,
) -> Result<()> {
    // Open and start the stream via the engine seam.
    let stream = engine.open_stream(device_name, sample_rate, Arc::clone(runtime))?;

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
    drop(stream);
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
        let runtime = Arc::new(RuntimeState::new(self.buffer_capacity()));

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
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        if let Some(runtime) = &self.runtime {
            runtime.muted.store(!open, Ordering::Release);
        }
        Ok(())
    }
}

/// Interpolate two stereo sample pairs using 4-point Catmull-Rom.
fn catmull_rom_samples(
    s0: (f32, f32),
    s1: (f32, f32),
    s2: (f32, f32),
    s3: (f32, f32),
    t: f32,
) -> (f32, f32) {
    (
        catmull_rom(s0.0, s1.0, s2.0, s3.0, t),
        catmull_rom(s0.1, s1.1, s2.1, s3.1, t),
    )
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

        // Check capacity before doing any conversion work.
        let resampling = pps != self.sample_rate;
        if resampling {
            let output_len = resampled_len(points.len(), pps, self.sample_rate);
            if !runtime.has_capacity_for(output_len) {
                return Ok(WriteOutcome::WouldBlock);
            }
        } else if !runtime.has_capacity_for(points.len()) {
            return Ok(WriteOutcome::WouldBlock);
        }

        // Convert points to stereo samples.
        self.sample_buffer.clear();
        self.sample_buffer.extend(
            points
                .iter()
                .map(|p| Self::point_to_samples(p, &self.config)),
        );

        if resampling {
            let output_len = resampled_len(self.sample_buffer.len(), pps, self.sample_rate);
            let last_src_idx = (self.sample_buffer.len() - 1) as f32;
            let step = if output_len > 1 {
                last_src_idx / (output_len - 1) as f32
            } else {
                0.0
            };

            let last = self.sample_buffer.len() - 1;
            for i in 0..output_len {
                let src_pos = i as f32 * step;
                let idx = (src_pos as usize).min(last);
                let t = src_pos - idx as f32;
                let s0 = self.sample_buffer[idx.saturating_sub(1)];
                let s1 = self.sample_buffer[idx];
                let s2 = self.sample_buffer[(idx + 1).min(last)];
                let s3 = self.sample_buffer[(idx + 2).min(last)];
                let _ = runtime.queue.push(catmull_rom_samples(s0, s1, s2, s3, t));
            }
        } else {
            for &sample in &self.sample_buffer {
                let _ = runtime.queue.push(sample);
            }
        }

        Ok(WriteOutcome::Written)
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::Mutex;
    use std::time::Instant;

    const RATE: u32 = 48_000;

    // ----- Pure conversion / channel-mapping tests -----

    #[test]
    fn point_to_samples_maps_x_left_y_right_without_inversion() {
        // Unlike the AVB backend, the oscilloscope does NOT invert polarity:
        // x maps straight to the left channel, y to the right.
        let cfg = OscilloscopeConfig::default();
        let p = LaserPoint::new(0.25, -0.5, 65535, 0, 0, 65535);
        let (l, r) = OscilloscopeBackend::point_to_samples(&p, &cfg);
        assert!((l - 0.25).abs() < 1e-6);
        assert!((r + 0.5).abs() < 1e-6);
    }

    #[test]
    fn point_to_samples_applies_gain_and_dc_offset() {
        let cfg = OscilloscopeConfig {
            gain: 0.5,
            dc_offset: 0.1,
            clip: false,
        };
        let p = LaserPoint::new(0.4, -0.4, 0, 0, 0, 0);
        let (l, r) = OscilloscopeBackend::point_to_samples(&p, &cfg);
        assert!((l - 0.3).abs() < 1e-6); // 0.4 * 0.5 + 0.1
        assert!((r - (-0.1)).abs() < 1e-6); // -0.4 * 0.5 + 0.1
    }

    #[test]
    fn point_to_samples_clips_when_enabled() {
        let cfg = OscilloscopeConfig {
            gain: 4.0,
            dc_offset: 0.0,
            clip: true,
        };
        let p = LaserPoint::new(0.5, -0.5, 0, 0, 0, 0);
        let (l, r) = OscilloscopeBackend::point_to_samples(&p, &cfg);
        assert_eq!(l, 1.0); // 2.0 clamped
        assert_eq!(r, -1.0); // -2.0 clamped
    }

    #[test]
    fn point_to_samples_passes_through_when_clip_disabled() {
        let cfg = OscilloscopeConfig {
            gain: 4.0,
            dc_offset: 0.0,
            clip: false,
        };
        let p = LaserPoint::new(0.5, -0.5, 0, 0, 0, 0);
        let (l, r) = OscilloscopeBackend::point_to_samples(&p, &cfg);
        assert_eq!(l, 2.0);
        assert_eq!(r, -2.0);
    }

    #[test]
    fn point_to_samples_blanked_still_outputs_position() {
        // Blanked points must still track the beam position (no "off" state).
        let cfg = OscilloscopeConfig::default();
        let p = LaserPoint::blanked(0.3, -0.2);
        let (l, r) = OscilloscopeBackend::point_to_samples(&p, &cfg);
        assert!((l - 0.3).abs() < 1e-6);
        assert!((r + 0.2).abs() < 1e-6);
    }

    // ----- Output buffer fill (audio callback) tests -----

    #[test]
    fn fill_f32_output_unmuted_emits_samples_and_drains() {
        let rt = RuntimeState::new(8);
        rt.muted.store(false, Ordering::Release);
        rt.queue.push((0.5, -0.5)).unwrap();
        rt.queue.push((0.25, 0.75)).unwrap();

        let mut data = [9.0f32; 4];
        fill_f32_output(&mut data, &rt);
        assert_eq!(data, [0.5, -0.5, 0.25, 0.75]);
        assert_eq!(rt.queued_points(), 0);
    }

    #[test]
    fn fill_f32_output_muted_emits_zero_but_still_drains() {
        let rt = RuntimeState::new(8); // muted defaults to true
        rt.queue.push((0.5, -0.5)).unwrap();

        let mut data = [9.0f32; 2];
        fill_f32_output(&mut data, &rt);
        assert_eq!(data, [0.0, 0.0]);
        // Drained even while muted, so the producer never stalls on WouldBlock.
        assert_eq!(rt.queued_points(), 0);
    }

    #[test]
    fn fill_f32_output_underrun_emits_zero() {
        let rt = RuntimeState::new(8);
        rt.muted.store(false, Ordering::Release);
        let mut data = [9.0f32; 4];
        fill_f32_output(&mut data, &rt);
        assert!(data.iter().all(|&v| v == 0.0));
    }

    #[test]
    fn fill_i16_output_scales_unmuted() {
        let rt = RuntimeState::new(8);
        rt.muted.store(false, Ordering::Release);
        rt.queue.push((1.0, -1.0)).unwrap();

        let mut data = [123i16; 2];
        fill_i16_output(&mut data, &rt);
        assert_eq!(data[0], i16::MAX);
        assert_eq!(data[1], -i16::MAX);
        assert_eq!(rt.queued_points(), 0);
    }

    #[test]
    fn fill_i16_output_muted_and_underrun_emit_zero() {
        let rt = RuntimeState::new(8); // muted
        rt.queue.push((1.0, -1.0)).unwrap();
        let mut data = [123i16; 4];
        fill_i16_output(&mut data, &rt);
        // First chunk: muted → zero (but drained). Second chunk: underrun → zero.
        assert_eq!(data, [0, 0, 0, 0]);
        assert_eq!(rt.queued_points(), 0);
    }

    // ----- RuntimeState / queue producer tests -----

    #[test]
    fn runtime_state_capacity_accounting() {
        let rt = RuntimeState::new(4);
        assert!(rt.has_capacity_for(0));
        assert!(rt.has_capacity_for(4));
        assert!(!rt.has_capacity_for(5));

        rt.queue.push((0.0, 0.0)).unwrap();
        assert_eq!(rt.queued_points(), 1);
        assert!(rt.has_capacity_for(3));
        assert!(!rt.has_capacity_for(4));

        rt.clear_queue();
        assert_eq!(rt.queued_points(), 0);
        assert!(rt.has_capacity_for(4));
    }

    #[test]
    fn buffer_capacity_scales_with_rate_and_has_floor() {
        assert_eq!(super::super::buffer_capacity(48_000), 4_800);
        // Below the 4096 floor at low rates.
        assert_eq!(super::super::buffer_capacity(10_000), 4_096);
    }

    // ----- Audio engine seam mock -----

    struct FakeRunningStream {
        stop_tx: Option<mpsc::Sender<()>>,
        handle: Option<JoinHandle<()>>,
    }

    impl Drop for FakeRunningStream {
        fn drop(&mut self) {
            if let Some(tx) = self.stop_tx.take() {
                let _ = tx.send(());
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    impl RunningAudioStream for FakeRunningStream {}

    /// Mock [`AudioEngine`] that simulates the audio callback by draining the
    /// queue on a background thread — no real audio device required.
    struct FakeAudioEngine {
        fail_open: AtomicBool,
        paused: Arc<AtomicBool>,
        captured_frames: Arc<Mutex<VecDeque<(f32, f32)>>>,
        frame_count: Arc<AtomicUsize>,
    }

    impl FakeAudioEngine {
        fn new() -> Self {
            Self {
                fail_open: AtomicBool::new(false),
                paused: Arc::new(AtomicBool::new(false)),
                captured_frames: Arc::new(Mutex::new(VecDeque::new())),
                frame_count: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn snapshot(&self) -> Vec<(f32, f32)> {
            self.captured_frames
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect()
        }

        fn frame_count(&self) -> usize {
            self.frame_count.load(Ordering::Acquire)
        }
    }

    impl AudioEngine for FakeAudioEngine {
        fn open_stream(
            &self,
            _device_name: &str,
            _sample_rate: u32,
            runtime: Arc<RuntimeState>,
        ) -> Result<Box<dyn RunningAudioStream>> {
            if self.fail_open.load(Ordering::Acquire) {
                return Err(Error::backend(std::io::Error::other("mock open failure")));
            }

            let (stop_tx, stop_rx) = mpsc::channel();
            let captured_frames = Arc::clone(&self.captured_frames);
            let frame_count = Arc::clone(&self.frame_count);
            let paused = Arc::clone(&self.paused);

            let handle = thread::spawn(move || loop {
                match stop_rx.recv_timeout(Duration::from_millis(2)) {
                    Ok(_) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        if paused.load(Ordering::Acquire) {
                            continue;
                        }
                        // Pull one stereo frame just like the real callback.
                        let mut frame = [0.0f32; 2];
                        fill_f32_output(&mut frame, &runtime);
                        if let Ok(mut captured) = captured_frames.lock() {
                            captured.push_back((frame[0], frame[1]));
                            if captured.len() > 8192 {
                                captured.pop_front();
                            }
                        }
                        frame_count.fetch_add(1, Ordering::Release);
                    }
                }
            });

            Ok(Box::new(FakeRunningStream {
                stop_tx: Some(stop_tx),
                handle: Some(handle),
            }))
        }
    }

    fn wait_for_frame_count(engine: &FakeAudioEngine, target: usize) {
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if engine.frame_count() >= target {
                return;
            }
            thread::sleep(Duration::from_millis(1));
        }
        panic!(
            "timed out waiting for frame count {}, got {}",
            target,
            engine.frame_count()
        );
    }

    fn backend_with(engine: Arc<FakeAudioEngine>) -> OscilloscopeBackend {
        let dyn_engine: Arc<dyn AudioEngine> = engine;
        OscilloscopeBackend::with_engine_for_test("scope".to_string(), RATE, dyn_engine)
    }

    // ----- Lifecycle / engine-driven tests -----

    #[test]
    fn connect_disconnect_lifecycle() {
        let fake = Arc::new(FakeAudioEngine::new());
        let mut backend = backend_with(fake.clone());

        assert!(!backend.is_connected());
        backend.connect().unwrap();
        assert!(backend.is_connected());

        // Idempotent: a second connect is a no-op that stays connected.
        backend.connect().unwrap();
        assert!(backend.is_connected());

        backend.disconnect().unwrap();
        assert!(!backend.is_connected());
    }

    #[test]
    fn write_before_connect_errors() {
        let fake = Arc::new(FakeAudioEngine::new());
        let mut backend = backend_with(fake);
        let err = backend
            .try_write_points(RATE, &[LaserPoint::blanked(0.0, 0.0)])
            .unwrap_err();
        assert!(err.to_string().contains("Not connected"));
    }

    #[test]
    fn run_audio_thread_propagates_open_failure() {
        // Exercises the failure path directly, avoiding the 5s connect timeout.
        let fake = Arc::new(FakeAudioEngine::new());
        fake.fail_open.store(true, Ordering::Release);
        let engine: Arc<dyn AudioEngine> = fake;
        let runtime = Arc::new(RuntimeState::new(16));
        let stop = AtomicBool::new(false);

        let err = run_audio_thread(&engine, "scope", RATE, &runtime, &stop).unwrap_err();
        assert!(err.to_string().contains("mock open failure"));
        assert!(!runtime.connected.load(Ordering::Acquire));
    }

    #[test]
    fn end_to_end_write_drains_to_output_when_unmuted() {
        let fake = Arc::new(FakeAudioEngine::new());
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

        let points = vec![LaserPoint::new(0.5, -0.5, 0, 0, 0, 0); 16];
        assert_eq!(
            backend.try_write_points(RATE, &points).unwrap(),
            WriteOutcome::Written
        );

        wait_for_frame_count(fake.as_ref(), 16);
        let frames = fake.snapshot();
        assert!(frames
            .iter()
            .any(|&(l, r)| (l - 0.5).abs() < 1e-3 && (r + 0.5).abs() < 1e-3));

        backend.disconnect().unwrap();
    }

    #[test]
    fn end_to_end_muted_output_is_silent_but_drains() {
        let fake = Arc::new(FakeAudioEngine::new());
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap(); // shutter closed by default → muted
        let rt = backend.runtime.as_ref().unwrap().clone();
        let points = vec![LaserPoint::new(0.5, -0.5, 0, 0, 0, 0); 16];
        assert_eq!(
            backend.try_write_points(RATE, &points).unwrap(),
            WriteOutcome::Written
        );

        // Even though muted, the callback drains the queue (never stalls).
        let deadline = Instant::now() + Duration::from_millis(500);
        while rt.queued_points() != 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(rt.queued_points(), 0);

        // All output produced while muted is silent.
        let frames = fake.snapshot();
        assert!(frames.iter().all(|&(l, r)| l == 0.0 && r == 0.0));

        backend.disconnect().unwrap();
    }

    #[test]
    fn wouldblock_when_full_then_recovers_after_drain() {
        let fake = Arc::new(FakeAudioEngine::new());
        fake.paused.store(true, Ordering::Release); // callback won't drain yet
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap();
        backend.set_shutter(true).unwrap();

        let cap = super::super::buffer_capacity(RATE);
        let fill = vec![LaserPoint::blanked(0.0, 0.0); cap];
        assert_eq!(
            backend.try_write_points(RATE, &fill).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(
            backend
                .try_write_points(RATE, &[LaserPoint::blanked(0.0, 0.0)])
                .unwrap(),
            WriteOutcome::WouldBlock
        );

        fake.paused.store(false, Ordering::Release);
        thread::sleep(Duration::from_millis(40));

        assert_eq!(
            backend
                .try_write_points(RATE, &[LaserPoint::blanked(0.0, 0.0)])
                .unwrap(),
            WriteOutcome::Written
        );

        backend.disconnect().unwrap();
    }

    #[test]
    fn stop_and_set_shutter_toggle_mute() {
        let fake = Arc::new(FakeAudioEngine::new());
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap();
        let rt = backend.runtime.as_ref().unwrap().clone();

        // Connect leaves shutter closed → muted.
        assert!(rt.muted.load(Ordering::Acquire));

        backend.set_shutter(true).unwrap();
        assert!(!rt.muted.load(Ordering::Acquire));

        backend.stop().unwrap();
        assert!(rt.muted.load(Ordering::Acquire));

        backend.set_shutter(true).unwrap();
        assert!(!rt.muted.load(Ordering::Acquire));

        backend.set_shutter(false).unwrap();
        assert!(rt.muted.load(Ordering::Acquire));

        backend.disconnect().unwrap();
    }

    #[test]
    fn passthrough_when_pps_matches_sample_rate() {
        let fake = Arc::new(FakeAudioEngine::new());
        fake.paused.store(true, Ordering::Release);
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap();
        let rt = backend.runtime.as_ref().unwrap().clone();

        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(0.0, 0.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 0, 0, 0, 0),
        ];
        assert_eq!(
            backend.try_write_points(RATE, &points).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(rt.queued_points(), 3);

        backend.disconnect().unwrap();
    }

    #[test]
    fn resamples_up_to_sample_rate() {
        let fake = Arc::new(FakeAudioEngine::new());
        fake.paused.store(true, Ordering::Release);
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap();
        let rt = backend.runtime.as_ref().unwrap().clone();

        // 2 points at 24kHz → ceil(2 * 48000 / 24000) = 4 output samples.
        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 0, 0, 0, 0),
        ];
        assert_eq!(
            backend.try_write_points(24_000, &points).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(rt.queued_points(), 4);

        // Endpoints preserved, interior interpolated.
        let first = rt.queue.pop().unwrap();
        assert!((first.0 - (-1.0)).abs() < 0.01);
        let _ = rt.queue.pop();
        let _ = rt.queue.pop();
        let last = rt.queue.pop().unwrap();
        assert!((last.0 - 1.0).abs() < 0.01);

        backend.disconnect().unwrap();
    }

    #[test]
    fn resamples_down_to_sample_rate() {
        let fake = Arc::new(FakeAudioEngine::new());
        fake.paused.store(true, Ordering::Release);
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap();
        let rt = backend.runtime.as_ref().unwrap().clone();

        // 4 points at 96kHz → ceil(4 * 48000 / 96000) = 2 output samples.
        let points = vec![
            LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0),
            LaserPoint::new(-0.5, -0.5, 0, 0, 0, 0),
            LaserPoint::new(0.5, 0.5, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 0, 0, 0, 0),
        ];
        assert_eq!(
            backend.try_write_points(96_000, &points).unwrap(),
            WriteOutcome::Written
        );
        assert_eq!(rt.queued_points(), 2);

        backend.disconnect().unwrap();
    }

    #[test]
    fn wouldblock_on_resampled_overflow() {
        let fake = Arc::new(FakeAudioEngine::new());
        fake.paused.store(true, Ordering::Release);
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap();
        let rt = backend.runtime.as_ref().unwrap().clone();

        // Fill to one below capacity, then a 2→4 upsample can't fit.
        let cap = super::super::buffer_capacity(RATE);
        let fill = vec![LaserPoint::blanked(0.0, 0.0); cap - 1];
        assert_eq!(
            backend.try_write_points(RATE, &fill).unwrap(),
            WriteOutcome::Written
        );

        let points = vec![
            LaserPoint::new(0.0, 0.0, 0, 0, 0, 0),
            LaserPoint::new(1.0, 1.0, 0, 0, 0, 0),
        ];
        assert_eq!(
            backend.try_write_points(24_000, &points).unwrap(),
            WriteOutcome::WouldBlock
        );
        // Nothing partially enqueued.
        assert_eq!(rt.queued_points(), cap as u64 - 1);

        backend.disconnect().unwrap();
    }

    #[test]
    fn estimator_reports_queue_depth_and_clears_on_disconnect() {
        let fake = Arc::new(FakeAudioEngine::new());
        fake.paused.store(true, Ordering::Release);
        let mut backend = backend_with(fake.clone());

        let now = Instant::now();
        // Before connect: estimator has no source → zero.
        assert_eq!(backend.estimator().estimated_fullness(now, RATE), 0);

        backend.connect().unwrap();
        let points = vec![LaserPoint::blanked(0.0, 0.0); 10];
        backend.try_write_points(RATE, &points).unwrap();
        assert_eq!(backend.estimator().estimated_fullness(now, RATE), 10);

        backend.disconnect().unwrap();
        // Source cleared on disconnect → back to zero.
        assert_eq!(backend.estimator().estimated_fullness(now, RATE), 0);
    }

    #[test]
    fn disconnect_clears_queue() {
        let fake = Arc::new(FakeAudioEngine::new());
        fake.paused.store(true, Ordering::Release);
        let mut backend = backend_with(fake.clone());

        backend.connect().unwrap();
        let rt = backend.runtime.as_ref().unwrap().clone();
        backend
            .try_write_points(RATE, &vec![LaserPoint::blanked(0.0, 0.0); 20])
            .unwrap();
        assert_eq!(rt.queued_points(), 20);

        backend.disconnect().unwrap();
        assert_eq!(rt.queued_points(), 0);
    }
}
