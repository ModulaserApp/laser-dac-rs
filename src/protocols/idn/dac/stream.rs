//! UDP streaming for sending laser frames to an IDN server.

use crate::protocols::idn::dac::{Addressed, ServerInfo};
use crate::protocols::idn::error::{CommunicationError, ProtocolError, ResponseError, Result};
use crate::protocols::idn::protocol::{
    AcknowledgeResponse, ChannelConfigHeader, ChannelMessageHeader, GroupRequest, GroupResponse,
    PacketHeader, ParameterGetRequest, ParameterResponse, ParameterSetRequest, Point, ReadBytes,
    SampleChunkHeader, SizeBytes, WriteBytes, EXTENDED_SAMPLE_SIZE, IDNCMD_GROUP_REQUEST,
    IDNCMD_GROUP_RESPONSE, IDNCMD_PING_REQUEST, IDNCMD_PING_RESPONSE,
    IDNCMD_SERVICE_PARAMS_REQUEST, IDNCMD_SERVICE_PARAMS_RESPONSE,
    IDNCMD_UNIT_PARAMS_REQUEST, IDNCMD_UNIT_PARAMS_RESPONSE,
    IDNCMD_RT_ACKNOWLEDGE, IDNCMD_RT_CNLMSG, IDNCMD_RT_CNLMSG_ACKREQ, IDNCMD_RT_CNLMSG_CLOSE,
    IDNCMD_RT_CNLMSG_CLOSE_ACKREQ,
    IDNFLG_CHNCFG_CLOSE, IDNFLG_CHNCFG_ROUTING, IDNFLG_CONTENTID_CHANNELMSG,
    IDNFLG_CONTENTID_CONFIG_LSTFRG, IDNVAL_CNKTYPE_LPGRF_WAVE, IDNVAL_CNKTYPE_VOID,
    IDNVAL_SMOD_LPGRF_CONTINUOUS, MAX_UDP_PAYLOAD, XYRGBI_SAMPLE_SIZE, XYRGB_HIGHRES_SAMPLE_SIZE,
};
use std::io;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// Point format for streaming.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointFormat {
    /// Standard 8-byte format: X, Y, R, G, B, I (8-bit colors)
    Xyrgbi,
    /// High-resolution 10-byte format: X, Y, R, G, B (16-bit colors)
    XyrgbHighRes,
    /// Extended 20-byte format: X, Y, R, G, B, I, U1, U2, U3, U4 (16-bit all)
    Extended,
}

impl PointFormat {
    /// Get the size in bytes for this point format.
    pub fn size_bytes(&self) -> usize {
        match self {
            PointFormat::Xyrgbi => XYRGBI_SAMPLE_SIZE,
            PointFormat::XyrgbHighRes => XYRGB_HIGHRES_SAMPLE_SIZE,
            PointFormat::Extended => EXTENDED_SAMPLE_SIZE,
        }
    }

    /// Get the word count for the channel config header.
    /// This is the number of 32-bit words (descriptor pairs) in the descriptor array.
    /// The C++ reference uses wordCount = descriptors.len() / 2 (4, 5, 10 for the formats).
    fn word_count(&self) -> u8 {
        (self.descriptors().len() / 2) as u8
    }

    /// Get the channel descriptors for this format.
    fn descriptors(&self) -> &'static [u16] {
        match self {
            PointFormat::Xyrgbi => &[
                0x4200, 0x4010, // X, 16-bit
                0x4210, 0x4010, // Y, 16-bit
                0x527E, // Red, 638nm
                0x5214, // Green, 532nm
                0x51CC, // Blue, 460nm
                0x5C10, // Intensity
            ],
            PointFormat::XyrgbHighRes => &[
                0x4200, 0x4010, // X, 16-bit
                0x4210, 0x4010, // Y, 16-bit
                0x527E, 0x4010, // Red, 16-bit
                0x5214, 0x4010, // Green, 16-bit
                0x51CC, 0x4010, // Blue, 16-bit
            ],
            PointFormat::Extended => &[
                0x4200, 0x4010, // X, 16-bit
                0x4210, 0x4010, // Y, 16-bit
                0x527E, 0x4010, // Red, 16-bit
                0x5214, 0x4010, // Green, 16-bit
                0x51CC, 0x4010, // Blue, 16-bit
                0x5C10, 0x4010, // Intensity, 16-bit
                0x51BD, 0x4010, // User 1 (deep blue), 16-bit
                0x5241, 0x4010, // User 2 (yellow), 16-bit
                0x51E8, 0x4010, // User 3 (cyan), 16-bit
                0x4201, 0x4010, // User 4 (X-prime), 16-bit
            ],
        }
    }
}

/// Link timeout duration (spec section 7.2)
pub const LINK_TIMEOUT: Duration = Duration::from_secs(1);

/// Recommended keepalive interval (half the timeout)
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(500);

/// Minimum number of samples per frame required by the spec.
const MIN_SAMPLES_PER_FRAME: usize = 20;

/// A streaming connection to an IDN server.
pub struct Stream {
    /// The addressed DAC (server + selected service)
    dac: Addressed,
    /// UDP socket for communication
    socket: UdpSocket,
    /// Client group (0-15)
    client_group: u8,
    /// Sequence number counter
    sequence: u16,
    /// Current timestamp in microseconds
    timestamp: u64,
    /// Scan speed in points per second
    scan_speed: u32,
    /// Current point format
    point_format: PointFormat,
    /// Service data match counter (increments on config change)
    service_data_match: u8,
    /// Last time config was sent
    last_config_time: Option<Instant>,
    /// Previous point format (for detecting changes)
    previous_format: Option<PointFormat>,
    /// Frame counter
    frame_count: u64,
    /// Packet buffer for building frames
    packet_buffer: Vec<u8>,
    /// Receive buffer for acknowledgments
    recv_buffer: [u8; 64],
    /// Last time data was sent (for keepalive tracking)
    last_send_time: Option<Instant>,
    /// Time when the stream was created (for non-zero initial timestamp)
    connect_time: Instant,
}

/// Connect to an IDN server for streaming.
///
/// # Arguments
///
/// * `server` - The server to connect to
/// * `service_id` - The service ID to stream to (typically a laser projector)
///
/// # Returns
///
/// A `Stream` ready for sending frames.
pub fn connect(server: &ServerInfo, service_id: u8) -> io::Result<Stream> {
    connect_with_group(server, service_id, 0)
}

/// Connect to an IDN server for streaming with a specific client group.
pub fn connect_with_group(
    server: &ServerInfo,
    service_id: u8,
    client_group: u8,
) -> io::Result<Stream> {
    let address = server
        .primary_address()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "server has no addresses"))?;

    let service = server
        .find_service(service_id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "service not found"))?;

    // Create UDP socket
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.connect(address)?;

    let dac = Addressed::new(server.clone(), service.clone(), *address);

    Ok(Stream {
        dac,
        socket,
        client_group: client_group & 0x0F,
        sequence: 0,
        timestamp: 0,
        scan_speed: 30000, // Default 30k pps
        point_format: PointFormat::Xyrgbi,
        service_data_match: 0,
        last_config_time: None,
        previous_format: None,
        frame_count: 0,
        packet_buffer: Vec::with_capacity(MAX_UDP_PAYLOAD * 2),
        recv_buffer: [0u8; 64],
        last_send_time: None,
        connect_time: Instant::now(),
    })
}

impl Stream {
    /// Get a reference to the addressed DAC.
    pub fn dac(&self) -> &Addressed {
        &self.dac
    }

    /// Get the current scan speed in points per second.
    pub fn scan_speed(&self) -> u32 {
        self.scan_speed
    }

    /// Set the scan speed in points per second.
    pub fn set_scan_speed(&mut self, pps: u32) {
        self.scan_speed = pps;
    }

    /// Get the current point format.
    pub fn point_format(&self) -> PointFormat {
        self.point_format
    }

    /// Set the point format for subsequent frames.
    pub fn set_point_format(&mut self, format: PointFormat) {
        self.point_format = format;
    }

    /// Check if a keepalive is needed to maintain the link.
    ///
    /// Per spec section 7.2, the link times out after 1 second of inactivity.
    /// This method returns true if more than `KEEPALIVE_INTERVAL` has passed
    /// since the last data was sent.
    pub fn needs_keepalive(&self) -> bool {
        match self.last_send_time {
            Some(last) => last.elapsed() >= KEEPALIVE_INTERVAL,
            None => self.frame_count > 0, // Need keepalive if we've sent anything before
        }
    }

    /// Get the time since the last data was sent.
    ///
    /// Returns `None` if no data has been sent yet.
    pub fn time_since_last_send(&self) -> Option<Duration> {
        self.last_send_time.map(|t| t.elapsed())
    }

    /// Send a keepalive void channel message to maintain both link and session.
    ///
    /// Unlike `ping()` which only keeps the link alive, this sends a void channel
    /// message that keeps both the link and the streaming session alive.
    /// The link will timeout after `LINK_TIMEOUT` (1 second) of inactivity.
    pub fn send_keepalive(&mut self) -> Result<()> {
        let service_id = self.dac.service_id();
        let channel_id = ((service_id.saturating_sub(1)) as u16 & 0x3F) << 8;
        let content_id =
            IDNFLG_CONTENTID_CHANNELMSG | channel_id | IDNVAL_CNKTYPE_VOID as u16;

        self.packet_buffer.clear();

        let packet_header = PacketHeader {
            command: IDNCMD_RT_CNLMSG,
            flags: self.client_group,
            sequence: self.next_sequence(),
        };
        self.packet_buffer.write_bytes(packet_header)?;

        let channel_msg = ChannelMessageHeader {
            total_size: ChannelMessageHeader::SIZE_BYTES as u16,
            content_id,
            timestamp: (self.timestamp & 0xFFFF_FFFF) as u32,
        };
        self.packet_buffer.write_bytes(channel_msg)?;

        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        Ok(())
    }

    /// Write a frame of points to the DAC.
    ///
    /// This is the main method for streaming laser data. Points are sent
    /// as UDP packets with appropriate timing information.
    pub fn write_frame<P: Point>(&mut self, points: &[P]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }

        // Pad to minimum sample count by repeating the last point
        let padded;
        let points = if points.len() < MIN_SAMPLES_PER_FRAME {
            padded = Self::pad_points(points);
            &padded[..]
        } else {
            points
        };

        if self.scan_speed == 0 {
            return Err(CommunicationError::Protocol(
                ProtocolError::InvalidPointFormat,
            ));
        }

        let now = Instant::now();

        // Determine if we need to send config
        let needs_config = self.frame_count == 0
            || self.previous_format != Some(self.point_format)
            || self
                .last_config_time
                .is_none_or(|t| now.duration_since(t).as_millis() > 250);

        if self.previous_format != Some(self.point_format) {
            self.service_data_match = self.service_data_match.wrapping_add(1);
        }

        // Set non-zero initial timestamp on the first frame
        if self.frame_count == 0 {
            self.timestamp = self.connect_time.elapsed().as_micros() as u64;
        }

        let bytes_per_sample = P::SIZE_BYTES;
        let service_id = self.dac.service_id();

        // Calculate content ID
        let channel_id = ((service_id.saturating_sub(1)) as u16 & 0x3F) << 8;
        let mut content_id = IDNFLG_CONTENTID_CHANNELMSG | channel_id;

        // Calculate header sizes
        let config_size = if needs_config {
            content_id |= IDNFLG_CONTENTID_CONFIG_LSTFRG;
            ChannelConfigHeader::SIZE_BYTES + self.point_format.descriptors().len() * 2
        } else {
            0
        };

        let header_size = PacketHeader::SIZE_BYTES
            + ChannelMessageHeader::SIZE_BYTES
            + config_size
            + SampleChunkHeader::SIZE_BYTES;

        // Calculate how many points fit in one packet, distributing samples
        // evenly across packets to produce uniform timing (matching C++ reference).
        let max_points_per_packet = (MAX_UDP_PAYLOAD - header_size) / bytes_per_sample;
        let num_packets = (points.len() / max_points_per_packet) + 1;
        let points_to_send = points.len() / num_packets;

        // Calculate duration for this chunk
        let duration_us = ((points_to_send as u64) * 1_000_000) / (self.scan_speed as u64);

        // Build the packet
        self.packet_buffer.clear();

        // Packet header
        let seq = self.next_sequence();
        let packet_header = PacketHeader {
            command: IDNCMD_RT_CNLMSG,
            flags: self.client_group,
            sequence: seq,
        };
        self.packet_buffer.write_bytes(packet_header)?;

        // Channel message header
        // totalSize = size from ChannelMessage start to end of packet payload
        // Per C++ reference: totalSize = bufferEnd - channelMsgHdr (includes full 8-byte header)
        let msg_size = ChannelMessageHeader::SIZE_BYTES
            + config_size
            + SampleChunkHeader::SIZE_BYTES
            + points_to_send * bytes_per_sample;

        let final_content_id = content_id | IDNVAL_CNKTYPE_LPGRF_WAVE as u16;
        let channel_msg = ChannelMessageHeader {
            total_size: msg_size as u16,
            content_id: final_content_id,
            timestamp: (self.timestamp & 0xFFFF_FFFF) as u32,
        };
        self.packet_buffer.write_bytes(channel_msg)?;

        // Channel config (if needed)
        if needs_config {
            let flags = IDNFLG_CHNCFG_ROUTING | (((self.service_data_match & 1) | 2) << 4);
            let config = ChannelConfigHeader {
                word_count: self.point_format.word_count(),
                flags,
                service_id,
                service_mode: IDNVAL_SMOD_LPGRF_CONTINUOUS,
            };
            self.packet_buffer.write_bytes(config)?;

            // Write descriptors (big-endian)
            for &desc in self.point_format.descriptors() {
                self.packet_buffer.push((desc >> 8) as u8);
                self.packet_buffer.push(desc as u8);
            }

            self.last_config_time = Some(now);
            self.previous_format = Some(self.point_format);
        }

        // Sample chunk header
        let chunk_flags = ((self.service_data_match & 1) | 2) << 4;
        let chunk_header = SampleChunkHeader::new(chunk_flags, duration_us as u32);
        self.packet_buffer.write_bytes(chunk_header)?;

        // Write points
        for point in points.iter().take(points_to_send) {
            self.packet_buffer.write_bytes(point)?;
        }

        // Send the packet
        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        // Update state
        self.timestamp += duration_us;
        self.frame_count += 1;

        // If there are remaining points, send them in subsequent packets
        if points_to_send < points.len() {
            self.write_frame_continuation(&points[points_to_send..])?;
        }

        Ok(())
    }

    /// Send remaining points without config header.
    fn write_frame_continuation<P: Point>(&mut self, points: &[P]) -> Result<()> {
        if points.is_empty() {
            return Ok(());
        }

        let bytes_per_sample = P::SIZE_BYTES;
        let service_id = self.dac.service_id();

        let channel_id = ((service_id.saturating_sub(1)) as u16 & 0x3F) << 8;
        let content_id = IDNFLG_CONTENTID_CHANNELMSG | channel_id;

        let header_size = PacketHeader::SIZE_BYTES
            + ChannelMessageHeader::SIZE_BYTES
            + SampleChunkHeader::SIZE_BYTES;

        let max_points_per_packet = (MAX_UDP_PAYLOAD - header_size) / bytes_per_sample;
        // Distribute remaining samples evenly across packets (matching C++ reference)
        let num_packets = (points.len() / max_points_per_packet) + 1;
        let points_to_send = points.len() / num_packets;

        let duration_us = ((points_to_send as u64) * 1_000_000) / (self.scan_speed as u64);

        self.packet_buffer.clear();

        // Packet header
        let packet_header = PacketHeader {
            command: IDNCMD_RT_CNLMSG,
            flags: self.client_group,
            sequence: self.next_sequence(),
        };
        self.packet_buffer.write_bytes(packet_header)?;

        // Channel message header
        // totalSize = size from ChannelMessage start to end of packet payload
        let msg_size = ChannelMessageHeader::SIZE_BYTES
            + SampleChunkHeader::SIZE_BYTES
            + points_to_send * bytes_per_sample;

        let channel_msg = ChannelMessageHeader {
            total_size: msg_size as u16,
            content_id: content_id | IDNVAL_CNKTYPE_LPGRF_WAVE as u16,
            timestamp: (self.timestamp & 0xFFFF_FFFF) as u32,
        };
        self.packet_buffer.write_bytes(channel_msg)?;

        // Sample chunk header
        let chunk_flags = ((self.service_data_match & 1) | 2) << 4;
        let chunk_header = SampleChunkHeader::new(chunk_flags, duration_us as u32);
        self.packet_buffer.write_bytes(chunk_header)?;

        // Write points
        for point in points.iter().take(points_to_send) {
            self.packet_buffer.write_bytes(point)?;
        }

        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        self.timestamp += duration_us;

        // Continue with remaining points
        if points_to_send < points.len() {
            self.write_frame_continuation(&points[points_to_send..])?;
        }

        Ok(())
    }

    /// Write a frame of points to the DAC and request acknowledgment.
    ///
    /// This is similar to `write_frame` but uses IDNCMD_RT_CNLMSG_ACKREQ
    /// and waits for an acknowledgment response from the server.
    ///
    /// # Arguments
    ///
    /// * `points` - The points to send
    /// * `timeout` - How long to wait for the acknowledgment
    pub fn write_frame_with_ack<P: Point>(
        &mut self,
        points: &[P],
        timeout: Duration,
    ) -> Result<AcknowledgeResponse> {
        if points.is_empty() {
            return Err(CommunicationError::Protocol(ProtocolError::BufferTooSmall));
        }

        // Pad to minimum sample count by repeating the last point
        let padded;
        let points = if points.len() < MIN_SAMPLES_PER_FRAME {
            padded = Self::pad_points(points);
            &padded[..]
        } else {
            points
        };

        if self.scan_speed == 0 {
            return Err(CommunicationError::Protocol(
                ProtocolError::InvalidPointFormat,
            ));
        }

        let now = Instant::now();

        // Determine if we need to send config
        let needs_config = self.frame_count == 0
            || self.previous_format != Some(self.point_format)
            || self
                .last_config_time
                .is_none_or(|t| now.duration_since(t).as_millis() > 250);

        if self.previous_format != Some(self.point_format) {
            self.service_data_match = self.service_data_match.wrapping_add(1);
        }

        // Set non-zero initial timestamp on the first frame
        if self.frame_count == 0 {
            self.timestamp = self.connect_time.elapsed().as_micros() as u64;
        }

        let bytes_per_sample = P::SIZE_BYTES;
        let service_id = self.dac.service_id();

        // Calculate content ID
        let channel_id = ((service_id.saturating_sub(1)) as u16 & 0x3F) << 8;
        let mut content_id = IDNFLG_CONTENTID_CHANNELMSG | channel_id;

        // Calculate header sizes
        let config_size = if needs_config {
            content_id |= IDNFLG_CONTENTID_CONFIG_LSTFRG;
            ChannelConfigHeader::SIZE_BYTES + self.point_format.descriptors().len() * 2
        } else {
            0
        };

        let header_size = PacketHeader::SIZE_BYTES
            + ChannelMessageHeader::SIZE_BYTES
            + config_size
            + SampleChunkHeader::SIZE_BYTES;

        // Calculate how many points fit in one packet, distributing samples
        // evenly across packets to produce uniform timing (matching C++ reference).
        let max_points_per_packet = (MAX_UDP_PAYLOAD - header_size) / bytes_per_sample;
        let num_packets = (points.len() / max_points_per_packet) + 1;
        let points_to_send = points.len() / num_packets;

        // Calculate duration for this chunk
        let duration_us = ((points_to_send as u64) * 1_000_000) / (self.scan_speed as u64);

        // Build the packet
        self.packet_buffer.clear();

        // Packet header - use ACKREQ command
        let ack_seq = self.next_sequence();
        let packet_header = PacketHeader {
            command: IDNCMD_RT_CNLMSG_ACKREQ,
            flags: self.client_group,
            sequence: ack_seq,
        };
        self.packet_buffer.write_bytes(packet_header)?;

        // Channel message header
        // totalSize = size from ChannelMessage start to end of packet payload
        let msg_size = ChannelMessageHeader::SIZE_BYTES
            + config_size
            + SampleChunkHeader::SIZE_BYTES
            + points_to_send * bytes_per_sample;

        let channel_msg = ChannelMessageHeader {
            total_size: msg_size as u16,
            content_id: content_id | IDNVAL_CNKTYPE_LPGRF_WAVE as u16,
            timestamp: (self.timestamp & 0xFFFF_FFFF) as u32,
        };
        self.packet_buffer.write_bytes(channel_msg)?;

        // Channel config (if needed)
        if needs_config {
            let flags = IDNFLG_CHNCFG_ROUTING | (((self.service_data_match & 1) | 2) << 4);
            let config = ChannelConfigHeader {
                word_count: self.point_format.word_count(),
                flags,
                service_id,
                service_mode: IDNVAL_SMOD_LPGRF_CONTINUOUS,
            };
            self.packet_buffer.write_bytes(config)?;

            // Write descriptors (big-endian)
            for &desc in self.point_format.descriptors() {
                self.packet_buffer.push((desc >> 8) as u8);
                self.packet_buffer.push(desc as u8);
            }

            self.last_config_time = Some(now);
            self.previous_format = Some(self.point_format);
        }

        // Sample chunk header
        let chunk_flags = ((self.service_data_match & 1) | 2) << 4;
        let chunk_header = SampleChunkHeader::new(chunk_flags, duration_us as u32);
        self.packet_buffer.write_bytes(chunk_header)?;

        // Write points
        for point in points.iter().take(points_to_send) {
            self.packet_buffer.write_bytes(point)?;
        }

        // Send the packet
        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        // Update state
        self.timestamp += duration_us;
        self.frame_count += 1;

        // Wait for acknowledgment
        let ack = self.recv_acknowledge(timeout, ack_seq)?;

        // Send remaining points in subsequent packets (fire-and-forget)
        if points_to_send < points.len() {
            self.write_frame_continuation(&points[points_to_send..])?;
        }

        Ok(ack)
    }

    /// Receive an acknowledgment response from the server.
    ///
    /// # Arguments
    ///
    /// * `timeout` - How long to wait for the acknowledgment
    /// * `expected_seq` - Expected sequence number to validate against the response
    pub fn recv_acknowledge(
        &mut self,
        timeout: Duration,
        expected_seq: u16,
    ) -> Result<AcknowledgeResponse> {
        self.socket.set_read_timeout(Some(timeout))?;

        let len = match self.socket.recv(&mut self.recv_buffer) {
            Ok(len) => len,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(CommunicationError::Response(ResponseError::Timeout));
            }
            Err(e) => return Err(CommunicationError::Io(e)),
        };

        if len < PacketHeader::SIZE_BYTES + AcknowledgeResponse::SIZE_BYTES {
            return Err(CommunicationError::Protocol(ProtocolError::BufferTooSmall));
        }

        let mut cursor = &self.recv_buffer[..len];

        // Parse packet header
        let header: PacketHeader = cursor.read_bytes()?;

        if header.command != IDNCMD_RT_ACKNOWLEDGE {
            return Err(CommunicationError::Response(
                ResponseError::UnexpectedResponse,
            ));
        }

        self.check_sequence(expected_seq, header.sequence)?;

        // Parse acknowledgment
        let ack: AcknowledgeResponse = cursor.read_bytes()?;

        // Check for errors
        if let Some(error) = ResponseError::from_ack_code(ack.result_code) {
            return Err(CommunicationError::Response(error));
        }

        Ok(ack)
    }

    /// Send a ping request and measure round-trip time (spec section 3.1).
    ///
    /// This can be used to measure network latency and verify the connection is alive.
    ///
    /// # Arguments
    ///
    /// * `timeout` - How long to wait for the ping response
    ///
    /// # Returns
    ///
    /// The round-trip time if successful.
    pub fn ping(&mut self, timeout: Duration) -> Result<Duration> {
        let start = Instant::now();

        // Build ping request packet
        self.packet_buffer.clear();

        let seq = self.next_sequence();
        let header = PacketHeader {
            command: IDNCMD_PING_REQUEST,
            flags: self.client_group,
            sequence: seq,
        };
        self.packet_buffer.write_bytes(header)?;

        // Send the ping
        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        // Wait for response
        self.socket.set_read_timeout(Some(timeout))?;

        let len = match self.socket.recv(&mut self.recv_buffer) {
            Ok(len) => len,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(CommunicationError::Response(ResponseError::Timeout));
            }
            Err(e) => return Err(CommunicationError::Io(e)),
        };

        if len < PacketHeader::SIZE_BYTES {
            return Err(CommunicationError::Protocol(ProtocolError::BufferTooSmall));
        }

        let mut cursor = &self.recv_buffer[..len];
        let response_header: PacketHeader = cursor.read_bytes()?;

        if response_header.command != IDNCMD_PING_RESPONSE {
            return Err(CommunicationError::Response(
                ResponseError::UnexpectedResponse,
            ));
        }

        self.check_sequence(seq, response_header.sequence)?;

        Ok(start.elapsed())
    }

    /// Get the server's client group mask.
    ///
    /// The mask indicates which client groups (0-15) the server accepts connections from.
    ///
    /// # Arguments
    ///
    /// * `timeout` - How long to wait for the response
    pub fn get_client_group_mask(&mut self, timeout: Duration) -> Result<GroupResponse> {
        self.packet_buffer.clear();

        let seq = self.next_sequence();
        let header = PacketHeader {
            command: IDNCMD_GROUP_REQUEST,
            flags: self.client_group,
            sequence: seq,
        };
        self.packet_buffer.write_bytes(header)?;

        let request = GroupRequest::get();
        self.packet_buffer.write_bytes(request)?;

        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        // Wait for response
        self.socket.set_read_timeout(Some(timeout))?;

        let len = match self.socket.recv(&mut self.recv_buffer) {
            Ok(len) => len,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(CommunicationError::Response(ResponseError::Timeout));
            }
            Err(e) => return Err(CommunicationError::Io(e)),
        };

        if len < PacketHeader::SIZE_BYTES + GroupResponse::SIZE_BYTES {
            return Err(CommunicationError::Protocol(ProtocolError::BufferTooSmall));
        }

        let mut cursor = &self.recv_buffer[..len];
        let response_header: PacketHeader = cursor.read_bytes()?;

        if response_header.command != IDNCMD_GROUP_RESPONSE {
            return Err(CommunicationError::Response(
                ResponseError::UnexpectedResponse,
            ));
        }

        self.check_sequence(seq, response_header.sequence)?;

        let response: GroupResponse = cursor.read_bytes()?;
        Ok(response)
    }

    /// Set the server's client group mask.
    ///
    /// The mask indicates which client groups (0-15) the server should accept connections from.
    /// Each bit in the mask corresponds to a group (bit 0 = group 0, bit 15 = group 15).
    ///
    /// # Arguments
    ///
    /// * `mask` - The new client group mask
    /// * `timeout` - How long to wait for the response
    ///
    /// # Returns
    ///
    /// The new group mask as confirmed by the server.
    pub fn set_client_group_mask(&mut self, mask: u16, timeout: Duration) -> Result<GroupResponse> {
        self.packet_buffer.clear();

        let seq = self.next_sequence();
        let header = PacketHeader {
            command: IDNCMD_GROUP_REQUEST,
            flags: self.client_group,
            sequence: seq,
        };
        self.packet_buffer.write_bytes(header)?;

        let request = GroupRequest::set(mask);
        self.packet_buffer.write_bytes(request)?;

        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        // Wait for response
        self.socket.set_read_timeout(Some(timeout))?;

        let len = match self.socket.recv(&mut self.recv_buffer) {
            Ok(len) => len,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(CommunicationError::Response(ResponseError::Timeout));
            }
            Err(e) => return Err(CommunicationError::Io(e)),
        };

        if len < PacketHeader::SIZE_BYTES + GroupResponse::SIZE_BYTES {
            return Err(CommunicationError::Protocol(ProtocolError::BufferTooSmall));
        }

        let mut cursor = &self.recv_buffer[..len];
        let response_header: PacketHeader = cursor.read_bytes()?;

        if response_header.command != IDNCMD_GROUP_RESPONSE {
            return Err(CommunicationError::Response(
                ResponseError::UnexpectedResponse,
            ));
        }

        self.check_sequence(seq, response_header.sequence)?;

        let response: GroupResponse = cursor.read_bytes()?;
        Ok(response)
    }

    /// Get a parameter value from the server.
    ///
    /// # Arguments
    ///
    /// * `service_id` - Service ID (0 for unit-level parameters)
    /// * `param_id` - Parameter ID to get
    /// * `timeout` - How long to wait for the response
    pub fn get_parameter(
        &mut self,
        service_id: u8,
        param_id: u16,
        timeout: Duration,
    ) -> Result<ParameterResponse> {
        let (request_cmd, response_cmd) = if service_id == 0 {
            (IDNCMD_UNIT_PARAMS_REQUEST, IDNCMD_UNIT_PARAMS_RESPONSE)
        } else {
            (IDNCMD_SERVICE_PARAMS_REQUEST, IDNCMD_SERVICE_PARAMS_RESPONSE)
        };

        self.packet_buffer.clear();

        let seq = self.next_sequence();
        let header = PacketHeader {
            command: request_cmd,
            flags: self.client_group,
            sequence: seq,
        };
        self.packet_buffer.write_bytes(header)?;

        let request = ParameterGetRequest {
            service_id,
            reserved: 0,
            param_id,
        };
        self.packet_buffer.write_bytes(request)?;

        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        // Wait for response
        self.socket.set_read_timeout(Some(timeout))?;

        let len = match self.socket.recv(&mut self.recv_buffer) {
            Ok(len) => len,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(CommunicationError::Response(ResponseError::Timeout));
            }
            Err(e) => return Err(CommunicationError::Io(e)),
        };

        if len < PacketHeader::SIZE_BYTES + ParameterResponse::SIZE_BYTES {
            return Err(CommunicationError::Protocol(ProtocolError::BufferTooSmall));
        }

        let mut cursor = &self.recv_buffer[..len];
        let response_header: PacketHeader = cursor.read_bytes()?;

        if response_header.command != response_cmd {
            return Err(CommunicationError::Response(
                ResponseError::UnexpectedResponse,
            ));
        }

        self.check_sequence(seq, response_header.sequence)?;

        let response: ParameterResponse = cursor.read_bytes()?;

        if !response.is_success() {
            if let Some(error) = ResponseError::from_ack_code(response.result_code) {
                return Err(CommunicationError::Response(error));
            }
        }

        Ok(response)
    }

    /// Set a parameter value on the server.
    ///
    /// # Arguments
    ///
    /// * `service_id` - Service ID (0 for unit-level parameters)
    /// * `param_id` - Parameter ID to set
    /// * `value` - New parameter value
    /// * `timeout` - How long to wait for the response
    pub fn set_parameter(
        &mut self,
        service_id: u8,
        param_id: u16,
        value: u32,
        timeout: Duration,
    ) -> Result<ParameterResponse> {
        let (request_cmd, response_cmd) = if service_id == 0 {
            (IDNCMD_UNIT_PARAMS_REQUEST, IDNCMD_UNIT_PARAMS_RESPONSE)
        } else {
            (IDNCMD_SERVICE_PARAMS_REQUEST, IDNCMD_SERVICE_PARAMS_RESPONSE)
        };

        self.packet_buffer.clear();

        let seq = self.next_sequence();
        let header = PacketHeader {
            command: request_cmd,
            flags: self.client_group,
            sequence: seq,
        };
        self.packet_buffer.write_bytes(header)?;

        let request = ParameterSetRequest {
            service_id,
            reserved: 0,
            param_id,
            value,
        };
        self.packet_buffer.write_bytes(request)?;

        self.socket.send(&self.packet_buffer)?;
        self.last_send_time = Some(Instant::now());

        // Wait for response
        self.socket.set_read_timeout(Some(timeout))?;

        let len = match self.socket.recv(&mut self.recv_buffer) {
            Ok(len) => len,
            Err(e)
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::TimedOut =>
            {
                return Err(CommunicationError::Response(ResponseError::Timeout));
            }
            Err(e) => return Err(CommunicationError::Io(e)),
        };

        if len < PacketHeader::SIZE_BYTES + ParameterResponse::SIZE_BYTES {
            return Err(CommunicationError::Protocol(ProtocolError::BufferTooSmall));
        }

        let mut cursor = &self.recv_buffer[..len];
        let response_header: PacketHeader = cursor.read_bytes()?;

        if response_header.command != response_cmd {
            return Err(CommunicationError::Response(
                ResponseError::UnexpectedResponse,
            ));
        }

        self.check_sequence(seq, response_header.sequence)?;

        let response: ParameterResponse = cursor.read_bytes()?;

        if !response.is_success() {
            if let Some(error) = ResponseError::from_ack_code(response.result_code) {
                return Err(CommunicationError::Response(error));
            }
        }

        Ok(response)
    }

    /// Close the streaming connection gracefully.
    pub fn close(&mut self) -> Result<()> {
        let service_id = self.dac.service_id();
        let channel_id = ((service_id.saturating_sub(1)) as u16 & 0x3F) << 8;

        // First, send channel close
        self.packet_buffer.clear();

        // Packet header
        let seq1 = self.next_sequence();
        let packet_header = PacketHeader {
            command: IDNCMD_RT_CNLMSG,
            flags: self.client_group,
            sequence: seq1,
        };
        self.packet_buffer.write_bytes(packet_header)?;

        // Channel message header with void chunk type
        let content_id = IDNFLG_CONTENTID_CHANNELMSG
            | IDNFLG_CONTENTID_CONFIG_LSTFRG
            | channel_id
            | IDNVAL_CNKTYPE_VOID as u16;

        // totalSize = size from ChannelMessage start to end of packet payload
        // Per C++ reference: totalSize = bufferEnd - channelMsgHdr = ChannelMsg(8) + Config(4) = 12
        let msg_size = ChannelMessageHeader::SIZE_BYTES + ChannelConfigHeader::SIZE_BYTES;
        let channel_msg = ChannelMessageHeader {
            total_size: msg_size as u16,
            content_id,
            timestamp: (self.timestamp & 0xFFFF_FFFF) as u32,
        };
        self.packet_buffer.write_bytes(channel_msg)?;

        // Channel config with close flag
        let config = ChannelConfigHeader {
            word_count: 0,
            flags: IDNFLG_CHNCFG_CLOSE,
            service_id,
            service_mode: 0,
        };
        self.packet_buffer.write_bytes(config)?;
        self.socket.send(&self.packet_buffer)?;

        // Then send session close
        self.packet_buffer.clear();

        let close_header = PacketHeader {
            command: IDNCMD_RT_CNLMSG_CLOSE,
            flags: self.client_group,
            sequence: self.next_sequence(),
        };
        self.packet_buffer.write_bytes(close_header)?;
        self.socket.send(&self.packet_buffer)?;

        Ok(())
    }

    /// Close the streaming connection gracefully and wait for acknowledgment.
    ///
    /// Same as `close()`, but uses IDNCMD_RT_CNLMSG_CLOSE_ACKREQ for the session
    /// close packet and waits for the server to confirm.
    ///
    /// # Arguments
    ///
    /// * `timeout` - How long to wait for the acknowledgment
    pub fn close_with_ack(&mut self, timeout: Duration) -> Result<AcknowledgeResponse> {
        let service_id = self.dac.service_id();
        let channel_id = ((service_id.saturating_sub(1)) as u16 & 0x3F) << 8;

        // First, send channel close (same as close())
        self.packet_buffer.clear();

        let packet_header = PacketHeader {
            command: IDNCMD_RT_CNLMSG,
            flags: self.client_group,
            sequence: self.next_sequence(),
        };
        self.packet_buffer.write_bytes(packet_header)?;

        let content_id = IDNFLG_CONTENTID_CHANNELMSG
            | IDNFLG_CONTENTID_CONFIG_LSTFRG
            | channel_id
            | IDNVAL_CNKTYPE_VOID as u16;

        let msg_size = ChannelMessageHeader::SIZE_BYTES + ChannelConfigHeader::SIZE_BYTES;
        let channel_msg = ChannelMessageHeader {
            total_size: msg_size as u16,
            content_id,
            timestamp: (self.timestamp & 0xFFFF_FFFF) as u32,
        };
        self.packet_buffer.write_bytes(channel_msg)?;

        let config = ChannelConfigHeader {
            word_count: 0,
            flags: IDNFLG_CHNCFG_CLOSE,
            service_id,
            service_mode: 0,
        };
        self.packet_buffer.write_bytes(config)?;
        self.socket.send(&self.packet_buffer)?;

        // Then send session close with ack request
        self.packet_buffer.clear();

        let close_seq = self.next_sequence();
        let close_header = PacketHeader {
            command: IDNCMD_RT_CNLMSG_CLOSE_ACKREQ,
            flags: self.client_group,
            sequence: close_seq,
        };
        self.packet_buffer.write_bytes(close_header)?;
        self.socket.send(&self.packet_buffer)?;

        // Wait for acknowledgment
        self.recv_acknowledge(timeout, close_seq)
    }

    /// Get the next sequence number.
    fn next_sequence(&mut self) -> u16 {
        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        seq
    }

    /// Verify that a response sequence number matches the expected value.
    fn check_sequence(&self, expected: u16, actual: u16) -> Result<()> {
        if actual != expected {
            return Err(CommunicationError::Protocol(
                ProtocolError::SequenceMismatch { expected, actual },
            ));
        }
        Ok(())
    }

    /// Pad a point slice to the minimum sample count by repeating the last point.
    fn pad_points<P: Point>(points: &[P]) -> Vec<P> {
        let last = *points.last().unwrap();
        let mut padded = Vec::with_capacity(MIN_SAMPLES_PER_FRAME);
        padded.extend_from_slice(points);
        padded.resize(MIN_SAMPLES_PER_FRAME, last);
        padded
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Try to close gracefully, ignore errors
        let _ = self.close();
    }
}

// -------------------------------------------------------------------------------------------------
//  Tests
// -------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------------------------
    //  PointFormat tests
    // -----------------------------------------------------------------------------------------

    #[test]
    fn point_format_size_bytes() {
        assert_eq!(PointFormat::Xyrgbi.size_bytes(), 8);
        assert_eq!(PointFormat::XyrgbHighRes.size_bytes(), 10);
        assert_eq!(PointFormat::Extended.size_bytes(), 20);
    }

    #[test]
    fn point_format_word_count() {
        // word_count is the number of 32-bit words (descriptor pairs)
        // Per C++ reference: XYRGBI=4, XyrgbHighRes=5, Extended=10
        assert_eq!(PointFormat::Xyrgbi.word_count(), 4);
        assert_eq!(PointFormat::XyrgbHighRes.word_count(), 5);
        assert_eq!(PointFormat::Extended.word_count(), 10);
    }

    #[test]
    fn point_format_descriptors_length() {
        assert_eq!(PointFormat::Xyrgbi.descriptors().len(), 8);
        assert_eq!(PointFormat::XyrgbHighRes.descriptors().len(), 10);
        assert_eq!(PointFormat::Extended.descriptors().len(), 20);
    }

    #[test]
    fn point_format_descriptors_not_empty() {
        // All descriptor arrays should have non-zero values
        for &desc in PointFormat::Xyrgbi.descriptors() {
            assert_ne!(desc, 0);
        }
        for &desc in PointFormat::XyrgbHighRes.descriptors() {
            assert_ne!(desc, 0);
        }
        for &desc in PointFormat::Extended.descriptors() {
            assert_ne!(desc, 0);
        }
    }

    // -----------------------------------------------------------------------------------------
    //  Duration calculation tests
    // -----------------------------------------------------------------------------------------

    #[test]
    fn duration_calculation() {
        // Duration formula: (points * 1_000_000) / scan_speed
        // At 30000 pps, 1000 points = 33333 microseconds
        let points = 1000u64;
        let scan_speed = 30000u64;
        let duration = (points * 1_000_000) / scan_speed;
        assert_eq!(duration, 33333);

        // At 30000 pps, 30000 points = 1 second = 1_000_000 microseconds
        let points = 30000u64;
        let duration = (points * 1_000_000) / scan_speed;
        assert_eq!(duration, 1_000_000);
    }

    // -----------------------------------------------------------------------------------------
    //  Max points per packet calculation tests
    // -----------------------------------------------------------------------------------------

    #[test]
    fn max_points_per_packet_with_config() {
        // Header size with config for Xyrgbi:
        // PacketHeader(4) + ChannelMessageHeader(8) + ChannelConfigHeader(4) +
        // descriptors(8 * 2 = 16) + SampleChunkHeader(4) = 36 bytes
        let header_size = PacketHeader::SIZE_BYTES
            + ChannelMessageHeader::SIZE_BYTES
            + ChannelConfigHeader::SIZE_BYTES
            + PointFormat::Xyrgbi.descriptors().len() * 2
            + SampleChunkHeader::SIZE_BYTES;
        assert_eq!(header_size, 36);

        // MAX_UDP_PAYLOAD = 1454
        // Available for samples = 1454 - 36 = 1418 bytes
        // At 8 bytes per sample = 177 points max
        let max_points = (MAX_UDP_PAYLOAD - header_size) / 8;
        assert_eq!(max_points, 177);
    }

    #[test]
    fn max_points_per_packet_without_config() {
        // Header size without config:
        // PacketHeader(4) + ChannelMessageHeader(8) + SampleChunkHeader(4) = 16 bytes
        let header_size = PacketHeader::SIZE_BYTES
            + ChannelMessageHeader::SIZE_BYTES
            + SampleChunkHeader::SIZE_BYTES;
        assert_eq!(header_size, 16);

        // MAX_UDP_PAYLOAD = 1454
        // Available for samples = 1454 - 16 = 1438 bytes
        // At 8 bytes per sample = 179 points max
        let max_points_xyrgbi = (MAX_UDP_PAYLOAD - header_size) / 8;
        assert_eq!(max_points_xyrgbi, 179);

        // At 10 bytes per sample = 143 points max
        let max_points_highres = (MAX_UDP_PAYLOAD - header_size) / 10;
        assert_eq!(max_points_highres, 143);

        // At 20 bytes per sample = 71 points max
        let max_points_extended = (MAX_UDP_PAYLOAD - header_size) / 20;
        assert_eq!(max_points_extended, 71);
    }

    // -----------------------------------------------------------------------------------------
    //  Content ID construction tests
    // -----------------------------------------------------------------------------------------

    #[test]
    fn content_id_construction() {
        // Service ID 1 -> channel_id should be 0 (1-1 = 0, shifted by 8)
        let service_id: u8 = 1;
        let channel_id = ((service_id.saturating_sub(1)) as u16 & 0x3F) << 8;
        assert_eq!(channel_id, 0x0000);

        // Service ID 2 -> channel_id should be 0x0100
        let service_id: u8 = 2;
        let channel_id = ((service_id.saturating_sub(1)) as u16 & 0x3F) << 8;
        assert_eq!(channel_id, 0x0100);

        // Combined content_id with flags
        let content_id = IDNFLG_CONTENTID_CHANNELMSG | channel_id;
        assert_eq!(content_id, 0x8100);
    }

    // -----------------------------------------------------------------------------------------
    //  Sequence number wrapping test
    // -----------------------------------------------------------------------------------------

    #[test]
    fn sequence_number_wrapping() {
        let mut seq: u16 = u16::MAX - 1;

        seq = seq.wrapping_add(1);
        assert_eq!(seq, u16::MAX);

        seq = seq.wrapping_add(1);
        assert_eq!(seq, 0);

        seq = seq.wrapping_add(1);
        assert_eq!(seq, 1);
    }

    // -----------------------------------------------------------------------------------------
    //  Service data match counter test
    // -----------------------------------------------------------------------------------------

    #[test]
    fn service_data_match_wrapping() {
        let mut sdm: u8 = 254;

        sdm = sdm.wrapping_add(1);
        assert_eq!(sdm, 255);

        sdm = sdm.wrapping_add(1);
        assert_eq!(sdm, 0);
    }

    // -----------------------------------------------------------------------------------------
    //  Timestamp truncation test
    // -----------------------------------------------------------------------------------------

    #[test]
    fn timestamp_truncation_to_u32() {
        // Timestamps are u64 internally but truncated to u32 for protocol
        let timestamp: u64 = 0x1_0000_ABCD; // Exceeds u32
        let truncated = (timestamp & 0xFFFF_FFFF) as u32;
        assert_eq!(truncated, 0x0000_ABCD);

        // Max u32 value should stay intact
        let timestamp: u64 = 0xFFFF_FFFF;
        let truncated = (timestamp & 0xFFFF_FFFF) as u32;
        assert_eq!(truncated, 0xFFFF_FFFF);
    }

    // -----------------------------------------------------------------------------------------
    //  pad_points tests
    // -----------------------------------------------------------------------------------------

    use crate::protocols::idn::protocol::PointXyrgbi;

    #[test]
    fn test_pad_points_pads_to_minimum() {
        // 5 points should be padded to MIN_SAMPLES_PER_FRAME (20)
        let points: Vec<PointXyrgbi> = (0..5)
            .map(|i| PointXyrgbi::new(i as i16, i as i16, 255, 0, 0, 255))
            .collect();
        let padded = Stream::pad_points(&points);
        assert_eq!(padded.len(), MIN_SAMPLES_PER_FRAME);
        // Original points preserved
        for i in 0..5 {
            assert_eq!(padded[i], points[i]);
        }
        // Remaining points should be copies of the last original point
        let last = points[4];
        for p in &padded[5..] {
            assert_eq!(*p, last);
        }
    }

    #[test]
    fn test_pad_points_single_point() {
        let points = vec![PointXyrgbi::new(100, -200, 128, 64, 32, 255)];
        let padded = Stream::pad_points(&points);
        assert_eq!(padded.len(), MIN_SAMPLES_PER_FRAME);
        // All 20 should be identical copies
        for p in &padded {
            assert_eq!(*p, points[0]);
        }
    }

    #[test]
    fn test_pad_points_nineteen() {
        // 19 points → 20, only one duplicate
        let points: Vec<PointXyrgbi> = (0..19)
            .map(|i| PointXyrgbi::new(i as i16, 0, 0, 0, 0, 0))
            .collect();
        let padded = Stream::pad_points(&points);
        assert_eq!(padded.len(), MIN_SAMPLES_PER_FRAME);
        // First 19 preserved
        for i in 0..19 {
            assert_eq!(padded[i], points[i]);
        }
        // 20th is copy of last
        assert_eq!(padded[19], points[18]);
    }

    // -----------------------------------------------------------------------------------------
    //  Even sample distribution tests
    // -----------------------------------------------------------------------------------------

    #[test]
    fn test_even_distribution_300_points() {
        // With config headers (Xyrgbi): max_points_per_packet = 177
        // 300 points: num_packets = (300 / 177) + 1 = 2
        // points_to_send = 300 / 2 = 150
        let header_size = PacketHeader::SIZE_BYTES
            + ChannelMessageHeader::SIZE_BYTES
            + ChannelConfigHeader::SIZE_BYTES
            + PointFormat::Xyrgbi.descriptors().len() * 2
            + SampleChunkHeader::SIZE_BYTES;
        let max_points_per_packet = (MAX_UDP_PAYLOAD - header_size) / PointXyrgbi::SIZE_BYTES;
        assert_eq!(max_points_per_packet, 177);

        let total = 300usize;
        let num_packets = (total / max_points_per_packet) + 1;
        let points_to_send = total / num_packets;

        assert_eq!(num_packets, 2);
        assert_eq!(points_to_send, 150);
    }

    #[test]
    fn test_even_distribution_500_points() {
        let header_size = PacketHeader::SIZE_BYTES
            + ChannelMessageHeader::SIZE_BYTES
            + ChannelConfigHeader::SIZE_BYTES
            + PointFormat::Xyrgbi.descriptors().len() * 2
            + SampleChunkHeader::SIZE_BYTES;
        let max_points_per_packet = (MAX_UDP_PAYLOAD - header_size) / PointXyrgbi::SIZE_BYTES;

        let total = 500usize;
        let num_packets = (total / max_points_per_packet) + 1;
        let points_to_send = total / num_packets;

        assert_eq!(num_packets, 3);
        assert_eq!(points_to_send, 166);
    }

    #[test]
    fn test_even_distribution_small_frame() {
        let header_size = PacketHeader::SIZE_BYTES
            + ChannelMessageHeader::SIZE_BYTES
            + ChannelConfigHeader::SIZE_BYTES
            + PointFormat::Xyrgbi.descriptors().len() * 2
            + SampleChunkHeader::SIZE_BYTES;
        let max_points_per_packet = (MAX_UDP_PAYLOAD - header_size) / PointXyrgbi::SIZE_BYTES;

        let total = 50usize;
        let num_packets = (total / max_points_per_packet) + 1;
        let points_to_send = total / num_packets;

        // Fits in 1 packet, all 50 sent
        assert_eq!(num_packets, 1);
        assert_eq!(points_to_send, 50);
    }
}
