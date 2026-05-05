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

    fn queued_points(&self) -> Option<u64> {
        self.runtime.as_ref().map(|rt| rt.queued_points())
    }
}
