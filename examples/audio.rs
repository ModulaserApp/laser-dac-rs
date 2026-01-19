//! Audio-reactive laser example.
//!
//! Displays audio input as an oscilloscope pattern:
//! - Mono input: time-domain oscilloscope (X=time, Y=amplitude)
//! - Stereo input: XY oscilloscope (left=X, right=Y)
//!
//! Run with: `cargo run --example audio`
//!
//! Requires a microphone or audio input device.

mod common;

use common::audio::{self, AudioConfig};
use laser_dac::{list_devices, open_device, Result, StreamConfig};

fn main() -> Result<()> {
    env_logger::init();

    // Initialize audio capture first
    if !audio::ensure_audio_initialized() {
        eprintln!("Failed to initialize audio capture. Make sure you have a microphone connected.");
        return Ok(());
    }

    println!("Scanning for DACs...\n");
    let devices = list_devices()?;

    if devices.is_empty() {
        println!("No DACs found.");
        return Ok(());
    }

    // Open first device
    let device_info = &devices[0];
    println!("  Found: {} ({})", device_info.name, device_info.kind);

    let device = open_device(&device_info.id)?;

    // Start streaming
    let config = StreamConfig::new(30_000);
    let (mut stream, info) = device.start_stream(config)?;

    println!(
        "\nStreaming audio to {}... Press Ctrl+C to stop\n",
        info.name
    );

    // Arm the output (allow laser to fire)
    stream.control().arm()?;

    let audio_config = AudioConfig::default();

    loop {
        let req = stream.next_request()?;

        // Generate audio-reactive points
        let points = audio::create_audio_points(&req, &audio_config);

        stream.write(&req, &points)?;
    }
}
