//! Transition blanking test — alternates between two shapes at different positions.
//!
//! Visually tests the `TransitionFn` callback in `FrameSession`. Two shapes swap
//! every 50ms so transition blanking is clearly visible (~20 transitions/second).
//!
//! - **Pass**: clean dark travel between shapes
//! - **Fail**: bright line or flash connecting the two shapes
//!
//! Modes:
//! - `default`  — distance-scaled dwell→travel→dwell transition
//! - `none`     — zero transition points (expect bright flash + galvo stress!)
//! - `animated` — 3 shapes cycling every 2 seconds
//!
//! Run with: `cargo run --example transitions -- [default|none|animated]`

mod common;

use clap::Parser;
use laser_dac::{
    default_transition, list_devices, open_device, Frame, FrameSessionConfig, LaserPoint, Result,
    TransitionFn, TransitionPlan,
};
use std::f32::consts::PI;
use std::thread;
use std::time::Duration;

#[derive(Parser)]
#[command(about = "Test transition blanking between frames")]
struct Args {
    /// Transition mode
    #[arg(value_enum, default_value_t = Mode::Default)]
    mode: Mode,

    /// Points per shape
    #[arg(short, long, default_value_t = 200)]
    points: usize,
}

#[derive(Copy, Clone, clap::ValueEnum)]
enum Mode {
    /// Distance-scaled dwell→travel→dwell transition (recommended)
    Default,
    /// No transition points (expect bright flash and galvo stress!)
    None,
    /// Animated: cycle through 3 frames every 2 seconds
    Animated,
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Scanning for DACs...\n");

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

    let device_info = &devices[0];
    let device = open_device(&device_info.id)?;

    let transition_fn: TransitionFn = match args.mode {
        Mode::Default | Mode::Animated => Box::new(default_transition),
        Mode::None => Box::new(|_: &LaserPoint, _: &LaserPoint| TransitionPlan::Transition(vec![])),
    };

    let config = FrameSessionConfig::new(30_000)
        .with_transition_fn(transition_fn)
        .with_startup_blank(Duration::from_millis(1));

    let (session, info) = device.start_frame_session(config)?;

    let mode_name = match args.mode {
        Mode::Default => "default (distance-scaled dwell→travel→dwell)",
        Mode::None => "none (expect bright flash + galvo stress!)",
        Mode::Animated => "animated (3 shapes cycling)",
    };
    println!(
        "\nTransition test [{}] on {}... Press Ctrl+C to stop\n",
        mode_name, info.name
    );

    session.control().arm()?;

    // Install Ctrl+C handler
    let control = session.control();
    ctrlc::set_handler(move || {
        let _ = control.stop();
    })
    .expect("failed to set Ctrl+C handler");

    let n = args.points;

    if matches!(args.mode, Mode::Animated) {
        let frames = vec![
            make_circle(n, -0.4, 0.25, 0.2, 65535, 0, 0), // red, top-left
            make_circle(n, 0.4, 0.25, 0.2, 0, 65535, 0),  // green, top-right
            make_circle(n, 0.0, -0.35, 0.2, 0, 0, 65535), // blue, bottom-center
        ];

        println!("  Cycling 3 colored circles every 2 seconds.");
        println!("  Watch for clean blanked transitions between positions.\n");

        let mut idx = 0;
        loop {
            session.send_frame(frames[idx].clone());
            idx = (idx + 1) % frames.len();

            for _ in 0..40 {
                thread::sleep(Duration::from_millis(50));
                if session.control().is_stop_requested() {
                    let exit = session.join()?;
                    println!("\nSession ended: {:?}", exit);
                    return Ok(());
                }
            }
        }
    } else {
        // Two shapes — alternates every 50ms (~20 swaps/second).
        // Shapes are 0.6 units apart (moderate distance).
        let frame_left = Frame::new(make_triangle(n, -0.3, 0.0, 0.35));
        let frame_right = make_circle(n, 0.3, 0.0, 0.18, 0, 65535, 0);

        println!("  Alternating: red triangle (left) ↔ green circle (right)");
        println!("  Swapping every ~50ms — watch the gap between shapes.\n");

        if matches!(args.mode, Mode::None) {
            println!("  !! Mode=none: you SHOULD see a bright flash between shapes.");
            println!("  !! Stop quickly if galvos sound stressed.\n");
        }

        let frames = [frame_left, frame_right];
        let mut idx = 0;
        loop {
            session.send_frame(frames[idx].clone());
            idx = (idx + 1) % frames.len();

            thread::sleep(Duration::from_millis(50));
            if session.control().is_stop_requested() {
                let exit = session.join()?;
                println!("\nSession ended: {:?}", exit);
                return Ok(());
            }
        }
    }
}

/// Generate a circle at an offset position.
fn make_circle(n: usize, cx: f32, cy: f32, radius: f32, r: u16, g: u16, b: u16) -> Frame {
    Frame::new(make_circle_points(n, cx, cy, radius, r, g, b))
}

fn make_circle_points(
    n: usize,
    cx: f32,
    cy: f32,
    radius: f32,
    r: u16,
    g: u16,
    b: u16,
) -> Vec<LaserPoint> {
    (0..n)
        .map(|i| {
            let angle = (i as f32 / n as f32) * 2.0 * PI;
            let x = (cx + radius * angle.cos()).clamp(-1.0, 1.0);
            let y = (cy + radius * angle.sin()).clamp(-1.0, 1.0);
            LaserPoint::new(x, y, r, g, b, 65535)
        })
        .collect()
}

/// Generate a triangle at an offset position.
fn make_triangle(n: usize, cx: f32, cy: f32, scale: f32) -> Vec<LaserPoint> {
    let vertices = [
        (cx - 0.5 * scale, cy - 0.4 * scale, 65535u16, 0u16, 0u16),
        (cx + 0.5 * scale, cy - 0.4 * scale, 65535, 0, 0),
        (cx, cy + 0.4 * scale, 65535, 0, 0),
    ];

    let points_per_edge = n / 3;
    let mut points = Vec::with_capacity(n);

    for edge in 0..3 {
        let (x1, y1, r, g, b) = vertices[edge];
        let (x2, y2, _, _, _) = vertices[(edge + 1) % 3];

        for i in 0..points_per_edge {
            let t = i as f32 / points_per_edge as f32;
            let x = (x1 + (x2 - x1) * t).clamp(-1.0, 1.0);
            let y = (y1 + (y2 - y1) * t).clamp(-1.0, 1.0);
            points.push(LaserPoint::new(x, y, r, g, b, 65535));
        }
    }

    points
}
