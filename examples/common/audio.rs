//! Audio capture and visualization for laser examples.
//!
//! Provides low-latency audio-reactive point generation for Jerobeam-style
//! XY oscilloscope rendering. Uses wall-clock anchored sampling from a ring
//! buffer for stable, drift-free output with configurable latency.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use laser_dac::{ChunkRequest, LaserPoint};
use log::{debug, error, info};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

/// Ring buffer size in frames (stereo pairs) - 2 seconds at 48kHz
const RING_BUFFER_FRAMES: usize = 96000;

/// Global audio state - lazily initialized on first use
static AUDIO_STATE: OnceLock<Arc<AudioState>> = OnceLock::new();

/// Flag to track if audio has been initialized
static AUDIO_INITIALIZED: OnceLock<bool> = OnceLock::new();

/// Audio mode based on channel count
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AudioMode {
    /// Mono: time-domain oscilloscope (X=time, Y=amplitude)
    Mono,
    /// Stereo: XY oscilloscope (left=X, right=Y)
    Stereo,
}

/// Configuration for audio rendering
#[derive(Debug, Clone)]
pub struct AudioConfig {
    /// Fixed intensity for all points (0-65535)
    pub intensity: u16,
    /// Color: (r, g, b) each 0-65535
    pub color: (u16, u16, u16),
    /// Blank when max amplitude is below this threshold (0.0 = never blank, 0.01 = typical)
    pub blank_threshold: f32,
    /// Extra audio delay in milliseconds for audio capture latency compensation.
    /// Higher values provide more margin against glitches but increase latency.
    /// Typical range: 5-20ms. Set to 0 for minimum latency (may cause glitches).
    pub extra_delay_ms: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            intensity: 65535,
            color: (0, 65535, 65535), // Cyan
            blank_threshold: 0.01,    // Blank on near-silence
            extra_delay_ms: 10.0,     // 10ms extra delay for safety
        }
    }
}

/// Snapshot of audio state for lock-free reading
struct AudioSnapshot {
    sample_clock: u64,
    write_pos: usize,
    sample_rate: u32,
    mode: AudioMode,
}

/// Shared audio state between the audio thread and render thread
struct AudioState {
    /// Ring buffer for left channel samples (written by audio thread)
    ring_left: Mutex<Vec<f32>>,
    /// Ring buffer for right channel samples (written by audio thread)
    ring_right: Mutex<Vec<f32>>,
    /// Write position in ring buffer (atomic for lock-free read)
    write_pos: AtomicU64,
    /// Sample clock: total frames written since start (atomic for lock-free read)
    sample_clock: AtomicU64,
    /// Audio sample rate in Hz (immutable after init)
    sample_rate: u32,
    /// Number of input channels (immutable after init)
    channels: usize,
    /// Audio mode (immutable after init)
    mode: AudioMode,
}

impl AudioState {
    fn new(channels: usize, sample_rate: u32) -> Self {
        let mode = if channels >= 2 {
            AudioMode::Stereo
        } else {
            AudioMode::Mono
        };

        info!(
            "Audio mode: {:?} ({} channels, {} Hz)",
            mode, channels, sample_rate
        );

        Self {
            ring_left: Mutex::new(vec![0.0; RING_BUFFER_FRAMES]),
            ring_right: Mutex::new(vec![0.0; RING_BUFFER_FRAMES]),
            write_pos: AtomicU64::new(0),
            sample_clock: AtomicU64::new(0),
            sample_rate,
            channels,
            mode,
        }
    }

    fn update_samples(&self, new_samples: &[f32]) {
        let frame_count = new_samples.len() / self.channels.max(1);

        // Lock both buffers briefly to write
        let mut left = self.ring_left.lock().unwrap();
        let mut right = self.ring_right.lock().unwrap();

        let mut write_pos = self.write_pos.load(Ordering::Relaxed) as usize;

        for i in 0..frame_count {
            let l = new_samples[i * self.channels];
            let r = if self.channels >= 2 {
                new_samples[i * self.channels + 1]
            } else {
                l
            };

            left[write_pos] = l;
            right[write_pos] = r;

            write_pos = (write_pos + 1) % RING_BUFFER_FRAMES;
        }

        // Update atomics after write completes
        self.write_pos.store(write_pos as u64, Ordering::Release);
        self.sample_clock.fetch_add(frame_count as u64, Ordering::Release);
    }

    /// Get a snapshot of current state for lock-free calculations
    fn snapshot(&self) -> AudioSnapshot {
        AudioSnapshot {
            sample_clock: self.sample_clock.load(Ordering::Acquire),
            write_pos: self.write_pos.load(Ordering::Acquire) as usize,
            sample_rate: self.sample_rate,
            mode: self.mode,
        }
    }

    /// Read samples for a range with minimal lock hold time.
    /// Copies the needed window under lock, then interpolates outside the lock.
    /// Returns a Vec of (left, right) pairs.
    fn read_samples(&self, start_frame: u64, count: usize, points_to_frames: f64) -> Vec<(f32, f32)> {
        if count == 0 {
            return Vec::new();
        }

        let snap = self.snapshot();

        // Calculate the frame range we need (with +1 for interpolation)
        let end_frame_f = start_frame as f64 + (count - 1) as f64 * points_to_frames + 1.0;
        let frames_needed = (end_frame_f.ceil() as u64).saturating_sub(start_frame) + 1;
        let frames_needed = (frames_needed as usize).min(RING_BUFFER_FRAMES);

        // Copy the needed window under lock
        let (left_copy, right_copy) = {
            let left = self.ring_left.lock().unwrap();
            let right = self.ring_right.lock().unwrap();

            let mut left_copy = Vec::with_capacity(frames_needed);
            let mut right_copy = Vec::with_capacity(frames_needed);

            for offset in 0..frames_needed {
                let frame_idx = start_frame + offset as u64;
                let (l, r) = Self::read_frame_from_slice(&left, &right, frame_idx, &snap);
                left_copy.push(l);
                right_copy.push(r);
            }

            (left_copy, right_copy)
        };
        // Lock released here

        // Interpolate from the copy (no lock held)
        let mut samples = Vec::with_capacity(count);
        for i in 0..count {
            let frame_f = i as f64 * points_to_frames;
            let idx = frame_f.floor() as usize;
            let frac = (frame_f - frame_f.floor()) as f32;

            let idx0 = idx.min(left_copy.len().saturating_sub(1));
            let idx1 = (idx + 1).min(left_copy.len().saturating_sub(1));

            let l = left_copy[idx0] + (left_copy[idx1] - left_copy[idx0]) * frac;
            let r = right_copy[idx0] + (right_copy[idx1] - right_copy[idx0]) * frac;
            samples.push((l, r));
        }

        samples
    }

    fn read_frame_from_slice(
        left: &[f32],
        right: &[f32],
        frame_idx: u64,
        snap: &AudioSnapshot,
    ) -> (f32, f32) {
        if frame_idx >= snap.sample_clock {
            // Future frame - return last available
            let idx = if snap.write_pos == 0 {
                RING_BUFFER_FRAMES - 1
            } else {
                snap.write_pos - 1
            };
            return (left[idx], right[idx]);
        }

        let frames_back = snap.sample_clock - frame_idx;
        if frames_back > RING_BUFFER_FRAMES as u64 {
            // Too far back - return oldest
            return (left[snap.write_pos], right[snap.write_pos]);
        }

        let idx = (snap.write_pos + RING_BUFFER_FRAMES - (frames_back as usize % RING_BUFFER_FRAMES))
            % RING_BUFFER_FRAMES;
        (left[idx], right[idx])
    }

}

/// Initialize audio capture if not already running.
/// Returns true if audio is available.
pub fn ensure_audio_initialized() -> bool {
    if let Some(&initialized) = AUDIO_INITIALIZED.get() {
        return initialized;
    }

    info!("Initializing audio capture...");

    let host = cpal::default_host();
    info!("Using audio host: {:?}", host.id());

    let input_device = match host.default_input_device() {
        Some(d) => d,
        None => {
            error!("No audio input device available");
            let _ = AUDIO_INITIALIZED.set(false);
            return false;
        }
    };
    info!("Using input device: {:?}", input_device.name());

    let config = match input_device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to get default input config: {}", e);
            let _ = AUDIO_INITIALIZED.set(false);
            return false;
        }
    };
    info!(
        "Audio config: {} channels, {} Hz, {:?}",
        config.channels(),
        config.sample_rate().0,
        config.sample_format()
    );

    let channels = config.channels() as usize;
    let sample_rate = config.sample_rate().0;
    let audio_state = Arc::new(AudioState::new(channels, sample_rate));

    let _ = AUDIO_STATE.set(Arc::clone(&audio_state));

    let sample_format = config.sample_format();
    let stream_config: cpal::StreamConfig = config.into();

    thread::spawn(move || {
        let err_fn = |err| error!("Audio stream error: {}", err);

        let stream = match sample_format {
            cpal::SampleFormat::F32 => {
                let state = Arc::clone(&audio_state);
                input_device.build_input_stream(
                    &stream_config,
                    move |data: &[f32], _: &cpal::InputCallbackInfo| {
                        state.update_samples(data);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::I16 => {
                let state = Arc::clone(&audio_state);
                input_device.build_input_stream(
                    &stream_config,
                    move |data: &[i16], _: &cpal::InputCallbackInfo| {
                        let float_data: Vec<f32> =
                            data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                        state.update_samples(&float_data);
                    },
                    err_fn,
                    None,
                )
            }
            cpal::SampleFormat::U16 => {
                let state = Arc::clone(&audio_state);
                input_device.build_input_stream(
                    &stream_config,
                    move |data: &[u16], _: &cpal::InputCallbackInfo| {
                        let float_data: Vec<f32> = data
                            .iter()
                            .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                            .collect();
                        state.update_samples(&float_data);
                    },
                    err_fn,
                    None,
                )
            }
            format => {
                error!("Unsupported sample format: {:?}", format);
                return;
            }
        };

        match stream {
            Ok(s) => {
                if let Err(e) = s.play() {
                    error!("Failed to start audio stream: {}", e);
                    return;
                }
                info!("Audio stream started");
                loop {
                    thread::park();
                }
            }
            Err(e) => {
                error!("Failed to build audio stream: {}", e);
            }
        }
    });

    thread::sleep(std::time::Duration::from_millis(100));
    let _ = AUDIO_INITIALIZED.set(true);
    true
}

/// Get the current audio sample rate, or None if audio isn't initialized.
#[allow(dead_code)]
pub fn get_audio_sample_rate() -> Option<u32> {
    Some(AUDIO_STATE.get()?.sample_rate)
}

/// Get the current audio mode, or None if audio isn't initialized.
#[allow(dead_code)]
pub fn get_audio_mode() -> Option<AudioMode> {
    Some(AUDIO_STATE.get()?.mode)
}

/// Create laser points from audio input.
///
/// Renders audio with low, fixed latency. The `extra_delay_ms` config provides
/// a safety margin for audio capture latency.
///
/// - **Stereo**: XY oscilloscope (left=X, right=Y)
/// - **Mono**: Time-domain oscilloscope (X=time sweep, Y=amplitude from timebase)
pub fn create_audio_points(req: &ChunkRequest, config: &AudioConfig) -> Vec<LaserPoint> {
    if req.n_points == 0 {
        return Vec::new();
    }

    if !ensure_audio_initialized() {
        return vec![LaserPoint::blanked(0.0, 0.0); req.n_points];
    }

    let state = match AUDIO_STATE.get() {
        Some(s) => s,
        None => return vec![LaserPoint::blanked(0.0, 0.0); req.n_points],
    };

    let snap = state.snapshot();
    let pps = req.pps;

    // Ratio for converting points to audio frames
    let points_to_frames = snap.sample_rate as f64 / pps as f64;

    // Calculate audio start frame:
    // - sample_clock is "now" in audio time
    // - We go back by extra_delay_ms for safety margin
    // - We go back by chunk duration so the chunk spans [start, start + n_points)
    let extra_delay_frames = (config.extra_delay_ms / 1000.0 * snap.sample_rate as f32) as u64;
    let chunk_frames = (req.n_points as f64 * points_to_frames) as u64;
    let audio_start_frame = snap.sample_clock
        .saturating_sub(extra_delay_frames)
        .saturating_sub(chunk_frames);

    // Read all samples we need (this minimizes lock hold time)
    let samples = state.read_samples(audio_start_frame, req.n_points, points_to_frames);

    // Check for silence
    if config.blank_threshold > 0.0 {
        let check_count = (req.n_points / 10).max(10).min(req.n_points);
        let max_amp = samples
            .iter()
            .step_by(req.n_points / check_count)
            .map(|(l, r)| l.abs().max(r.abs()))
            .fold(0.0f32, f32::max);

        if max_amp < config.blank_threshold {
            debug!("Audio: silence detected (max_amp={:.4}), blanking", max_amp);
            return vec![LaserPoint::blanked(0.0, 0.0); req.n_points];
        }
    }

    // Generate points
    let (cr, cg, cb) = config.color;
    let points: Vec<LaserPoint> = match snap.mode {
        AudioMode::Stereo => samples
            .iter()
            .map(|(l, r)| {
                LaserPoint::new(
                    l.clamp(-1.0, 1.0),
                    r.clamp(-1.0, 1.0),
                    cr, cg, cb,
                    config.intensity,
                )
            })
            .collect(),
        AudioMode::Mono => samples
            .iter()
            .enumerate()
            .map(|(i, (l, _r))| {
                let t = i as f32 / (req.n_points - 1).max(1) as f32;
                let x = t * 2.0 - 1.0;
                LaserPoint::new(x, l.clamp(-1.0, 1.0), cr, cg, cb, config.intensity)
            })
            .collect(),
    };

    debug!(
        "Audio: {} points, mode={:?}, audio_frame={}, delay={}ms",
        points.len(),
        snap.mode,
        audio_start_frame,
        config.extra_delay_ms
    );

    points
}
