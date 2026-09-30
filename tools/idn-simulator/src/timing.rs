//! Stream timing utilities for handling IDN timestamps.

use laser_dac::receiver::ChunkType;

use crate::protocol_handler::{ParsedChunk, RenderPoint};

/// A point with an associated stream timestamp.
#[derive(Clone, Debug)]
pub struct TimedPoint {
    /// Timestamp in microseconds (monotonic, unwrapped from u32).
    pub t_us: u64,
    /// The render point data.
    pub p: RenderPoint,
    /// True if this is the first point of a chunk (for debugging).
    pub is_chunk_start: bool,
}

/// State machine for unwrapping u32 timestamps to monotonic u64.
pub struct TimestampUnwrapper {
    last_ts_u32: Option<u32>,
    wrap_base: u64,
}

impl TimestampUnwrapper {
    pub fn new() -> Self {
        Self {
            last_ts_u32: None,
            wrap_base: 0,
        }
    }

    /// Unwrap a u32 timestamp to monotonic u64.
    /// Detects wraps when the new timestamp is significantly smaller than the previous.
    pub fn unwrap(&mut self, ts_u32: u32) -> u64 {
        if let Some(last) = self.last_ts_u32 {
            // If new timestamp is much smaller than last, assume a wrap occurred
            // Use a threshold of half the u32 range to detect wraps
            if ts_u32 < last && (last - ts_u32) > (1u32 << 31) {
                self.wrap_base += 1u64 << 32;
                log::debug!("Timestamp wrap detected: {} -> {}", last, ts_u32);
            }
        }
        self.last_ts_u32 = Some(ts_u32);
        self.wrap_base + ts_u32 as u64
    }

    /// Reset the unwrapper state (e.g., on client disconnect).
    pub fn reset(&mut self) {
        self.last_ts_u32 = None;
        self.wrap_base = 0;
    }
}

impl Default for TimestampUnwrapper {
    fn default() -> Self {
        Self::new()
    }
}

/// Joins the fragments of a Frame-mode frame so it is timed as one chunk.
///
/// A multi-datagram frame arrives as a `FrameFirst` chunk, which carries the
/// whole frame's duration, then `FrameSequel` chunks, which have no sample
/// chunk header and report a duration of 0. Timing each fragment on its own
/// would spread the frame's duration over the first fragment's points and
/// leave the sequels with no rate, so fragments are held until the frame is
/// complete.
#[derive(Default)]
pub struct FrameAssembler {
    pending: Option<ParsedChunk>,
}

impl FrameAssembler {
    /// Cap on a pending frame's points, so a frame whose last fragment is
    /// lost cannot grow without bound.
    const MAX_PENDING_POINTS: usize = 1 << 20;

    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one received chunk and return the chunks that are ready to play,
    /// in order.
    ///
    /// A frame whose last fragment never arrives is released, as far as it
    /// got, when the next frame or wave chunk starts.
    pub fn push(&mut self, chunk: ParsedChunk) -> Vec<ParsedChunk> {
        let mut ready = Vec::new();
        match chunk.chunk_type {
            ChunkType::FrameSequel => {
                let Some(frame) = self.pending.as_mut() else {
                    log::debug!("Dropping frame sequel without a first fragment");
                    return ready;
                };
                frame.points.extend(chunk.points);
                if chunk.is_last_fragment || frame.points.len() >= Self::MAX_PENDING_POINTS {
                    ready.extend(self.pending.take());
                }
            }
            ChunkType::FrameFirst => ready.extend(self.pending.replace(chunk)),
            _ => {
                ready.extend(self.pending.take());
                ready.push(chunk);
            }
        }
        ready
    }

    /// Drop any partially received frame (e.g., on client disconnect).
    pub fn reset(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(chunk_type: ChunkType, last: bool, duration_us: u32, xs: &[f32]) -> ParsedChunk {
        ParsedChunk {
            chunk_type,
            is_last_fragment: last,
            timestamp_us_u32: 1_000,
            duration_us,
            points: xs
                .iter()
                .map(|&x| RenderPoint {
                    x,
                    y: 0.0,
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    intensity: 1.0,
                })
                .collect(),
        }
    }

    fn xs(chunk: &ParsedChunk) -> Vec<f32> {
        chunk.points.iter().map(|p| p.x).collect()
    }

    #[test]
    fn fragments_are_joined_into_one_frame_with_its_duration() {
        let mut asm = FrameAssembler::new();
        assert!(asm
            .push(chunk(ChunkType::FrameFirst, false, 300, &[0.0, 0.1]))
            .is_empty());
        assert!(asm
            .push(chunk(ChunkType::FrameSequel, false, 0, &[0.2]))
            .is_empty());

        let ready = asm.push(chunk(ChunkType::FrameSequel, true, 0, &[0.3]));
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].duration_us, 300);
        assert_eq!(ready[0].timestamp_us_u32, 1_000);
        assert_eq!(xs(&ready[0]), [0.0, 0.1, 0.2, 0.3]);
    }

    #[test]
    fn wave_and_single_frame_chunks_pass_through() {
        let mut asm = FrameAssembler::new();
        assert_eq!(
            asm.push(chunk(ChunkType::Wave, false, 100, &[0.5])).len(),
            1
        );
        assert_eq!(
            asm.push(chunk(ChunkType::Frame, false, 100, &[0.5])).len(),
            1
        );
    }

    #[test]
    fn orphan_sequel_is_dropped() {
        let mut asm = FrameAssembler::new();
        assert!(asm
            .push(chunk(ChunkType::FrameSequel, true, 0, &[0.5]))
            .is_empty());
    }

    #[test]
    fn unfinished_frame_is_released_before_the_next_chunk() {
        let mut asm = FrameAssembler::new();
        asm.push(chunk(ChunkType::FrameFirst, false, 300, &[0.0]));
        asm.push(chunk(ChunkType::FrameSequel, false, 0, &[0.1]));

        let ready = asm.push(chunk(ChunkType::FrameFirst, false, 300, &[0.9]));
        assert_eq!(ready.len(), 1);
        assert_eq!(xs(&ready[0]), [0.0, 0.1]);

        let ready = asm.push(chunk(ChunkType::Wave, false, 100, &[0.5]));
        assert_eq!(ready.len(), 2);
        assert_eq!(xs(&ready[0]), [0.9]);
        assert_eq!(xs(&ready[1]), [0.5]);
    }
}
