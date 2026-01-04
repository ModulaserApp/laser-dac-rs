//! Callback-based example using DacCallbackWorker.
//!
//! This demonstrates the callback/pull approach where the DAC drives timing.
//! The callback is invoked whenever the device is ready for more data.
//!
//! Run with: `cargo run --example callback -- [triangle|circle]`

mod common;

use clap::Parser;
use common::{create_frame, Args};
use laser_dac::discovery::DacDiscovery;
use laser_dac::{CallbackError, DacCallbackWorker, EnabledDacTypes, Result};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

    // Track total frames written across all workers
    let total_frames = Arc::new(AtomicU64::new(0));

    let mut workers = Vec::new();
    for device in devices {
        let device_name = device.name().to_string();
        let device_type = device.dac_type();
        println!("  Found: {} ({})", device_name, device_type);

        let backend = discovery.connect(device)?;

        // Phase 1: Create worker (connected but not running)
        let mut worker = DacCallbackWorker::new(device_name.clone(), device_type, backend);

        // Phase 2: Start with callbacks
        let shape = args.shape;
        let min_points = args.min_points;
        let counter = Arc::clone(&total_frames);
        let name_for_error = device_name;

        worker.start(
            // Data callback - invoked when device needs more data
            move |ctx| {
                counter.fetch_add(1, Ordering::Relaxed);
                let frame = create_frame(shape, min_points, ctx.frames_written as usize);
                Some(frame)
            },
            // Error callback - invoked on connection loss or panic
            move |err: CallbackError| {
                eprintln!("Error on {}: {:?}", name_for_error, err);
            },
        );

        workers.push(worker);
    }

    println!(
        "\nStreaming {} via callback... Press Ctrl+C to stop\n",
        args.shape.name()
    );

    // Main loop just monitors status - the callbacks do the real work
    loop {
        let mut any_alive = false;
        for worker in &mut workers {
            worker.update();
            if worker.is_running() {
                any_alive = true;
            }
        }

        // Print frame count periodically
        let frames = total_frames.load(Ordering::Relaxed);
        print!("\rFrames sent: {}", frames);

        if !any_alive {
            println!("\nAll workers stopped.");
            break;
        }

        thread::sleep(Duration::from_millis(100));
    }

    Ok(())
}
