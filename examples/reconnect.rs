//! Reconnecting session example.
//!
//! This example demonstrates the ReconnectingSession which automatically
//! reconnects to the DAC if the connection is lost.
//!
//! Run with: `cargo run --example reconnect -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{fill_points, Args};
use laser_dac::{list_devices, ChunkRequest, LaserPoint, ReconnectingSession, Result, StreamConfig};
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Scanning for DACs...\n");
    let devices = list_devices()?;

    if devices.is_empty() {
        println!("No DACs found.");
        return Ok(());
    }

    // Get first device ID
    let device_info = &devices[0];
    println!("  Found: {} ({})", device_info.name, device_info.kind);

    // Create a reconnecting session
    let mut session = ReconnectingSession::new(&device_info.id, StreamConfig::new(30_000))
        .with_backoff(Duration::from_secs(1))
        .on_disconnect(|err| eprintln!("\nDisconnected: {}", err))
        .on_reconnect(|info| println!("Reconnected to {}", info.name));

    println!(
        "\nStreaming {} to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        device_info.name
    );

    // Arm the output (allow laser to fire) - persists across reconnects
    session.control().arm()?;

    // Run the stream - reconnects automatically on disconnect
    let shape = args.shape;
    session.run(
        move |req: &ChunkRequest, buffer: &mut [LaserPoint]| fill_points(shape, req, buffer),
        |err| eprintln!("Stream error: {}", err),
    )?;

    Ok(())
}
