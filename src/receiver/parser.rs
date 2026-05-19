//! IDN channel-message parser for the receiver server.
//!
//! Parses the channel message header, channel config header, descriptor
//! array, and sample chunk to produce a [`ParsedChunk`] of
//! [`ReceivedPoint`]s. The parser is descriptor-driven: it reads the
//! declared sample format from the channel config and dispatches on it,
//! so XY, XYRGB, and XYRGBI streams are all handled. Unsupported sample
//! formats are logged and skipped.

use byteorder::{ReadBytesExt, BE};
use std::io::Cursor;

// Content ID flags from the IDN protocol
const IDNFLG_CONTENTID_CONFIG_LSTFRG: u16 = 0x4000;
const IDNMSK_CONTENTID_CNKTYPE: u16 = 0x00FF;
const IDNVAL_CNKTYPE_VOID: u8 = 0x00;

// Descriptor identifiers (high byte = category, low byte = channel within category).
const DESC_X: u16 = 0x4200;
const DESC_Y: u16 = 0x4210;
const DESC_RED_638: u16 = 0x527E;
const DESC_GREEN_532: u16 = 0x5214;
const DESC_BLUE_460: u16 = 0x51CC;
const DESC_INTENSITY: u16 = 0x5C10;
const DESC_PRECISION_16BIT: u16 = 0x4010;
const DESC_NIL: u16 = 0x0000;

/// A parsed point ready for the consumer (sender or simulator).
///
/// Coordinates are normalized: `x` and `y` in `-1.0..=1.0`, colors in
/// `0.0..=1.0`. Intensity is pre-multiplied into RGB at parse time, so
/// consumers don't need to track a separate intensity field — see
/// [`parse_frame_data`] for the rationale.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReceivedPoint {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

/// Sample format identified from the channel descriptors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleFormat {
    /// X + Y, both 16-bit (4 bytes/sample).
    Xy,
    /// X + Y (16-bit) + R + G + B (8-bit), 7 bytes/sample.
    Xyrgb,
    /// X + Y (16-bit) + R + G + B + Intensity (8-bit), 8 bytes/sample.
    Xyrgbi,
}

impl SampleFormat {
    /// Bytes per sample for this format.
    pub fn sample_size(&self) -> usize {
        match self {
            SampleFormat::Xy => 4,
            SampleFormat::Xyrgb => 7,
            SampleFormat::Xyrgbi => 8,
        }
    }
}

/// A parsed chunk with timing information.
#[derive(Clone, Debug)]
pub struct ParsedChunk {
    /// Timestamp from the channel message header (u32, microseconds, wraps).
    pub timestamp_us_u32: u32,
    /// Duration of this chunk in microseconds (24-bit value from sample chunk header).
    pub duration_us: u32,
    /// Sample format used for this chunk (after descriptor dispatch).
    pub format: SampleFormat,
    /// The parsed points in this chunk.
    pub points: Vec<ReceivedPoint>,
}

/// Parse channel-message frame data from an RT_CNLMSG packet.
///
/// `data` is the raw UDP payload starting at the packet header (command
/// byte). Returns `None` for void/keepalive packets, malformed packets, or
/// packets carrying an unsupported sample format.
pub fn parse_frame_data(data: &[u8]) -> Option<ParsedChunk> {
    if data.len() < 12 {
        log::warn!("Packet too small: {} bytes", data.len());
        return None;
    }

    // Quick-reject void/keepalive packets (4-byte packet header + 8-byte
    // channel message header, no sample data). Chunk type is in the low
    // 8 bits of content_id (bytes 6-7 of the packet).
    let content_id_peek = u16::from_be_bytes([data[6], data[7]]);
    let chunk_type = (content_id_peek & IDNMSK_CONTENTID_CNKTYPE) as u8;
    if chunk_type == IDNVAL_CNKTYPE_VOID {
        log::trace!("Void/keepalive packet, ignoring");
        return None;
    }

    if data.len() < 16 {
        log::warn!("Packet too small for sample chunk: {} bytes", data.len());
        return None;
    }

    let mut cursor = Cursor::new(data);

    // Skip packet header (4 bytes) — already validated command
    cursor.set_position(4);

    // Channel message header (8 bytes)
    let _total_size = cursor.read_u16::<BE>().ok()?;
    let content_id = cursor.read_u16::<BE>().ok()?;
    let timestamp_us_u32 = cursor.read_u32::<BE>().ok()?;

    let has_config = (content_id & IDNFLG_CONTENTID_CONFIG_LSTFRG) != 0;

    // Default to XYRGBI when no config is in this packet (e.g. continuation
    // packets within a frame). The first packet of a stream always carries
    // a config, so in practice this fallback only kicks in for sequels.
    let mut format = SampleFormat::Xyrgbi;

    if has_config {
        // Channel config header (4 bytes): word_count, flags, service_id, service_mode
        let word_count = cursor.read_u8().ok()?;
        let _flags = cursor.read_u8().ok()?;
        let _service_id = cursor.read_u8().ok()?;
        let _service_mode = cursor.read_u8().ok()?;

        // word_count = number of 32-bit words of descriptors (so descriptor
        // count = word_count * 2; descriptor size = word_count * 4 bytes).
        let descriptor_count = word_count as usize * 2;
        let mut descriptors = Vec::with_capacity(descriptor_count);
        for _ in 0..descriptor_count {
            descriptors.push(cursor.read_u16::<BE>().ok()?);
        }

        match classify_format(&descriptors) {
            Some(fmt) => format = fmt,
            None => {
                log::warn!(
                    "Unsupported sample format (descriptors = {:?}); skipping chunk",
                    descriptors
                        .iter()
                        .map(|d| format!("0x{:04X}", d))
                        .collect::<Vec<_>>()
                );
                return None;
            }
        }
    }

    // Sample chunk header (4 bytes): upper 8 bits = flags, lower 24 bits = duration_us
    let flags_duration = cursor.read_u32::<BE>().ok()?;
    let duration_us = flags_duration & 0x00FF_FFFF;

    let remaining = data.len() as u64 - cursor.position();
    let sample_size = format.sample_size() as u64;
    let point_count = (remaining / sample_size) as usize;

    let mut points = Vec::with_capacity(point_count);
    for _ in 0..point_count {
        let raw_x = cursor.read_i16::<BE>().ok()?;
        let raw_y = cursor.read_i16::<BE>().ok()?;

        // TODO(coords): the sender inverts X/Y before transmit, so we
        // currently invert them back here. This is wrong when receiving
        // from third-party senders (e.g. TouchDesigner) — incoming streams
        // render flipped. The proper fix is to drop the inversion on both
        // sender and receiver in a coordinated change.
        let x = -(raw_x as f32) / 32767.0;
        let y = -(raw_y as f32) / 32767.0;

        let (r, g, b) = match format {
            SampleFormat::Xy => (0.0, 0.0, 0.0),
            SampleFormat::Xyrgb => {
                let r = cursor.read_u8().ok()? as f32 / 255.0;
                let g = cursor.read_u8().ok()? as f32 / 255.0;
                let b = cursor.read_u8().ok()? as f32 / 255.0;
                (r, g, b)
            }
            SampleFormat::Xyrgbi => {
                let r = cursor.read_u8().ok()? as f32 / 255.0;
                let g = cursor.read_u8().ok()? as f32 / 255.0;
                let b = cursor.read_u8().ok()? as f32 / 255.0;
                let intensity = cursor.read_u8().ok()? as f32 / 255.0;
                // Pre-multiply intensity into RGB. Modulaser's pipeline has no
                // separate intensity concept, and consumers shouldn't have to
                // know whether their source carries an intensity channel — so
                // we collapse it here and expose a single normalized RGB
                // triple to all downstream code.
                (r * intensity, g * intensity, b * intensity)
            }
        };

        points.push(ReceivedPoint { x, y, r, g, b });
    }

    Some(ParsedChunk {
        timestamp_us_u32,
        duration_us,
        format,
        points,
    })
}

/// Classify the descriptor array into a known [`SampleFormat`].
///
/// Walks the descriptors, treating `0x4010` as a "make previous channel
/// 16-bit precision" modifier and `0x0000` as padding, then matches the
/// resulting channel signature against the known formats.
fn classify_format(descriptors: &[u16]) -> Option<SampleFormat> {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Channel {
        id: u16,
        bytes: u8,
    }

    let mut channels: Vec<Channel> = Vec::with_capacity(descriptors.len());
    let mut i = 0;
    while i < descriptors.len() {
        let desc = descriptors[i];
        if desc == DESC_NIL {
            i += 1;
            continue;
        }
        // If next descriptor is the 16-bit precision modifier, the channel
        // is 2 bytes wide; otherwise it's the default 1 byte.
        let (bytes, advance) = if descriptors.get(i + 1).copied() == Some(DESC_PRECISION_16BIT) {
            (2u8, 2)
        } else {
            (1u8, 1)
        };
        channels.push(Channel { id: desc, bytes });
        i += advance;
    }

    let ids: Vec<u16> = channels.iter().map(|c| c.id).collect();
    let total_bytes: usize = channels.iter().map(|c| c.bytes as usize).sum();

    match ids.as_slice() {
        [DESC_X, DESC_Y] if total_bytes == 4 => Some(SampleFormat::Xy),
        [DESC_X, DESC_Y, DESC_RED_638, DESC_GREEN_532, DESC_BLUE_460] if total_bytes == 7 => {
            Some(SampleFormat::Xyrgb)
        }
        [DESC_X, DESC_Y, DESC_RED_638, DESC_GREEN_532, DESC_BLUE_460, DESC_INTENSITY]
            if total_bytes == 8 =>
        {
            Some(SampleFormat::Xyrgbi)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IDN packet header (command + flags + sequence) — 4 bytes.
    fn header(seq: u16) -> Vec<u8> {
        let mut v = Vec::with_capacity(4);
        v.push(0x40); // IDNCMD_RT_CNLMSG
        v.push(0x00);
        v.extend_from_slice(&seq.to_be_bytes());
        v
    }

    /// Build a full RT_CNLMSG packet with config + sample chunk for the
    /// supplied descriptors and packed point bytes.
    fn build_packet(descriptors: &[u16], chunk_type: u8, sample_bytes: &[u8]) -> Vec<u8> {
        let mut pkt = header(1);

        // Compute total_size = ChannelMessage (8) + ChannelConfig (4) +
        // descriptor bytes + SampleChunkHeader (4) + sample bytes
        let descriptor_bytes = descriptors.len() * 2;
        let total_size = 8 + 4 + descriptor_bytes + 4 + sample_bytes.len();

        // Channel message header (8 bytes)
        pkt.extend_from_slice(&(total_size as u16).to_be_bytes());
        // content_id: CHANNELMSG | CONFIG_LSTFRG | chunk_type
        let content_id: u16 = 0x8000 | 0x4000 | chunk_type as u16;
        pkt.extend_from_slice(&content_id.to_be_bytes());
        pkt.extend_from_slice(&0x12345678u32.to_be_bytes()); // timestamp

        // Channel config header (4 bytes)
        assert!(
            descriptors.len().is_multiple_of(2),
            "descriptor count must be even"
        );
        let word_count = (descriptors.len() / 2) as u8;
        pkt.push(word_count);
        pkt.push(0x00); // flags
        pkt.push(1); // service_id
        pkt.push(0x02); // service_mode (discrete frame)

        // Descriptor words
        for &d in descriptors {
            pkt.extend_from_slice(&d.to_be_bytes());
        }

        // Sample chunk header (flags=0, duration=1000us)
        pkt.extend_from_slice(&1000u32.to_be_bytes());

        // Sample bytes
        pkt.extend_from_slice(sample_bytes);

        pkt
    }

    fn descriptors_xy() -> Vec<u16> {
        vec![DESC_X, DESC_PRECISION_16BIT, DESC_Y, DESC_PRECISION_16BIT]
    }

    fn descriptors_xyrgb() -> Vec<u16> {
        vec![
            DESC_X,
            DESC_PRECISION_16BIT,
            DESC_Y,
            DESC_PRECISION_16BIT,
            DESC_RED_638,
            DESC_GREEN_532,
            DESC_BLUE_460,
            DESC_NIL,
        ]
    }

    fn descriptors_xyrgbi() -> Vec<u16> {
        vec![
            DESC_X,
            DESC_PRECISION_16BIT,
            DESC_Y,
            DESC_PRECISION_16BIT,
            DESC_RED_638,
            DESC_GREEN_532,
            DESC_BLUE_460,
            DESC_INTENSITY,
        ]
    }

    #[test]
    fn classify_xy() {
        assert_eq!(classify_format(&descriptors_xy()), Some(SampleFormat::Xy));
    }

    #[test]
    fn classify_xyrgb_with_padding() {
        assert_eq!(
            classify_format(&descriptors_xyrgb()),
            Some(SampleFormat::Xyrgb)
        );
    }

    #[test]
    fn classify_xyrgbi() {
        assert_eq!(
            classify_format(&descriptors_xyrgbi()),
            Some(SampleFormat::Xyrgbi)
        );
    }

    #[test]
    fn classify_unknown_descriptors_returns_none() {
        // Made-up channel id
        let bogus = vec![0x1234, 0x4010];
        assert!(classify_format(&bogus).is_none());
    }

    #[test]
    fn parse_xyrgbi_packet() {
        // One point: x = 16383 (half right), y = -16384 (half down),
        // r=255, g=128, b=0, intensity=128 -> pre-multiplied
        let mut samples = Vec::new();
        samples.extend_from_slice(&16383i16.to_be_bytes());
        samples.extend_from_slice(&(-16384i16).to_be_bytes());
        samples.extend_from_slice(&[255, 128, 0, 128]);

        let pkt = build_packet(&descriptors_xyrgbi(), 0x02, &samples);
        let parsed = parse_frame_data(&pkt).expect("should parse");

        assert_eq!(parsed.format, SampleFormat::Xyrgbi);
        assert_eq!(parsed.points.len(), 1);
        let p = parsed.points[0];

        // Coordinate inversion is still in effect (see TODO in parser).
        assert!((p.x - -(16383.0 / 32767.0)).abs() < 1e-4);
        assert!((p.y - -(-16384.0 / 32767.0)).abs() < 1e-4);

        // Intensity pre-multiplied: 0.5 * each color channel
        let i = 128.0 / 255.0;
        assert!((p.r - (255.0 / 255.0) * i).abs() < 1e-4);
        assert!((p.g - (128.0 / 255.0) * i).abs() < 1e-4);
        assert!((p.b - 0.0).abs() < 1e-4);
    }

    #[test]
    fn parse_xyrgb_packet() {
        // Two points; XYRGB has no intensity, so colors come through as-is.
        let mut samples = Vec::new();
        // point 0: x=0, y=0, rgb=(255,0,0)
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&[255, 0, 0]);
        // point 1: x=32767, y=-32767, rgb=(0,255,0)
        samples.extend_from_slice(&32767i16.to_be_bytes());
        samples.extend_from_slice(&(-32767i16).to_be_bytes());
        samples.extend_from_slice(&[0, 255, 0]);

        let pkt = build_packet(&descriptors_xyrgb(), 0x02, &samples);
        let parsed = parse_frame_data(&pkt).expect("should parse");

        assert_eq!(parsed.format, SampleFormat::Xyrgb);
        assert_eq!(parsed.points.len(), 2);

        let p0 = parsed.points[0];
        assert!((p0.r - 1.0).abs() < 1e-4);
        assert!(p0.g.abs() < 1e-4);
        assert!(p0.b.abs() < 1e-4);

        let p1 = parsed.points[1];
        // Inverted X: 32767 -> -1.0
        assert!((p1.x - -1.0).abs() < 1e-3);
        // Inverted Y: -32767 -> 1.0
        assert!((p1.y - 1.0).abs() < 1e-3);
        assert!((p1.g - 1.0).abs() < 1e-4);
    }

    #[test]
    fn parse_xy_packet_has_no_color() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&8191i16.to_be_bytes());
        samples.extend_from_slice(&8191i16.to_be_bytes());

        let pkt = build_packet(&descriptors_xy(), 0x02, &samples);
        let parsed = parse_frame_data(&pkt).expect("should parse");

        assert_eq!(parsed.format, SampleFormat::Xy);
        assert_eq!(parsed.points.len(), 1);
        assert_eq!(parsed.points[0].r, 0.0);
        assert_eq!(parsed.points[0].g, 0.0);
        assert_eq!(parsed.points[0].b, 0.0);
    }

    #[test]
    fn parse_unsupported_format_returns_none() {
        // Made-up 6-byte format with two bogus channels — descriptor count
        // is even (word_count = 1) but doesn't match any known signature.
        let descriptors = vec![0x1234u16, 0x5678u16];
        let mut samples = Vec::new();
        samples.extend_from_slice(&0u16.to_be_bytes());
        samples.extend_from_slice(&0u16.to_be_bytes());
        samples.extend_from_slice(&0u16.to_be_bytes());

        let pkt = build_packet(&descriptors, 0x02, &samples);
        assert!(parse_frame_data(&pkt).is_none());
    }

    #[test]
    fn parse_void_packet_returns_none() {
        // 4-byte packet header + 8-byte channel message header (chunk type 0)
        let mut pkt = header(0);
        pkt.extend_from_slice(&8u16.to_be_bytes()); // total_size
        pkt.extend_from_slice(&0x8000u16.to_be_bytes()); // content_id with VOID chunk
        pkt.extend_from_slice(&0u32.to_be_bytes()); // timestamp

        assert!(parse_frame_data(&pkt).is_none());
    }
}
