//! Shared code for examples.

use clap::{Parser, ValueEnum};
use laser_dac::{LaserFrame, LaserPoint};
use std::f32::consts::PI;

#[derive(Parser)]
#[command(about = "Send test patterns to connected laser DACs")]
pub struct Args {
    /// Shape to display
    #[arg(value_enum, default_value_t = Shape::Triangle)]
    pub shape: Shape,

    /// Minimum number of points per frame
    #[arg(short, long, default_value_t = 600)]
    pub min_points: usize,
}

#[derive(Copy, Clone, ValueEnum)]
pub enum Shape {
    Triangle,
    Circle,
}

impl Shape {
    pub fn name(&self) -> &'static str {
        match self {
            Shape::Triangle => "triangle",
            Shape::Circle => "circle",
        }
    }
}

pub fn create_frame(shape: Shape, min_points: usize) -> LaserFrame {
    match shape {
        Shape::Triangle => create_triangle_frame(min_points),
        Shape::Circle => create_circle_frame(min_points),
    }
}

/// Create an RGB triangle frame with proper blanking and interpolation.
fn create_triangle_frame(min_points: usize) -> LaserFrame {
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

    LaserFrame::new(30000, points)
}

/// Create a rainbow circle frame.
fn create_circle_frame(num_points: usize) -> LaserFrame {
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

    LaserFrame::new(30000, points)
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
