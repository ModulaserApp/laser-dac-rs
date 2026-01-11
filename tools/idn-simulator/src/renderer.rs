//! egui renderer for laser points.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2};

use super::protocol_handler::RenderPoint;

/// Settings for rendering the laser canvas.
#[derive(Clone)]
pub struct RenderSettings {
    pub point_size: f32,
    pub show_grid: bool,
    pub show_blanking: bool,
    pub invert_y: bool,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            point_size: 2.0,
            show_grid: true,
            show_blanking: false,
            invert_y: false,
        }
    }
}

/// Render laser points as connected lines on an egui canvas.
pub fn render_laser_canvas(ui: &mut egui::Ui, points: &[RenderPoint], settings: &RenderSettings) {
    let available_size = ui.available_size();
    let (response, painter) = ui.allocate_painter(available_size, egui::Sense::hover());
    let rect = response.rect;

    // Draw black background
    painter.rect_filled(rect, 0.0, Color32::BLACK);

    // Draw coordinate grid (faint)
    if settings.show_grid {
        draw_grid(&painter, rect);
    }

    if points.is_empty() {
        return;
    }

    // Debug: log first point
    let p = &points[0];
    log::debug!(
        "First point: x={:.3}, y={:.3}, r={:.3}, g={:.3}, b={:.3}, i={:.3}",
        p.x,
        p.y,
        p.r,
        p.g,
        p.b,
        p.intensity
    );

    // Calculate transform
    let center = rect.center();
    let scale = rect.width().min(rect.height()) * 0.45;
    let y_mult = if settings.invert_y { 1.0 } else { -1.0 };

    // Draw laser lines
    let mut shapes = Vec::new();
    let mut prev_point: Option<&RenderPoint> = None;

    for point in points {
        if let Some(prev) = prev_point {
            let p1 = Pos2::new(
                center.x + prev.x * scale,
                center.y + prev.y * scale * y_mult,
            );
            let p2 = Pos2::new(
                center.x + point.x * scale,
                center.y + point.y * scale * y_mult,
            );

            // Check if previous point was blanked
            if prev.intensity > 0.01 {
                // Use previous point's color for the line
                // Make any non-zero color channel fully bright for visibility
                let r = if prev.r > 0.001 { 255 } else { 0 };
                let g = if prev.g > 0.001 { 255 } else { 0 };
                let b = if prev.b > 0.001 { 255 } else { 0 };

                // Fallback to white if all colors are zero but point is lit
                let color = if r == 0 && g == 0 && b == 0 {
                    Color32::WHITE
                } else {
                    Color32::from_rgb(r, g, b)
                };

                shapes.push(egui::Shape::line_segment(
                    [p1, p2],
                    Stroke::new(settings.point_size, color),
                ));
            } else if settings.show_blanking {
                // Draw blank moves as faint gray dashed lines
                shapes.push(egui::Shape::line_segment(
                    [p1, p2],
                    Stroke::new(1.0, Color32::from_gray(60)),
                ));
            }
        }
        prev_point = Some(point);
    }

    painter.extend(shapes);
}

/// Draw a faint coordinate grid.
fn draw_grid(painter: &egui::Painter, rect: Rect) {
    let center = rect.center();
    let scale = rect.width().min(rect.height()) * 0.45;
    let grid_color = Color32::from_gray(40);

    // Draw border at -1 to 1
    let border_rect = Rect::from_center_size(center, Vec2::splat(scale * 2.0));
    painter.rect_stroke(border_rect, 0.0, Stroke::new(1.0, grid_color));

    // Draw center crosshairs
    painter.line_segment(
        [
            Pos2::new(center.x - scale, center.y),
            Pos2::new(center.x + scale, center.y),
        ],
        Stroke::new(1.0, grid_color),
    );
    painter.line_segment(
        [
            Pos2::new(center.x, center.y - scale),
            Pos2::new(center.x, center.y + scale),
        ],
        Stroke::new(1.0, grid_color),
    );
}
