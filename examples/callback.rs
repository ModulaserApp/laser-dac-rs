//! Callback-based example using Stream::run().
//!
//! This demonstrates the callback/pull approach where the stream drives timing.
//! The callback is invoked when the buffer needs filling.
//!
//! Run with: `cargo run --example callback -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{make_producer, Args};
use laser_dac::{list_devices, open_device, Result, StreamConfig};
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

    // Install Ctrl+C handler to stop stream gracefully (disarm + stop backend)
    let control = stream.control().clone();
    ctrlc::set_handler(move || {
        let _ = control.stop();
    })
    .expect("failed to set Ctrl+C handler");

    let mut producer = make_producer(args.shape, args.points, args.scale);

    // Run in callback mode with zero-allocation API
    let exit = stream.run(
        // Producer callback - invoked when buffer needs filling
        move |req, buffer| {
            counter.fetch_add(1, Ordering::Relaxed);

            producer(req, buffer)
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
