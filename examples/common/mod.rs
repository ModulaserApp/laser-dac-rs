//! Shared code for examples.

pub mod audio;

use clap::{Parser, ValueEnum};
use laser_dac::{ChunkRequest, LaserPoint, StreamInstant};
use serde::Deserialize;
use std::f32::consts::{PI, TAU};

#[derive(Parser)]
#[command(about = "Send test patterns to connected laser DACs")]
pub struct Args {
    /// Shape to display
    #[arg(value_enum, default_value_t = Shape::Triangle)]
    pub shape: Shape,

    /// Minimum number of points per frame
    #[arg(short, long, default_value_t = 200)]
    pub min_points: usize,
}

#[derive(Copy, Clone, ValueEnum)]
pub enum Shape {
    Triangle,
    Circle,
    OrbitingCircle,
    TestPattern,
    Audio,
}

impl Shape {
    pub fn name(&self) -> &'static str {
        match self {
            Shape::Triangle => "triangle",
            Shape::Circle => "circle",
            Shape::OrbitingCircle => "orbiting-circle",
            Shape::TestPattern => "test-pattern",
            Shape::Audio => "audio",
        }
    }
}

/// Create points for the given shape (streaming API).
///
/// The `req` parameter provides timing info for time-based shapes.
pub fn create_points(shape: Shape, req: &ChunkRequest) -> Vec<LaserPoint> {
    let n_points = req.n_points;
    let mut points = match shape {
        Shape::Triangle => create_triangle_points(n_points),
        Shape::Circle => create_circle_points(n_points),
        Shape::OrbitingCircle => create_orbiting_circle_points(req),
        Shape::TestPattern => create_test_pattern_points(n_points),
        Shape::Audio => audio::create_audio_points(n_points),
    };

    // Ensure exactly n_points are returned (pad or truncate as needed)
    normalize_point_count(&mut points, n_points);
    points
}

/// Create points for frame-based usage (no stream timing).
///
/// For shapes that need stream time (OrbitingCircle), this produces
/// a static frame at t=0. Use `create_points` with a real ChunkRequest
/// for time-based animation.
#[allow(dead_code)] // Used by frame_adapter example, not all examples
pub fn create_frame_points(shape: Shape, n_points: usize) -> Vec<LaserPoint> {
    // Create a dummy request for frame-based usage
    let dummy_req = ChunkRequest {
        start: StreamInstant::new(0),
        pps: 30_000,
        n_points,
        scheduled_ahead_points: 0,
        device_queued_points: None,
    };

    create_points(shape, &dummy_req)
}

/// Normalize a point vector to exactly `target` points.
///
/// If there are too few points, pads with the last point (or blanked origin).
/// If there are too many points, truncates.
fn normalize_point_count(points: &mut Vec<LaserPoint>, target: usize) {
    match points.len().cmp(&target) {
        std::cmp::Ordering::Less => {
            // Pad with last point (or blanked origin if empty)
            let pad_point = points
                .last()
                .copied()
                .unwrap_or(LaserPoint::blanked(0.0, 0.0));
            points.resize(target, pad_point);
        }
        std::cmp::Ordering::Greater => {
            points.truncate(target);
        }
        std::cmp::Ordering::Equal => {}
    }
}

/// Create an RGB triangle with proper blanking and interpolation.
fn create_triangle_points(min_points: usize) -> Vec<LaserPoint> {
    let vertices = [
        (-0.5_f32, -0.5_f32, 65535_u16, 0_u16, 0_u16),
        (0.5_f32, -0.5_f32, 0_u16, 65535_u16, 0_u16),
        (0.0_f32, 0.5_f32, 0_u16, 0_u16, 65535_u16),
    ];

    const BLANK_COUNT: usize = 5;
    const DWELL_COUNT: usize = 3;

    let fixed_points = BLANK_COUNT + 3 * DWELL_COUNT + DWELL_COUNT;
    let points_per_edge = ((min_points.saturating_sub(fixed_points)) / 3).max(20);

    let mut points = Vec::with_capacity(min_points);

    for _ in 0..BLANK_COUNT {
        points.push(LaserPoint::blanked(vertices[0].0, vertices[0].1));
    }

    for edge in 0..3 {
        let (x1, y1, r1, g1, b1) = vertices[edge];
        let (x2, y2, r2, g2, b2) = vertices[(edge + 1) % 3];

        for _ in 0..DWELL_COUNT {
            points.push(LaserPoint::new(x1, y1, r1, g1, b1, 65535));
        }

        for i in 1..=points_per_edge {
            let t = i as f32 / points_per_edge as f32;
            let x = x1 + (x2 - x1) * t;
            let y = y1 + (y2 - y1) * t;
            let r = (r1 as f32 + (r2 as f32 - r1 as f32) * t) as u16;
            let g = (g1 as f32 + (g2 as f32 - g1 as f32) * t) as u16;
            let b = (b1 as f32 + (b2 as f32 - b1 as f32) * t) as u16;
            points.push(LaserPoint::new(x, y, r, g, b, 65535));
        }
    }

    let (x, y, r, g, b) = vertices[0];
    for _ in 0..DWELL_COUNT {
        points.push(LaserPoint::new(x, y, r, g, b, 65535));
    }

    points
}

/// Create a rainbow circle.
fn create_circle_points(num_points: usize) -> Vec<LaserPoint> {
    let mut points = Vec::with_capacity(num_points + 10);

    const BLANK_COUNT: usize = 5;

    for _ in 0..BLANK_COUNT {
        points.push(LaserPoint::blanked(0.5, 0.0));
    }

    for i in 0..=num_points {
        let angle = (i as f32 / num_points as f32) * 2.0 * PI;
        let x = 0.5 * angle.cos();
        let y = 0.5 * angle.sin();

        let hue = i as f32 / num_points as f32;
        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);

        points.push(LaserPoint::new(x, y, r, g, b, 65535));
    }

    points
}

/// Create a time-based orbiting circle with smooth continuous motion.
///
/// Each point's position is calculated from its exact stream timestamp,
/// so the orbit advances smoothly even within a single chunk.
/// No blanking - the beam continuously traces the circle as it orbits.
fn create_orbiting_circle_points(req: &ChunkRequest) -> Vec<LaserPoint> {
    const ORBIT_RADIUS: f32 = 0.5;
    const CIRCLE_RADIUS: f32 = 0.15;
    const ORBIT_PERIOD_SECS: f32 = 4.0;
    const POINTS_PER_CIRCLE: usize = 200;

    let mut points = Vec::with_capacity(req.n_points);
    let pps = req.pps as f64;

    for i in 0..req.n_points {
        let point_index = req.start.0 + i as u64;
        let t_secs = point_index as f64 / pps;

        // Orbit position based on exact time (continuous, not stepped)
        let orbit_angle = (t_secs as f32 / ORBIT_PERIOD_SECS) * TAU;
        let center_x = ORBIT_RADIUS * orbit_angle.cos();
        let center_y = ORBIT_RADIUS * orbit_angle.sin();

        // Circle position based on point index
        let circle_angle =
            (point_index % POINTS_PER_CIRCLE as u64) as f32 / POINTS_PER_CIRCLE as f32 * TAU;
        let x = center_x + CIRCLE_RADIUS * circle_angle.cos();
        let y = center_y + CIRCLE_RADIUS * circle_angle.sin();

        // Rainbow color that shifts with time
        let hue = (t_secs as f32 / 3.0) % 1.0;
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

#[derive(Deserialize)]
struct PatternPoint {
    x: f32,
    y: f32,
    r: u8,
    g: u8,
    b: u8,
}

fn create_test_pattern_points(n_points: usize) -> Vec<LaserPoint> {
    let json_str = include_str!("test-pattern.json");
    let pattern_points: Vec<PatternPoint> = serde_json::from_str(json_str).unwrap();

    // Scale to requested number of points by repeating/truncating
    let mut points: Vec<LaserPoint> = pattern_points
        .into_iter()
        .map(|p| {
            LaserPoint::new(
                p.x,
                p.y,
                p.r as u16 * 257,
                p.g as u16 * 257,
                p.b as u16 * 257,
                65535,
            )
        })
        .collect();

    // Repeat pattern to fill requested points
    if points.len() < n_points {
        let original = points.clone();
        while points.len() < n_points {
            for p in &original {
                points.push(*p);
                if points.len() >= n_points {
                    break;
                }
            }
        }
    }

    points.truncate(n_points);
    points
}
