//! Frame adapter example demonstrating frame-based streaming.
//!
//! This example shows how to use `FrameAdapter` to convert frame-based
//! content to the streaming API. This is useful when you have a fixed
//! frame (like from a render loop) that should be cycled at the DAC's
//! point rate.
//!
//! Run with: `cargo run --example frame_adapter -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{create_points, Args};
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

    // Create a frame adapter in "latest frame" mode
    // The adapter will cycle through the frame's points at the DAC's point rate
    let mut adapter = FrameAdapter::latest(30.0);

    // Create the initial frame based on shape
    let initial_frame = create_frame(args.shape, args.min_points, 0);
    adapter.update_frame(initial_frame);

    // Arm the output
    stream.control().arm()?;

    let mut frame_count = 0usize;
    loop {
        // Get chunk request from the stream
        let req = stream.next_request()?;

        // The adapter produces exactly req.n_points by cycling through the frame
        let points = adapter.next_chunk(&req);
        stream.write(&req, &points)?;

        // Update frame periodically for animated shapes
        frame_count = frame_count.wrapping_add(1);
        if frame_count % 10 == 0 {
            let new_frame = create_frame(args.shape, args.min_points, frame_count);
            adapter.update_frame(new_frame);
        }
    }
}

/// Create a frame with a fixed number of points for the given shape.
///
/// Unlike the streaming examples where we generate exactly N points per request,
/// here we create a complete frame that the adapter will cycle through.
fn create_frame(shape: common::Shape, n_points: usize, frame_count: usize) -> Frame {
    let points = create_points(shape, n_points, frame_count);
    // 30 FPS conceptual frame rate
    Frame::new(points, 30.0)
}
