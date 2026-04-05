#![allow(dead_code)]

//! Shared code for examples.

pub mod audio;

use clap::{Parser, ValueEnum};
use laser_dac::{ChunkRequest, ChunkResult, LaserPoint};
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
    Orientation,
    TestPattern,
}

impl Shape {
    #[allow(dead_code)] // Used by stream/frame examples, not audio
    pub fn name(&self) -> &'static str {
        match self {
            Shape::Triangle => "triangle",
            Shape::Circle => "circle",
            Shape::OrbitingCircle => "orbiting-circle",
            Shape::Orientation => "orientation",
            Shape::TestPattern => "test-pattern",
        }
    }
}

/// Generate a complete frame of points for a shape.
///
/// The frame contains exactly `n_points` points representing one full cycle
/// of the shape. This frame is then streamed continuously by wrapping around
/// — the DAC never waits for frame boundaries.
///
/// For time-based shapes (OrbitingCircle), this produces a static circle.
/// Use `make_producer` for timestamp-driven animation in the stream API.
pub fn generate_frame(shape: Shape, n_points: usize, scale: f32) -> Vec<LaserPoint> {
    let mut frame = vec![LaserPoint::default(); n_points];
    match shape {
        Shape::Triangle => fill_triangle_points(&mut frame, n_points),
        Shape::Circle | Shape::OrbitingCircle => fill_circle_points(&mut frame, n_points),
        Shape::Orientation => return fill_orientation_points(n_points, scale),
        Shape::TestPattern => fill_test_pattern_points(&mut frame, n_points),
    }
    if (scale - 1.0).abs() > f32::EPSILON {
        scale_points(&mut frame[..n_points], scale);
    }
    frame
}

/// Create a producer callback for streaming any shape to a DAC.
///
/// For static shapes (Triangle, Circle, TestPattern), this generates a frame
/// once and cycles through it with a cursor — the index wraps at the end,
/// just like real laser software works.
///
/// For time-based shapes (OrbitingCircle), the callback computes each
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
        _ => {
            let frame = generate_frame(shape, points, scale);
            let mut cursor = 0usize;
            Box::new(move |req, buffer| {
                let n = req.target_points.min(buffer.len());
                for i in 0..n {
                    buffer[i] = frame[cursor];
                    cursor += 1;
                    if cursor >= frame.len() {
                        cursor = 0;
                    }
                }
                ChunkResult::Filled(n)
            })
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
///
/// Generates a closed circle: the last point equals the first point,
/// so the transition function produces no blanking at the loop seam.
fn fill_circle_points(buffer: &mut [LaserPoint], n_points: usize) {
    for i in 0..n_points {
        let angle = (i as f32 / n_points as f32) * 2.0 * PI;
        let x = 0.5 * angle.cos();
        let y = 0.5 * angle.sin();

        let hue = i as f32 / n_points as f32;
        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);

        buffer[i] = LaserPoint::new(x, y, r, g, b, 65535);
    }
}

/// Build an orientation test frame: circle (left), triangle (center), line (right).
///
/// Each sub-shape is a closed loop. Transitions between shapes use the same
/// blanking strategy as `default_transition`: end-dwell → eased transit → start-dwell.
/// This keeps galvo travel smooth even though multiple shapes share one frame.
///
/// Returns the frame directly (handles its own scaling) so it bypasses the
/// buffer-based fill pattern.
///
/// If X is flipped: shapes appear in reversed left-right order.
/// If Y is flipped: the triangle points downward.
fn fill_orientation_points(n_points: usize, scale: f32) -> Vec<LaserPoint> {
    const WHITE: u16 = 65535;
    // Blanking budget per transition
    const END_DWELL: usize = 3;
    const START_DWELL: usize = 12;

    /// Quintic ease-in-out for smooth galvo transit.
    fn ease(t: f32) -> f32 {
        if t < 0.5 {
            16.0 * t * t * t * t * t
        } else {
            let u = 2.0 * t - 2.0;
            0.5 * u * u * u * u * u + 1.0
        }
    }

    /// Generate blanked transition points between two positions.
    fn blanked_transition(from: (f32, f32), to: (f32, f32)) -> Vec<LaserPoint> {
        let dx = to.0 - from.0;
        let dy = to.1 - from.1;
        let d_inf = dx.abs().max(dy.abs());
        let transit = (64.0 * d_inf).ceil().clamp(0.0, 128.0) as usize;
        let total = END_DWELL + transit + START_DWELL;
        let mut points = Vec::with_capacity(total);

        // End dwell at source
        for _ in 0..END_DWELL {
            points.push(LaserPoint::blanked(from.0, from.1));
        }
        // Eased transit
        for i in 0..transit {
            let t = (i as f32 + 1.0) / (transit as f32 + 1.0);
            let t = ease(t);
            points.push(LaserPoint::blanked(from.0 + dx * t, from.1 + dy * t));
        }
        // Start dwell at destination
        for _ in 0..START_DWELL {
            points.push(LaserPoint::blanked(to.0, to.1));
        }
        points
    }

    // --- Build sub-shapes as closed loops ---
    // Target ~1000 points total (~30 fps at 30kpps) for gentle galvo movement.

    // Circle (left, at x=-0.4)
    let circle_cx = -0.4_f32;
    let circle_r = 0.2_f32;
    let circle_n = 200;
    let circle_segments = circle_n - 1;
    let circle: Vec<LaserPoint> = (0..circle_n)
        .map(|i| {
            let angle = (i as f32 / circle_segments as f32) * TAU;
            let x = circle_cx + circle_r * angle.cos();
            let y = circle_r * angle.sin();
            LaserPoint::new(x, y, WHITE, 0, 0, WHITE) // red
        })
        .collect();

    // Triangle (center, pointing up) — with dwell at each vertex
    let tri_size = 0.2_f32;
    let tri_verts = [
        (-tri_size, -tri_size * 0.7), // bottom-left
        (tri_size, -tri_size * 0.7),  // bottom-right
        (0.0, tri_size),              // apex (top)
    ];
    let pts_per_edge = 45;
    let corner_dwell = 3;
    let mut triangle = Vec::with_capacity(pts_per_edge * 3 + corner_dwell * 3);
    for edge in 0..3 {
        let (x1, y1) = tri_verts[edge];
        let (x2, y2) = tri_verts[(edge + 1) % 3];
        // Dwell at start vertex
        for _ in 0..corner_dwell {
            triangle.push(LaserPoint::new(x1, y1, 0, WHITE, 0, WHITE));
        }
        for i in 1..=pts_per_edge {
            let t = i as f32 / pts_per_edge as f32;
            triangle.push(LaserPoint::new(
                x1 + (x2 - x1) * t,
                y1 + (y2 - y1) * t,
                0,
                WHITE,
                0,
                WHITE, // green
            ));
        }
    }

    // Line (right, vertical at x=0.4, bounces top→bottom→top) — with dwell at reversals
    let line_x = 0.4_f32;
    let line_half = 0.25_f32;
    let line_half_n = 25;
    let reversal_dwell = 5;
    let mut line = Vec::with_capacity(line_half_n * 2 + reversal_dwell * 2);
    // Dwell at top
    for _ in 0..reversal_dwell {
        line.push(LaserPoint::new(line_x, line_half, 0, 0, WHITE, WHITE));
    }
    // Down stroke
    for i in 0..line_half_n {
        let t = i as f32 / line_half_n as f32;
        let y = line_half - 2.0 * line_half * t;
        line.push(LaserPoint::new(line_x, y, 0, 0, WHITE, WHITE)); // blue
    }
    // Dwell at bottom
    for _ in 0..reversal_dwell {
        line.push(LaserPoint::new(line_x, -line_half, 0, 0, WHITE, WHITE));
    }
    // Up stroke (closes back to start)
    for i in 0..line_half_n {
        let t = i as f32 / line_half_n as f32;
        let y = -line_half + 2.0 * line_half * t;
        line.push(LaserPoint::new(line_x, y, 0, 0, WHITE, WHITE)); // blue
    }

    // --- Assemble with transitions ---
    let circle_end = (circle.last().unwrap().x, circle.last().unwrap().y);
    let tri_start = (triangle[0].x, triangle[0].y);
    let tri_end = (triangle.last().unwrap().x, triangle.last().unwrap().y);
    let line_start = (line[0].x, line[0].y);
    let line_end = (line.last().unwrap().x, line.last().unwrap().y);
    let circle_start = (circle[0].x, circle[0].y);

    let t1 = blanked_transition(circle_end, tri_start);
    let t2 = blanked_transition(tri_end, line_start);
    let t3 = blanked_transition(line_end, circle_start);

    let mut frame = Vec::with_capacity(
        circle.len() + t1.len() + triangle.len() + t2.len() + line.len() + t3.len(),
    );
    frame.extend_from_slice(&circle);
    frame.extend_from_slice(&t1);
    frame.extend_from_slice(&triangle);
    frame.extend_from_slice(&t2);
    frame.extend_from_slice(&line);
    frame.extend_from_slice(&t3);

    // Apply scale
    if (scale - 1.0).abs() > f32::EPSILON {
        for p in &mut frame {
            p.x = (p.x * scale).clamp(-1.0, 1.0);
            p.y = (p.y * scale).clamp(-1.0, 1.0);
        }
    }

    // Ignore n_points for this shape — the point count is determined by the
    // geometry and blanking requirements to keep galvos safe.
    let _ = n_points;
    frame
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
        // All points have color (no leading blanks)
        assert!(frame.iter().all(|p| p.intensity != 0));
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
