//! Callback-based example using Stream::run().
//!
//! This demonstrates the callback/pull approach where the stream drives timing.
//! The callback is invoked whenever the device is ready for more data.
//!
//! Run with: `cargo run --example callback -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{create_points, Args};
use laser_dac::{list_devices, open_device, ChunkRequest, StreamConfig, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

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

    // Track chunks written
    let chunk_count = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&chunk_count);

    // Start streaming
    let config = StreamConfig::new(30_000);
    let (stream, info) = device.start_stream(config)?;

    println!(
        "\nStreaming {} via callback to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        info.name
    );

    // Arm the output
    stream.control().arm()?;

    let shape = args.shape;

    // Run in callback mode
    let exit = stream.run(
        // Producer callback - invoked when device needs more data
        move |req: ChunkRequest| {
            let count = counter.fetch_add(1, Ordering::Relaxed);
            let points = create_points(shape, &req);

            // Print progress periodically
            if count % 100 == 0 {
                println!("Chunks sent: {}", count);
            }

            Some(points)
        },
        // Error callback
        |err| {
            eprintln!("Stream error: {}", err);
        },
    )?;

    println!("\nStream ended: {:?}", exit);
    println!("Total chunks: {}", chunk_count.load(Ordering::Relaxed));

    Ok(())
}
