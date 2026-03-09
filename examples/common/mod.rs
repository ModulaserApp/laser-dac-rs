//! Shared code for examples.

pub mod audio;

use clap::{Parser, ValueEnum};
use laser_dac::{ChunkRequest, ChunkResult, Frame, FrameAdapter, LaserPoint};
use serde::Deserialize;
use std::f32::consts::{PI, TAU};

#[derive(Parser)]
#[command(about = "Send test patterns to connected laser DACs")]
pub struct Args {
    /// Shape to display
    #[arg(value_enum, default_value_t = Shape::Triangle)]
    pub shape: Shape,

    /// Number of points per frame (detail level for static shapes)
    #[arg(short, long, default_value_t = 200)]
    pub points: usize,

    /// Geometry scale around center (0,0); range: (0, 10]
    #[arg(long, default_value_t = 1.0, value_parser = parse_scale)]
    pub scale: f32,
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

/// Generate a complete frame of points for a static shape.
///
/// The frame contains exactly `n_points` points representing one full cycle
/// of the shape. This frame is then streamed continuously by wrapping around
/// — the DAC never waits for frame boundaries.
///
/// For time-based shapes (OrbitingCircle, Audio), use `make_producer` instead.
pub fn generate_frame(shape: Shape, n_points: usize, scale: f32) -> Vec<LaserPoint> {
    let mut frame = vec![LaserPoint::default(); n_points];
    match shape {
        Shape::Triangle => fill_triangle_points(&mut frame, n_points),
        Shape::Circle => fill_circle_points(&mut frame, n_points),
        Shape::TestPattern => fill_test_pattern_points(&mut frame, n_points),
        Shape::OrbitingCircle | Shape::Audio => {
            panic!("time-based shapes don't have static frames; use make_producer()")
        }
    }
    if (scale - 1.0).abs() > f32::EPSILON {
        scale_points(&mut frame[..n_points], scale);
    }
    frame
}

/// Create a producer callback for streaming any shape to a DAC.
///
/// For static shapes (Triangle, Circle, TestPattern), this generates a frame
/// once and streams it continuously using `FrameAdapter` — the cursor wraps
/// around at the end of the frame, just like real laser software works.
///
/// For time-based shapes (OrbitingCircle, Audio), the callback computes each
/// point from the stream timestamp, producing smooth continuous animation.
pub fn make_producer(
    shape: Shape,
    points: usize,
    scale: f32,
) -> Box<dyn FnMut(&ChunkRequest, &mut [LaserPoint]) -> ChunkResult + Send> {
    match shape {
        Shape::OrbitingCircle => Box::new(move |req, buffer| {
            let n = req.target_points.min(buffer.len());
            fill_orbiting_circle_points(req, buffer, n);
            if (scale - 1.0).abs() > f32::EPSILON {
                scale_points(&mut buffer[..n], scale);
            }
            ChunkResult::Filled(n)
        }),
        Shape::Audio => {
            let audio_config = audio::AudioConfig::default();
            Box::new(move |req, buffer| {
                let n = req.target_points.min(buffer.len());
                audio::fill_audio_points(req, buffer, n, &audio_config);
                ChunkResult::Filled(n)
            })
        }
        _ => {
            let frame = generate_frame(shape, points, scale);
            let mut adapter = FrameAdapter::new();
            adapter.update(Frame::new(frame));
            Box::new(move |req, buffer| adapter.fill_chunk(req, buffer))
        }
    }
}

fn parse_scale(value: &str) -> Result<f32, String> {
    let scale: f32 = value
        .parse()
        .map_err(|_| format!("invalid scale '{value}': expected a float in (0, 10]"))?;
    if !scale.is_finite() || scale <= 0.0 || scale > 10.0 {
        return Err("scale must be finite and in (0, 10]".to_string());
    }
    Ok(scale)
}

fn scale_points(points: &mut [LaserPoint], scale: f32) {
    for point in points {
        point.x = (point.x * scale).clamp(-1.0, 1.0);
        point.y = (point.y * scale).clamp(-1.0, 1.0);
    }
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
fn fill_orbiting_circle_points(req: &ChunkRequest, buffer: &mut [LaserPoint], n_points: usize) {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_frame_produces_correct_point_count() {
        let frame = generate_frame(Shape::Circle, 200, 1.0);
        assert_eq!(frame.len(), 200);
        // First 5 points are blanked (leading blank), rest have color
        assert!(frame[..5].iter().all(|p| p.intensity == 0));
        assert!(frame[5..].iter().any(|p| p.intensity != 0));
    }

    #[test]
    fn generate_frame_applies_scale() {
        let unscaled = generate_frame(Shape::Circle, 100, 1.0);
        let scaled = generate_frame(Shape::Circle, 100, 0.5);
        // Scaled points should be closer to center
        let unscaled_max_x = unscaled.iter().map(|p| p.x.abs()).fold(0.0f32, f32::max);
        let scaled_max_x = scaled.iter().map(|p| p.x.abs()).fold(0.0f32, f32::max);
        assert!(scaled_max_x < unscaled_max_x);
    }

    #[test]
    fn scale_points_scales_xy_around_origin() {
        let mut points = [
            LaserPoint::new(0.5, -0.4, 1, 2, 3, 4),
            LaserPoint::new(-0.8, 0.9, 5, 6, 7, 8),
        ];
        scale_points(&mut points, 0.3);

        assert!((points[0].x - 0.15).abs() < 1e-6);
        assert!((points[0].y + 0.12).abs() < 1e-6);
        assert!((points[1].x + 0.24).abs() < 1e-6);
        assert!((points[1].y - 0.27).abs() < 1e-6);
    }

    #[test]
    fn parse_scale_enforces_bounds() {
        assert!(parse_scale("0.3").is_ok());
        assert!(parse_scale("10").is_ok());
        assert!(parse_scale("0").is_err());
        assert!(parse_scale("10.0001").is_err());
    }
}
