//! Frame session example demonstrating frame-first streaming.
//!
//! This example shows how to use `FrameSession` and `Frame` to stream
//! complete frames with automatic transition blanking between them.
//!
//! Run with: `cargo run --example frame_session -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{generate_frame, Args};
use laser_dac::{list_devices, open_device, Frame, FrameSessionConfig, Result};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Scanning for DACs (5 seconds)...\n");

    let mut devices = Vec::new();
    for _ in 0..50 {
        devices = list_devices()?;
        if !devices.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    if devices.is_empty() {
        println!("No DACs found.");
        return Ok(());
    }

    for device in &devices {
        println!("  Found: {} ({})", device.name, device.kind);
    }

    let device_info = &devices[0];
    let device = open_device(&device_info.id)?;

    // Start a frame session (frame-first API)
    let config = FrameSessionConfig::new(30_000);
    let (session, info) = device.start_frame_session(config)?;

    println!(
        "\nStreaming {} via FrameSession to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        info.name
    );

    // Arm the output
    session.control().arm()?;

    // Generate and submit a frame
    let points = generate_frame(args.shape, args.points, args.scale);
    session.send_frame(Frame::new(points));

    // Install Ctrl+C handler
    let control = session.control();
    ctrlc::set_handler(move || {
        let _ = control.stop();
    })
    .expect("failed to set Ctrl+C handler");

    // Wait for the session to end (Ctrl+C)
    let exit = session.join()?;
    println!("\nSession ended: {:?}", exit);
    Ok(())
}
