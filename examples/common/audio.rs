//! Audio capture and visualization for laser examples.
//!
//! Provides audio-reactive point generation that can be used as a Shape variant.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use laser_dac::LaserPoint;
use log::{debug, error, info};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

/// Audio buffer size for processing
const AUDIO_BUFFER_SIZE: usize = 1024;

/// Threshold below which we consider the signal silent (for blanking)
const SILENCE_THRESHOLD: f32 = 0.01;

/// Global audio state - lazily initialized on first use
static AUDIO_STATE: OnceLock<Arc<Mutex<AudioState>>> = OnceLock::new();

/// Flag to track if audio has been initialized
static AUDIO_INITIALIZED: OnceLock<bool> = OnceLock::new();

/// Audio mode based on channel count
#[derive(Debug, Clone, Copy)]
pub enum AudioMode {
    /// Mono: time-domain oscilloscope (X=time, Y=amplitude)
    Mono,
    /// Stereo: XY oscilloscope (left=X, right=Y)
    Stereo,
}

/// Shared audio state between the audio thread and render thread
struct AudioState {
    /// Latest audio samples (interleaved if stereo)
    samples: Vec<f32>,
    /// Number of valid samples in the buffer
    sample_count: usize,
    /// Number of channels
    channels: usize,
    /// Audio mode
    mode: AudioMode,
}

impl AudioState {
    fn new(channels: usize) -> Self {
        let mode = if channels >= 2 {
            AudioMode::Stereo
        } else {
            AudioMode::Mono
        };

        info!("Audio mode: {:?} ({} channels)", mode, channels);

        Self {
            samples: vec![0.0; AUDIO_BUFFER_SIZE * channels],
            sample_count: 0,
            channels,
            mode,
        }
    }

    fn update_samples(&mut self, new_samples: &[f32]) {
        let copy_len = new_samples.len().min(self.samples.len());
        self.samples[..copy_len].copy_from_slice(&new_samples[..copy_len]);
        self.sample_count = copy_len;
    }
}

/// Initialize audio capture if not already running.
/// Returns true if audio is available.
pub fn ensure_audio_initialized() -> bool {
    // Check if already initialized
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
    let audio_state = Arc::new(Mutex::new(AudioState::new(channels)));

    // Store the state
    let _ = AUDIO_STATE.set(Arc::clone(&audio_state));

    // Spawn a thread to own and run the audio stream
    // The stream must be created and played on the same thread
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
                        if let Ok(mut state) = state.lock() {
                            state.update_samples(data);
                        }
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
                        if let Ok(mut state) = state.lock() {
                            let float_data: Vec<f32> =
                                data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                            state.update_samples(&float_data);
                        }
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
                        if let Ok(mut state) = state.lock() {
                            let float_data: Vec<f32> = data
                                .iter()
                                .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                                .collect();
                            state.update_samples(&float_data);
                        }
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

                // Keep the thread (and stream) alive forever
                loop {
                    thread::park();
                }
            }
            Err(e) => {
                error!("Failed to build audio stream: {}", e);
            }
        }
    });

    // Give the audio thread a moment to start
    thread::sleep(std::time::Duration::from_millis(100));

    let _ = AUDIO_INITIALIZED.set(true);
    true
}

/// Create laser points from audio input.
pub fn create_audio_points(n_points: usize) -> Vec<LaserPoint> {
    if !ensure_audio_initialized() {
        // Return blanked points if audio isn't available
        return vec![LaserPoint::blanked(0.0, 0.0); n_points];
    }

    let state = match AUDIO_STATE.get() {
        Some(s) => s,
        None => return vec![LaserPoint::blanked(0.0, 0.0); n_points],
    };

    let (mode, samples, channels) = {
        let state = state.lock().unwrap();
        let valid_samples = state.samples[..state.sample_count].to_vec();
        (state.mode, valid_samples, state.channels)
    };

    match mode {
        AudioMode::Mono => generate_mono_oscilloscope(&samples, n_points),
        AudioMode::Stereo => generate_xy_oscilloscope(&samples, n_points, channels),
    }
}

/// Generate laser points for mono time-domain oscilloscope display
fn generate_mono_oscilloscope(samples: &[f32], n_points: usize) -> Vec<LaserPoint> {
    if samples.is_empty() {
        return vec![LaserPoint::blanked(0.0, 0.0); n_points];
    }

    // Calculate signal amplitude for silence detection
    let amplitude = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);

    if amplitude < SILENCE_THRESHOLD {
        debug!("Signal below threshold ({:.4}), blanking output", amplitude);
        return vec![LaserPoint::blanked(0.0, 0.0); n_points];
    }

    debug!("Mono amplitude: {:.4}", amplitude);

    // Normalize to use full range
    let scale = (1.0 / amplitude.max(0.1)).min(1.0);

    let mut points = Vec::with_capacity(n_points);

    // Add leading blank points
    let blank_count = 3;
    let first_y = samples.first().copied().unwrap_or(0.0) * scale;
    for _ in 0..blank_count {
        points.push(LaserPoint::blanked(-1.0, first_y));
    }

    // Map samples to laser points: X = time (linear ramp), Y = amplitude
    let usable_points = n_points.saturating_sub(blank_count * 2);
    for i in 0..usable_points {
        let t = i as f32 / usable_points as f32;
        let x = t * 2.0 - 1.0; // -1 to 1

        // Map point index to sample index
        let sample_idx = (t * (samples.len() - 1) as f32) as usize;
        let sample_idx = sample_idx.min(samples.len() - 1);

        let y = (samples[sample_idx] * scale).clamp(-1.0, 1.0);

        // Green color for mono oscilloscope
        let intensity = ((y.abs() * 0.5 + 0.5) * 65535.0) as u16;
        points.push(LaserPoint::new(x, y, 0, intensity, 0, intensity));
    }

    // Add trailing blank points
    let last_y = samples.last().copied().unwrap_or(0.0) * scale;
    for _ in 0..blank_count {
        points.push(LaserPoint::blanked(1.0, last_y));
    }

    // Ensure we have exactly n_points
    while points.len() < n_points {
        points.push(LaserPoint::blanked(0.0, 0.0));
    }
    points.truncate(n_points);

    points
}

/// Generate laser points for XY oscilloscope display
fn generate_xy_oscilloscope(samples: &[f32], n_points: usize, channels: usize) -> Vec<LaserPoint> {
    if channels < 2 || samples.is_empty() {
        return vec![LaserPoint::blanked(0.0, 0.0); n_points];
    }

    // Deinterleave stereo samples
    let sample_pairs = samples.len() / channels;
    let mut left: Vec<f32> = Vec::with_capacity(sample_pairs);
    let mut right: Vec<f32> = Vec::with_capacity(sample_pairs);

    for i in 0..sample_pairs {
        left.push(samples[i * channels]);
        right.push(samples[i * channels + 1]);
    }

    if left.is_empty() {
        return vec![LaserPoint::blanked(0.0, 0.0); n_points];
    }

    // Calculate signal amplitude
    let amplitude = left
        .iter()
        .chain(right.iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max);

    if amplitude < SILENCE_THRESHOLD {
        debug!("Signal below threshold ({:.4}), blanking output", amplitude);
        return vec![LaserPoint::blanked(0.0, 0.0); n_points];
    }

    debug!("XY amplitude: {:.4}", amplitude);

    // Normalize to use full range
    let scale = (1.0 / amplitude.max(0.1)).min(1.0);

    let mut points = Vec::with_capacity(n_points);

    // Add leading blank points
    let blank_count = 3;
    let first_x = left.first().copied().unwrap_or(0.0) * scale;
    let first_y = right.first().copied().unwrap_or(0.0) * scale;
    for _ in 0..blank_count {
        points.push(LaserPoint::blanked(first_x, first_y));
    }

    // Map samples to laser points
    let usable_points = n_points.saturating_sub(blank_count * 2);
    for i in 0..usable_points {
        // Map point index to sample index
        let sample_idx = (i * left.len()) / usable_points;
        let sample_idx = sample_idx.min(left.len() - 1);

        let x = (left[sample_idx] * scale).clamp(-1.0, 1.0);
        let y = (right[sample_idx] * scale).clamp(-1.0, 1.0);

        // Color based on amplitude (brighter = more amplitude)
        let point_amp = (x.abs() + y.abs()) / 2.0;
        let intensity = ((point_amp * 0.7 + 0.3) * 65535.0) as u16;

        // Cyan color for XY scope
        points.push(LaserPoint::new(x, y, 0, intensity, intensity, intensity));
    }

    // Add trailing blank points
    let last_x = left.last().copied().unwrap_or(0.0) * scale;
    let last_y = right.last().copied().unwrap_or(0.0) * scale;
    for _ in 0..blank_count {
        points.push(LaserPoint::blanked(last_x, last_y));
    }

    // Ensure we have exactly n_points
    while points.len() < n_points {
        points.push(LaserPoint::blanked(0.0, 0.0));
    }
    points.truncate(n_points);

    points
}
