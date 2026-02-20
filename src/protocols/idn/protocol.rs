//! Types and constants that precisely match the IDN protocol specification.
//!
//! The ILDA Digital Network (IDN) protocol uses UDP on port 7255 for all communication.
//! Unlike Ether Dream, IDN uses big-endian byte order for point data.

use byteorder::{ReadBytesExt, WriteBytesExt, BE};
use std::io;

use crate::types::LaserPoint;

// -------------------------------------------------------------------------------------------------
//  Constants
// -------------------------------------------------------------------------------------------------

/// IDN protocol UDP port.
pub const IDN_PORT: u16 = 7255;

/// Maximum UDP payload size to avoid IP fragmentation.
pub const MAX_UDP_PAYLOAD: usize = 1454;

// Hello protocol commands
pub const IDNCMD_VOID: u8 = 0x00;
pub const IDNCMD_PING_REQUEST: u8 = 0x08;
pub const IDNCMD_PING_RESPONSE: u8 = 0x09;
pub const IDNCMD_GROUP_REQUEST: u8 = 0x0C;
pub const IDNCMD_GROUP_RESPONSE: u8 = 0x0D;
pub const IDNCMD_SCAN_REQUEST: u8 = 0x10;
pub const IDNCMD_SCAN_RESPONSE: u8 = 0x11;
pub const IDNCMD_SERVICEMAP_REQUEST: u8 = 0x12;
pub const IDNCMD_SERVICEMAP_RESPONSE: u8 = 0x13;

// Parameter commands
pub const IDNCMD_SERVICE_PARAMS_REQUEST: u8 = 0x20;
pub const IDNCMD_SERVICE_PARAMS_RESPONSE: u8 = 0x21;
pub const IDNCMD_UNIT_PARAMS_REQUEST: u8 = 0x22;
pub const IDNCMD_UNIT_PARAMS_RESPONSE: u8 = 0x23;
pub const IDNCMD_LINK_PARAMS_REQUEST: u8 = 0x28;
pub const IDNCMD_LINK_PARAMS_RESPONSE: u8 = 0x29;

// Realtime stream commands
pub const IDNCMD_RT_CNLMSG: u8 = 0x40;
pub const IDNCMD_RT_CNLMSG_ACKREQ: u8 = 0x41;
pub const IDNCMD_RT_CNLMSG_CLOSE: u8 = 0x44;
pub const IDNCMD_RT_CNLMSG_CLOSE_ACKREQ: u8 = 0x45;
pub const IDNCMD_RT_ABORT: u8 = 0x46;
pub const IDNCMD_RT_ACKNOWLEDGE: u8 = 0x47;

// Packet flags masks
pub const IDNMSK_PKTFLAGS_GROUP: u8 = 0x0F;

// Scan response status flags
pub const IDNFLG_SCAN_STATUS_MALFUNCTION: u8 = 0x80;
pub const IDNFLG_SCAN_STATUS_OFFLINE: u8 = 0x40;
pub const IDNFLG_SCAN_STATUS_EXCLUDED: u8 = 0x20;
pub const IDNFLG_SCAN_STATUS_OCCUPIED: u8 = 0x10;
pub const IDNFLG_SCAN_STATUS_REALTIME: u8 = 0x01;

// Group operation codes (idn-hello.h:84-89)
pub const IDNVAL_GROUPOP_SUCCESS: i8 = 0x00;
pub const IDNVAL_GROUPOP_GETMASK: i8 = 0x01;
pub const IDNVAL_GROUPOP_SETMASK: i8 = 0x02;
pub const IDNVAL_GROUPOP_ERR_AUTH: i8 = -3; // 0xFD
pub const IDNVAL_GROUPOP_ERR_OPERATION: i8 = -2; // 0xFE
pub const IDNVAL_GROUPOP_ERR_REQUEST: i8 = -1; // 0xFF

// Service types
pub const IDNVAL_STYPE_RELAY: u8 = 0x00;
pub const IDNVAL_STYPE_UART: u8 = 0x04;
pub const IDNVAL_STYPE_DMX512: u8 = 0x05;
pub const IDNVAL_STYPE_LAPRO: u8 = 0x80; // Standard laser projector

// Service map entry flags
/// Default Service ID flag - indicates this is the default service for its type
pub const IDNFLG_SERVICEMAP_DSID: u8 = 0x01;

// Channel message content IDs
pub const IDNFLG_CONTENTID_CHANNELMSG: u16 = 0x8000;
pub const IDNFLG_CONTENTID_CONFIG_LSTFRG: u16 = 0x4000;
pub const IDNMSK_CONTENTID_CHANNELID: u16 = 0x3F00;
pub const IDNMSK_CONTENTID_CNKTYPE: u16 = 0x00FF;

// Data chunk types
pub const IDNVAL_CNKTYPE_VOID: u8 = 0x00;
pub const IDNVAL_CNKTYPE_LPGRF_WAVE: u8 = 0x01;
pub const IDNVAL_CNKTYPE_LPGRF_FRAME: u8 = 0x02;
pub const IDNVAL_CNKTYPE_LPGRF_FRAME_FIRST: u8 = 0x03;
pub const IDNVAL_CNKTYPE_LPGRF_FRAME_SEQUEL: u8 = 0xC0;

// Channel configuration flags
pub const IDNFLG_CHNCFG_ROUTING: u8 = 0x01;
pub const IDNFLG_CHNCFG_CLOSE: u8 = 0x02;

// Service modes
pub const IDNVAL_SMOD_VOID: u8 = 0x00;
pub const IDNVAL_SMOD_LPGRF_CONTINUOUS: u8 = 0x01;
pub const IDNVAL_SMOD_LPGRF_DISCRETE: u8 = 0x02;

// Sample sizes
pub const XYRGBI_SAMPLE_SIZE: usize = 8;
pub const XYRGB_HIGHRES_SAMPLE_SIZE: usize = 10;
pub const EXTENDED_SAMPLE_SIZE: usize = 20;

// -------------------------------------------------------------------------------------------------
//  Traits
// -------------------------------------------------------------------------------------------------

/// A trait for writing any of the IDN protocol types to bytes.
pub trait WriteBytes {
    fn write_bytes<P: WriteToBytes>(&mut self, protocol: P) -> io::Result<()>;
}

/// A trait for reading any of the IDN protocol types from bytes.
pub trait ReadBytes {
    fn read_bytes<P: ReadFromBytes>(&mut self) -> io::Result<P>;
}

/// Protocol types that may be written to bytes.
pub trait WriteToBytes {
    fn write_to_bytes<W: WriteBytesExt>(&self, writer: W) -> io::Result<()>;
}

/// Protocol types that may be read from bytes.
pub trait ReadFromBytes: Sized {
    fn read_from_bytes<R: ReadBytesExt>(reader: R) -> io::Result<Self>;
}

/// Types that have a constant size when written to or read from bytes.
pub trait SizeBytes {
    const SIZE_BYTES: usize;
}

// -------------------------------------------------------------------------------------------------
//  Packet Header (4 bytes)
// -------------------------------------------------------------------------------------------------

/// IDN packet header - present in all IDN packets.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct PacketHeader {
    /// The command code (IDNCMD_*)
    pub command: u8,
    /// Upper 4 bits: Flags; Lower 4 bits: Client group
    pub flags: u8,
    /// Sequence counter, must count up
    pub sequence: u16,
}

impl WriteToBytes for PacketHeader {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.command)?;
        writer.write_u8(self.flags)?;
        writer.write_u16::<BE>(self.sequence)?;
        Ok(())
    }
}

impl ReadFromBytes for PacketHeader {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let command = reader.read_u8()?;
        let flags = reader.read_u8()?;
        let sequence = reader.read_u16::<BE>()?;
        Ok(PacketHeader {
            command,
            flags,
            sequence,
        })
    }
}

impl SizeBytes for PacketHeader {
    const SIZE_BYTES: usize = 4;
}

// -------------------------------------------------------------------------------------------------
//  Scan Response
// -------------------------------------------------------------------------------------------------

/// Response to a scan request, containing unit identification and status.
#[repr(C)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ScanResponse {
    /// Size of the struct (for versioning)
    pub struct_size: u8,
    /// Protocol version: Upper 4 bits = Major, Lower 4 bits = Minor
    pub protocol_version: u8,
    /// Unit and link status flags
    pub status: u8,
    /// Reserved byte
    pub reserved: u8,
    /// Unit ID: \[0\] = Len, \[1\] = Cat, \[2..Len\] = ID, padded with '\0'
    pub unit_id: [u8; 16],
    /// Hostname, not null-terminated, padded with '\0'
    pub hostname: [u8; 20],
}

impl WriteToBytes for ScanResponse {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.struct_size)?;
        writer.write_u8(self.protocol_version)?;
        writer.write_u8(self.status)?;
        writer.write_u8(self.reserved)?;
        for &byte in &self.unit_id {
            writer.write_u8(byte)?;
        }
        for &byte in &self.hostname {
            writer.write_u8(byte)?;
        }
        Ok(())
    }
}

impl ReadFromBytes for ScanResponse {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let struct_size = reader.read_u8()?;
        let protocol_version = reader.read_u8()?;
        let status = reader.read_u8()?;
        let reserved = reader.read_u8()?;
        let mut unit_id = [0u8; 16];
        for byte in &mut unit_id {
            *byte = reader.read_u8()?;
        }
        let mut hostname = [0u8; 20];
        for byte in &mut hostname {
            *byte = reader.read_u8()?;
        }
        Ok(ScanResponse {
            struct_size,
            protocol_version,
            status,
            reserved,
            unit_id,
            hostname,
        })
    }
}

impl SizeBytes for ScanResponse {
    const SIZE_BYTES: usize = 40;
}

impl ScanResponse {
    /// Parse the hostname as a string, trimming null bytes.
    pub fn hostname_str(&self) -> &str {
        let end = self
            .hostname
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.hostname.len());
        std::str::from_utf8(&self.hostname[..end]).unwrap_or("")
    }
}

// -------------------------------------------------------------------------------------------------
//  Service Map Response
// -------------------------------------------------------------------------------------------------

/// Header for service map response.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServiceMapResponseHeader {
    /// Size of this struct
    pub struct_size: u8,
    /// Size of an entry
    pub entry_size: u8,
    /// Number of relay entries
    pub relay_entry_count: u8,
    /// Number of service entries
    pub service_entry_count: u8,
}

impl WriteToBytes for ServiceMapResponseHeader {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.struct_size)?;
        writer.write_u8(self.entry_size)?;
        writer.write_u8(self.relay_entry_count)?;
        writer.write_u8(self.service_entry_count)?;
        Ok(())
    }
}

impl ReadFromBytes for ServiceMapResponseHeader {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let struct_size = reader.read_u8()?;
        let entry_size = reader.read_u8()?;
        let relay_entry_count = reader.read_u8()?;
        let service_entry_count = reader.read_u8()?;
        Ok(ServiceMapResponseHeader {
            struct_size,
            entry_size,
            relay_entry_count,
            service_entry_count,
        })
    }
}

impl SizeBytes for ServiceMapResponseHeader {
    const SIZE_BYTES: usize = 4;
}

/// An entry in the service map (used for both relays and services).
#[repr(C)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServiceMapEntry {
    /// Service: The ID (!=0); Relay: Must be 0
    pub service_id: u8,
    /// The type of the service; Relay: Must be 0
    pub service_type: u8,
    /// Status flags and options
    pub flags: u8,
    /// Service: Root(0)/Relay(>0); Relay: Number (!=0)
    pub relay_number: u8,
    /// Name, not null-terminated, padded with '\0'
    pub name: [u8; 20],
}

impl WriteToBytes for ServiceMapEntry {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.service_id)?;
        writer.write_u8(self.service_type)?;
        writer.write_u8(self.flags)?;
        writer.write_u8(self.relay_number)?;
        for &byte in &self.name {
            writer.write_u8(byte)?;
        }
        Ok(())
    }
}

impl ReadFromBytes for ServiceMapEntry {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let service_id = reader.read_u8()?;
        let service_type = reader.read_u8()?;
        let flags = reader.read_u8()?;
        let relay_number = reader.read_u8()?;
        let mut name = [0u8; 20];
        for byte in &mut name {
            *byte = reader.read_u8()?;
        }
        Ok(ServiceMapEntry {
            service_id,
            service_type,
            flags,
            relay_number,
            name,
        })
    }
}

impl SizeBytes for ServiceMapEntry {
    const SIZE_BYTES: usize = 24;
}

impl ServiceMapEntry {
    /// Parse the name as a string, trimming null bytes.
    pub fn name_str(&self) -> &str {
        let end = self
            .name
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.name.len());
        std::str::from_utf8(&self.name[..end]).unwrap_or("")
    }

    /// Check if this entry is a relay (service_id == 0).
    pub fn is_relay(&self) -> bool {
        self.service_id == 0
    }
}

// -------------------------------------------------------------------------------------------------
//  Channel Message Header (8 bytes)
// -------------------------------------------------------------------------------------------------

/// Channel message header for real-time streaming.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChannelMessageHeader {
    /// Total size of the channel message payload
    pub total_size: u16,
    /// Content ID with flags and chunk type
    pub content_id: u16,
    /// Timestamp in microseconds
    pub timestamp: u32,
}

impl WriteToBytes for ChannelMessageHeader {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u16::<BE>(self.total_size)?;
        writer.write_u16::<BE>(self.content_id)?;
        writer.write_u32::<BE>(self.timestamp)?;
        Ok(())
    }
}

impl ReadFromBytes for ChannelMessageHeader {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let total_size = reader.read_u16::<BE>()?;
        let content_id = reader.read_u16::<BE>()?;
        let timestamp = reader.read_u32::<BE>()?;
        Ok(ChannelMessageHeader {
            total_size,
            content_id,
            timestamp,
        })
    }
}

impl SizeBytes for ChannelMessageHeader {
    const SIZE_BYTES: usize = 8;
}

// -------------------------------------------------------------------------------------------------
//  Channel Configuration Header (4 bytes)
// -------------------------------------------------------------------------------------------------

/// Channel configuration header, sent when format changes or periodically.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ChannelConfigHeader {
    /// Number of descriptor pairs (32-bit words) in the descriptor array
    pub word_count: u8,
    /// Upper 4 bits: Decoder flags; Lower 4 bits: Config flags
    pub flags: u8,
    /// Service ID to route to
    pub service_id: u8,
    /// Service mode (continuous/discrete)
    pub service_mode: u8,
}

impl WriteToBytes for ChannelConfigHeader {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.word_count)?;
        writer.write_u8(self.flags)?;
        writer.write_u8(self.service_id)?;
        writer.write_u8(self.service_mode)?;
        Ok(())
    }
}

impl ReadFromBytes for ChannelConfigHeader {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let word_count = reader.read_u8()?;
        let flags = reader.read_u8()?;
        let service_id = reader.read_u8()?;
        let service_mode = reader.read_u8()?;
        Ok(ChannelConfigHeader {
            word_count,
            flags,
            service_id,
            service_mode,
        })
    }
}

impl SizeBytes for ChannelConfigHeader {
    const SIZE_BYTES: usize = 4;
}

// -------------------------------------------------------------------------------------------------
//  Sample Chunk Header (4 bytes)
// -------------------------------------------------------------------------------------------------

/// Sample chunk header containing flags and duration.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct SampleChunkHeader {
    /// Upper 8 bits: Flags; Lower 24 bits: Duration in microseconds
    pub flags_duration: u32,
}

impl SampleChunkHeader {
    /// Create a new sample chunk header with the given flags and duration.
    pub fn new(flags: u8, duration_us: u32) -> Self {
        let flags_duration = ((flags as u32) << 24) | (duration_us & 0x00FF_FFFF);
        Self { flags_duration }
    }

    /// Get the flags from the header.
    pub fn flags(&self) -> u8 {
        (self.flags_duration >> 24) as u8
    }

    /// Get the duration in microseconds from the header.
    pub fn duration_us(&self) -> u32 {
        self.flags_duration & 0x00FF_FFFF
    }
}

impl WriteToBytes for SampleChunkHeader {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u32::<BE>(self.flags_duration)?;
        Ok(())
    }
}

impl ReadFromBytes for SampleChunkHeader {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let flags_duration = reader.read_u32::<BE>()?;
        Ok(SampleChunkHeader { flags_duration })
    }
}

impl SizeBytes for SampleChunkHeader {
    const SIZE_BYTES: usize = 4;
}

// -------------------------------------------------------------------------------------------------
//  Acknowledgment Response (10 bytes)
// -------------------------------------------------------------------------------------------------

/// Acknowledgment response from the server (spec section 6.3.3).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct AcknowledgeResponse {
    /// Size of this struct (always 10)
    pub struct_size: u8,
    /// Result code: 0 = success, negative = error (see IDNVAL_RTACK_ERR_*)
    pub result_code: i8,
    /// Input event flags (e.g., interlock triggered)
    pub input_event_flags: u16,
    /// Pipeline event flags (e.g., buffer underflow)
    pub pipeline_event_flags: u16,
    /// Status flags (e.g., projector ready)
    pub status_flags: u8,
    /// Link quality (0-255, higher is better)
    pub link_quality: u8,
    /// Latency in microseconds (server processing time)
    pub latency_us: u16,
}

impl WriteToBytes for AcknowledgeResponse {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.struct_size)?;
        writer.write_i8(self.result_code)?;
        writer.write_u16::<BE>(self.input_event_flags)?;
        writer.write_u16::<BE>(self.pipeline_event_flags)?;
        writer.write_u8(self.status_flags)?;
        writer.write_u8(self.link_quality)?;
        writer.write_u16::<BE>(self.latency_us)?;
        Ok(())
    }
}

impl ReadFromBytes for AcknowledgeResponse {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let struct_size = reader.read_u8()?;
        let result_code = reader.read_i8()?;
        let input_event_flags = reader.read_u16::<BE>()?;
        let pipeline_event_flags = reader.read_u16::<BE>()?;
        let status_flags = reader.read_u8()?;
        let link_quality = reader.read_u8()?;
        let latency_us = reader.read_u16::<BE>()?;
        Ok(AcknowledgeResponse {
            struct_size,
            result_code,
            input_event_flags,
            pipeline_event_flags,
            status_flags,
            link_quality,
            latency_us,
        })
    }
}

impl SizeBytes for AcknowledgeResponse {
    const SIZE_BYTES: usize = 10;
}

impl AcknowledgeResponse {
    /// Check if the acknowledgment indicates success.
    pub fn is_success(&self) -> bool {
        self.result_code >= 0
    }

    /// Get the latency as a Duration.
    pub fn latency(&self) -> std::time::Duration {
        std::time::Duration::from_micros(self.latency_us as u64)
    }
}

// -------------------------------------------------------------------------------------------------
//  Client Group Request/Response (idn-hello.h:124-140)
// -------------------------------------------------------------------------------------------------

/// Client group request payload for get/set operations (16 bytes).
/// Sent with IDNCMD_GROUP_REQUEST, response via IDNCMD_GROUP_RESPONSE.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct GroupRequest {
    /// Size of this struct (always 16)
    pub struct_size: u8,
    /// Operation code: GETMASK (0x01) or SETMASK (0x02)
    pub op_code: i8,
    /// Mask for the client groups
    pub group_mask: u16,
    /// Authentication code, padded with '\0'
    pub auth_code: [u8; 12],
}

impl GroupRequest {
    /// Create a GET request to retrieve the current group mask.
    pub fn get() -> Self {
        Self {
            struct_size: 16,
            op_code: IDNVAL_GROUPOP_GETMASK,
            group_mask: 0,
            auth_code: [0u8; 12],
        }
    }

    /// Create a SET request to change the group mask.
    pub fn set(mask: u16) -> Self {
        Self {
            struct_size: 16,
            op_code: IDNVAL_GROUPOP_SETMASK,
            group_mask: mask,
            auth_code: [0u8; 12],
        }
    }

    /// Create a SET request with authentication.
    pub fn set_with_auth(mask: u16, auth: &[u8]) -> Self {
        let mut auth_code = [0u8; 12];
        let len = auth.len().min(12);
        auth_code[..len].copy_from_slice(&auth[..len]);
        Self {
            struct_size: 16,
            op_code: IDNVAL_GROUPOP_SETMASK,
            group_mask: mask,
            auth_code,
        }
    }
}

impl WriteToBytes for GroupRequest {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.struct_size)?;
        writer.write_i8(self.op_code)?;
        writer.write_u16::<BE>(self.group_mask)?;
        for &byte in &self.auth_code {
            writer.write_u8(byte)?;
        }
        Ok(())
    }
}

impl ReadFromBytes for GroupRequest {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let struct_size = reader.read_u8()?;
        let op_code = reader.read_i8()?;
        let group_mask = reader.read_u16::<BE>()?;
        let mut auth_code = [0u8; 12];
        for byte in &mut auth_code {
            *byte = reader.read_u8()?;
        }
        Ok(GroupRequest {
            struct_size,
            op_code,
            group_mask,
            auth_code,
        })
    }
}

impl SizeBytes for GroupRequest {
    const SIZE_BYTES: usize = 16;
}

/// Client group response payload (4 bytes).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct GroupResponse {
    /// Size of this struct (always 4)
    pub struct_size: u8,
    /// Operation result: success (>=0) or error (<0)
    pub op_code: i8,
    /// Current client group mask (16 bits, one per group 0-15)
    pub group_mask: u16,
}

impl WriteToBytes for GroupResponse {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.struct_size)?;
        writer.write_i8(self.op_code)?;
        writer.write_u16::<BE>(self.group_mask)?;
        Ok(())
    }
}

impl ReadFromBytes for GroupResponse {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let struct_size = reader.read_u8()?;
        let op_code = reader.read_i8()?;
        let group_mask = reader.read_u16::<BE>()?;
        Ok(GroupResponse {
            struct_size,
            op_code,
            group_mask,
        })
    }
}

impl SizeBytes for GroupResponse {
    const SIZE_BYTES: usize = 4;
}

impl GroupResponse {
    /// Check if the operation was successful.
    pub fn is_success(&self) -> bool {
        self.op_code >= 0
    }

    /// Check if a specific group (0-15) is enabled.
    pub fn is_group_enabled(&self, group: u8) -> bool {
        if group > 15 {
            return false;
        }
        (self.group_mask & (1 << group)) != 0
    }

    /// Get list of enabled groups.
    pub fn enabled_groups(&self) -> Vec<u8> {
        (0..16).filter(|&g| self.is_group_enabled(g)).collect()
    }
}

// -------------------------------------------------------------------------------------------------
//  Parameter Request/Response
// -------------------------------------------------------------------------------------------------

/// Parameter get request payload.
/// The service_id specifies which service's parameter to get.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParameterGetRequest {
    /// Service ID (0 for unit-level parameters)
    pub service_id: u8,
    /// Reserved byte
    pub reserved: u8,
    /// Parameter ID
    pub param_id: u16,
}

impl WriteToBytes for ParameterGetRequest {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.service_id)?;
        writer.write_u8(self.reserved)?;
        writer.write_u16::<BE>(self.param_id)?;
        Ok(())
    }
}

impl ReadFromBytes for ParameterGetRequest {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let service_id = reader.read_u8()?;
        let reserved = reader.read_u8()?;
        let param_id = reader.read_u16::<BE>()?;
        Ok(ParameterGetRequest {
            service_id,
            reserved,
            param_id,
        })
    }
}

impl SizeBytes for ParameterGetRequest {
    const SIZE_BYTES: usize = 4;
}

/// Parameter response payload (for both get and set responses).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParameterResponse {
    /// Service ID (0 for unit-level parameters)
    pub service_id: u8,
    /// Result code: 0 = success, negative = error
    pub result_code: i8,
    /// Parameter ID
    pub param_id: u16,
    /// Parameter value (interpretation depends on param_id)
    pub value: u32,
}

impl WriteToBytes for ParameterResponse {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.service_id)?;
        writer.write_i8(self.result_code)?;
        writer.write_u16::<BE>(self.param_id)?;
        writer.write_u32::<BE>(self.value)?;
        Ok(())
    }
}

impl ReadFromBytes for ParameterResponse {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let service_id = reader.read_u8()?;
        let result_code = reader.read_i8()?;
        let param_id = reader.read_u16::<BE>()?;
        let value = reader.read_u32::<BE>()?;
        Ok(ParameterResponse {
            service_id,
            result_code,
            param_id,
            value,
        })
    }
}

impl SizeBytes for ParameterResponse {
    const SIZE_BYTES: usize = 8;
}

impl ParameterResponse {
    /// Check if the response indicates success.
    pub fn is_success(&self) -> bool {
        self.result_code >= 0
    }
}

/// Parameter set request payload.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct ParameterSetRequest {
    /// Service ID (0 for unit-level parameters)
    pub service_id: u8,
    /// Reserved byte
    pub reserved: u8,
    /// Parameter ID
    pub param_id: u16,
    /// New parameter value
    pub value: u32,
}

impl WriteToBytes for ParameterSetRequest {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.service_id)?;
        writer.write_u8(self.reserved)?;
        writer.write_u16::<BE>(self.param_id)?;
        writer.write_u32::<BE>(self.value)?;
        Ok(())
    }
}

impl ReadFromBytes for ParameterSetRequest {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let service_id = reader.read_u8()?;
        let reserved = reader.read_u8()?;
        let param_id = reader.read_u16::<BE>()?;
        let value = reader.read_u32::<BE>()?;
        Ok(ParameterSetRequest {
            service_id,
            reserved,
            param_id,
            value,
        })
    }
}

impl SizeBytes for ParameterSetRequest {
    const SIZE_BYTES: usize = 8;
}

// -------------------------------------------------------------------------------------------------
//  Point Types
// -------------------------------------------------------------------------------------------------

/// Standard XYRGBI point format (8 bytes).
/// Coordinates are signed 16-bit, colors are unsigned 8-bit.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct PointXyrgbi {
    /// X coordinate (-32768 to 32767)
    pub x: i16,
    /// Y coordinate (-32768 to 32767)
    pub y: i16,
    /// Red channel (0-255)
    pub r: u8,
    /// Green channel (0-255)
    pub g: u8,
    /// Blue channel (0-255)
    pub b: u8,
    /// Intensity channel (0-255)
    pub i: u8,
}

impl PointXyrgbi {
    pub fn new(x: i16, y: i16, r: u8, g: u8, b: u8, i: u8) -> Self {
        Self { x, y, r, g, b, i }
    }
}

impl WriteToBytes for PointXyrgbi {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_i16::<BE>(self.x)?;
        writer.write_i16::<BE>(self.y)?;
        writer.write_u8(self.r)?;
        writer.write_u8(self.g)?;
        writer.write_u8(self.b)?;
        writer.write_u8(self.i)?;
        Ok(())
    }
}

impl ReadFromBytes for PointXyrgbi {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let x = reader.read_i16::<BE>()?;
        let y = reader.read_i16::<BE>()?;
        let r = reader.read_u8()?;
        let g = reader.read_u8()?;
        let b = reader.read_u8()?;
        let i = reader.read_u8()?;
        Ok(PointXyrgbi { x, y, r, g, b, i })
    }
}

impl SizeBytes for PointXyrgbi {
    const SIZE_BYTES: usize = XYRGBI_SAMPLE_SIZE;
}

impl From<&LaserPoint> for PointXyrgbi {
    /// Convert a LaserPoint to an IDN PointXyrgbi.
    ///
    /// LaserPoint uses f32 coordinates (-1.0 to 1.0) and u16 colors (0-65535).
    /// IDN PointXyrgbi uses i16 signed coordinates (-32768 to 32767) and u8 colors.
    /// Coordinates are inverted to match hardware orientation.
    fn from(p: &LaserPoint) -> Self {
        let x = (p.x.clamp(-1.0, 1.0) * -32767.0).round() as i16;
        let y = (p.y.clamp(-1.0, 1.0) * -32767.0).round() as i16;
        PointXyrgbi::new(
            x,
            y,
            (p.r >> 8) as u8,
            (p.g >> 8) as u8,
            (p.b >> 8) as u8,
            (p.intensity >> 8) as u8,
        )
    }
}

/// High-resolution XYRGB point format (10 bytes).
/// Coordinates are signed 16-bit, colors are unsigned 16-bit.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct PointXyrgbHighRes {
    /// X coordinate (-32768 to 32767)
    pub x: i16,
    /// Y coordinate (-32768 to 32767)
    pub y: i16,
    /// Red channel (0-65535)
    pub r: u16,
    /// Green channel (0-65535)
    pub g: u16,
    /// Blue channel (0-65535)
    pub b: u16,
}

impl PointXyrgbHighRes {
    pub fn new(x: i16, y: i16, r: u16, g: u16, b: u16) -> Self {
        Self { x, y, r, g, b }
    }
}

impl WriteToBytes for PointXyrgbHighRes {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_i16::<BE>(self.x)?;
        writer.write_i16::<BE>(self.y)?;
        writer.write_u16::<BE>(self.r)?;
        writer.write_u16::<BE>(self.g)?;
        writer.write_u16::<BE>(self.b)?;
        Ok(())
    }
}

impl ReadFromBytes for PointXyrgbHighRes {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let x = reader.read_i16::<BE>()?;
        let y = reader.read_i16::<BE>()?;
        let r = reader.read_u16::<BE>()?;
        let g = reader.read_u16::<BE>()?;
        let b = reader.read_u16::<BE>()?;
        Ok(PointXyrgbHighRes { x, y, r, g, b })
    }
}

impl SizeBytes for PointXyrgbHighRes {
    const SIZE_BYTES: usize = XYRGB_HIGHRES_SAMPLE_SIZE;
}

/// Extended point format (20 bytes).
/// Includes user-defined channels for additional colors or effects.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct PointExtended {
    /// X coordinate (-32768 to 32767)
    pub x: i16,
    /// Y coordinate (-32768 to 32767)
    pub y: i16,
    /// Red channel (0-65535)
    pub r: u16,
    /// Green channel (0-65535)
    pub g: u16,
    /// Blue channel (0-65535)
    pub b: u16,
    /// Intensity channel (0-65535)
    pub i: u16,
    /// User channel 1 (e.g., deep blue)
    pub u1: u16,
    /// User channel 2 (e.g., yellow)
    pub u2: u16,
    /// User channel 3 (e.g., cyan)
    pub u3: u16,
    /// User channel 4 (e.g., x-prime)
    pub u4: u16,
}

impl PointExtended {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        x: i16,
        y: i16,
        r: u16,
        g: u16,
        b: u16,
        i: u16,
        u1: u16,
        u2: u16,
        u3: u16,
        u4: u16,
    ) -> Self {
        Self {
            x,
            y,
            r,
            g,
            b,
            i,
            u1,
            u2,
            u3,
            u4,
        }
    }
}

impl WriteToBytes for PointExtended {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_i16::<BE>(self.x)?;
        writer.write_i16::<BE>(self.y)?;
        writer.write_u16::<BE>(self.r)?;
        writer.write_u16::<BE>(self.g)?;
        writer.write_u16::<BE>(self.b)?;
        writer.write_u16::<BE>(self.i)?;
        writer.write_u16::<BE>(self.u1)?;
        writer.write_u16::<BE>(self.u2)?;
        writer.write_u16::<BE>(self.u3)?;
        writer.write_u16::<BE>(self.u4)?;
        Ok(())
    }
}

impl ReadFromBytes for PointExtended {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let x = reader.read_i16::<BE>()?;
        let y = reader.read_i16::<BE>()?;
        let r = reader.read_u16::<BE>()?;
        let g = reader.read_u16::<BE>()?;
        let b = reader.read_u16::<BE>()?;
        let i = reader.read_u16::<BE>()?;
        let u1 = reader.read_u16::<BE>()?;
        let u2 = reader.read_u16::<BE>()?;
        let u3 = reader.read_u16::<BE>()?;
        let u4 = reader.read_u16::<BE>()?;
        Ok(PointExtended {
            x,
            y,
            r,
            g,
            b,
            i,
            u1,
            u2,
            u3,
            u4,
        })
    }
}

impl SizeBytes for PointExtended {
    const SIZE_BYTES: usize = EXTENDED_SAMPLE_SIZE;
}

// -------------------------------------------------------------------------------------------------
//  Blanket Implementations
// -------------------------------------------------------------------------------------------------

impl<P> WriteToBytes for &P
where
    P: WriteToBytes,
{
    fn write_to_bytes<W: WriteBytesExt>(&self, writer: W) -> io::Result<()> {
        (*self).write_to_bytes(writer)
    }
}

impl<W> WriteBytes for W
where
    W: WriteBytesExt,
{
    fn write_bytes<P: WriteToBytes>(&mut self, protocol: P) -> io::Result<()> {
        protocol.write_to_bytes(self)
    }
}

impl<R> ReadBytes for R
where
    R: ReadBytesExt,
{
    fn read_bytes<P: ReadFromBytes>(&mut self) -> io::Result<P> {
        P::read_from_bytes(self)
    }
}

// -------------------------------------------------------------------------------------------------
//  Point Trait
// -------------------------------------------------------------------------------------------------

/// Trait for laser point types that can be serialized to IDN format.
pub trait Point: WriteToBytes + SizeBytes + Copy {
    /// Get the X coordinate.
    fn x(&self) -> i16;
    /// Get the Y coordinate.
    fn y(&self) -> i16;
}

impl Point for PointXyrgbi {
    fn x(&self) -> i16 {
        self.x
    }
    fn y(&self) -> i16 {
        self.y
    }
}

impl Point for PointXyrgbHighRes {
    fn x(&self) -> i16 {
        self.x
    }
    fn y(&self) -> i16 {
        self.y
    }
}

impl Point for PointExtended {
    fn x(&self) -> i16 {
        self.x
    }
    fn y(&self) -> i16 {
        self.y
    }
}

// -------------------------------------------------------------------------------------------------
//  Channel Descriptors
// -------------------------------------------------------------------------------------------------

/// Channel descriptor values for different point formats.
/// These match the C++ reference implementation exactly.
/// Format: base descriptor followed by precision modifier (0x4010 = 16-bit precision).
pub mod channel_descriptors {
    /// XYRGBI format descriptor (8 bytes per sample)
    /// X, Y with 16-bit precision, then R/G/B at specific wavelengths, plus intensity
    pub const XYRGBI: &[u16] = &[
        0x4200, 0x4010, // X, 16-bit precision
        0x4210, 0x4010, // Y, 16-bit precision
        0x527E, // Red, 638nm
        0x5214, // Green, 532nm
        0x51CC, // Blue, 460nm
        0x5C10, // Intensity, legacy signal
    ];

    /// High-res XYRGB format descriptor (10 bytes per sample)
    /// X, Y, R, G, B all with 16-bit precision
    pub const XYRGB_HIGHRES: &[u16] = &[
        0x4200, 0x4010, // X, 16-bit precision
        0x4210, 0x4010, // Y, 16-bit precision
        0x527E, 0x4010, // Red 638nm, 16-bit precision
        0x5214, 0x4010, // Green 532nm, 16-bit precision
        0x51CC, 0x4010, // Blue 460nm, 16-bit precision
    ];

    /// Extended format descriptor (20 bytes per sample)
    /// X, Y, R, G, B, I, plus 4 user channels, all 16-bit precision
    pub const EXTENDED: &[u16] = &[
        0x4200, 0x4010, // X, 16-bit precision
        0x4210, 0x4010, // Y, 16-bit precision
        0x527E, 0x4010, // Red 638nm, 16-bit precision
        0x5214, 0x4010, // Green 532nm, 16-bit precision
        0x51CC, 0x4010, // Blue 460nm, 16-bit precision
        0x5C10, 0x4010, // Intensity, 16-bit precision
        0x51BD, 0x4010, // User 1 (deep blue 445nm), 16-bit precision
        0x5241, 0x4010, // User 2 (yellow 577nm), 16-bit precision
        0x51E8, 0x4010, // User 3 (cyan 488nm), 16-bit precision
        0x4201, 0x4010, // User 4 (X-prime), 16-bit precision
    ];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LaserPoint;

    // ==========================================================================
    // PointXyrgbi Conversion Tests - Testing the From<&LaserPoint> implementation
    // ==========================================================================

    #[test]
    fn test_idn_conversion_center() {
        // Center point (0, 0) should map to (0, 0)
        // Colors: u16 values that downscale to expected u8 values (128, 64, 32, 200)
        let laser_point = LaserPoint::new(0.0, 0.0, 128 * 257, 64 * 257, 32 * 257, 200 * 257);
        let idn_point: PointXyrgbi = (&laser_point).into();

        assert_eq!(idn_point.x, 0);
        assert_eq!(idn_point.y, 0);
        // IDN PointXyrgbi uses u8 colors (downscaled from u16 via >> 8)
        assert_eq!(idn_point.r, 128);
        assert_eq!(idn_point.g, 64);
        assert_eq!(idn_point.b, 32);
        assert_eq!(idn_point.i, 200);
    }

    #[test]
    fn test_idn_conversion_min() {
        // Min point (-1, -1) should map to (32767, 32767) due to axis inversion
        let laser_point = LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0);
        let idn_point: PointXyrgbi = (&laser_point).into();

        assert_eq!(idn_point.x, 32767);
        assert_eq!(idn_point.y, 32767);
    }

    #[test]
    fn test_idn_conversion_max() {
        // Max point (1, 1) should map to (-32767, -32767) due to axis inversion
        let laser_point = LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535);
        let idn_point: PointXyrgbi = (&laser_point).into();

        assert_eq!(idn_point.x, -32767);
        assert_eq!(idn_point.y, -32767);
        // Max u16 (65535) >> 8 = 255
        assert_eq!(idn_point.r, 255);
        assert_eq!(idn_point.g, 255);
        assert_eq!(idn_point.b, 255);
        assert_eq!(idn_point.i, 255);
    }

    #[test]
    fn test_idn_conversion_half_values() {
        let laser_point = LaserPoint::new(0.5, -0.5, 100 * 257, 100 * 257, 100 * 257, 100 * 257);
        let idn_point: PointXyrgbi = (&laser_point).into();

        // 0.5 * -32767 = -16383.5 -> -16384 (axis inverted)
        assert_eq!(idn_point.x, -16384);
        assert_eq!(idn_point.y, 16384);
    }

    #[test]
    fn test_idn_conversion_clamps_out_of_range() {
        // Out of range values should clamp, then invert
        let laser_point = LaserPoint::new(2.0, -3.0, 65535, 65535, 65535, 65535);
        let idn_point: PointXyrgbi = (&laser_point).into();

        assert_eq!(idn_point.x, -32767);
        assert_eq!(idn_point.y, 32767);
    }

    #[test]
    fn test_idn_coordinate_symmetry() {
        // Verify that x and -x produce symmetric results around 0
        let p1 = LaserPoint::new(0.5, 0.0, 0, 0, 0, 0);
        let p2 = LaserPoint::new(-0.5, 0.0, 0, 0, 0, 0);
        let i1: PointXyrgbi = (&p1).into();
        let i2: PointXyrgbi = (&p2).into();

        assert_eq!(i1.x, -i2.x);
    }

    #[test]
    fn test_idn_conversion_infinity_clamps() {
        let laser_point = LaserPoint::new(f32::INFINITY, f32::NEG_INFINITY, 0, 0, 0, 0);
        let idn_point: PointXyrgbi = (&laser_point).into();

        assert_eq!(idn_point.x, -32767);
        assert_eq!(idn_point.y, 32767);
    }

    // ==========================================================================
    // GroupRequest/GroupResponse Tests
    // ==========================================================================

    #[test]
    fn test_group_request_size() {
        assert_eq!(GroupRequest::SIZE_BYTES, 16);
    }

    #[test]
    fn test_group_response_size() {
        assert_eq!(GroupResponse::SIZE_BYTES, 4);
    }

    #[test]
    fn test_group_request_get_constructor() {
        let req = GroupRequest::get();
        assert_eq!(req.struct_size, 16);
        assert_eq!(req.op_code, IDNVAL_GROUPOP_GETMASK);
        assert_eq!(req.group_mask, 0);
        assert_eq!(req.auth_code, [0u8; 12]);
    }

    #[test]
    fn test_group_request_set_constructor() {
        let req = GroupRequest::set(0xABCD);
        assert_eq!(req.struct_size, 16);
        assert_eq!(req.op_code, IDNVAL_GROUPOP_SETMASK);
        assert_eq!(req.group_mask, 0xABCD);
        assert_eq!(req.auth_code, [0u8; 12]);
    }

    #[test]
    fn test_group_request_set_with_auth() {
        let auth = b"secret123";
        let req = GroupRequest::set_with_auth(0x1234, auth);
        assert_eq!(req.struct_size, 16);
        assert_eq!(req.op_code, IDNVAL_GROUPOP_SETMASK);
        assert_eq!(req.group_mask, 0x1234);
        assert_eq!(&req.auth_code[..9], b"secret123");
        assert_eq!(&req.auth_code[9..], &[0u8; 3]);
    }

    #[test]
    fn test_group_request_roundtrip() {
        let original = GroupRequest::set(0xFFFF);
        let mut buffer = Vec::new();
        original.write_to_bytes(&mut buffer).unwrap();
        assert_eq!(buffer.len(), 16);

        let parsed = GroupRequest::read_from_bytes(&buffer[..]).unwrap();
        assert_eq!(parsed.struct_size, original.struct_size);
        assert_eq!(parsed.op_code, original.op_code);
        assert_eq!(parsed.group_mask, original.group_mask);
        assert_eq!(parsed.auth_code, original.auth_code);
    }

    #[test]
    fn test_group_request_byte_layout() {
        let req = GroupRequest::set(0x1234);
        let mut buffer = Vec::new();
        req.write_to_bytes(&mut buffer).unwrap();

        // Byte 0: struct_size = 16
        assert_eq!(buffer[0], 16);
        // Byte 1: op_code = 0x02 (SETMASK)
        assert_eq!(buffer[1], 0x02);
        // Bytes 2-3: group_mask = 0x1234 (big-endian)
        assert_eq!(buffer[2], 0x12);
        assert_eq!(buffer[3], 0x34);
        // Bytes 4-15: auth_code (all zeros)
        assert_eq!(&buffer[4..16], &[0u8; 12]);
    }

    #[test]
    fn test_group_response_roundtrip() {
        let original = GroupResponse {
            struct_size: 4,
            op_code: IDNVAL_GROUPOP_SUCCESS,
            group_mask: 0x8000,
        };
        let mut buffer = Vec::new();
        original.write_to_bytes(&mut buffer).unwrap();
        assert_eq!(buffer.len(), 4);

        let parsed = GroupResponse::read_from_bytes(&buffer[..]).unwrap();
        assert_eq!(parsed.struct_size, original.struct_size);
        assert_eq!(parsed.op_code, original.op_code);
        assert_eq!(parsed.group_mask, original.group_mask);
    }

    #[test]
    fn test_group_response_byte_layout() {
        let resp = GroupResponse {
            struct_size: 4,
            op_code: IDNVAL_GROUPOP_SUCCESS,
            group_mask: 0xABCD,
        };
        let mut buffer = Vec::new();
        resp.write_to_bytes(&mut buffer).unwrap();

        // Byte 0: struct_size = 4
        assert_eq!(buffer[0], 4);
        // Byte 1: op_code = 0x00 (SUCCESS)
        assert_eq!(buffer[1], 0x00);
        // Bytes 2-3: group_mask = 0xABCD (big-endian)
        assert_eq!(buffer[2], 0xAB);
        assert_eq!(buffer[3], 0xCD);
    }

    #[test]
    fn test_group_response_is_success() {
        let success = GroupResponse {
            struct_size: 4,
            op_code: IDNVAL_GROUPOP_SUCCESS,
            group_mask: 0,
        };
        assert!(success.is_success());

        let error_auth = GroupResponse {
            struct_size: 4,
            op_code: IDNVAL_GROUPOP_ERR_AUTH,
            group_mask: 0,
        };
        assert!(!error_auth.is_success());

        let error_op = GroupResponse {
            struct_size: 4,
            op_code: IDNVAL_GROUPOP_ERR_OPERATION,
            group_mask: 0,
        };
        assert!(!error_op.is_success());

        let error_req = GroupResponse {
            struct_size: 4,
            op_code: IDNVAL_GROUPOP_ERR_REQUEST,
            group_mask: 0,
        };
        assert!(!error_req.is_success());
    }

    #[test]
    fn test_group_response_is_group_enabled() {
        let resp = GroupResponse {
            struct_size: 4,
            op_code: 0,
            group_mask: 0b0000_0000_0000_0101, // Groups 0 and 2 enabled
        };
        assert!(resp.is_group_enabled(0));
        assert!(!resp.is_group_enabled(1));
        assert!(resp.is_group_enabled(2));
        assert!(!resp.is_group_enabled(3));
        assert!(!resp.is_group_enabled(16)); // Out of range
    }

    #[test]
    fn test_group_response_enabled_groups() {
        let resp = GroupResponse {
            struct_size: 4,
            op_code: 0,
            group_mask: 0b1000_0000_0000_0011, // Groups 0, 1, and 15 enabled
        };
        assert_eq!(resp.enabled_groups(), vec![0, 1, 15]);
    }
}
