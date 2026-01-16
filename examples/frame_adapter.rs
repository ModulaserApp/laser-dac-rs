//! Frame adapter example demonstrating static point buffer streaming.
//!
//! This example shows how to use `FrameAdapter` to convert a static point
//! buffer (frame) into a continuous stream. The adapter cycles through the
//! frame's points at the DAC's point rate.
//!
//! Use this when you have pre-computed geometry that should loop continuously.
//! For time-varying animated content, use the streaming API directly (see
//! the `automatic` example with `orbiting-circle`).
//!
//! Run with: `cargo run --example frame_adapter -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{create_frame_points, Args};
use laser_dac::{list_devices, open_device, Frame, FrameAdapter, StreamConfig, Result};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Scanning for DACs (5 seconds)...\n");

    // Wait for devices to be discovered
    let mut devices = Vec::new();
    for _ in 0..50 {
        devices = list_devices()?;
        if !devices.is_empty() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    if devices.is_empty() {
        println!("No DACs found.");
        return Ok(());
    }

    for device in &devices {
        println!("  Found: {} ({})", device.name, device.kind);
    }

    // Open first device
    let device_info = &devices[0];
    let device = open_device(&device_info.id)?;

    // Start streaming
    let config = StreamConfig::new(30_000);
    let (mut stream, info) = device.start_stream(config)?;

    println!(
        "\nStreaming {} via FrameAdapter to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        info.name
    );

    // Create a frame adapter
    let mut adapter = FrameAdapter::new();

    // Create the frame - a static point buffer that will be cycled
    let points = create_frame_points(args.shape, args.min_points);
    adapter.update(Frame::new(points));

    // Arm the output
    stream.control().arm()?;

    // Stream loop - the adapter cycles through the frame continuously
    loop {
        let req = stream.next_request()?;
        let points = adapter.next_chunk(&req);
        stream.write(&req, &points)?;
    }
}
