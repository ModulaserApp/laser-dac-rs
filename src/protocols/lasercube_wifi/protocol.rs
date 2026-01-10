//! Low-level protocol types and constants for LaserCube WiFi DAC communication.

use byteorder::{ByteOrder, LittleEndian, ReadBytesExt, WriteBytesExt};
use std::io;

use crate::types::LaserPoint;

// Network ports
/// Keep-alive / heartbeat port (not actively used).
pub const ALIVE_PORT: u16 = 45456;
/// Command and control port.
pub const CMD_PORT: u16 = 45457;
/// Point data transmission port.
pub const DATA_PORT: u16 = 45458;

// Command bytes
/// Request device info (used for discovery).
pub const CMD_GET_FULL_INFO: u8 = 0x77;
/// Enable buffer size responses on data packets.
pub const CMD_ENABLE_BUFFER_SIZE_RESPONSE: u8 = 0x78;
/// Enable or disable laser output.
pub const CMD_SET_OUTPUT: u8 = 0x80;
/// Set the playback rate in Hz.
pub const CMD_SET_RATE: u8 = 0x82;
/// Query free buffer space (response via data port).
pub const CMD_GET_RINGBUFFER_EMPTY: u8 = 0x8A;
/// Clear the internal ring buffer.
pub const CMD_CLEAR_RINGBUFFER: u8 = 0x8D;
/// Send point/sample data.
pub const CMD_SAMPLE_DATA: u8 = 0xA9;

/// Maximum number of points per UDP packet (to fit within MTU).
pub const MAX_POINTS_PER_PACKET: usize = 140;

/// Size of a single point in bytes.
pub const POINT_SIZE_BYTES: usize = 10;

/// Size of the data packet header (command + reserved + message_number + frame_number).
pub const DATA_HEADER_SIZE: usize = 4;

/// Default buffer size for LaserCube devices.
pub const DEFAULT_BUFFER_CAPACITY: u16 = 6000;

/// A trait for writing protocol types to little-endian bytes.
pub trait WriteBytes {
    fn write_bytes<P: WriteToBytes>(&mut self, protocol: P) -> io::Result<()>;
}

/// A trait for reading protocol types from little-endian bytes.
pub trait ReadBytes {
    fn read_bytes<P: ReadFromBytes>(&mut self) -> io::Result<P>;
}

/// Protocol types that may be written to little-endian bytes.
pub trait WriteToBytes {
    fn write_to_bytes<W: WriteBytesExt>(&self, writer: W) -> io::Result<()>;
}

/// Protocol types that may be read from little-endian bytes.
pub trait ReadFromBytes: Sized {
    fn read_from_bytes<R: ReadBytesExt>(reader: R) -> io::Result<Self>;
}

/// Types that have a constant size when written to or read from bytes.
pub trait SizeBytes {
    const SIZE_BYTES: usize;
}

/// A laser point with position and color.
///
/// Coordinates use offset binary encoding where 0x8000 is the center (origin).
/// - X: 0x0000 = left edge, 0x8000 = center, 0xFFFF = right edge
/// - Y: 0x0000 = bottom edge, 0x8000 = center, 0xFFFF = top edge
/// - Colors: 12-bit values (0-4095 typical, stored in u16)
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Point {
    /// X coordinate (offset binary, 0x8000 = center).
    pub x: u16,
    /// Y coordinate (offset binary, 0x8000 = center).
    pub y: u16,
    /// Red intensity (12-bit, 0-4095).
    pub r: u16,
    /// Green intensity (12-bit, 0-4095).
    pub g: u16,
    /// Blue intensity (12-bit, 0-4095).
    pub b: u16,
}

impl Point {
    /// The center coordinate value in offset binary encoding.
    pub const CENTER: u16 = 0x8000;

    /// Create a new point at the center with no color (blanked).
    pub fn blank() -> Self {
        Self {
            x: Self::CENTER,
            y: Self::CENTER,
            r: 0,
            g: 0,
            b: 0,
        }
    }

    /// Create a new point from signed coordinates (-32768 to 32767) and 12-bit colors.
    ///
    /// This converts from the typical signed laser coordinate system to offset binary.
    pub fn from_signed(x: i16, y: i16, r: u16, g: u16, b: u16) -> Self {
        Self {
            x: (x as i32 + 0x8000) as u16,
            y: (y as i32 + 0x8000) as u16,
            r,
            g,
            b,
        }
    }

    /// Convert this point's coordinates to signed values (-32768 to 32767).
    pub fn to_signed(&self) -> (i16, i16) {
        let x = (self.x as i32 - 0x8000) as i16;
        let y = (self.y as i32 - 0x8000) as i16;
        (x, y)
    }
}

impl WriteToBytes for Point {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u16::<LittleEndian>(self.x)?;
        writer.write_u16::<LittleEndian>(self.y)?;
        writer.write_u16::<LittleEndian>(self.r)?;
        writer.write_u16::<LittleEndian>(self.g)?;
        writer.write_u16::<LittleEndian>(self.b)?;
        Ok(())
    }
}

impl ReadFromBytes for Point {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let x = reader.read_u16::<LittleEndian>()?;
        let y = reader.read_u16::<LittleEndian>()?;
        let r = reader.read_u16::<LittleEndian>()?;
        let g = reader.read_u16::<LittleEndian>()?;
        let b = reader.read_u16::<LittleEndian>()?;
        Ok(Point { x, y, r, g, b })
    }
}

impl SizeBytes for Point {
    const SIZE_BYTES: usize = POINT_SIZE_BYTES;
}

impl From<&LaserPoint> for Point {
    /// Convert a LaserPoint to a LaserCube WiFi Point.
    ///
    /// LaserPoint uses f32 coordinates (-1.0 to 1.0) and u16 colors (0-65535).
    /// LaserCube WiFi uses 12-bit coordinates (0-4095) with inverted axes, and 12-bit colors.
    fn from(p: &LaserPoint) -> Self {
        // Map [-1..1] -> [0..1] -> invert -> [0..4095]
        let x = ((1.0 - (p.x + 1.0) / 2.0).clamp(0.0, 1.0) * 4095.0) as u16;
        let y = ((1.0 - (p.y + 1.0) / 2.0).clamp(0.0, 1.0) * 4095.0) as u16;

        Point {
            x,
            y,
            r: p.r >> 4,
            g: p.g >> 4,
            b: p.b >> 4,
        }
    }
}

/// Device information received during discovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Protocol version.
    pub version: u8,
    /// Maximum buffer capacity for points.
    pub max_buffer_space: u16,
}

impl DeviceInfo {
    /// Parse device info from a discovery response buffer.
    ///
    /// Expected buffer layout:
    /// - Offset 0: Command echo (0x77)
    /// - Offset 2: Version byte
    /// - Offset 21-23: max_buffer_space (u16 LE)
    /// - Offset 26-32: Serial number (6 bytes, hex encoded)
    pub fn from_discovery_response(buffer: &[u8]) -> io::Result<Self> {
        if buffer.len() < 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "discovery response too short: {} bytes, expected at least 32",
                    buffer.len()
                ),
            ));
        }

        // Check command echo
        if buffer[0] != CMD_GET_FULL_INFO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "unexpected command in discovery response: 0x{:02X}",
                    buffer[0]
                ),
            ));
        }

        let version = buffer[2];
        let max_buffer_space = LittleEndian::read_u16(&buffer[21..23]);

        Ok(DeviceInfo {
            version,
            max_buffer_space,
        })
    }
}

/// Buffer status response received on the data port.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BufferStatus {
    /// Number of free sample slots in the device buffer.
    pub free_space: u16,
}

impl BufferStatus {
    /// Parse buffer status from a response buffer.
    ///
    /// Expected layout:
    /// - Offset 0: Command (0x8A)
    /// - Offset 2-4: free_space (u16 LE)
    pub fn from_response(buffer: &[u8]) -> io::Result<Self> {
        if buffer.len() < 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "buffer status response too short: {} bytes, expected at least 4",
                    buffer.len()
                ),
            ));
        }

        if buffer[0] != CMD_GET_RINGBUFFER_EMPTY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected command in buffer status: 0x{:02X}", buffer[0]),
            ));
        }

        let free_space = LittleEndian::read_u16(&buffer[2..4]);
        Ok(BufferStatus { free_space })
    }
}

/// Commands that can be sent to the LaserCube.
pub mod command {
    use super::*;

    /// Build a GET_FULL_INFO command for discovery.
    pub fn get_full_info() -> [u8; 1] {
        [CMD_GET_FULL_INFO]
    }

    /// Build an ENABLE_BUFFER_SIZE_RESPONSE command.
    pub fn enable_buffer_size_response(enable: bool) -> [u8; 2] {
        [CMD_ENABLE_BUFFER_SIZE_RESPONSE, u8::from(enable)]
    }

    /// Build a SET_OUTPUT command.
    pub fn set_output(enable: bool) -> [u8; 2] {
        [CMD_SET_OUTPUT, u8::from(enable)]
    }

    /// Build a SET_RATE command.
    pub fn set_rate(rate: u32) -> [u8; 5] {
        let mut buf = [0u8; 5];
        buf[0] = CMD_SET_RATE;
        LittleEndian::write_u32(&mut buf[1..5], rate);
        buf
    }

    /// Build a CLEAR_RINGBUFFER command.
    pub fn clear_ringbuffer() -> [u8; 1] {
        [CMD_CLEAR_RINGBUFFER]
    }

    /// Build a sample data packet header.
    ///
    /// Returns (header, points_offset) where points should be written starting at points_offset.
    pub fn sample_data_header(message_number: u8, frame_number: u8) -> [u8; DATA_HEADER_SIZE] {
        [CMD_SAMPLE_DATA, 0x00, message_number, frame_number]
    }
}

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
