//! Streaming example.
//!
//! This example demonstrates the basic streaming workflow: discover devices,
//! open a connection, and stream points using the zero-allocation callback API.
//!
//! Run with: `cargo run --example stream -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{fill_points, Args};
use laser_dac::{list_devices, open_device, ChunkRequest, LaserPoint, Result, StreamConfig};

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

    let shape = args.shape;

    // Run stream with zero-allocation callback
    let exit = stream.run(
        move |req: &ChunkRequest, buffer: &mut [LaserPoint]| fill_points(shape, req, buffer),
        |err| {
            eprintln!("Stream error: {}", err);
        },
    )?;

    println!("\nStream ended: {:?}", exit);
    Ok(())
}
