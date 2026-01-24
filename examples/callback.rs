//! Callback-based example using Stream::run() with reusable buffer.
//!
//! This demonstrates the callback/pull approach where the stream drives timing.
//! The callback is invoked whenever the device is ready for more data, receiving
//! a pre-allocated buffer to fill (zero per-chunk allocation).
//!
//! Run with: `cargo run --example callback -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{fill_buffer, Args};
use laser_dac::{
    list_devices, open_device, ChunkRequest, LaserPoint, ProducerResult, Result, StreamConfig,
};
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

    // Run in callback mode with reusable buffer
    let exit = stream.run(
        // Producer callback - invoked when device needs more data
        // The buffer is pre-allocated and reused across calls
        move |req: ChunkRequest, buffer: &mut [LaserPoint]| {
            let count = counter.fetch_add(1, Ordering::Relaxed);

            // Fill the buffer with shape points
            fill_buffer(shape, &req, buffer);

            // Print progress periodically
            if count.is_multiple_of(100) {
                println!("Chunks sent: {}", count);
            }

            ProducerResult::Continue
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
