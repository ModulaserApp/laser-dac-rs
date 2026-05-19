//! Thin adapter on top of [`laser_dac::receiver`] for the simulator's
//! UI/render layer. The real protocol parsing now lives in `laser-dac`
//! (behind the `receiver` feature); this file just keeps the simulator's
//! local `RenderPoint` shape so the rest of the UI doesn't need to change.

use laser_dac::receiver::{ReceivedChunk, ReceivedPoint};

/// A point ready for rendering.
///
/// Intensity is kept as a separate field (always `1.0`) so the renderer's
/// existing `r * intensity` math stays intact. The receiver already
/// pre-multiplies intensity into RGB at parse time.
#[derive(Clone, Debug)]
pub struct RenderPoint {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub intensity: f32,
}

impl From<&ReceivedPoint> for RenderPoint {
    fn from(p: &ReceivedPoint) -> Self {
        RenderPoint {
            x: p.x,
            y: p.y,
            r: p.r,
            g: p.g,
            b: p.b,
            intensity: 1.0,
        }
    }
}

/// A parsed chunk with timing information.
#[derive(Clone, Debug)]
pub struct ParsedChunk {
    pub timestamp_us_u32: u32,
    pub duration_us: u32,
    pub points: Vec<RenderPoint>,
}

/// Convert parsed receiver chunk data for the simulator render layer.
pub fn parsed_chunk_from_received(chunk: ReceivedChunk<'_>) -> ParsedChunk {
    ParsedChunk {
        timestamp_us_u32: chunk.timestamp_us_u32,
        duration_us: chunk.duration_us,
        points: chunk.points.iter().map(RenderPoint::from).collect(),
    }
}
