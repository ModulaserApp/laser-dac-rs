//! Protocol handling for parsing IDN frame data packets.

use byteorder::{ReadBytesExt, BE};
use std::io::Cursor;

/// A point ready for rendering.
#[derive(Clone, Debug)]
pub struct RenderPoint {
    pub x: f32,
    pub y: f32,
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub intensity: f32,
}

/// A parsed chunk with timing information.
#[derive(Clone, Debug)]
pub struct ParsedChunk {
    /// Timestamp from the channel message header (u32, microseconds, wraps).
    pub timestamp_us_u32: u32,
    /// Duration of this chunk in microseconds (24-bit value from sample chunk header).
    pub duration_us: u32,
    /// The parsed points in this chunk.
    pub points: Vec<RenderPoint>,
}

/// Content ID flags from protocol
const IDNFLG_CONTENTID_CONFIG_LSTFRG: u16 = 0x4000;
const IDNMSK_CONTENTID_CNKTYPE: u16 = 0x00FF;
const IDNVAL_CNKTYPE_VOID: u8 = 0x00;

/// Sample sizes for different point formats
const XYRGBI_SAMPLE_SIZE: usize = 8;

/// Parse frame data from an RT_CNLMSG packet.
///
/// Returns the parsed chunk with timing info or None if parsing failed.
pub fn parse_frame_data(data: &[u8]) -> Option<ParsedChunk> {
    if data.len() < 12 {
        log::warn!("Packet too small: {} bytes", data.len());
        return None;
    }

    // Check for void/keepalive packets (4-byte header + 8-byte channel msg, no sample data).
    // The chunk type is in the lower 8 bits of content_id (bytes 6-7).
    let content_id = u16::from_be_bytes([data[6], data[7]]);
    let chunk_type = (content_id & IDNMSK_CONTENTID_CNKTYPE) as u8;
    if chunk_type == IDNVAL_CNKTYPE_VOID {
        log::trace!("Void/keepalive packet, ignoring");
        return None;
    }

    if data.len() < 16 {
        log::warn!("Packet too small: {} bytes", data.len());
        return None;
    }

    // Log first 64 bytes as hex for debugging
    let hex_dump: String = data
        .iter()
        .take(64)
        .map(|b| format!("{:02X} ", b))
        .collect();
    log::trace!("Raw packet ({} bytes): {}", data.len(), hex_dump);

    let mut cursor = Cursor::new(data);

    // Skip packet header (4 bytes) - already validated command
    cursor.set_position(4);

    // Parse channel message header (8 bytes)
    let total_size = cursor.read_u16::<BE>().ok()?;
    let content_id = cursor.read_u16::<BE>().ok()?;
    let timestamp_us_u32 = cursor.read_u32::<BE>().ok()?;

    log::trace!(
        "Channel msg: total_size={}, content_id=0x{:04X}, timestamp={}, has_config={}",
        total_size,
        content_id,
        timestamp_us_u32,
        (content_id & IDNFLG_CONTENTID_CONFIG_LSTFRG) != 0
    );

    // Check for config flag
    let has_config = (content_id & IDNFLG_CONTENTID_CONFIG_LSTFRG) != 0;

    if has_config {
        // Parse channel config header (4 bytes)
        let word_count = cursor.read_u8().ok()?;
        let flags = cursor.read_u8().ok()?;
        let service_id = cursor.read_u8().ok()?;
        let service_mode = cursor.read_u8().ok()?;

        log::trace!(
            "Config: word_count={}, flags=0x{:02X}, service_id={}, service_mode={}",
            word_count,
            flags,
            service_id,
            service_mode
        );

        // Skip descriptor words (word_count represents 32-bit words, i.e., descriptor pairs)
        // Each word is 4 bytes (2 descriptors * 2 bytes each)
        let skip_bytes = word_count as u64 * 4;
        log::trace!(
            "Skipping {} descriptor bytes, cursor before={}",
            skip_bytes,
            cursor.position()
        );
        cursor.set_position(cursor.position() + skip_bytes);
        log::trace!("Cursor after skip={}", cursor.position());
    } else {
        log::trace!("No config in packet");
    }

    // Parse sample chunk header (4 bytes)
    // The upper 8 bits are flags, the lower 24 bits are duration_us
    let flags_duration = cursor.read_u32::<BE>().ok()?;
    let duration_us = flags_duration & 0x00FF_FFFF;
    let chunk_flags = (flags_duration >> 24) as u8;
    log::trace!(
        "Sample chunk header: flags=0x{:02X}, duration_us={}, cursor now={}",
        chunk_flags,
        duration_us,
        cursor.position()
    );

    // Parse remaining bytes as XYRGBI points
    let remaining = data.len() as u64 - cursor.position();
    let point_count = remaining as usize / XYRGBI_SAMPLE_SIZE;

    log::trace!(
        "Parsing {} points from {} remaining bytes (pos={})",
        point_count,
        remaining,
        cursor.position()
    );

    // Dump first 24 bytes of point data
    let point_start = cursor.position() as usize;
    if point_start + 24 <= data.len() {
        let point_hex: String = data[point_start..point_start + 24]
            .iter()
            .map(|b| format!("{:02X} ", b))
            .collect();
        log::trace!("First 24 bytes of point data: {}", point_hex);
    }

    let mut points = Vec::with_capacity(point_count);

    for i in 0..point_count {
        let x = cursor.read_i16::<BE>().ok()?;
        let y = cursor.read_i16::<BE>().ok()?;
        let r = cursor.read_u8().ok()?;
        let g = cursor.read_u8().ok()?;
        let b = cursor.read_u8().ok()?;
        let intensity = cursor.read_u8().ok()?;

        // Log first 3 points for debugging
        if i < 3 {
            let norm_x = -(x as f32) / 32767.0;
            let norm_y = -(y as f32) / 32767.0;
            log::trace!(
                "Point {}: raw x={}, y={}, r={}, g={}, b={}, i={} -> norm x={:.3}, y={:.3}",
                i,
                x,
                y,
                r,
                g,
                b,
                intensity,
                norm_x,
                norm_y
            );
        }

        // Convert from IDN format to normalized coordinates
        // Note: IDN backend inverts coordinates, so we invert them back
        points.push(RenderPoint {
            x: -(x as f32) / 32767.0,
            y: -(y as f32) / 32767.0,
            r: r as f32 / 255.0,
            g: g as f32 / 255.0,
            b: b as f32 / 255.0,
            intensity: intensity as f32 / 255.0,
        });
    }

    Some(ParsedChunk {
        timestamp_us_u32,
        duration_us,
        points,
    })
}
