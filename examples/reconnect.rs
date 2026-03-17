//! Reconnecting stream example.
//!
//! This example demonstrates automatic reconnection via `StreamConfig::with_reconnect`,
//! which transparently reconnects to the DAC if the connection is lost.
//!
//! Run with: `cargo run --example reconnect -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{make_producer, Args};
use laser_dac::{list_devices, open_device, ReconnectConfig, Result, StreamConfig};
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

    // Open device and create a reconnecting stream via config
    let config = StreamConfig::new(30_000).with_reconnect(
        ReconnectConfig::new()
            .backoff(Duration::from_secs(1))
            .on_disconnect(|err| eprintln!("\nDisconnected: {}", err))
            .on_reconnect(|info| println!("Reconnected to {}", info.name)),
    );

    let dac = open_device(&device_info.id)?;
    let (stream, _info) = dac.start_stream(config)?;

    println!(
        "\nStreaming {} to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        device_info.name
    );

    // Arm the output (allow laser to fire) - persists across reconnects
    stream.control().arm()?;

    // Install Ctrl+C handler to stop stream gracefully (disarm + stop backend)
    let control = stream.control();
    ctrlc::set_handler(move || {
        let _ = control.stop();
    })
    .expect("failed to set Ctrl+C handler");

    // Run the stream - reconnects automatically on disconnect
    let mut producer = make_producer(args.shape, args.points, args.scale);
    stream.run(
        move |req, buffer| producer(req, buffer),
        |err| eprintln!("Stream error: {}", err),
    )?;

    Ok(())
}
