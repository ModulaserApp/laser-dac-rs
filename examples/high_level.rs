//! High-level API example using DacDiscoveryWorker.
//!
//! This is the easiest way to use this crate - background discovery
//! automatically finds and connects to DAC devices.
//!
//! Run with: `cargo run --example high_level -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{create_frame, Args};
use laser_dac::{DacDiscoveryWorker, EnabledDacTypes};
use std::thread;
use std::time::Duration;

fn main() {
    env_logger::init();
    let args = Args::parse();

    println!("Starting DAC discovery...");
    let discovery = DacDiscoveryWorker::new(EnabledDacTypes::all());

    let mut workers = Vec::new();
    println!("Scanning for 5 seconds...\n");
    for _ in 0..50 {
        for worker in discovery.poll_new_workers() {
            println!("  Found: {} ({})", worker.device_name(), worker.dac_type());
            workers.push(worker);
        }
        thread::sleep(Duration::from_millis(100));
    }

    if workers.is_empty() {
        println!("No DACs found.");
        return;
    }

    let frame = create_frame(args.shape, args.min_points);
    println!("\nSending {}... Press Ctrl+C to stop\n", args.shape.name());

    loop {
        for worker in &mut workers {
            worker.update();
            worker.submit_frame(frame.clone());
        }
        thread::sleep(Duration::from_millis(33));
    }
}
