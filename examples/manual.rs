//! Manual discovery example using DacDiscovery.
//!
//! This gives you more control over device discovery and connection,
//! while still using the unified abstraction layer.
//!
//! Run with: `cargo run --example manual -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{create_frame, Args};
use laser_dac::discovery::DacDiscovery;
use laser_dac::{DacWorker, EnabledDacTypes, Result};
use std::thread;
use std::time::Duration;

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Scanning for DACs...\n");
    let mut discovery = DacDiscovery::new(EnabledDacTypes::all());
    let devices = discovery.scan();

    if devices.is_empty() {
        println!("No DACs found.");
        return Ok(());
    }

    let mut workers = Vec::new();
    for device in devices {
        let device_name = device.name().to_string();
        let device_type = device.dac_type();
        println!("  Found: {} ({})", device_name, device_type);

        let backend = discovery.connect(device)?;
        workers.push(DacWorker::new(device_name, device_type, backend));
    }

    println!("\nSending {}... Press Ctrl+C to stop\n", args.shape.name());

    let mut frame_count = 0usize;
    loop {
        let frame = create_frame(args.shape, args.min_points, frame_count);
        let mut any_accepted = false;
        for worker in &mut workers {
            worker.update();
            if worker.submit_frame(frame.clone()) {
                any_accepted = true;
            }
        }
        if any_accepted {
            frame_count = frame_count.wrapping_add(1);
        }
        thread::sleep(Duration::from_millis(33));
    }
}
