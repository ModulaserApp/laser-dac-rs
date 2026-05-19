//! IDN channel-message parser for the receiver server.
//!
//! Parses the channel message header, channel config header, descriptor
//! array, and sample chunk to produce a [`ParsedChunk`] of
//! [`ReceivedPoint`]s. The parser is descriptor-driven: it reads the
//! declared sample format from the channel config and dispatches on it,
//! so XY, XYRGB, XYRGBI, high-res XYRGB, and extended streams are handled.
//! Unsupported sample formats are logged and skipped.

use byteorder::{ReadBytesExt, BE};
use std::collections::HashMap;
use std::io::Cursor;
use std::net::SocketAddr;

// Content ID flags from the IDN protocol
const IDNFLG_CONTENTID_CHANNELMSG: u16 = 0x8000;
const IDNFLG_CONTENTID_CONFIG_LSTFRG: u16 = 0x4000;
const IDNMSK_CONTENTID_CHANNELID: u16 = 0x3F00;
const IDNMSK_CONTENTID_CNKTYPE: u16 = 0x00FF;
const IDNVAL_CNKTYPE_VOID: u8 = 0x00;
const IDNVAL_CNKTYPE_LPGRF_WAVE: u8 = 0x01;
const IDNVAL_CNKTYPE_LPGRF_FRAME: u8 = 0x02;
const IDNVAL_CNKTYPE_LPGRF_FRAME_FIRST: u8 = 0x03;
const IDNVAL_CNKTYPE_LPGRF_FRAME_SEQUEL: u8 = 0xC0;

// Channel configuration flags from IDN-Stream section 2.2.
const IDNFLG_CHNCFG_ROUTING: u8 = 0x01;
const IDNFLG_CHNCFG_CLOSE: u8 = 0x02;
const IDNMSK_CHNCFG_SDM: u8 = 0x30;
const IDNMSK_CHUNKFLAGS_SCM: u8 = 0x30;

// Laser projector service modes.
const IDNVAL_SMOD_VOID: u8 = 0x00;
const IDNVAL_SMOD_LPGRF_CONTINUOUS: u8 = 0x01;
const IDNVAL_SMOD_LPGRF_DISCRETE: u8 = 0x02;

// Descriptor identifiers (high byte = category, low byte = channel within category).
const DESC_X: u16 = 0x4200;
const DESC_Y: u16 = 0x4210;
const DESC_RED_638: u16 = 0x527E;
const DESC_GREEN_532: u16 = 0x5214;
const DESC_BLUE_460: u16 = 0x51CC;
const DESC_INTENSITY: u16 = 0x5C10;
const DESC_USER_1: u16 = 0x51BD;
const DESC_USER_2: u16 = 0x5241;
const DESC_USER_3: u16 = 0x51E8;
const DESC_X_PRIME: u16 = 0x4201;
const DESC_PRECISION_16BIT: u16 = 0x4010;
#[cfg(test)]
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
    /// X + Y + R + G + B, all 16-bit (10 bytes/sample).
    XyrgbHighRes,
    /// X + Y + R + G + B + Intensity + four user channels, all 16-bit (20 bytes/sample).
    Extended,
}

/// Logical IDN laser-graphics chunk type parsed from the content id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkType {
    /// Void/keepalive message with no point data.
    Void,
    /// Continuous/wave sample stream without discrete frame boundaries.
    Wave,
    /// Complete discrete frame in a single channel message.
    Frame,
    /// First channel message of a multi-message discrete frame.
    FrameFirst,
    /// Continuation channel message of a multi-message discrete frame.
    FrameSequel,
    /// Unknown chunk type byte.
    Other(u8),
}

impl ChunkType {
    fn from_byte(value: u8) -> Self {
        match value {
            IDNVAL_CNKTYPE_VOID => ChunkType::Void,
            IDNVAL_CNKTYPE_LPGRF_WAVE => ChunkType::Wave,
            IDNVAL_CNKTYPE_LPGRF_FRAME => ChunkType::Frame,
            IDNVAL_CNKTYPE_LPGRF_FRAME_FIRST => ChunkType::FrameFirst,
            IDNVAL_CNKTYPE_LPGRF_FRAME_SEQUEL => ChunkType::FrameSequel,
            other => ChunkType::Other(other),
        }
    }
}

impl SampleFormat {
    /// Bytes per sample for this format.
    pub fn sample_size(&self) -> usize {
        match self {
            SampleFormat::Xy => 4,
            SampleFormat::Xyrgb => 7,
            SampleFormat::Xyrgbi => 8,
            SampleFormat::XyrgbHighRes => 10,
            SampleFormat::Extended => 20,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ParserChannelKey {
    source_addr: SocketAddr,
    channel_id: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ChannelState {
    format: SampleFormat,
    service_data_match: u8,
    service_mode: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StateUpdate {
    None,
    Store(ChannelState),
    Remove,
}

#[derive(Clone, Debug)]
struct ParsedPacket {
    chunk: Option<ParsedChunk>,
    state_update: StateUpdate,
}

/// Stateful IDN channel-message parser.
///
/// IDN channel configuration is per source/channel and may be omitted from
/// later messages. This parser caches the last supported sample format for each
/// source/channel and uses it for no-config messages instead of guessing.
#[derive(Default)]
pub struct FrameParser {
    channels: HashMap<ParserChannelKey, ChannelState>,
}

impl FrameParser {
    /// Create an empty parser with no cached channel configuration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear all cached channel configuration.
    pub fn reset(&mut self) {
        self.channels.clear();
    }

    /// Parse channel-message frame data using cached per-source/channel config.
    pub fn parse_frame_data(
        &mut self,
        source_addr: SocketAddr,
        data: &[u8],
    ) -> Option<ParsedChunk> {
        let metadata = parse_metadata(data)?;
        let key = ParserChannelKey {
            source_addr,
            channel_id: metadata.channel_id,
        };
        let cached_state = self.channels.get(&key).copied();
        let parsed = parse_frame_data_with_metadata(data, metadata, cached_state)?;

        match parsed.state_update {
            StateUpdate::None => {}
            StateUpdate::Store(state) => {
                self.channels.insert(key, state);
            }
            StateUpdate::Remove => {
                self.channels.remove(&key);
            }
        }

        parsed.chunk
    }
}

#[derive(Clone, Copy, Debug)]
struct ChunkMetadata {
    sequence: u16,
    content_id: u16,
    channel_id: u8,
    chunk_type: ChunkType,
    config_or_last_fragment: bool,
    has_config: bool,
    is_last_fragment: bool,
    timestamp_us_u32: u32,
    total_size: usize,
}

#[derive(Clone, Copy, Debug)]
struct ChannelConfig {
    flags: u8,
    service_mode: u8,
    format: Option<SampleFormat>,
}

impl ChannelConfig {
    fn has_routing(self) -> bool {
        (self.flags & IDNFLG_CHNCFG_ROUTING) != 0
    }

    fn closes_channel(self) -> bool {
        (self.flags & IDNFLG_CHNCFG_CLOSE) != 0
    }

    fn service_data_match(self) -> u8 {
        self.flags & IDNMSK_CHNCFG_SDM
    }
}

/// A parsed chunk with timing information.
#[derive(Clone, Debug)]
pub struct ParsedChunk {
    /// Packet sequence number from the IDN packet header.
    pub sequence: u16,
    /// Raw channel-message content id.
    pub content_id: u16,
    /// Six-bit channel id parsed from the content id.
    pub channel_id: u8,
    /// Chunk type parsed from the low byte of the content id.
    pub chunk_type: ChunkType,
    /// Raw state of the CONFIG/LSTFRG content-id bit (`0x4000`).
    pub config_or_last_fragment: bool,
    /// Whether this message contains a channel configuration header.
    pub has_config: bool,
    /// Whether this chunk completes a discrete frame.
    pub is_last_fragment: bool,
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
/// byte). This stateless helper only parses messages that carry channel
/// configuration. Use [`FrameParser`] for full IDN receiver behavior where
/// no-config messages reuse prior per-source/channel configuration.
///
/// Returns `None` for void/keepalive packets, malformed packets, no-config
/// packets without parser state, or packets carrying an unsupported sample
/// format.
pub fn parse_frame_data(data: &[u8]) -> Option<ParsedChunk> {
    let metadata = parse_metadata(data)?;
    parse_frame_data_with_metadata(data, metadata, None)?.chunk
}

fn parse_metadata(data: &[u8]) -> Option<ChunkMetadata> {
    if data.len() < 12 {
        log::warn!("Packet too small: {} bytes", data.len());
        return None;
    }

    let total_size = u16::from_be_bytes([data[4], data[5]]) as usize;
    if !(8..=0xFF00).contains(&total_size) {
        log::warn!(
            "Channel message size out of spec range: {} bytes",
            total_size
        );
        return None;
    }

    let packet_len = 4 + total_size;
    if data.len() < packet_len {
        log::warn!(
            "Packet truncated: got {} bytes, channel message declares {} bytes",
            data.len(),
            total_size
        );
        return None;
    }

    let sequence = u16::from_be_bytes([data[2], data[3]]);
    let content_id = u16::from_be_bytes([data[6], data[7]]);
    if (content_id & IDNFLG_CONTENTID_CHANNELMSG) == 0 {
        log::warn!("Non-channel IDN message received: content_id=0x{content_id:04X}");
        return None;
    }

    let timestamp_us_u32 = u32::from_be_bytes([data[8], data[9], data[10], data[11]]);

    let config_or_last_fragment = (content_id & IDNFLG_CONTENTID_CONFIG_LSTFRG) != 0;
    let channel_id = ((content_id & IDNMSK_CONTENTID_CHANNELID) >> 8) as u8;
    let chunk_type_byte = (content_id & IDNMSK_CONTENTID_CNKTYPE) as u8;
    let chunk_type = ChunkType::from_byte(chunk_type_byte);
    let has_config = config_or_last_fragment && chunk_type_byte < 0xC0;
    let is_last_fragment = match chunk_type {
        ChunkType::Frame => true,
        ChunkType::FrameSequel => config_or_last_fragment,
        _ => false,
    };

    Some(ChunkMetadata {
        sequence,
        content_id,
        channel_id,
        chunk_type,
        config_or_last_fragment,
        has_config,
        is_last_fragment,
        timestamp_us_u32,
        total_size,
    })
}

fn parse_frame_data_with_metadata(
    data: &[u8],
    metadata: ChunkMetadata,
    cached_state: Option<ChannelState>,
) -> Option<ParsedPacket> {
    let packet_len = 4 + metadata.total_size;
    let data = &data[..packet_len];
    let mut cursor = Cursor::new(data);
    cursor.set_position(12);

    let config = if metadata.has_config {
        Some(parse_channel_config(&mut cursor)?)
    } else {
        None
    };

    let close_after_message = config.is_some_and(ChannelConfig::closes_channel);

    let state = match config {
        Some(config) if config.has_routing() => match config.format {
            Some(format) if config.service_mode != IDNVAL_SMOD_VOID => Some(ChannelState {
                format,
                service_data_match: config.service_data_match(),
                service_mode: config.service_mode,
            }),
            Some(_) | None => {
                return Some(ParsedPacket {
                    chunk: None,
                    state_update: StateUpdate::Remove,
                });
            }
        },
        Some(_) | None => cached_state,
    };

    if metadata.chunk_type == ChunkType::Void {
        let update = state_update_for_config(config, state, close_after_message);
        return Some(ParsedPacket {
            chunk: None,
            state_update: update,
        });
    }

    let Some(state) = state else {
        log::warn!(
            "No channel configuration cached for channel {}; skipping chunk",
            metadata.channel_id
        );
        return None;
    };

    if !service_mode_accepts_chunk(state.service_mode, metadata.chunk_type) {
        log::warn!(
            "Service mode 0x{:02X} does not accept {:?}; skipping chunk",
            state.service_mode,
            metadata.chunk_type
        );
        let update = state_update_for_config(config, Some(state), close_after_message);
        return Some(ParsedPacket {
            chunk: None,
            state_update: update,
        });
    }

    // Sample chunk header (4 bytes): upper 8 bits = flags, lower 24 bits = duration_us
    let flags_duration = cursor.read_u32::<BE>().ok()?;
    let chunk_flags = (flags_duration >> 24) as u8;
    if (chunk_flags & IDNMSK_CHUNKFLAGS_SCM) != state.service_data_match {
        log::warn!(
            "Service data match mismatch on channel {}: chunk=0x{:02X}, config=0x{:02X}",
            metadata.channel_id,
            chunk_flags & IDNMSK_CHUNKFLAGS_SCM,
            state.service_data_match
        );
        return Some(ParsedPacket {
            chunk: None,
            state_update: StateUpdate::Remove,
        });
    }

    let duration_us = flags_duration & 0x00FF_FFFF;

    let remaining = data.len() as u64 - cursor.position();
    let sample_size = state.format.sample_size() as u64;
    if !remaining.is_multiple_of(sample_size) {
        log::warn!(
            "Sample data size {} is not a multiple of sample size {}; skipping chunk",
            remaining,
            sample_size
        );
        return None;
    }
    let point_count = (remaining / sample_size) as usize;

    let mut points = Vec::with_capacity(point_count);
    for _ in 0..point_count {
        let raw_x = cursor.read_i16::<BE>().ok()?;
        let raw_y = cursor.read_i16::<BE>().ok()?;

        let x = normalize_i16_coordinate(raw_x);
        let y = normalize_i16_coordinate(raw_y);

        let (r, g, b) = match state.format {
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
            SampleFormat::XyrgbHighRes => {
                let r = cursor.read_u16::<BE>().ok()? as f32 / 65535.0;
                let g = cursor.read_u16::<BE>().ok()? as f32 / 65535.0;
                let b = cursor.read_u16::<BE>().ok()? as f32 / 65535.0;
                (r, g, b)
            }
            SampleFormat::Extended => {
                let r = cursor.read_u16::<BE>().ok()? as f32 / 65535.0;
                let g = cursor.read_u16::<BE>().ok()? as f32 / 65535.0;
                let b = cursor.read_u16::<BE>().ok()? as f32 / 65535.0;
                let intensity = cursor.read_u16::<BE>().ok()? as f32 / 65535.0;
                for _ in 0..4 {
                    let _ = cursor.read_u16::<BE>().ok()?;
                }
                (r * intensity, g * intensity, b * intensity)
            }
        };

        points.push(ReceivedPoint { x, y, r, g, b });
    }

    let chunk = ParsedChunk {
        sequence: metadata.sequence,
        content_id: metadata.content_id,
        channel_id: metadata.channel_id,
        chunk_type: metadata.chunk_type,
        config_or_last_fragment: metadata.config_or_last_fragment,
        has_config: metadata.has_config,
        is_last_fragment: metadata.is_last_fragment,
        timestamp_us_u32: metadata.timestamp_us_u32,
        duration_us,
        format: state.format,
        points,
    };

    Some(ParsedPacket {
        chunk: Some(chunk),
        state_update: state_update_for_config(config, Some(state), close_after_message),
    })
}

fn parse_channel_config(cursor: &mut Cursor<&[u8]>) -> Option<ChannelConfig> {
    // Channel config header (4 bytes): word_count, flags, service_id, service_mode.
    let word_count = cursor.read_u8().ok()?;
    let flags = cursor.read_u8().ok()?;
    let _service_id = cursor.read_u8().ok()?;
    let service_mode = cursor.read_u8().ok()?;

    // word_count = number of 32-bit words in the service config. For laser
    // graphics this is the descriptor array, so descriptor count = word_count * 2.
    let descriptor_count = word_count as usize * 2;
    let mut descriptors = Vec::with_capacity(descriptor_count);
    for _ in 0..descriptor_count {
        descriptors.push(cursor.read_u16::<BE>().ok()?);
    }

    let format = if (flags & IDNFLG_CHNCFG_ROUTING) != 0 && service_mode != IDNVAL_SMOD_VOID {
        match classify_format(&descriptors) {
            Some(fmt) => Some(fmt),
            None => {
                log::warn!(
                    "Unsupported sample format (descriptors = {:?}); skipping chunk",
                    descriptors
                        .iter()
                        .map(|d| format!("0x{:04X}", d))
                        .collect::<Vec<_>>()
                );
                None
            }
        }
    } else {
        None
    };

    Some(ChannelConfig {
        flags,
        service_mode,
        format,
    })
}

fn state_update_for_config(
    config: Option<ChannelConfig>,
    state: Option<ChannelState>,
    close_after_message: bool,
) -> StateUpdate {
    if close_after_message {
        StateUpdate::Remove
    } else if config.is_some_and(ChannelConfig::has_routing) {
        state.map_or(StateUpdate::Remove, StateUpdate::Store)
    } else {
        StateUpdate::None
    }
}

fn service_mode_accepts_chunk(service_mode: u8, chunk_type: ChunkType) -> bool {
    matches!(
        (service_mode, chunk_type),
        (IDNVAL_SMOD_LPGRF_CONTINUOUS, ChunkType::Wave)
            | (IDNVAL_SMOD_LPGRF_DISCRETE, ChunkType::Frame)
            | (IDNVAL_SMOD_LPGRF_DISCRETE, ChunkType::FrameFirst)
            | (IDNVAL_SMOD_LPGRF_DISCRETE, ChunkType::FrameSequel)
    )
}

fn normalize_i16_coordinate(value: i16) -> f32 {
    if value == i16::MIN {
        -1.0
    } else {
        value as f32 / 32767.0
    }
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
        if (desc & 0xF000) == 0x0000 {
            // Category 0 void tags have no sample octet and may carry suffix
            // words. 0x0000 is the common padding case.
            i += 1 + (desc & 0x000F) as usize;
            continue;
        }
        if (desc & 0xFF00) == 0x1000 {
            // Break starts another scan-head group. This receiver exposes one
            // XY/RGB output, so it decodes the first group and ignores the rest.
            break;
        }
        if (desc & 0xF000) == 0x1000 {
            // Category 1 modifiers, including space modifiers, do not describe
            // sample octets.
            i += 1;
            continue;
        }
        if desc == DESC_PRECISION_16BIT {
            return None;
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
        [DESC_X, DESC_Y, DESC_RED_638, DESC_GREEN_532, DESC_BLUE_460] if total_bytes == 10 => {
            Some(SampleFormat::XyrgbHighRes)
        }
        [DESC_X, DESC_Y, DESC_RED_638, DESC_GREEN_532, DESC_BLUE_460, DESC_INTENSITY]
            if total_bytes == 8 =>
        {
            Some(SampleFormat::Xyrgbi)
        }
        [DESC_X, DESC_Y, DESC_RED_638, DESC_GREEN_532, DESC_BLUE_460, DESC_INTENSITY, DESC_USER_1, DESC_USER_2, DESC_USER_3, DESC_X_PRIME]
            if total_bytes == 20 =>
        {
            Some(SampleFormat::Extended)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

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
        build_packet_on_channel(descriptors, 0, chunk_type, sample_bytes)
    }

    fn build_packet_on_channel(
        descriptors: &[u16],
        channel_id: u8,
        chunk_type: u8,
        sample_bytes: &[u8],
    ) -> Vec<u8> {
        build_packet_on_channel_with_flags(
            descriptors,
            channel_id,
            chunk_type,
            sample_bytes,
            IDNFLG_CHNCFG_ROUTING,
            0,
        )
    }

    fn build_packet_on_channel_with_flags(
        descriptors: &[u16],
        channel_id: u8,
        chunk_type: u8,
        sample_bytes: &[u8],
        config_flags: u8,
        sample_flags: u8,
    ) -> Vec<u8> {
        let mut pkt = header(1);

        // Compute total_size = ChannelMessage (8) + ChannelConfig (4) +
        // descriptor bytes + SampleChunkHeader (4) + sample bytes
        let descriptor_bytes = descriptors.len() * 2;
        let total_size = 8 + 4 + descriptor_bytes + 4 + sample_bytes.len();

        // Channel message header (8 bytes)
        pkt.extend_from_slice(&(total_size as u16).to_be_bytes());
        // content_id: CHANNELMSG | CONFIG_LSTFRG | chunk_type
        let content_id: u16 =
            0x8000 | 0x4000 | (((channel_id as u16) & 0x3F) << 8) | chunk_type as u16;
        pkt.extend_from_slice(&content_id.to_be_bytes());
        pkt.extend_from_slice(&0x12345678u32.to_be_bytes()); // timestamp

        // Channel config header (4 bytes)
        assert!(
            descriptors.len().is_multiple_of(2),
            "descriptor count must be even"
        );
        let word_count = (descriptors.len() / 2) as u8;
        pkt.push(word_count);
        pkt.push(config_flags);
        pkt.push(channel_id + 1); // service_id
        pkt.push(0x02); // service_mode (discrete frame)

        // Descriptor words
        for &d in descriptors {
            pkt.extend_from_slice(&d.to_be_bytes());
        }

        // Sample chunk header (upper byte flags, lower 24 bits duration_us)
        pkt.extend_from_slice(&(((sample_flags as u32) << 24) | 1000).to_be_bytes());

        // Sample bytes
        pkt.extend_from_slice(sample_bytes);

        pkt
    }

    fn build_void_config_packet(descriptors: &[u16], channel_id: u8, config_flags: u8) -> Vec<u8> {
        let mut pkt = header(1);
        let descriptor_bytes = descriptors.len() * 2;
        let total_size = 8 + 4 + descriptor_bytes;

        pkt.extend_from_slice(&(total_size as u16).to_be_bytes());
        let content_id: u16 = 0x8000 | 0x4000 | (((channel_id as u16) & 0x3F) << 8);
        pkt.extend_from_slice(&content_id.to_be_bytes());
        pkt.extend_from_slice(&0x12345678u32.to_be_bytes());

        assert!(descriptors.len().is_multiple_of(2));
        pkt.push((descriptors.len() / 2) as u8);
        pkt.push(config_flags);
        pkt.push(channel_id + 1);
        pkt.push(0x02);
        for &d in descriptors {
            pkt.extend_from_slice(&d.to_be_bytes());
        }

        pkt
    }

    fn build_sequel_packet(
        seq: u16,
        channel_id: u8,
        last_fragment: bool,
        sample_bytes: &[u8],
    ) -> Vec<u8> {
        let mut pkt = header(seq);
        let total_size = 8 + 4 + sample_bytes.len();
        let content_id = 0x8000
            | if last_fragment { 0x4000 } else { 0x0000 }
            | (((channel_id as u16) & 0x3F) << 8)
            | IDNVAL_CNKTYPE_LPGRF_FRAME_SEQUEL as u16;

        pkt.extend_from_slice(&(total_size as u16).to_be_bytes());
        pkt.extend_from_slice(&content_id.to_be_bytes());
        pkt.extend_from_slice(&0x87654321u32.to_be_bytes());
        pkt.extend_from_slice(&1000u32.to_be_bytes());
        pkt.extend_from_slice(sample_bytes);
        pkt
    }

    fn build_no_config_packet(
        seq: u16,
        channel_id: u8,
        chunk_type: u8,
        sample_bytes: &[u8],
    ) -> Vec<u8> {
        let mut pkt = header(seq);
        let total_size = 8 + 4 + sample_bytes.len();
        let content_id = 0x8000 | (((channel_id as u16) & 0x3F) << 8) | chunk_type as u16;

        pkt.extend_from_slice(&(total_size as u16).to_be_bytes());
        pkt.extend_from_slice(&content_id.to_be_bytes());
        pkt.extend_from_slice(&0x11111111u32.to_be_bytes());
        pkt.extend_from_slice(&1000u32.to_be_bytes());
        pkt.extend_from_slice(sample_bytes);
        pkt
    }

    fn source_addr() -> SocketAddr {
        SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 12345))
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

    fn descriptors_xyrgb_highres() -> Vec<u16> {
        vec![
            DESC_X,
            DESC_PRECISION_16BIT,
            DESC_Y,
            DESC_PRECISION_16BIT,
            DESC_RED_638,
            DESC_PRECISION_16BIT,
            DESC_GREEN_532,
            DESC_PRECISION_16BIT,
            DESC_BLUE_460,
            DESC_PRECISION_16BIT,
        ]
    }

    fn descriptors_extended() -> Vec<u16> {
        vec![
            DESC_X,
            DESC_PRECISION_16BIT,
            DESC_Y,
            DESC_PRECISION_16BIT,
            DESC_RED_638,
            DESC_PRECISION_16BIT,
            DESC_GREEN_532,
            DESC_PRECISION_16BIT,
            DESC_BLUE_460,
            DESC_PRECISION_16BIT,
            DESC_INTENSITY,
            DESC_PRECISION_16BIT,
            DESC_USER_1,
            DESC_PRECISION_16BIT,
            DESC_USER_2,
            DESC_PRECISION_16BIT,
            DESC_USER_3,
            DESC_PRECISION_16BIT,
            DESC_X_PRIME,
            DESC_PRECISION_16BIT,
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
    fn classify_xyrgb_highres() {
        assert_eq!(
            classify_format(&descriptors_xyrgb_highres()),
            Some(SampleFormat::XyrgbHighRes)
        );
    }

    #[test]
    fn classify_extended() {
        assert_eq!(
            classify_format(&descriptors_extended()),
            Some(SampleFormat::Extended)
        );
    }

    #[test]
    fn classify_unknown_descriptors_returns_none() {
        // Made-up channel id
        let bogus = vec![0x1234, 0x4010];
        assert!(classify_format(&bogus).is_none());
    }

    #[test]
    fn classify_ignores_non_sample_modifiers() {
        let mut descriptors = vec![0x1100]; // space modifier, no associated sample octet
        descriptors.extend(descriptors_xyrgb());

        assert_eq!(classify_format(&descriptors), Some(SampleFormat::Xyrgb));
    }

    #[test]
    fn classify_skips_void_tag_suffix_words() {
        let mut descriptors = vec![0x0001, 0xBEEF]; // void tag with one suffix word
        descriptors.extend(descriptors_xy());

        assert_eq!(classify_format(&descriptors), Some(SampleFormat::Xy));
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

        assert!((p.x - (16383.0 / 32767.0)).abs() < 1e-4);
        assert!((p.y - (-16384.0 / 32767.0)).abs() < 1e-4);

        // Intensity pre-multiplied: 0.5 * each color channel
        let i = 128.0 / 255.0;
        assert!((p.r - (255.0 / 255.0) * i).abs() < 1e-4);
        assert!((p.g - (128.0 / 255.0) * i).abs() < 1e-4);
        assert!((p.b - 0.0).abs() < 1e-4);

        assert_eq!(parsed.sequence, 1);
        assert_eq!(parsed.channel_id, 0);
        assert_eq!(parsed.chunk_type, ChunkType::Frame);
        assert!(parsed.config_or_last_fragment);
        assert!(parsed.has_config);
        assert!(parsed.is_last_fragment);
        assert_eq!(parsed.timestamp_us_u32, 0x12345678);
        assert_eq!(parsed.duration_us, 1000);
    }

    #[test]
    fn parse_frame_sequel_last_fragment_without_config() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&[255, 0, 0, 255]);

        let pkt = build_sequel_packet(7, 5, true, &samples);
        assert!(parse_frame_data(&pkt).is_none());

        let mut parser = FrameParser::new();
        let config_pkt = build_packet_on_channel(&descriptors_xyrgbi(), 5, 0x03, &samples);
        parser
            .parse_frame_data(source_addr(), &config_pkt)
            .expect("should cache config");
        let parsed = parser
            .parse_frame_data(source_addr(), &pkt)
            .expect("should parse sequel with cached config");

        assert_eq!(parsed.sequence, 7);
        assert_eq!(parsed.channel_id, 5);
        assert_eq!(parsed.chunk_type, ChunkType::FrameSequel);
        assert!(parsed.config_or_last_fragment);
        assert!(!parsed.has_config);
        assert!(parsed.is_last_fragment);
        assert_eq!(parsed.format, SampleFormat::Xyrgbi);
        assert_eq!(parsed.points.len(), 1);
        assert_eq!(parsed.timestamp_us_u32, 0x87654321);
    }

    #[test]
    fn parse_frame_sequel_not_last_fragment() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&[0, 255, 0, 255]);

        let pkt = build_sequel_packet(8, 2, false, &samples);
        let mut parser = FrameParser::new();
        let config_pkt = build_packet_on_channel(&descriptors_xyrgbi(), 2, 0x03, &samples);
        parser
            .parse_frame_data(source_addr(), &config_pkt)
            .expect("should cache config");
        let parsed = parser
            .parse_frame_data(source_addr(), &pkt)
            .expect("should parse sequel with cached config");

        assert_eq!(parsed.chunk_type, ChunkType::FrameSequel);
        assert!(!parsed.config_or_last_fragment);
        assert!(!parsed.has_config);
        assert!(!parsed.is_last_fragment);
    }

    #[test]
    fn frame_parser_reuses_cached_xyrgb_format() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&[255, 0, 0]);

        let mut parser = FrameParser::new();
        let config_pkt = build_packet(&descriptors_xyrgb(), 0x02, &samples);
        parser
            .parse_frame_data(source_addr(), &config_pkt)
            .expect("should cache XYRGB config");

        let no_config_pkt = build_no_config_packet(9, 0, 0x02, &samples);
        let parsed = parser
            .parse_frame_data(source_addr(), &no_config_pkt)
            .expect("should parse with cached XYRGB config");

        assert_eq!(parsed.format, SampleFormat::Xyrgb);
        assert_eq!(parsed.points.len(), 1);
        assert!(!parsed.has_config);
    }

    #[test]
    fn void_routing_config_opens_idle_channel_for_later_data() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&[255, 0, 0, 255]);

        let mut parser = FrameParser::new();
        let config_pkt = build_void_config_packet(&descriptors_xyrgbi(), 4, IDNFLG_CHNCFG_ROUTING);
        assert!(parser
            .parse_frame_data(source_addr(), &config_pkt)
            .is_none());

        let data_pkt = build_no_config_packet(11, 4, 0x02, &samples);
        let parsed = parser
            .parse_frame_data(source_addr(), &data_pkt)
            .expect("void config should establish channel state");

        assert_eq!(parsed.format, SampleFormat::Xyrgbi);
        assert_eq!(parsed.points.len(), 1);
        assert!(!parsed.has_config);
    }

    #[test]
    fn channel_close_config_clears_cached_state_after_data() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&[255, 0, 0, 255]);

        let mut parser = FrameParser::new();
        let close_pkt = build_packet_on_channel_with_flags(
            &descriptors_xyrgbi(),
            6,
            0x02,
            &samples,
            IDNFLG_CHNCFG_ROUTING | IDNFLG_CHNCFG_CLOSE,
            0,
        );
        assert!(parser.parse_frame_data(source_addr(), &close_pkt).is_some());

        let data_pkt = build_no_config_packet(12, 6, 0x02, &samples);
        assert!(parser.parse_frame_data(source_addr(), &data_pkt).is_none());
    }

    #[test]
    fn routingless_config_header_does_not_replace_cached_decoder() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&[255, 0, 0, 255]);

        let mut parser = FrameParser::new();
        let initial = build_packet_on_channel(&descriptors_xyrgbi(), 7, 0x02, &samples);
        parser
            .parse_frame_data(source_addr(), &initial)
            .expect("should cache initial config");

        let bogus_descriptors = vec![0x1234u16, 0x5678u16];
        let no_routing =
            build_packet_on_channel_with_flags(&bogus_descriptors, 7, 0x02, &samples, 0, 0);
        let parsed = parser
            .parse_frame_data(source_addr(), &no_routing)
            .expect("routingless config data should use cached decoder");

        assert_eq!(parsed.format, SampleFormat::Xyrgbi);
        assert!(parsed.has_config);
    }

    #[test]
    fn service_data_match_mismatch_discards_channel_state() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&[255, 0, 0, 255]);

        let mut parser = FrameParser::new();
        let config = build_packet_on_channel_with_flags(
            &descriptors_xyrgbi(),
            8,
            0x02,
            &samples,
            IDNFLG_CHNCFG_ROUTING | 0x10,
            0,
        );
        assert!(parser.parse_frame_data(source_addr(), &config).is_none());

        let matching_config = build_packet_on_channel_with_flags(
            &descriptors_xyrgbi(),
            8,
            0x02,
            &samples,
            IDNFLG_CHNCFG_ROUTING | 0x10,
            0x10,
        );
        assert!(parser
            .parse_frame_data(source_addr(), &matching_config)
            .is_some());
    }

    #[test]
    fn frame_parser_keeps_channel_configs_separate() {
        let mut xy_samples = Vec::new();
        xy_samples.extend_from_slice(&0i16.to_be_bytes());
        xy_samples.extend_from_slice(&0i16.to_be_bytes());

        let mut rgb_samples = xy_samples.clone();
        rgb_samples.extend_from_slice(&[0, 255, 0]);

        let mut parser = FrameParser::new();
        let xy_config = build_packet(&descriptors_xy(), 0x02, &xy_samples);
        parser
            .parse_frame_data(source_addr(), &xy_config)
            .expect("should cache channel 0 XY config");

        let channel_1_no_config = build_no_config_packet(10, 1, 0x02, &rgb_samples);
        assert!(parser
            .parse_frame_data(source_addr(), &channel_1_no_config)
            .is_none());
    }

    #[test]
    fn parse_xyrgb_highres_packet() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&65535u16.to_be_bytes());
        samples.extend_from_slice(&32768u16.to_be_bytes());
        samples.extend_from_slice(&0u16.to_be_bytes());

        let pkt = build_packet(&descriptors_xyrgb_highres(), 0x02, &samples);
        let parsed = parse_frame_data(&pkt).expect("should parse high-res packet");

        assert_eq!(parsed.format, SampleFormat::XyrgbHighRes);
        assert_eq!(parsed.points.len(), 1);
        assert!((parsed.points[0].r - 1.0).abs() < 1e-4);
        assert!((parsed.points[0].g - (32768.0 / 65535.0)).abs() < 1e-4);
        assert!(parsed.points[0].b.abs() < 1e-4);
    }

    #[test]
    fn parse_extended_packet_premultiplies_intensity_and_skips_user_channels() {
        let mut samples = Vec::new();
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&0i16.to_be_bytes());
        samples.extend_from_slice(&65535u16.to_be_bytes());
        samples.extend_from_slice(&0u16.to_be_bytes());
        samples.extend_from_slice(&0u16.to_be_bytes());
        samples.extend_from_slice(&32768u16.to_be_bytes());
        for value in [1u16, 2, 3, 4] {
            samples.extend_from_slice(&value.to_be_bytes());
        }

        let pkt = build_packet(&descriptors_extended(), 0x02, &samples);
        let parsed = parse_frame_data(&pkt).expect("should parse extended packet");

        assert_eq!(parsed.format, SampleFormat::Extended);
        assert_eq!(parsed.points.len(), 1);
        assert!((parsed.points[0].r - (32768.0 / 65535.0)).abs() < 1e-4);
        assert!(parsed.points[0].g.abs() < 1e-4);
        assert!(parsed.points[0].b.abs() < 1e-4);
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
        assert!((p1.x - 1.0).abs() < 1e-3);
        assert!((p1.y - -1.0).abs() < 1e-3);
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
