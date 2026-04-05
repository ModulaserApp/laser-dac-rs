//! Frame API example.
//!
//! Demonstrates the recommended frame-first workflow: discover a device,
//! start a frame session, and submit frames. The shape orbits around the
//! center, showing how `send_frame()` updates the output in real time.
//!
//! The library handles looping (each frame repeats until replaced),
//! transition blanking, and transport-appropriate delivery.
//!
//! Run with: `cargo run --example frame -- [triangle|circle|orientation|test-pattern]`

mod common;

use clap::Parser;
use common::{generate_frame, Args};
use laser_dac::{list_devices, open_device, Frame, FrameSessionConfig, Result};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Scanning for DACs...\n");
    let devices = list_devices()?;

    if devices.is_empty() {
        println!("No DACs found.");
        return Ok(());
    }

    let device_info = &devices[0];
    println!("  Found: {} ({})", device_info.name, device_info.kind);

    let device = open_device(&device_info.id)?;

    let config = FrameSessionConfig::new(30_000);
    let (session, info) = device.start_frame_session(config)?;

    println!(
        "\nStreaming {} via Frame API to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        info.name
    );

    session.control().arm()?;

    // Install Ctrl+C handler
    let control = session.control();
    ctrlc::set_handler(move || {
        let _ = control.stop();
    })
    .expect("failed to set Ctrl+C handler");

    // Generate the base shape once
    let base_frame = generate_frame(args.shape, args.points, args.scale);

    // Submit frames at ~60fps — the shape orbits around the center.
    // Each send_frame() replaces the current output; the library loops
    // the latest frame until a new one arrives.
    let start = Instant::now();
    loop {
        let t = start.elapsed().as_secs_f32();
        let cx = 0.3 * (t * 0.5).cos();
        let cy = 0.3 * (t * 0.5).sin();

        let mut points = base_frame.clone();
        for p in &mut points {
            p.x = (p.x + cx).clamp(-1.0, 1.0);
            p.y = (p.y + cy).clamp(-1.0, 1.0);
        }

        session.send_frame(Frame::new(points));

        thread::sleep(Duration::from_millis(16));
        if session.control().is_stop_requested() {
            break;
        }
    }

    let exit = session.join()?;
    println!("\nSession ended: {:?}", exit);
    Ok(())
}
