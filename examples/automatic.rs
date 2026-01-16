//! Automatic discovery example using DacDiscoveryWorker.
//!
//! This example demonstrates background discovery that automatically
//! finds and connects to DAC devices. The worker runs in a background
//! thread and yields ready-to-use Device instances.
//!
//! Run with: `cargo run --example automatic -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{create_points, Args};
use laser_dac::{DacDiscoveryWorker, EnabledDacTypes, StreamConfig, Result};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Starting background discovery...\n");

    // Create a discovery worker that scans in the background
    let discovery = DacDiscoveryWorker::builder()
        .enabled_types(EnabledDacTypes::all())
        .build();

    // Wait for a device to be discovered and connected
    println!("Scanning for DACs (5 seconds)...\n");
    let mut device = None;
    for _ in 0..50 {
        // poll_new_devices() returns already-connected Device instances
        if let Some(d) = discovery.poll_new_devices().next() {
            device = Some(d);
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let Some(device) = device else {
        println!("No DACs found.");
        return Ok(());
    };

    println!("  Connected: {} ({})", device.info().name, device.info().kind);

    // Start streaming
    let config = StreamConfig::new(30_000);
    let (mut stream, info) = device.start_stream(config)?;

    println!(
        "\nStreaming {} to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        info.name
    );

    // Arm the output
    stream.control().arm()?;

    loop {
        let req = stream.next_request()?;
        let points = create_points(args.shape, &req);
        stream.write(&req, &points)?;
    }
}
