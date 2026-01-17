//! egui renderer for laser points.

use eframe::egui::{self, Color32, Pos2, Rect, Stroke, Vec2};

use super::protocol_handler::RenderPoint;

/// Settings for rendering the laser canvas.
#[derive(Clone)]
pub struct RenderSettings {
    pub point_size: f32,
    pub show_blanking: bool,
    pub invert_y: bool,
    pub show_grid: bool,
}

impl Default for RenderSettings {
    fn default() -> Self {
        Self {
            point_size: 2.0,
            show_blanking: false,
            invert_y: false,
            show_grid: false,
        }
    }
}

/// Render laser points as connected lines on an egui canvas.
/// Returns the rect used for rendering (for drawing overlays like grids).
pub fn render_laser_canvas(
    ui: &mut egui::Ui,
    points: &[RenderPoint],
    settings: &RenderSettings,
) -> Rect {
    let available_size = ui.available_size();
    let (response, painter) = ui.allocate_painter(available_size, egui::Sense::hover());
    let rect = response.rect;

    // Draw black background
    painter.rect_filled(rect, 0.0, Color32::BLACK);

    // Calculate transform - laser area is a centered square
    let center = rect.center();
    let scale = rect.width().min(rect.height()) * 0.45;
    let laser_rect = Rect::from_center_size(center, Vec2::splat(scale * 2.0));

    // Draw grid under laser content if enabled
    if settings.show_grid {
        draw_grid_overlay(&painter, laser_rect);
    }

    if points.is_empty() {
        return laser_rect;
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
                // Use previous point's color for the line with actual color values
                let r = (prev.r * prev.intensity * 255.0).clamp(0.0, 255.0) as u8;
                let g = (prev.g * prev.intensity * 255.0).clamp(0.0, 255.0) as u8;
                let b = (prev.b * prev.intensity * 255.0).clamp(0.0, 255.0) as u8;

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
    laser_rect
}
/// Draw a grid overlay on top of the persistence buffer image.
/// The rect should be the image rect (where the persistence buffer is displayed).
pub fn draw_grid_overlay(painter: &egui::Painter, img_rect: Rect) {
    let grid_color = Color32::from_gray(60);
    let label_color = Color32::from_gray(120);

    // Draw border around the image
    painter.rect_stroke(img_rect, 0.0, Stroke::new(1.0, grid_color));

    // Draw center crosshairs
    let center = img_rect.center();
    painter.line_segment(
        [
            Pos2::new(img_rect.left(), center.y),
            Pos2::new(img_rect.right(), center.y),
        ],
        Stroke::new(1.0, grid_color),
    );
    painter.line_segment(
        [
            Pos2::new(center.x, img_rect.top()),
            Pos2::new(center.x, img_rect.bottom()),
        ],
        Stroke::new(1.0, grid_color),
    );

    // Add scale labels (inset inside the grid)
    let font_id = egui::FontId::monospace(10.0);
    let label_offset = 4.0;

    // X-axis labels (bottom edge, inset)
    // -1 at left
    painter.text(
        Pos2::new(
            img_rect.left() + label_offset,
            img_rect.bottom() - label_offset,
        ),
        egui::Align2::LEFT_BOTTOM,
        "-1",
        font_id.clone(),
        label_color,
    );
    // +1 at right
    painter.text(
        Pos2::new(
            img_rect.right() - label_offset,
            img_rect.bottom() - label_offset,
        ),
        egui::Align2::RIGHT_BOTTOM,
        "+1",
        font_id.clone(),
        label_color,
    );
    // X label
    painter.text(
        Pos2::new(img_rect.right() - label_offset, center.y + label_offset),
        egui::Align2::RIGHT_TOP,
        "X",
        font_id.clone(),
        label_color,
    );

    // Y-axis labels (left edge, inset)
    // +1 at top
    painter.text(
        Pos2::new(
            img_rect.left() + label_offset,
            img_rect.top() + label_offset,
        ),
        egui::Align2::LEFT_TOP,
        "+1",
        font_id.clone(),
        label_color,
    );
    // -1 at bottom
    painter.text(
        Pos2::new(
            img_rect.left() + label_offset,
            img_rect.bottom() - label_offset,
        ),
        egui::Align2::LEFT_BOTTOM,
        "-1",
        font_id.clone(),
        label_color,
    );
    // Y label
    painter.text(
        Pos2::new(center.x + label_offset, img_rect.top() + label_offset),
        egui::Align2::LEFT_TOP,
        "Y",
        font_id,
        label_color,
    );
}
