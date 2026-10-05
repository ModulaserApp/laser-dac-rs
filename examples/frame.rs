//! Frame API example.
//!
//! Demonstrates the recommended frame-first workflow: discover a device,
//! start a frame session, and submit frames. The shape orbits around the
//! center, showing how `send_frame()` updates the output in real time.
//! Test patterns are the exception: they play raw (no orbit, no transition
//! blanking) so the scanner sees exactly the authored points.
//!
//! The library handles looping (each frame repeats until replaced),
//! transition blanking, and transport-appropriate delivery.
//!
//! Run with: `cargo run --example frame -- [triangle|circle|orientation|test-pattern|ilda-test-pattern]`

mod common;

use clap::Parser;
use common::{generate_frame, select_device, Args};
use laser_dac::{
    list_devices, open_device, Frame, FrameSessionConfig, LaserPoint, Result, TransitionPlan,
};
use std::thread;
use std::time::{Duration, Instant};

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Scanning for DACs...\n");
    let devices = list_devices()?;

    if devices.is_empty() {
        println!("No DACs found.");
        return Ok(());
    }

    let Some(device_info) = select_device(&devices, args.device.as_deref()) else {
        println!("No DAC matching {:?}.", args.device.unwrap_or_default());
        return Ok(());
    };
    println!("  Found: {} ({})", device_info.name, device_info.kind);

    let device = open_device(&device_info.id)?;

    let mut config = FrameSessionConfig::new(30_000);
    if args.shape.is_raw() {
        config = config.with_transition_fn(Box::new(|_: &LaserPoint, _: &LaserPoint, _| {
            TransitionPlan::Transition(vec![])
        }));
    }
    let (session, info) = device.start_frame_session(config)?;

    println!(
        "\nStreaming {} via Frame API to {}... Press Ctrl+C to stop\n",
        args.shape.name(),
        info.name
    );

    session.control().arm()?;

    // Install Ctrl+C handler
    let control = session.control();
    ctrlc::set_handler(move || {
        let _ = control.stop();
    })
    .expect("failed to set Ctrl+C handler");

    // Generate the base shape once
    let base_frame = generate_frame(args.shape, args.points, args.scale);

    // Raw patterns are sent once; the library loops the frame until stopped.
    if args.shape.is_raw() {
        session.send_frame(Frame::new(base_frame));
        // Also exit if the session ends on its own (device error, disconnect).
        while !session.control().is_stop_requested() && !session.is_finished() {
            thread::sleep(Duration::from_millis(16));
        }
        let exit = session.join()?;
        println!("\nSession ended: {:?}", exit);
        return Ok(());
    }

    // Submit frames at ~60fps — the shape orbits around the center.
    // Each send_frame() replaces the current output; the library loops
    // the latest frame until a new one arrives.
    let start = Instant::now();
    loop {
        let t = start.elapsed().as_secs_f32();
        let cx = 0.3 * (t * 0.5).cos();
        let cy = 0.3 * (t * 0.5).sin();

        let mut points = base_frame.clone();
        for p in &mut points {
            p.x = (p.x + cx).clamp(-1.0, 1.0);
            p.y = (p.y + cy).clamp(-1.0, 1.0);
        }

        session.send_frame(Frame::new(points));

        thread::sleep(Duration::from_millis(16));
        if session.control().is_stop_requested() || session.is_finished() {
            break;
        }
    }

    let exit = session.join()?;
    println!("\nSession ended: {:?}", exit);
    Ok(())
}
