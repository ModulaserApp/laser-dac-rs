//! Audio-reactive laser example.
//!
//! This example captures audio from the default input device and renders it to the laser:
//!
//! - **Mono (1 channel)**: Time-domain oscilloscope (X=time, Y=amplitude)
//! - **Stereo (2+ channels)**: XY oscilloscope mode (left=X, right=Y)
//!
//! In both modes, the laser is blanked when the signal is very low to prevent beam dwell.
//!
//! Run with: `cargo run --example audio`

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use laser_dac::{list_devices, open_device, ChunkRequest, LaserPoint, StreamConfig, Result};
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Audio buffer size for processing
const AUDIO_BUFFER_SIZE: usize = 1024;

/// Threshold below which we consider the signal silent (for XY blanking)
const SILENCE_THRESHOLD: f32 = 0.01;

/// How many points to generate per chunk for the laser
const POINTS_PER_CHUNK: usize = 512;

/// Audio mode based on channel count
#[derive(Debug, Clone, Copy)]
enum AudioMode {
    /// Mono: time-domain oscilloscope (X=time, Y=amplitude)
    Mono,
    /// Stereo: XY oscilloscope (left=X, right=Y)
    Stereo,
}

/// Shared audio state between the audio thread and laser thread
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

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("Audio-reactive laser example starting...");

    // Initialize audio
    let host = cpal::default_host();
    info!("Using audio host: {:?}", host.id());

    let input_device = host
        .default_input_device()
        .expect("No audio input device available");
    info!("Using input device: {:?}", input_device.name());

    let config = input_device
        .default_input_config()
        .expect("Failed to get default input config");
    info!(
        "Audio config: {} channels, {} Hz, {:?}",
        config.channels(),
        config.sample_rate().0,
        config.sample_format()
    );

    let channels = config.channels() as usize;

    // Shared state
    let audio_state = Arc::new(Mutex::new(AudioState::new(channels)));
    let audio_state_audio = Arc::clone(&audio_state);

    // Build audio input stream
    let err_fn = |err| error!("Audio stream error: {}", err);

    let audio_stream = match config.sample_format() {
        cpal::SampleFormat::F32 => input_device
            .build_input_stream(
                &config.into(),
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let mut state = audio_state_audio.lock().unwrap();
                    state.update_samples(data);
                },
                err_fn,
                None,
            )
            .expect("Failed to build audio stream"),
        cpal::SampleFormat::I16 => input_device
            .build_input_stream(
                &config.into(),
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let mut state = audio_state_audio.lock().unwrap();
                    let float_data: Vec<f32> =
                        data.iter().map(|&s| s as f32 / i16::MAX as f32).collect();
                    state.update_samples(&float_data);
                },
                err_fn,
                None,
            )
            .expect("Failed to build audio stream"),
        cpal::SampleFormat::U16 => input_device
            .build_input_stream(
                &config.into(),
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let mut state = audio_state_audio.lock().unwrap();
                    let float_data: Vec<f32> = data
                        .iter()
                        .map(|&s| (s as f32 / u16::MAX as f32) * 2.0 - 1.0)
                        .collect();
                    state.update_samples(&float_data);
                },
                err_fn,
                None,
            )
            .expect("Failed to build audio stream"),
        format => panic!("Unsupported sample format: {:?}", format),
    };

    audio_stream.play().expect("Failed to start audio stream");
    info!("Audio stream started");

    // Discover and connect to laser
    info!("Scanning for laser DACs...");
    let devices = list_devices()?;

    if devices.is_empty() {
        error!("No laser DACs found!");
        return Ok(());
    }

    let device_info = &devices[0];
    info!(
        "Found DAC: {} ({}) - PPS range: {}-{}",
        device_info.name, device_info.kind, device_info.caps.pps_min, device_info.caps.pps_max
    );

    let device = open_device(&device_info.id)?;
    let pps = 30_000u32;
    let config = StreamConfig::new(pps).with_chunk_points(POINTS_PER_CHUNK);

    info!("Starting laser stream at {} PPS", pps);
    let (stream, info) = device.start_stream(config)?;

    info!("Connected to: {}", info.name);
    stream.control().arm()?;
    info!("Laser output armed");

    info!("Streaming audio to laser - press Ctrl+C to stop");

    // Track chunks for logging
    let chunk_count = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&chunk_count);

    // Run in callback mode - the stream calls our producer when it needs data
    let exit = stream.run(
        move |req: ChunkRequest| {
            let count = counter.fetch_add(1, Ordering::Relaxed);

            // Get current audio state (only the valid samples)
            let (mode, samples, channels_count) = {
                let state = audio_state.lock().unwrap();
                let valid_samples = state.samples[..state.sample_count].to_vec();
                (state.mode, valid_samples, state.channels)
            };

            // Generate laser points based on mode
            let points = match mode {
                AudioMode::Mono => generate_mono_oscilloscope(&samples, req.n_points),
                AudioMode::Stereo => generate_xy_oscilloscope(&samples, req.n_points, channels_count),
            };

            if count % 100 == 0 {
                debug!("Chunks: {}, Mode: {:?}", count, mode);
            }

            Some(points)
        },
        |err| {
            error!("Stream error: {}", err);
        },
    )?;

    info!("Stream ended: {:?}", exit);
    info!("Total chunks: {}", chunk_count.load(Ordering::Relaxed));

    Ok(())
}

/// Generate laser points for mono time-domain oscilloscope display
fn generate_mono_oscilloscope(samples: &[f32], n_points: usize) -> Vec<LaserPoint> {
    // Calculate signal amplitude for silence detection
    let amplitude = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);

    let is_silent = amplitude < SILENCE_THRESHOLD;

    if is_silent {
        debug!("Signal below threshold ({:.4}), blanking output", amplitude);
        return vec![LaserPoint::blanked(0.0, 0.0); n_points];
    }

    debug!("Mono amplitude: {:.4}", amplitude);

    // Normalize to use full range
    let scale = (1.0 / amplitude.max(0.1)).min(1.0);

    // Generate points
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
    if channels < 2 {
        warn!("XY mode requires 2 channels, falling back to blanked output");
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

    // Calculate signal amplitude
    let amplitude = left
        .iter()
        .chain(right.iter())
        .map(|s| s.abs())
        .fold(0.0f32, f32::max);

    let is_silent = amplitude < SILENCE_THRESHOLD;

    if is_silent {
        debug!("Signal below threshold ({:.4}), blanking output", amplitude);
        return vec![LaserPoint::blanked(0.0, 0.0); n_points];
    }

    debug!("XY amplitude: {:.4}", amplitude);

    // Normalize to use full range
    let scale = (1.0 / amplitude.max(0.1)).min(1.0);

    // Generate points
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
