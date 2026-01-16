//! Manual discovery example using the streaming API.
//!
//! This example demonstrates using DacDiscovery to find devices,
//! then streaming to them using the blocking mode API.
//!
//! Run with: `cargo run --example manual -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{create_points, Args};
use laser_dac::{list_devices, open_device, StreamConfig, Result};

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
    let (mut stream, info) = device.start_stream(config)?;

    println!(
        "\nStreaming {} to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        info.name
    );

    // Arm the output (allow laser to fire)
    stream.control().arm()?;

    loop {
        let req = stream.next_request()?;

        // Generate points for this chunk
        let points = create_points(args.shape, &req);

        stream.write(&req, &points)?;
    }
}
