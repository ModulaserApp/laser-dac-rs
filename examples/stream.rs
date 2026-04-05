//! Streaming API example.
//!
//! Demonstrates the streaming workflow: discover devices, open a connection,
//! and stream points via a producer callback with buffer-driven timing.
//!
//! Try `orbiting-circle` for the best demo — it uses stream timestamps for
//! smooth continuous animation, which is the streaming API's strength.
//!
//! Run with: `cargo run --example stream -- [orbiting-circle|triangle|circle]`

mod common;

use clap::Parser;
use common::{make_producer, Args};
use laser_dac::{list_devices, open_device, Result, StreamConfig};

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

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
    let (stream, info) = device.start_stream(config)?;

    println!(
        "\nStreaming {} to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        info.name
    );

    // Arm the output (allow laser to fire)
    stream.control().arm()?;

    // Install Ctrl+C handler to stop stream gracefully (disarm + stop backend)
    let control = stream.control().clone();
    ctrlc::set_handler(move || {
        let _ = control.stop();
    })
    .expect("failed to set Ctrl+C handler");

    let mut producer = make_producer(args.shape, args.points, args.scale);

    // Run stream — static shapes cycle through a pre-generated frame,
    // time-based shapes compute points from stream timestamps
    let exit = stream.run(
        move |req, buffer| producer(req, buffer),
        |err| {
            eprintln!("Stream error: {}", err);
        },
    )?;

    println!("\nStream ended: {:?}", exit);
    Ok(())
}
