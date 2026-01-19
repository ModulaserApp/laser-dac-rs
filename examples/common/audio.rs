//! Audio capture and visualization for laser examples.
//!
//! Provides timebase-aligned audio-reactive point generation for Jerobeam-style
//! XY oscilloscope rendering. Audio samples are aligned to the stream's point
//! timeline for stable, jitter-free output.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use laser_dac::{ChunkRequest, LaserPoint};
use log::{debug, error, info};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;

/// Ring buffer size in frames (stereo pairs) - 2 seconds at 48kHz
const RING_BUFFER_FRAMES: usize = 96000;

/// Global audio state - lazily initialized on first use
static AUDIO_STATE: OnceLock<Arc<Mutex<AudioState>>> = OnceLock::new();

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
    /// Audio delay in milliseconds. Lower = less latency but may cause glitches.
    /// Typical range: 10-30ms. Must be > audio capture latency (~10ms).
    pub delay_ms: f32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            intensity: 65535,
            color: (0, 65535, 65535), // Cyan
            blank_threshold: 0.01,    // Blank on near-silence
            delay_ms: 20.0,           // 20ms delay
        }
    }
}

/// Shared audio state between the audio thread and render thread
struct AudioState {
    /// Ring buffer for left channel samples
    ring_left: Vec<f32>,
    /// Ring buffer for right channel samples
    ring_right: Vec<f32>,
    /// Write position in ring buffer (next frame to write)
    write_pos: usize,
    /// Sample clock: total frames written since start (monotonic)
    sample_clock: u64,
    /// Audio sample rate in Hz
    sample_rate: u32,
    /// Number of input channels
    channels: usize,
    /// Audio mode
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
            ring_left: vec![0.0; RING_BUFFER_FRAMES],
            ring_right: vec![0.0; RING_BUFFER_FRAMES],
            write_pos: 0,
            sample_clock: 0,
            sample_rate,
            channels,
            mode,
        }
    }

    fn update_samples(&mut self, new_samples: &[f32]) {
        let frame_count = new_samples.len() / self.channels.max(1);

        for i in 0..frame_count {
            let left = new_samples[i * self.channels];
            let right = if self.channels >= 2 {
                new_samples[i * self.channels + 1]
            } else {
                left
            };

            self.ring_left[self.write_pos] = left;
            self.ring_right[self.write_pos] = right;

            self.write_pos = (self.write_pos + 1) % RING_BUFFER_FRAMES;
        }

        self.sample_clock += frame_count as u64;
    }

    /// Read a sample from the ring buffer at a given frame index.
    /// Returns (left, right).
    fn read_frame(&self, frame_idx: u64) -> (f32, f32) {
        let frames_available = self.sample_clock;
        if frame_idx >= frames_available {
            // Requested frame is in the future - return last available
            let idx = if self.write_pos == 0 {
                RING_BUFFER_FRAMES - 1
            } else {
                self.write_pos - 1
            };
            return (self.ring_left[idx], self.ring_right[idx]);
        }

        let frames_back = frames_available - frame_idx;
        if frames_back > RING_BUFFER_FRAMES as u64 {
            // Too far back - return oldest available
            return (
                self.ring_left[self.write_pos],
                self.ring_right[self.write_pos],
            );
        }

        // Calculate ring buffer index
        let idx = (self.write_pos + RING_BUFFER_FRAMES - (frames_back as usize % RING_BUFFER_FRAMES))
            % RING_BUFFER_FRAMES;
        (self.ring_left[idx], self.ring_right[idx])
    }

    /// Read a sample with linear interpolation at a fractional frame index.
    fn read_frame_interp(&self, frame_f: f64) -> (f32, f32) {
        let frame_idx = frame_f.floor() as u64;
        let frac = (frame_f - frame_f.floor()) as f32;

        let (l0, r0) = self.read_frame(frame_idx);
        let (l1, r1) = self.read_frame(frame_idx + 1);

        let l = l0 + (l1 - l0) * frac;
        let r = r0 + (r1 - r0) * frac;
        (l, r)
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
    let sample_rate = config.sample_rate().0;
    let audio_state = Arc::new(Mutex::new(AudioState::new(channels, sample_rate)));

    // Store the state
    let _ = AUDIO_STATE.set(Arc::clone(&audio_state));

    // Spawn a thread to own and run the audio stream
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

/// Get the current audio sample rate, or None if audio isn't initialized.
#[allow(dead_code)]
pub fn get_audio_sample_rate() -> Option<u32> {
    let state = AUDIO_STATE.get()?;
    let state = state.lock().ok()?;
    Some(state.sample_rate)
}

/// Get the current audio mode, or None if audio isn't initialized.
#[allow(dead_code)]
pub fn get_audio_mode() -> Option<AudioMode> {
    let state = AUDIO_STATE.get()?;
    let state = state.lock().ok()?;
    Some(state.mode)
}

/// Create laser points from audio input using stream timebase.
///
/// This function aligns audio samples to the stream's point timeline, providing
/// stable, jitter-free rendering. It uses the ChunkRequest's timing info
/// to render the audio that corresponds to each point's beam position.
///
/// - **Stereo**: XY oscilloscope (left=X, right=Y)
/// - **Mono**: Time-domain oscilloscope (X=time sweep, Y=amplitude from timebase)
///
/// # Arguments
/// * `req` - The chunk request from the stream, containing timing information
/// * `config` - Audio rendering configuration
///
/// # Returns
/// A vector of exactly `req.n_points` LaserPoints
pub fn create_audio_points(req: &ChunkRequest, config: &AudioConfig) -> Vec<LaserPoint> {
    if !ensure_audio_initialized() {
        return vec![LaserPoint::blanked(0.0, 0.0); req.n_points];
    }

    let state = match AUDIO_STATE.get() {
        Some(s) => s,
        None => return vec![LaserPoint::blanked(0.0, 0.0); req.n_points],
    };

    let state = match state.lock() {
        Ok(s) => s,
        Err(_) => return vec![LaserPoint::blanked(0.0, 0.0); req.n_points],
    };

    let sample_rate = state.sample_rate;
    let pps = req.pps;
    let mode = state.mode;

    // Convert points to audio frames ratio
    let points_to_frames_ratio = sample_rate as f64 / pps as f64;

    // Calculate audio start frame based on configured delay
    // We render audio that was captured `delay_ms` ago, giving us low-latency output
    let delay_frames = (config.delay_ms / 1000.0) * sample_rate as f32;
    let chunk_frames = (req.n_points as f64 * points_to_frames_ratio) as u64;
    let audio_start_frame = state.sample_clock
        .saturating_sub(delay_frames as u64)
        .saturating_sub(chunk_frames);

    // Check for silence by sampling across the chunk
    if config.blank_threshold > 0.0 {
        let check_count = (req.n_points / 10).max(10).min(req.n_points);
        let mut max_amp = 0.0f32;
        for i in 0..check_count {
            let audio_frame_f = audio_start_frame as f64 + (i * req.n_points / check_count) as f64 * points_to_frames_ratio;
            let (l, r) = state.read_frame_interp(audio_frame_f);
            max_amp = max_amp.max(l.abs()).max(r.abs());
        }
        if max_amp < config.blank_threshold {
            debug!("Audio: silence detected (max_amp={:.4}), blanking", max_amp);
            return vec![LaserPoint::blanked(0.0, 0.0); req.n_points];
        }
    }

    let mut points = Vec::with_capacity(req.n_points);
    let (cr, cg, cb) = config.color;

    match mode {
        AudioMode::Stereo => {
            // XY mode: left channel = X, right channel = Y
            for i in 0..req.n_points {
                let audio_frame_f = audio_start_frame as f64 + i as f64 * points_to_frames_ratio;
                let (l, r) = state.read_frame_interp(audio_frame_f);

                let x = l.clamp(-1.0, 1.0);
                let y = r.clamp(-1.0, 1.0);

                points.push(LaserPoint::new(x, y, cr, cg, cb, config.intensity));
            }
        }
        AudioMode::Mono => {
            // Time-domain mode: X = time sweep (-1 to +1), Y = audio amplitude
            for i in 0..req.n_points {
                let t = i as f32 / (req.n_points - 1).max(1) as f32;
                let x = t * 2.0 - 1.0; // -1 to +1 sweep

                let audio_frame_f = audio_start_frame as f64 + i as f64 * points_to_frames_ratio;
                let (l, _r) = state.read_frame_interp(audio_frame_f);
                let y = l.clamp(-1.0, 1.0);

                points.push(LaserPoint::new(x, y, cr, cg, cb, config.intensity));
            }
        }
    }

    debug!(
        "Audio: rendered {} points, mode={:?}, audio_start_frame={}, delay={}ms",
        points.len(),
        mode,
        audio_start_frame,
        config.delay_ms
    );

    points
}
