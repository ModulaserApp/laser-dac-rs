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
use common::Args;
use laser_dac::{
    list_devices, open_device, Frame, FrameAdapter, LaserPoint, StreamConfig, Result,
};
use std::f32::consts::PI;
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
    let initial_frame = create_frame(args.shape, 0);
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
            let new_frame = create_frame(args.shape, frame_count);
            adapter.update_frame(new_frame);
        }
    }
}

/// Create a frame with a fixed number of points for the given shape.
///
/// Unlike the streaming examples where we generate exactly N points per request,
/// here we create a complete frame that the adapter will cycle through.
fn create_frame(shape: common::Shape, frame_count: usize) -> Frame {
    let points = match shape {
        common::Shape::Triangle => create_triangle(),
        common::Shape::Circle => create_circle(),
        common::Shape::OrbitingCircle => create_orbiting_circle(frame_count),
        common::Shape::TestPattern => create_triangle(), // Fallback
    };

    // 30 FPS conceptual frame rate
    Frame::new(points, 30.0)
}

/// Create a triangle with a fixed number of points.
fn create_triangle() -> Vec<LaserPoint> {
    let vertices = [
        (-0.5_f32, -0.5_f32, 65535_u16, 0_u16, 0_u16),
        (0.5_f32, -0.5_f32, 0_u16, 65535_u16, 0_u16),
        (0.0_f32, 0.5_f32, 0_u16, 0_u16, 65535_u16),
    ];

    const BLANK_COUNT: usize = 5;
    const DWELL_COUNT: usize = 3;
    const POINTS_PER_EDGE: usize = 100;

    let mut points = Vec::new();

    // Blanking points to start position
    for _ in 0..BLANK_COUNT {
        points.push(LaserPoint::blanked(vertices[0].0, vertices[0].1));
    }

    // Draw each edge
    for edge in 0..3 {
        let (x1, y1, r1, g1, b1) = vertices[edge];
        let (x2, y2, r2, g2, b2) = vertices[(edge + 1) % 3];

        // Dwell at corner
        for _ in 0..DWELL_COUNT {
            points.push(LaserPoint::new(x1, y1, r1, g1, b1, 65535));
        }

        // Interpolate along edge
        for i in 1..=POINTS_PER_EDGE {
            let t = i as f32 / POINTS_PER_EDGE as f32;
            let x = x1 + (x2 - x1) * t;
            let y = y1 + (y2 - y1) * t;
            let r = (r1 as f32 + (r2 as f32 - r1 as f32) * t) as u16;
            let g = (g1 as f32 + (g2 as f32 - g1 as f32) * t) as u16;
            let b = (b1 as f32 + (b2 as f32 - b1 as f32) * t) as u16;
            points.push(LaserPoint::new(x, y, r, g, b, 65535));
        }
    }

    // Dwell at final corner to close
    let (x, y, r, g, b) = vertices[0];
    for _ in 0..DWELL_COUNT {
        points.push(LaserPoint::new(x, y, r, g, b, 65535));
    }

    points
}

/// Create a rainbow circle with a fixed number of points.
fn create_circle() -> Vec<LaserPoint> {
    const NUM_POINTS: usize = 200;
    const BLANK_COUNT: usize = 5;

    let mut points = Vec::with_capacity(NUM_POINTS + BLANK_COUNT);

    // Blanking points
    for _ in 0..BLANK_COUNT {
        points.push(LaserPoint::blanked(0.5, 0.0));
    }

    // Draw circle
    for i in 0..=NUM_POINTS {
        let angle = (i as f32 / NUM_POINTS as f32) * 2.0 * PI;
        let x = 0.5 * angle.cos();
        let y = 0.5 * angle.sin();

        let hue = i as f32 / NUM_POINTS as f32;
        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);

        points.push(LaserPoint::new(x, y, r, g, b, 65535));
    }

    points
}

/// Create an orbiting circle that moves based on frame count.
fn create_orbiting_circle(frame_count: usize) -> Vec<LaserPoint> {
    const NUM_POINTS: usize = 150;
    const BLANK_COUNT: usize = 5;
    const CIRCLE_RADIUS: f32 = 0.15;
    const ORBIT_RADIUS: f32 = 0.4;
    const ORBIT_SPEED: f32 = 0.05;

    // Calculate orbit position
    let orbit_angle = frame_count as f32 * ORBIT_SPEED;
    let center_x = ORBIT_RADIUS * orbit_angle.cos();
    let center_y = ORBIT_RADIUS * orbit_angle.sin();

    let mut points = Vec::with_capacity(NUM_POINTS + BLANK_COUNT);

    // Blanking to start position
    let start_x = center_x + CIRCLE_RADIUS;
    let start_y = center_y;
    for _ in 0..BLANK_COUNT {
        points.push(LaserPoint::blanked(start_x, start_y));
    }

    // Draw circle at orbit position
    for i in 0..=NUM_POINTS {
        let angle = (i as f32 / NUM_POINTS as f32) * 2.0 * PI;
        let x = center_x + CIRCLE_RADIUS * angle.cos();
        let y = center_y + CIRCLE_RADIUS * angle.sin();

        let hue = i as f32 / NUM_POINTS as f32;
        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);

        points.push(LaserPoint::new(x, y, r, g, b, 65535));
    }

    points
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u16, u16, u16) {
    let h = h * 6.0;
    let i = h.floor() as i32;
    let f = h - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));

    let (r, g, b) = match i % 6 {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };

    (
        (r * 65535.0) as u16,
        (g * 65535.0) as u16,
        (b * 65535.0) as u16,
    )
}
