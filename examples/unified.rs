//! Unified example demonstrating the laser-dac crate.
//!
//! This example shows how to:
//! - Discover DAC devices using DacDiscoveryWorker
//! - Create laser frames with normalized coordinates
//! - Send frames to connected devices
//!
//! Run with: `cargo run --example unified`

use laser_dac::{DacDiscoveryWorker, DacType, EnabledDacTypes, LaserFrame, LaserPoint};
use std::f32::consts::PI;
use std::thread;
use std::time::Duration;

fn main() {
    println!("laser-dac unified example\n");

    // Display supported DAC types
    println!("Supported DAC types:");
    for dac_type in DacType::all() {
        println!(
            "  - {} ({})",
            dac_type.display_name(),
            dac_type.description()
        );
    }
    println!();

    // Create discovery worker (discovers all DAC types by default)
    println!("Starting DAC discovery...");
    let discovery = DacDiscoveryWorker::new(EnabledDacTypes::all());

    // Collect discovered workers
    let mut workers = Vec::new();

    println!("Scanning for DACs for 5 seconds...\n");
    for _ in 0..50 {
        // Poll for newly discovered devices
        for worker in discovery.poll_new_workers() {
            println!("  Found: {} ({})", worker.device_name(), worker.dac_type());
            workers.push(worker);
        }
        thread::sleep(Duration::from_millis(100));
    }

    if workers.is_empty() {
        println!("No DACs found. Make sure devices are connected and powered on.\n");
        println!("Example would continue here with connected devices...\n");

        // Show what we would do with connected devices
        show_api_usage();
        return;
    }

    println!(
        "\nFound {} device(s). Sending test frames...\n",
        workers.len()
    );

    // Create test frames
    let triangle_frame = create_triangle_frame();
    let circle_frame = create_circle_frame(100);

    println!(
        "Triangle frame: {} points at {} pps",
        triangle_frame.points.len(),
        triangle_frame.pps
    );
    println!(
        "Circle frame: {} points at {} pps\n",
        circle_frame.points.len(),
        circle_frame.pps
    );

    // Send frames to all connected DACs
    println!("Sending frames for 5 seconds...");
    let mut frame_count = 0;
    for _ in 0..150 {
        for worker in &mut workers {
            worker.update(); // Check for status updates

            // Alternate between triangle and circle
            let frame = if frame_count % 60 < 30 {
                &triangle_frame
            } else {
                &circle_frame
            };

            worker.submit_frame(frame.clone());
        }
        frame_count += 1;
        thread::sleep(Duration::from_millis(33)); // ~30 fps
    }

    // Stop output on all devices
    println!("\nStopping output...");
    for worker in &workers {
        worker.stop_output();
    }

    println!("Done!");
}

/// Show API usage when no devices are connected.
fn show_api_usage() {
    println!("API usage examples:\n");

    println!("1. High-level API (DacDiscoveryWorker):");
    println!(
        r#"    let discovery = DacDiscoveryWorker::new(EnabledDacTypes::all());
    for worker in discovery.poll_new_workers() {{
        worker.submit_frame(frame.clone());
    }}"#
    );
    println!();

    println!("2. Low-level API (UnifiedDiscovery):");
    println!(
        r#"    let mut discovery = UnifiedDiscovery::new(EnabledDacTypes::all());
    for device in discovery.scan() {{
        let backend = discovery.connect(device)?;
        let worker = DacWorker::new(name, dac_type, backend);
    }}"#
    );
    println!();

    println!("3. Direct backend usage:");
    println!(
        r#"    backend.write_frame(&frame)?;  // Returns WriteResult::Written or DeviceBusy
    backend.stop();                   // Stop laser output"#
    );
}

/// Create a simple RGB triangle frame.
fn create_triangle_frame() -> LaserFrame {
    let points = vec![
        // Move to first vertex (blanked)
        LaserPoint::blanked(-0.5, -0.5),
        // Red vertex (bottom-left)
        LaserPoint::new(-0.5, -0.5, 255, 0, 0, 255),
        // Green vertex (bottom-right)
        LaserPoint::new(0.5, -0.5, 0, 255, 0, 255),
        // Blue vertex (top)
        LaserPoint::new(0.0, 0.5, 0, 0, 255, 255),
        // Back to red vertex to close
        LaserPoint::new(-0.5, -0.5, 255, 0, 0, 255),
    ];

    LaserFrame::new(30000, points)
}

/// Create a circle frame with the specified number of points.
fn create_circle_frame(num_points: usize) -> LaserFrame {
    let mut points = Vec::with_capacity(num_points + 2);

    // Move to starting position (blanked)
    points.push(LaserPoint::blanked(0.5, 0.0));

    // Draw circle
    for i in 0..=num_points {
        let angle = (i as f32 / num_points as f32) * 2.0 * PI;
        let x = 0.5 * angle.cos();
        let y = 0.5 * angle.sin();

        // Rainbow colors based on position
        let hue = i as f32 / num_points as f32;
        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);

        points.push(LaserPoint::new(x, y, r, g, b, 255));
    }

    LaserFrame::new(30000, points)
}

/// Convert HSV to RGB (simple implementation for the example).
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
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

    ((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8)
}
