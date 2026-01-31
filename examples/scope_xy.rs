//! Oscilloscope XY output via audio interface.
//!
//! Outputs Lissajous patterns to stereo audio (L=X, R=Y).
//! Connect audio outputs to an oscilloscope in XY mode.
//!
//! Run with: `cargo run --example scope_xy --features audio-out`
//!
//! **Note:** A DC-coupled audio interface is required for accurate DC
//! representation. AC-coupled interfaces will high-pass filter the signal,
//! causing the image to drift.

use laser_dac::{list_devices, open_device, DacType, LaserPoint, Result, StreamConfig};
use std::f32::consts::PI;

fn main() -> Result<()> {
    env_logger::init();

    println!("Scanning for audio output devices...\n");
    let devices = list_devices()?;

    // Find audio output devices
    let audio_devices: Vec<_> = devices
        .iter()
        .filter(|d| d.kind == DacType::Audio)
        .collect();

    if audio_devices.is_empty() {
        println!("No audio output devices found.");
        println!("Make sure you have an audio output device connected.");
        return Ok(());
    }

    println!("Found {} audio device(s):", audio_devices.len());
    for (i, dev) in audio_devices.iter().enumerate() {
        println!("  [{}] {} (sample rate: {} Hz)", i, dev.name, dev.caps.pps_max);
    }
    println!();

    // Use BlackHole 2ch if available, otherwise first device
    let audio_device = audio_devices
        .iter()
        .find(|d| d.name.contains("BlackHole 2ch"))
        .copied()
        .unwrap_or(audio_devices[0]);
    println!("Using: {}", audio_device.name);

    let device = open_device(&audio_device.id)?;
    let sample_rate = device.caps().pps_max; // PPS = sample rate for audio

    println!("Sample rate: {} Hz", sample_rate);
    println!();

    // Start streaming at the device's sample rate
    let config = StreamConfig::new(sample_rate);
    let (mut stream, info) = device.start_stream(config)?;

    println!("Streaming Lissajous pattern to {}...", info.name);
    println!("Connect audio L/R outputs to oscilloscope X/Y inputs.");
    println!("Press Ctrl+C to stop.\n");

    // Arm the output (unmute)
    stream.control().arm()?;

    // Lissajous pattern parameters
    let freq_x = 3.0; // X frequency
    let freq_y = 2.0; // Y frequency (3:2 ratio creates a nice figure)
    let phase_offset = PI / 4.0; // Phase offset for Y

    let mut t = 0.0f32;
    let dt = 1.0 / sample_rate as f32;

    loop {
        let req = stream.next_request()?;

        // Generate Lissajous figure points
        let points: Vec<LaserPoint> = (0..req.n_points)
            .map(|i| {
                let time = t + i as f32 * dt;
                let x = (2.0 * PI * freq_x * time).sin();
                let y = (2.0 * PI * freq_y * time + phase_offset).sin();

                // Full intensity white (though color doesn't matter for audio)
                LaserPoint::new(x, y, 65535, 65535, 65535, 65535)
            })
            .collect();

        t += req.n_points as f32 * dt;

        // Keep t from growing too large (wrap at 1 second)
        if t > 1.0 {
            t -= 1.0;
        }

        stream.write(&req, &points)?;
    }
}
