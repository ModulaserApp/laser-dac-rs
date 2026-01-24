//! Reconnecting session example with reusable buffer.
//!
//! This example demonstrates the ReconnectingSession which automatically
//! reconnects to the DAC if the connection is lost. Uses the zero-allocation
//! buffer API.
//!
//! Run with: `cargo run --example reconnect -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{fill_buffer, Args};
use laser_dac::{
    list_devices, LaserPoint, ProducerResult, ReconnectingSession, Result, StreamConfig,
};
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

    // Run the stream with reusable buffer - reconnects automatically on disconnect
    let shape = args.shape;
    session.run(
        move |req, buffer: &mut [LaserPoint]| {
            fill_buffer(shape, &req, buffer);
            ProducerResult::Continue
        },
        |err| eprintln!("Stream error: {}", err),
    )?;

    Ok(())
}
