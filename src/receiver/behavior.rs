//! Behavior trait for customizing receiver server responses.

use std::net::SocketAddr;
use std::time::Duration;

use super::constants::IDNFLG_STATUS_REALTIME;
use super::parser::{ChunkType, ParsedChunk, ReceivedPoint, SampleFormat};

/// Borrowed parsed IDN channel-message data delivered to [`ServerBehavior`].
///
/// This preserves the protocol metadata needed to reconstruct logical frames
/// across multiple UDP packets while keeping the point slice borrowed from the
/// parser-owned chunk for the duration of the callback.
#[derive(Clone, Copy, Debug)]
pub struct ReceivedChunk<'a> {
    /// UDP source address for this channel message.
    pub source_addr: SocketAddr,
    /// Packet sequence number from the IDN packet header.
    pub sequence: u16,
    /// Raw channel-message content id.
    pub content_id: u16,
    /// Six-bit channel id parsed from the content id.
    pub channel_id: u8,
    /// Chunk type parsed from the content id.
    pub chunk_type: ChunkType,
    /// Raw state of the CONFIG/LSTFRG content-id bit (`0x4000`).
    pub config_or_last_fragment: bool,
    /// Whether this message contains a channel configuration header.
    pub has_config: bool,
    /// Whether this chunk completes a discrete frame.
    pub is_last_fragment: bool,
    /// Timestamp from the channel message header (u32, microseconds, wraps).
    pub timestamp_us_u32: u32,
    /// Duration of this chunk in microseconds.
    pub duration_us: u32,
    /// Sample format used for this chunk.
    pub format: SampleFormat,
    /// Parsed points in this chunk.
    pub points: &'a [ReceivedPoint],
}

impl<'a> ReceivedChunk<'a> {
    pub(crate) fn new(source_addr: SocketAddr, chunk: &'a ParsedChunk) -> Self {
        Self {
            source_addr,
            sequence: chunk.sequence,
            content_id: chunk.content_id,
            channel_id: chunk.channel_id,
            chunk_type: chunk.chunk_type,
            config_or_last_fragment: chunk.config_or_last_fragment,
            has_config: chunk.has_config,
            is_last_fragment: chunk.is_last_fragment,
            timestamp_us_u32: chunk.timestamp_us_u32,
            duration_us: chunk.duration_us,
            format: chunk.format,
            points: &chunk.points,
        }
    }
}

/// Behavior hooks for customizing receiver server responses.
///
/// Implement this trait to control how the server responds to various events
/// and commands, and to consume incoming point streams. The same server
/// core can therefore be used for:
/// - Test infrastructure (simple flags, packet capture)
/// - Visual simulators (UI-driven settings, full frame parsing)
/// - Production receivers (consume parsed points directly)
pub trait ServerBehavior: Send + 'static {
    /// Called for every packet received by the server.
    ///
    /// This fires before any command processing — useful when you want to
    /// capture all traffic regardless of command type.
    fn on_packet_received(&mut self, _raw_data: &[u8]) {}

    /// Called when raw frame data is received (RT_CNLMSG or RT_CNLMSG_ACKREQ).
    ///
    /// The raw packet data includes the full packet starting from the command
    /// byte. Production consumers should normally rely on
    /// [`Self::on_chunk_received`] instead; this raw hook exists for tools
    /// (like the simulator) that want byte-level inspection.
    fn on_frame_received(&mut self, _raw_data: &[u8]) {}

    /// Called once per parsed channel message with protocol metadata and points.
    ///
    /// The default implementation preserves the older point-only callback by
    /// forwarding to [`Self::on_points_received`]. Override this method when a
    /// receiver needs IDN frame-boundary metadata such as chunk type, channel id,
    /// sequence, timestamp, duration, or the CONFIG/LSTFRG bit.
    fn on_chunk_received(&mut self, chunk: ReceivedChunk<'_>) {
        self.on_points_received(chunk.points);
    }

    /// Called once per channel message with the parsed points.
    ///
    /// Points are normalized (`x`, `y` in `-1.0..=1.0`; `r`, `g`, `b` in
    /// `0.0..=1.0`) with intensity already pre-multiplied into RGB. Frames
    /// that use an unsupported sample format are skipped and don't fire this
    /// callback. New consumers that need frame boundaries should implement
    /// [`Self::on_chunk_received`] instead.
    fn on_points_received(&mut self, _points: &[ReceivedPoint]) {}

    /// Whether to respond to a given command.
    ///
    /// Return `false` to simulate offline/silent mode where the server
    /// receives packets but doesn't respond.
    fn should_respond(&self, command: u8) -> bool;

    /// Get the status byte for scan responses.
    ///
    /// Combine `IDNFLG_STATUS_*` flags.
    fn get_status_byte(&self) -> u8;

    /// Get the ACK result code for frame acknowledgments.
    ///
    /// Return `0x00` for success, or an error code like:
    /// - `0xEB` - Empty close
    /// - `0xEC` - Sessions occupied
    /// - `0xED` - Group excluded
    /// - `0xEE` - Invalid payload
    /// - `0xEF` - Processing error
    fn get_ack_result_code(&self) -> u8;

    /// Optional pre-processing latency to simulate network delay.
    fn get_simulated_latency(&self) -> Duration {
        Duration::ZERO
    }

    /// Called when a client connects (first frame received from new address).
    fn on_client_connected(&mut self, _addr: SocketAddr) {}

    /// Called when a client disconnects (timeout or force disconnect).
    fn on_client_disconnected(&mut self) {}

    /// Check if the current client should be force-disconnected.
    ///
    /// If this returns `true`, the server will:
    /// 1. Disconnect the current client
    /// 2. Ignore packets from them for 3 seconds
    /// 3. Call [`Self::on_client_disconnected`]
    ///
    /// Implementations should clear any "force disconnect" flag when returning `true`.
    fn should_force_disconnect(&mut self) -> bool {
        false
    }

    /// Whether to reject a new client when one is already connected.
    fn is_occupied(&self) -> bool {
        false
    }

    /// Whether to reject all real-time messages with an "excluded" error.
    fn is_excluded(&self) -> bool {
        false
    }
}

/// A simple behavior implementation for basic testing.
///
/// - Always responds to commands
/// - Reports REALTIME capability in status
/// - Returns success for ACK responses
/// - Ignores frame data and connection events
#[derive(Default)]
pub struct SimpleBehavior;

impl ServerBehavior for SimpleBehavior {
    fn should_respond(&self, _command: u8) -> bool {
        true
    }

    fn get_status_byte(&self) -> u8 {
        IDNFLG_STATUS_REALTIME
    }

    fn get_ack_result_code(&self) -> u8 {
        0x00
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A behavior that only implements the required methods, so the trait's
    /// default method bodies are exercised as-authored.
    #[derive(Default)]
    struct MinimalBehavior {
        forwarded_points: usize,
    }

    impl ServerBehavior for MinimalBehavior {
        fn should_respond(&self, _command: u8) -> bool {
            true
        }
        fn get_status_byte(&self) -> u8 {
            0
        }
        fn get_ack_result_code(&self) -> u8 {
            0
        }
        // Override only the leaf callback so we can observe that the default
        // `on_chunk_received` forwards to it.
        fn on_points_received(&mut self, points: &[ReceivedPoint]) {
            self.forwarded_points += points.len();
        }
    }

    fn sample_chunk() -> ParsedChunk {
        ParsedChunk {
            sequence: 9,
            content_id: 0xC003,
            channel_id: 2,
            chunk_type: ChunkType::Frame,
            config_or_last_fragment: true,
            has_config: true,
            is_last_fragment: true,
            timestamp_us_u32: 1234,
            duration_us: 1000,
            format: SampleFormat::Xyrgbi,
            points: vec![
                ReceivedPoint {
                    x: 0.0,
                    y: 0.0,
                    r: 1.0,
                    g: 0.0,
                    b: 0.0,
                },
                ReceivedPoint {
                    x: 0.5,
                    y: -0.5,
                    r: 0.0,
                    g: 1.0,
                    b: 0.0,
                },
            ],
        }
    }

    #[test]
    fn simple_behavior_defaults() {
        let mut b = SimpleBehavior;
        assert!(b.should_respond(0x10));
        assert_eq!(b.get_status_byte(), IDNFLG_STATUS_REALTIME);
        assert_eq!(b.get_ack_result_code(), 0x00);
        assert_eq!(b.get_simulated_latency(), Duration::ZERO);
        assert!(!b.should_force_disconnect());
        assert!(!b.is_occupied());
        assert!(!b.is_excluded());
    }

    #[test]
    fn default_on_chunk_received_forwards_to_on_points_received() {
        let chunk = sample_chunk();
        let addr: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        let received = ReceivedChunk::new(addr, &chunk);

        let mut b = MinimalBehavior::default();
        b.on_chunk_received(received);
        assert_eq!(
            b.forwarded_points, 2,
            "default on_chunk_received should forward the chunk's points to on_points_received"
        );
    }

    #[test]
    fn received_chunk_mirrors_parsed_chunk_metadata() {
        let chunk = sample_chunk();
        let addr: SocketAddr = "127.0.0.1:5001".parse().unwrap();
        let received = ReceivedChunk::new(addr, &chunk);

        assert_eq!(received.source_addr, addr);
        assert_eq!(received.sequence, chunk.sequence);
        assert_eq!(received.content_id, chunk.content_id);
        assert_eq!(received.channel_id, chunk.channel_id);
        assert_eq!(received.chunk_type, chunk.chunk_type);
        assert_eq!(
            received.config_or_last_fragment,
            chunk.config_or_last_fragment
        );
        assert_eq!(received.has_config, chunk.has_config);
        assert_eq!(received.is_last_fragment, chunk.is_last_fragment);
        assert_eq!(received.timestamp_us_u32, chunk.timestamp_us_u32);
        assert_eq!(received.duration_us, chunk.duration_us);
        assert_eq!(received.format, chunk.format);
        assert_eq!(received.points.len(), chunk.points.len());
    }
}
