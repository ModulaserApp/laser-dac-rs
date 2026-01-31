//! Audio output streaming backend implementation.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig as CpalStreamConfig};

use super::AudioOutConfig;
use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::types::{DacCapabilities, DacType, LaserPoint};

/// Ring buffer for lock-free audio sample transfer.
///
/// Uses a single-producer, single-consumer design with monotonically-increasing
/// atomic counters. The counters never wrap at the capacity boundary — they
/// increment forever (wrapping only at `usize::MAX`). Buffer indices are
/// computed with `% capacity` only when accessing the underlying storage.
///
/// This avoids the error-prone `capacity + 1` modular arithmetic and makes
/// `len()` trivially correct: `write_pos - read_pos`.
struct RingBuffer {
    /// Interleaved stereo samples (L, R, L, R, ...)
    /// Wrapped in `UnsafeCell` because the producer writes and the consumer
    /// reads from non-overlapping regions concurrently.
    buffer: UnsafeCell<Box<[f32]>>,
    /// Write position (producer) — monotonically increasing
    write_pos: AtomicUsize,
    /// Read position (consumer) — monotonically increasing
    read_pos: AtomicUsize,
    /// Capacity in stereo sample pairs
    capacity: usize,
}

impl RingBuffer {
    fn new(capacity_samples: usize) -> Self {
        // Capacity in stereo pairs, buffer holds interleaved samples
        let buffer_size = capacity_samples * 2;
        Self {
            buffer: UnsafeCell::new(vec![0.0; buffer_size].into_boxed_slice()),
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
            capacity: capacity_samples,
        }
    }

    /// Returns number of sample pairs available to read.
    fn len(&self) -> usize {
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);
        write.wrapping_sub(read)
    }

    /// Returns available space for writing (in sample pairs).
    fn available(&self) -> usize {
        self.capacity - self.len()
    }

    /// Try to write samples to the buffer.
    /// Returns number of sample pairs actually written.
    fn write(&self, samples: &[(f32, f32)]) -> usize {
        let available = self.available();
        let to_write = samples.len().min(available);

        if to_write == 0 {
            return 0;
        }

        let write_pos = self.write_pos.load(Ordering::Relaxed);

        // SAFETY: Single-producer guarantee — we're the only writer.
        // Producer and consumer access non-overlapping regions of the buffer,
        // guaranteed by the atomic position tracking.
        let buffer = unsafe { &mut *self.buffer.get() };

        for (i, (l, r)) in samples[..to_write].iter().enumerate() {
            let idx = ((write_pos + i) % self.capacity) * 2;
            buffer[idx] = *l;
            buffer[idx + 1] = *r;
        }

        self.write_pos
            .store(write_pos.wrapping_add(to_write), Ordering::Release);

        to_write
    }

    /// Read samples from the buffer into the output slice.
    /// Returns number of sample pairs actually read.
    fn read(&self, output: &mut [(f32, f32)]) -> usize {
        let available = self.len();
        let to_read = output.len().min(available);

        if to_read == 0 {
            return 0;
        }

        let read_pos = self.read_pos.load(Ordering::Relaxed);

        // SAFETY: Single-consumer guarantee — we're the only reader.
        // Producer and consumer access non-overlapping regions.
        let buffer = unsafe { &*self.buffer.get() };

        for (i, (l, r)) in output[..to_read].iter_mut().enumerate() {
            let idx = ((read_pos + i) % self.capacity) * 2;
            *l = buffer[idx];
            *r = buffer[idx + 1];
        }

        self.read_pos
            .store(read_pos.wrapping_add(to_read), Ordering::Release);

        to_read
    }

    /// Read samples directly into an interleaved output buffer.
    fn read_interleaved(&self, output: &mut [f32]) -> usize {
        let pairs_requested = output.len() / 2;
        let available = self.len();
        let to_read = pairs_requested.min(available);

        if to_read == 0 {
            return 0;
        }

        let read_pos = self.read_pos.load(Ordering::Relaxed);

        // SAFETY: Single-consumer guarantee — we're the only reader.
        // Producer and consumer access non-overlapping regions.
        let buffer = unsafe { &*self.buffer.get() };

        for i in 0..to_read {
            let src_idx = ((read_pos + i) % self.capacity) * 2;
            let dst_idx = i * 2;
            output[dst_idx] = buffer[src_idx];
            output[dst_idx + 1] = buffer[src_idx + 1];
        }

        self.read_pos
            .store(read_pos.wrapping_add(to_read), Ordering::Release);

        to_read
    }
}

// SAFETY: RingBuffer uses UnsafeCell for interior mutability with atomic
// position tracking ensuring producer and consumer access non-overlapping regions.
unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

/// Handle to the audio thread.
struct AudioThread {
    handle: JoinHandle<()>,
    stop_flag: Arc<AtomicBool>,
}

/// Audio output backend for oscilloscope XY mode.
///
/// Maps `LaserPoint.x` → Left channel, `LaserPoint.y` → Right channel.
pub struct AudioOutBackend {
    /// Device name for connection
    device_name: String,
    /// Sample rate (also used as PPS)
    sample_rate: u32,
    /// Audio configuration
    config: AudioOutConfig,
    /// Device capabilities
    caps: DacCapabilities,
    /// Ring buffer shared with audio callback
    ring_buffer: Arc<RingBuffer>,
    /// Whether we're connected
    connected: Arc<AtomicBool>,
    /// Mute flag for shutter control
    muted: Arc<AtomicBool>,
    /// Audio thread handle
    audio_thread: Mutex<Option<AudioThread>>,
}

impl AudioOutBackend {
    /// Create a new audio output backend for the given device.
    pub fn new(device_name: String, sample_rate: u32) -> Self {
        // Ring buffer sized for ~100ms of audio at the sample rate
        let buffer_size = (sample_rate as usize / 10).max(4096);

        Self {
            device_name,
            sample_rate,
            config: AudioOutConfig::default(),
            caps: super::default_capabilities(sample_rate),
            ring_buffer: Arc::new(RingBuffer::new(buffer_size)),
            connected: Arc::new(AtomicBool::new(false)),
            muted: Arc::new(AtomicBool::new(true)),
            audio_thread: Mutex::new(None),
        }
    }

    /// Set the audio output configuration.
    pub fn set_config(&mut self, config: AudioOutConfig) {
        self.config = config;
    }

    /// Convert a laser point to stereo audio samples.
    fn point_to_samples(p: &LaserPoint, config: &AudioOutConfig) -> (f32, f32) {
        // Blanked points (intensity == 0) output silence
        if p.intensity == 0 {
            return (0.0, 0.0);
        }

        let mut l = p.x * config.gain + config.dc_offset;
        let mut r = p.y * config.gain + config.dc_offset;

        if config.clip {
            l = l.clamp(-1.0, 1.0);
            r = r.clamp(-1.0, 1.0);
        }

        (l, r)
    }

    /// Start the audio thread.
    fn start_audio_thread(&self) -> Result<AudioThread> {
        let device_name = self.device_name.clone();
        let sample_rate = self.sample_rate;
        let ring_buffer = Arc::clone(&self.ring_buffer);
        let muted = Arc::clone(&self.muted);
        let connected = Arc::clone(&self.connected);
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = Arc::clone(&stop_flag);

        let handle = thread::Builder::new()
            .name(format!("audio-out-{}", device_name))
            .spawn(move || {
                if let Err(e) = run_audio_thread(
                    &device_name,
                    sample_rate,
                    ring_buffer,
                    muted,
                    connected,
                    stop_flag_clone,
                ) {
                    log::error!("Audio thread error: {}", e);
                }
            })
            .map_err(|e| Error::backend(std::io::Error::other(format!("Failed to spawn audio thread: {}", e))))?;

        Ok(AudioThread { handle, stop_flag })
    }
}

/// Run the audio output on a dedicated thread.
fn run_audio_thread(
    device_name: &str,
    sample_rate: u32,
    ring_buffer: Arc<RingBuffer>,
    muted: Arc<AtomicBool>,
    connected: Arc<AtomicBool>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()> {
    let host = cpal::default_host();

    // Find the device by name
    let device = host
        .output_devices()
        .map_err(|e| Error::backend(std::io::Error::other(format!("Failed to enumerate devices: {}", e))))?
        .find(|d| d.name().map(|n| n == device_name).unwrap_or(false))
        .ok_or_else(|| Error::disconnected(format!("Audio device '{}' not found", device_name)))?;

    // Get supported config
    let supported_config = device
        .supported_output_configs()
        .map_err(|e| Error::backend(std::io::Error::other(format!("Failed to get audio configs: {}", e))))?
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

    // Build stream based on sample format
    let stream = match sample_format {
        SampleFormat::F32 => {
            let ring_buffer = Arc::clone(&ring_buffer);
            let muted = Arc::clone(&muted);
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [f32], _| {
                        // Always consume from ring buffer to keep the read
                        // pointer advancing. Without this, muting would stall
                        // the ring buffer and cause the producer to livelock
                        // on WouldBlock.
                        let read = ring_buffer.read_interleaved(data);
                        // Fill remainder with silence if buffer underrun
                        if read * 2 < data.len() {
                            data[read * 2..].fill(0.0);
                        }
                        // Muted (shutter closed): overwrite with silence
                        if muted.load(Ordering::Relaxed) {
                            data.fill(0.0);
                        }
                    },
                    |err| {
                        log::error!("Audio stream error: {}", err);
                    },
                    None,
                )
                .map_err(|e| {
                    Error::backend(std::io::Error::other(format!("Failed to build audio stream: {}", e)))
                })?
        }
        SampleFormat::I16 => {
            let ring_buffer = Arc::clone(&ring_buffer);
            let muted = Arc::clone(&muted);
            device
                .build_output_stream(
                    &config,
                    move |data: &mut [i16], _| {
                        // Always consume from ring buffer (see F32 path comment)
                        let pairs = data.len() / 2;
                        let mut temp = vec![(0.0f32, 0.0f32); pairs];
                        let read = ring_buffer.read(&mut temp);

                        // Muted: output silence but still consumed from buffer above
                        if muted.load(Ordering::Relaxed) {
                            data.fill(0);
                            return;
                        }

                        for (i, (l, r)) in temp[..read].iter().enumerate() {
                            data[i * 2] = (*l * i16::MAX as f32) as i16;
                            data[i * 2 + 1] = (*r * i16::MAX as f32) as i16;
                        }
                        // Fill remainder with silence
                        if read * 2 < data.len() {
                            data[read * 2..].fill(0);
                        }
                    },
                    |err| {
                        log::error!("Audio stream error: {}", err);
                    },
                    None,
                )
                .map_err(|e| {
                    Error::backend(std::io::Error::other(format!("Failed to build audio stream: {}", e)))
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
        Error::backend(std::io::Error::other(format!("Failed to start audio stream: {}", e)))
    })?;

    // Mark as connected
    connected.store(true, Ordering::Release);

    log::info!(
        "Audio thread started for '{}' at {} Hz",
        device_name,
        sample_rate
    );

    // Keep the stream alive until stop is requested
    while !stop_flag.load(Ordering::Relaxed) {
        thread::sleep(std::time::Duration::from_millis(10));
    }

    // Stream is dropped here, stopping audio output
    connected.store(false, Ordering::Release);

    log::info!("Audio thread stopped for '{}'", device_name);

    Ok(())
}

impl StreamBackend for AudioOutBackend {
    fn dac_type(&self) -> DacType {
        DacType::Audio
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        if self.connected.load(Ordering::Relaxed) {
            return Ok(());
        }

        let audio_thread = self.start_audio_thread()?;

        // Wait for the thread to connect (with timeout)
        let start = std::time::Instant::now();
        while !self.connected.load(Ordering::Acquire) {
            if start.elapsed() > std::time::Duration::from_secs(5) {
                // Stop the thread
                audio_thread.stop_flag.store(true, Ordering::Release);
                let _ = audio_thread.handle.join();
                return Err(Error::backend(std::io::Error::other("Timeout waiting for audio connection")));
            }
            thread::sleep(std::time::Duration::from_millis(10));
        }

        *self.audio_thread.lock().unwrap() = Some(audio_thread);

        log::info!(
            "Connected to audio device '{}' at {} Hz",
            self.device_name,
            self.sample_rate
        );

        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(audio_thread) = self.audio_thread.lock().unwrap().take() {
            audio_thread.stop_flag.store(true, Ordering::Release);
            let _ = audio_thread.handle.join();
        }
        self.connected.store(false, Ordering::Release);
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    fn try_write_chunk(&mut self, _pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        if !self.is_connected() {
            return Err(Error::disconnected("Not connected"));
        }

        // Check if we have space in the ring buffer
        let available = self.ring_buffer.available();
        if available < points.len() {
            return Ok(WriteOutcome::WouldBlock);
        }

        // Convert points to samples
        let samples: Vec<(f32, f32)> = points
            .iter()
            .map(|p| Self::point_to_samples(p, &self.config))
            .collect();

        // Write to ring buffer
        let written = self.ring_buffer.write(&samples);
        if written < samples.len() {
            // Partial write - shouldn't happen since we checked available space
            log::warn!(
                "Audio ring buffer partial write: {} of {} samples",
                written,
                samples.len()
            );
        }

        Ok(WriteOutcome::Written)
    }

    fn stop(&mut self) -> Result<()> {
        // Mute the output
        self.muted.store(true, Ordering::Release);
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        // Mute when shutter is closed
        self.muted.store(!open, Ordering::Release);
        Ok(())
    }

    fn queued_points(&self) -> Option<u64> {
        Some(self.ring_buffer.len() as u64)
    }
}
