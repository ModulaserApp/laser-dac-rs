//! Shared code for examples.

pub mod audio;

use clap::{Parser, ValueEnum};
use laser_dac::{FillRequest, FillResult, LaserPoint, StreamInstant};
use serde::Deserialize;
use std::f32::consts::{PI, TAU};
use std::time::Duration;

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
    #[allow(dead_code)] // Used by stream/callback/frame_adapter examples, not audio
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

/// Fill points for the given shape into the buffer (streaming API).
///
/// The `req` parameter provides timing info for time-based shapes.
/// Returns `FillResult::Filled(n)` where n is the number of points written.
pub fn fill_points(shape: Shape, req: &FillRequest, buffer: &mut [LaserPoint]) -> FillResult {
    let n_points = req.target_points.min(buffer.len());
    match shape {
        Shape::Triangle => fill_triangle_points(buffer, n_points),
        Shape::Circle => fill_circle_points(buffer, n_points),
        Shape::OrbitingCircle => fill_orbiting_circle_points(req, buffer, n_points),
        Shape::TestPattern => fill_test_pattern_points(buffer, n_points),
        Shape::Audio => audio::fill_audio_points(req, buffer, n_points, &audio::AudioConfig::default()),
    }
    FillResult::Filled(n_points)
}

/// Create points for frame-based usage (no stream timing).
///
/// For shapes that need stream time (OrbitingCircle, Audio), this produces
/// a static frame at t=0. Use `fill_points` with a real FillRequest
/// for proper time-based animation.
#[allow(dead_code)] // Used by frame_adapter example, not all examples
pub fn create_frame_points(shape: Shape, n_points: usize) -> Vec<LaserPoint> {
    // Create a dummy request for frame-based usage
    let dummy_req = FillRequest {
        start: StreamInstant::new(0),
        pps: 30_000,
        tick_interval: Duration::from_millis(10),
        min_points: n_points,
        target_points: n_points,
        buffered_points: 0,
        buffered: Duration::ZERO,
        device_queued_points: None,
    };
    let mut buffer = vec![LaserPoint::default(); n_points];
    fill_points(shape, &dummy_req, &mut buffer);
    buffer
}

/// Fill an RGB triangle with proper blanking and interpolation into buffer.
fn fill_triangle_points(buffer: &mut [LaserPoint], n_points: usize) {
    let vertices = [
        (-0.5_f32, -0.5_f32, 65535_u16, 0_u16, 0_u16),
        (0.5_f32, -0.5_f32, 0_u16, 65535_u16, 0_u16),
        (0.0_f32, 0.5_f32, 0_u16, 0_u16, 65535_u16),
    ];

    const BLANK_COUNT: usize = 5;
    const DWELL_COUNT: usize = 3;

    let fixed_points = BLANK_COUNT + 3 * DWELL_COUNT + DWELL_COUNT;
    let points_per_edge = ((n_points.saturating_sub(fixed_points)) / 3).max(20);

    let mut idx = 0;

    // Blank points
    for _ in 0..BLANK_COUNT.min(n_points - idx) {
        buffer[idx] = LaserPoint::blanked(vertices[0].0, vertices[0].1);
        idx += 1;
        if idx >= n_points {
            return;
        }
    }

    // Draw edges
    for edge in 0..3 {
        let (x1, y1, r1, g1, b1) = vertices[edge];
        let (x2, y2, r2, g2, b2) = vertices[(edge + 1) % 3];

        for _ in 0..DWELL_COUNT {
            if idx >= n_points {
                return;
            }
            buffer[idx] = LaserPoint::new(x1, y1, r1, g1, b1, 65535);
            idx += 1;
        }

        for i in 1..=points_per_edge {
            if idx >= n_points {
                return;
            }
            let t = i as f32 / points_per_edge as f32;
            let x = x1 + (x2 - x1) * t;
            let y = y1 + (y2 - y1) * t;
            let r = (r1 as f32 + (r2 as f32 - r1 as f32) * t) as u16;
            let g = (g1 as f32 + (g2 as f32 - g1 as f32) * t) as u16;
            let b = (b1 as f32 + (b2 as f32 - b1 as f32) * t) as u16;
            buffer[idx] = LaserPoint::new(x, y, r, g, b, 65535);
            idx += 1;
        }
    }

    // Final dwell
    let (x, y, r, g, b) = vertices[0];
    while idx < n_points {
        buffer[idx] = LaserPoint::new(x, y, r, g, b, 65535);
        idx += 1;
    }
}

/// Fill a rainbow circle into buffer.
fn fill_circle_points(buffer: &mut [LaserPoint], n_points: usize) {
    const BLANK_COUNT: usize = 5;

    let mut idx = 0;

    for _ in 0..BLANK_COUNT.min(n_points) {
        buffer[idx] = LaserPoint::blanked(0.5, 0.0);
        idx += 1;
    }

    let circle_points = n_points.saturating_sub(BLANK_COUNT);
    for i in 0..circle_points {
        let angle = (i as f32 / circle_points as f32) * 2.0 * PI;
        let x = 0.5 * angle.cos();
        let y = 0.5 * angle.sin();

        let hue = i as f32 / circle_points as f32;
        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);

        buffer[idx] = LaserPoint::new(x, y, r, g, b, 65535);
        idx += 1;
    }
}

/// Fill a time-based orbiting circle with smooth continuous motion into buffer.
///
/// Each point's position is calculated from its exact stream timestamp,
/// so the orbit advances smoothly even within a single chunk.
/// No blanking - the beam continuously traces the circle as it orbits.
fn fill_orbiting_circle_points(req: &FillRequest, buffer: &mut [LaserPoint], n_points: usize) {
    const ORBIT_RADIUS: f32 = 0.5;
    const CIRCLE_RADIUS: f32 = 0.15;
    const ORBIT_PERIOD_SECS: f32 = 4.0;
    const POINTS_PER_CIRCLE: usize = 200;

    let pps = req.pps as f64;

    for i in 0..n_points {
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

        buffer[i] = LaserPoint::new(x, y, r, g, b, 65535);
    }
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

fn fill_test_pattern_points(buffer: &mut [LaserPoint], n_points: usize) {
    let json_str = include_str!("test-pattern.json");
    let pattern_points: Vec<PatternPoint> = serde_json::from_str(json_str).unwrap();

    let points: Vec<LaserPoint> = pattern_points
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

    for (i, point) in points.iter().cycle().take(n_points).enumerate() {
        buffer[i] = *point;
    }
}
