//! Persistence buffer for time-based beam rendering.
//!
//! This module provides a 2D buffer that accumulates energy over time and decays,
//! simulating the phosphor persistence of a real laser display.

use eframe::egui;

/// Resolution of the persistence buffer.
/// Higher values reduce pixelation but use more memory.
pub const BUFFER_RESOLUTION: usize = 1024;

/// 2D persistence buffer for time-based rendering.
/// Stores RGB energy values that decay over time.
pub struct PersistenceBuffer {
    /// RGB energy buffer (linear space), stored as [r, g, b] per pixel.
    /// Layout: row-major, (y * width + x) * 3 + channel
    data: Vec<f32>,
    /// Reusable pixel buffer to avoid allocation every frame.
    pixels: Vec<egui::Color32>,
    width: usize,
    height: usize,
}

impl PersistenceBuffer {
    pub fn new(width: usize, height: usize) -> Self {
        Self {
            data: vec![0.0; width * height * 3],
            pixels: vec![egui::Color32::BLACK; width * height],
            width,
            height,
        }
    }

    /// Clear the buffer to black.
    pub fn clear(&mut self) {
        self.data.fill(0.0);
        self.pixels.fill(egui::Color32::BLACK);
    }

    /// Check if buffer has any visible content (optimization for idle state).
    /// Returns false if all pixels are essentially black.
    pub fn has_content(&self) -> bool {
        // Check a sample of pixels for performance (every 64th pixel)
        // Threshold is very low since we're in linear space
        const THRESHOLD: f32 = 0.0001;
        self.data.iter().step_by(64).any(|&v| v > THRESHOLD)
    }

    /// Apply exponential decay to all pixels.
    /// decay_factor should be exp(-dt_ms / tau_ms)
    pub fn apply_decay(&mut self, decay_factor: f32) {
        for v in &mut self.data {
            *v *= decay_factor;
        }
    }

    /// Deposit energy at a pixel coordinate.
    /// Coordinates are in pixel space (0..width, 0..height).
    pub fn deposit(&mut self, x: usize, y: usize, r: f32, g: f32, b: f32) {
        if x < self.width && y < self.height {
            let idx = (y * self.width + x) * 3;
            self.data[idx] += r;
            self.data[idx + 1] += g;
            self.data[idx + 2] += b;
        }
    }

    /// Deposit energy along a line segment using Bresenham's algorithm.
    /// Energy is conserved: total_r/g/b is distributed across all pixels in the line.
    /// Longer lines result in dimmer pixels (energy spread over more pixels).
    pub fn deposit_line(
        &mut self,
        (x0, y0): (i32, i32),
        (x1, y1): (i32, i32),
        total_rgb: [f32; 3],
    ) {
        // Calculate number of steps (pixels) in the line
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let steps = dx.max(dy) as f32 + 1.0;

        // Energy per pixel = total energy / number of pixels
        let r_per_step = total_rgb[0] / steps;
        let g_per_step = total_rgb[1] / steps;
        let b_per_step = total_rgb[2] / steps;

        // Bresenham's line algorithm
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx - dy;

        let mut x = x0;
        let mut y = y0;

        loop {
            if x >= 0 && y >= 0 {
                self.deposit(x as usize, y as usize, r_per_step, g_per_step, b_per_step);
            }

            if x == x1 && y == y1 {
                break;
            }

            let e2 = 2 * err;
            if e2 > -dy {
                err -= dy;
                x += sx;
            }
            if e2 < dx {
                err += dx;
                y += sy;
            }
        }
    }

    /// Convert normalized coordinates [-1, 1] to pixel coordinates.
    pub fn norm_to_pixel(&self, x: f32, y: f32, invert_y: bool) -> (i32, i32) {
        // Map [-1, 1] to [0, width-1] and [0, height-1]
        let px = ((x + 1.0) * 0.5 * (self.width - 1) as f32) as i32;
        let py = if invert_y {
            ((y + 1.0) * 0.5 * (self.height - 1) as f32) as i32
        } else {
            ((-y + 1.0) * 0.5 * (self.height - 1) as f32) as i32
        };
        (px, py)
    }

    /// Convert buffer to egui ColorImage for display.
    /// Uses the internal reusable pixel buffer to avoid allocations.
    /// Applies tone mapping: out = 1 - exp(-gain * energy)
    /// This preserves relative brightness while preventing harsh clipping.
    pub fn render_color_image(&mut self) -> egui::ColorImage {
        // Tone mapping gain - controls how quickly highlights roll off
        // Higher = faster saturation, lower = more headroom
        const GAIN: f32 = 3.0;

        for y in 0..self.height {
            for x in 0..self.width {
                let idx = (y * self.width + x) * 3;
                let pixel_idx = y * self.width + x;
                // Tone map: bounded, smooth rolloff, preserves relative brightness
                let r = ((1.0 - (-GAIN * self.data[idx]).exp()) * 255.0) as u8;
                let g = ((1.0 - (-GAIN * self.data[idx + 1]).exp()) * 255.0) as u8;
                let b = ((1.0 - (-GAIN * self.data[idx + 2]).exp()) * 255.0) as u8;
                self.pixels[pixel_idx] = egui::Color32::from_rgb(r, g, b);
            }
        }

        egui::ColorImage {
            size: [self.width, self.height],
            pixels: self.pixels.clone(),
        }
    }
}

impl Default for PersistenceBuffer {
    fn default() -> Self {
        Self::new(BUFFER_RESOLUTION, BUFFER_RESOLUTION)
    }
}
