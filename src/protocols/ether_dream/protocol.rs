//! Types and constants that precisely match the Ether Dream protocol specification.

use byteorder::{ReadBytesExt, WriteBytesExt, LE};
use std::io;

use crate::point::LaserPoint;

pub use self::command::Command;

/// Communication with the DAC happens over TCP on port 7765.
pub const COMMUNICATION_PORT: u16 = 7765;

/// The DAC sends UDP broadcast messages on port 7654.
pub const BROADCAST_PORT: u16 = 7654;

/// A trait for writing any of the Ether Dream protocol types to little-endian bytes.
pub trait WriteBytes {
    fn write_bytes<P: WriteToBytes>(&mut self, protocol: P) -> io::Result<()>;
}

/// A trait for reading any of the Ether Dream protocol types from little-endian bytes.
pub trait ReadBytes {
    fn read_bytes<P: ReadFromBytes>(&mut self) -> io::Result<P>;
}

/// Protocol types that may be written to little endian bytes.
pub trait WriteToBytes {
    fn write_to_bytes<W: WriteBytesExt>(&self, writer: W) -> io::Result<()>;
}

/// Protocol types that may be read from little endian bytes.
pub trait ReadFromBytes: Sized {
    fn read_from_bytes<R: ReadBytesExt>(reader: R) -> io::Result<Self>;
}

/// Types that have a constant size when written to or read from bytes.
pub trait SizeBytes {
    const SIZE_BYTES: usize;
}

/// Periodically, and as part of ACK packets, the DAC sends its current playback status.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DacStatus {
    pub protocol: u8,
    pub light_engine_state: u8,
    pub playback_state: u8,
    pub source: u8,
    pub light_engine_flags: u16,
    pub playback_flags: u16,
    pub source_flags: u16,
    pub buffer_fullness: u16,
    pub point_rate: u32,
    pub point_count: u32,
}

/// Each DAC broadcasts a status/ID datagram over UDP once per second.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DacBroadcast {
    pub mac_address: [u8; 6],
    pub hw_revision: u16,
    pub sw_revision: u16,
    pub buffer_capacity: u16,
    pub max_point_rate: u32,
    pub dac_status: DacStatus,
}

/// A point with position and color values.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DacPoint {
    pub control: u16,
    pub x: i16,
    pub y: i16,
    pub r: u16,
    pub g: u16,
    pub b: u16,
    pub i: u16,
    pub u1: u16,
    pub u2: u16,
}

/// A response from a DAC.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct DacResponse {
    pub response: u8,
    pub command: u8,
    pub dac_status: DacStatus,
}

impl DacStatus {
    pub const LIGHT_ENGINE_READY: u8 = 0;
    pub const LIGHT_ENGINE_WARMUP: u8 = 1;
    pub const LIGHT_ENGINE_COOLDOWN: u8 = 2;
    pub const LIGHT_ENGINE_EMERGENCY_STOP: u8 = 3;

    pub const PLAYBACK_IDLE: u8 = 0;
    pub const PLAYBACK_PREPARED: u8 = 1;
    pub const PLAYBACK_PLAYING: u8 = 2;

    pub const SOURCE_NETWORK_STREAMING: u8 = 0;
    pub const SOURCE_ILDA_PLAYBACK_SD: u8 = 1;
    pub const SOURCE_INTERNAL_ABSTRACT_GENERATOR: u8 = 2;
}

impl DacResponse {
    pub const ACK: u8 = 0x61;
    pub const NAK_FULL: u8 = 0x46;
    pub const NAK_INVALID: u8 = 0x49;
    pub const NAK_STOP_CONDITION: u8 = 0x21;
}

impl WriteToBytes for DacStatus {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.protocol)?;
        writer.write_u8(self.light_engine_state)?;
        writer.write_u8(self.playback_state)?;
        writer.write_u8(self.source)?;
        writer.write_u16::<LE>(self.light_engine_flags)?;
        writer.write_u16::<LE>(self.playback_flags)?;
        writer.write_u16::<LE>(self.source_flags)?;
        writer.write_u16::<LE>(self.buffer_fullness)?;
        writer.write_u32::<LE>(self.point_rate)?;
        writer.write_u32::<LE>(self.point_count)?;
        Ok(())
    }
}

impl WriteToBytes for DacBroadcast {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        for &byte in &self.mac_address {
            writer.write_u8(byte)?;
        }
        writer.write_u16::<LE>(self.hw_revision)?;
        writer.write_u16::<LE>(self.sw_revision)?;
        writer.write_u16::<LE>(self.buffer_capacity)?;
        writer.write_u32::<LE>(self.max_point_rate)?;
        writer.write_bytes(self.dac_status)?;
        Ok(())
    }
}

impl WriteToBytes for DacPoint {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u16::<LE>(self.control)?;
        writer.write_i16::<LE>(self.x)?;
        writer.write_i16::<LE>(self.y)?;
        writer.write_u16::<LE>(self.r)?;
        writer.write_u16::<LE>(self.g)?;
        writer.write_u16::<LE>(self.b)?;
        writer.write_u16::<LE>(self.i)?;
        writer.write_u16::<LE>(self.u1)?;
        writer.write_u16::<LE>(self.u2)?;
        Ok(())
    }
}

impl WriteToBytes for DacResponse {
    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u8(self.response)?;
        writer.write_u8(self.command)?;
        writer.write_bytes(self.dac_status)?;
        Ok(())
    }
}

impl ReadFromBytes for DacStatus {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        Ok(DacStatus {
            protocol: reader.read_u8()?,
            light_engine_state: reader.read_u8()?,
            playback_state: reader.read_u8()?,
            source: reader.read_u8()?,
            light_engine_flags: reader.read_u16::<LE>()?,
            playback_flags: reader.read_u16::<LE>()?,
            source_flags: reader.read_u16::<LE>()?,
            buffer_fullness: reader.read_u16::<LE>()?,
            point_rate: reader.read_u32::<LE>()?,
            point_count: reader.read_u32::<LE>()?,
        })
    }
}

impl ReadFromBytes for DacBroadcast {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        let mac_address = [
            reader.read_u8()?,
            reader.read_u8()?,
            reader.read_u8()?,
            reader.read_u8()?,
            reader.read_u8()?,
            reader.read_u8()?,
        ];
        Ok(DacBroadcast {
            mac_address,
            hw_revision: reader.read_u16::<LE>()?,
            sw_revision: reader.read_u16::<LE>()?,
            buffer_capacity: reader.read_u16::<LE>()?,
            max_point_rate: reader.read_u32::<LE>()?,
            dac_status: reader.read_bytes::<DacStatus>()?,
        })
    }
}

impl ReadFromBytes for DacPoint {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        Ok(DacPoint {
            control: reader.read_u16::<LE>()?,
            x: reader.read_i16::<LE>()?,
            y: reader.read_i16::<LE>()?,
            r: reader.read_u16::<LE>()?,
            g: reader.read_u16::<LE>()?,
            b: reader.read_u16::<LE>()?,
            i: reader.read_u16::<LE>()?,
            u1: reader.read_u16::<LE>()?,
            u2: reader.read_u16::<LE>()?,
        })
    }
}

impl ReadFromBytes for DacResponse {
    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
        Ok(DacResponse {
            response: reader.read_u8()?,
            command: reader.read_u8()?,
            dac_status: reader.read_bytes::<DacStatus>()?,
        })
    }
}

impl SizeBytes for DacStatus {
    const SIZE_BYTES: usize = 20;
}

impl SizeBytes for DacBroadcast {
    const SIZE_BYTES: usize = DacStatus::SIZE_BYTES + 16;
}

impl SizeBytes for DacPoint {
    const SIZE_BYTES: usize = 18;
}

impl From<&LaserPoint> for DacPoint {
    /// Convert a LaserPoint to an Ether Dream DacPoint.
    ///
    /// LaserPoint uses f32 coordinates (-1.0 to 1.0) and u16 colors (0-65535).
    /// Ether Dream uses i16 signed coordinates and u16 colors (direct mapping).
    ///
    /// # Coordinate convention
    ///
    /// **Both** the X and Y axes are inverted here (via
    /// `LaserPoint::coord_to_i16_inverted`). This is a deliberate
    /// crate-wide orientation choice: relative to stock Ether Dream tooling
    /// (which maps the crate's `+1.0` to `+32767` on both axes), output is
    /// rotated 180°. Content authored against this crate's convention will
    /// therefore appear upside-down/mirrored on tools that assume the stock
    /// mapping, and vice-versa. Changing this would silently flip every
    /// existing show, so it is intentionally fixed rather than configurable.
    fn from(p: &LaserPoint) -> Self {
        DacPoint {
            control: 0,
            x: LaserPoint::coord_to_i16_inverted(p.x),
            y: LaserPoint::coord_to_i16_inverted(p.y),
            r: p.r,
            g: p.g,
            b: p.b,
            i: p.intensity,
            u1: 0,
            u2: 0,
        }
    }
}

impl SizeBytes for DacResponse {
    const SIZE_BYTES: usize = DacStatus::SIZE_BYTES + 2;
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

/// Commands that can be sent to the DAC.
pub mod command {
    use super::{DacPoint, ReadBytes, ReadFromBytes, SizeBytes, WriteBytes, WriteToBytes};
    use byteorder::{ReadBytesExt, WriteBytesExt, LE};
    use std::borrow::Cow;
    use std::io;

    /// Types that may be submitted as commands to the DAC.
    pub trait Command {
        const START_BYTE: u8;
        fn start_byte(&self) -> u8 {
            Self::START_BYTE
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct PrepareStream;

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct Begin {
        pub low_water_mark: u16,
        pub point_rate: u32,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct Update {
        pub low_water_mark: u16,
        pub point_rate: u32,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct PointRate(pub u32);

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    pub struct Data<'a> {
        pub points: Cow<'a, [DacPoint]>,
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct Stop;

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct EmergencyStop;

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct EmergencyStopAlt;

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct ClearEmergencyStop;

    #[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
    pub struct Ping;

    impl Begin {
        pub fn read_fields<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            Ok(Begin {
                low_water_mark: reader.read_u16::<LE>()?,
                point_rate: reader.read_u32::<LE>()?,
            })
        }
    }

    impl Update {
        pub fn read_fields<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            Ok(Update {
                low_water_mark: reader.read_u16::<LE>()?,
                point_rate: reader.read_u32::<LE>()?,
            })
        }
    }

    impl PointRate {
        pub fn read_fields<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            Ok(PointRate(reader.read_u32::<LE>()?))
        }
    }

    impl<'a> Data<'a> {
        pub fn read_n_points<R: ReadBytesExt>(mut reader: R) -> io::Result<u16> {
            reader.read_u16::<LE>()
        }

        pub fn read_points<R: ReadBytesExt>(
            mut reader: R,
            n_points: u16,
            points: &mut Vec<DacPoint>,
        ) -> io::Result<()> {
            for _ in 0..n_points {
                points.push(reader.read_bytes::<DacPoint>()?);
            }
            Ok(())
        }
    }

    impl Data<'static> {
        pub fn read_fields<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            let n_points = Self::read_n_points(&mut reader)?;
            let mut data = Vec::with_capacity(n_points as _);
            Self::read_points(reader, n_points, &mut data)?;
            Ok(Data {
                points: Cow::Owned(data),
            })
        }
    }

    impl<C> Command for &C
    where
        C: Command,
    {
        const START_BYTE: u8 = C::START_BYTE;
    }

    impl Command for PrepareStream {
        const START_BYTE: u8 = 0x70;
    }
    impl Command for Begin {
        const START_BYTE: u8 = 0x62;
    }
    impl Command for Update {
        const START_BYTE: u8 = 0x75;
    }
    impl Command for PointRate {
        const START_BYTE: u8 = 0x74;
    }
    impl<'a> Command for Data<'a> {
        const START_BYTE: u8 = 0x64;
    }
    impl Command for Stop {
        const START_BYTE: u8 = 0x73;
    }
    impl Command for EmergencyStop {
        const START_BYTE: u8 = 0x00;
    }
    impl Command for EmergencyStopAlt {
        const START_BYTE: u8 = 0xff;
    }
    impl Command for ClearEmergencyStop {
        const START_BYTE: u8 = 0x63;
    }
    impl Command for Ping {
        const START_BYTE: u8 = 0x3f;
    }

    impl SizeBytes for PrepareStream {
        const SIZE_BYTES: usize = 1;
    }
    impl SizeBytes for Begin {
        const SIZE_BYTES: usize = 7;
    }
    impl SizeBytes for Update {
        const SIZE_BYTES: usize = 7;
    }
    impl SizeBytes for PointRate {
        const SIZE_BYTES: usize = 5;
    }
    impl SizeBytes for Stop {
        const SIZE_BYTES: usize = 1;
    }
    impl SizeBytes for EmergencyStop {
        const SIZE_BYTES: usize = 1;
    }
    impl SizeBytes for ClearEmergencyStop {
        const SIZE_BYTES: usize = 1;
    }
    impl SizeBytes for Ping {
        const SIZE_BYTES: usize = 1;
    }

    macro_rules! impl_unit_command_bytes {
        ($($cmd:ident),* $(,)?) => {
            $(
                impl WriteToBytes for $cmd {
                    fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
                        writer.write_u8(Self::START_BYTE)
                    }
                }

                impl ReadFromBytes for $cmd {
                    fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
                        if reader.read_u8()? != Self::START_BYTE {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "invalid command",
                            ));
                        }
                        Ok($cmd)
                    }
                }
            )*
        };
    }

    impl_unit_command_bytes!(PrepareStream, Stop, ClearEmergencyStop, Ping);

    impl WriteToBytes for Begin {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)?;
            writer.write_u16::<LE>(self.low_water_mark)?;
            writer.write_u32::<LE>(self.point_rate)?;
            Ok(())
        }
    }

    impl WriteToBytes for Update {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)?;
            writer.write_u16::<LE>(self.low_water_mark)?;
            writer.write_u32::<LE>(self.point_rate)?;
            Ok(())
        }
    }

    impl WriteToBytes for PointRate {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)?;
            writer.write_u32::<LE>(self.0)?;
            Ok(())
        }
    }

    impl<'a> WriteToBytes for Data<'a> {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            if self.points.len() > u16::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "too many points",
                ));
            }
            writer.write_u8(Self::START_BYTE)?;
            writer.write_u16::<LE>(self.points.len() as u16)?;
            for point in self.points.iter() {
                writer.write_bytes(point)?;
            }
            Ok(())
        }
    }

    impl WriteToBytes for EmergencyStop {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)
        }
    }

    impl WriteToBytes for EmergencyStopAlt {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)
        }
    }

    impl ReadFromBytes for Begin {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            if reader.read_u8()? != Self::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Self::read_fields(reader)
        }
    }

    impl ReadFromBytes for Update {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            if reader.read_u8()? != Self::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Self::read_fields(reader)
        }
    }

    impl ReadFromBytes for PointRate {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            if reader.read_u8()? != Self::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Self::read_fields(reader)
        }
    }

    impl ReadFromBytes for Data<'static> {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            if reader.read_u8()? != Self::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Self::read_fields(reader)
        }
    }

    impl ReadFromBytes for EmergencyStop {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            let command = reader.read_u8()?;
            if command != Self::START_BYTE && command != EmergencyStopAlt::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Ok(EmergencyStop)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::point::LaserPoint;

    // ==========================================================================
    // DacPoint Conversion Tests
    // These test the From<&LaserPoint> implementation which handles:
    // - 16-bit signed coordinate conversion (f32 -1..1 to i16 -32767..32767)
    // - Direct u16 color pass-through (no scaling needed)
    // - Out-of-range clamping
    // ==========================================================================

    #[test]
    fn test_ether_dream_conversion_center() {
        // Center point (0, 0) should map to (0, 0)
        let laser_point = LaserPoint::new(0.0, 0.0, 128 * 257, 64 * 257, 32 * 257, 200 * 257);
        let dac_point: DacPoint = (&laser_point).into();

        assert_eq!(dac_point.x, 0);
        assert_eq!(dac_point.y, 0);
        // Colors: direct u16 pass-through
        assert_eq!(dac_point.r, 128 * 257);
        assert_eq!(dac_point.g, 64 * 257);
        assert_eq!(dac_point.b, 32 * 257);
        assert_eq!(dac_point.i, 200 * 257);
    }

    #[test]
    fn test_ether_dream_conversion_boundaries() {
        // Min point (-1, -1) should map to (32767, 32767) due to axis inversion
        let min = LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0);
        let min_dac: DacPoint = (&min).into();
        assert_eq!(min_dac.x, 32767);
        assert_eq!(min_dac.y, 32767);

        // Max point (1, 1) should map to (-32767, -32767) due to axis inversion
        let max = LaserPoint::new(1.0, 1.0, 65535, 65535, 65535, 65535);
        let max_dac: DacPoint = (&max).into();
        assert_eq!(max_dac.x, -32767);
        assert_eq!(max_dac.y, -32767);
    }

    #[test]
    fn test_ether_dream_conversion_clamps_out_of_range() {
        // Out of range values should clamp, then invert
        let laser_point = LaserPoint::new(2.0, -3.0, 65535, 65535, 65535, 65535);
        let dac_point: DacPoint = (&laser_point).into();

        assert_eq!(dac_point.x, -32767);
        assert_eq!(dac_point.y, 32767);
    }

    #[test]
    fn test_ether_dream_color_direct_passthrough() {
        // Colors should pass through directly without scaling
        let laser_point = LaserPoint::new(0.0, 0.0, 0, 32639, 65535, 257);
        let dac_point: DacPoint = (&laser_point).into();

        assert_eq!(dac_point.r, 0);
        assert_eq!(dac_point.g, 32639);
        assert_eq!(dac_point.b, 65535);
        assert_eq!(dac_point.i, 257);
    }

    #[test]
    fn test_ether_dream_coordinate_symmetry() {
        // Verify that x and -x produce symmetric results around 0
        let p1 = LaserPoint::new(0.5, 0.0, 0, 0, 0, 0);
        let p2 = LaserPoint::new(-0.5, 0.0, 0, 0, 0, 0);
        let d1: DacPoint = (&p1).into();
        let d2: DacPoint = (&p2).into();

        assert_eq!(d1.x, -d2.x);
    }

    #[test]
    fn test_ether_dream_conversion_infinity_clamps() {
        let laser_point = LaserPoint::new(f32::INFINITY, f32::NEG_INFINITY, 0, 0, 0, 0);
        let dac_point: DacPoint = (&laser_point).into();

        assert_eq!(dac_point.x, -32767);
        assert_eq!(dac_point.y, 32767);
    }

    // ==========================================================================
    // Wire-format round-trip tests
    //
    // These exercise the WriteToBytes / ReadFromBytes impls end to end and lock
    // in the little-endian layout and byte sizes of every protocol type.
    // ==========================================================================

    fn encode<P: WriteToBytes>(value: P) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.write_bytes(value).expect("encode");
        buf
    }

    fn decode<P: ReadFromBytes>(bytes: &[u8]) -> P {
        let mut slice = bytes;
        slice.read_bytes::<P>().expect("decode")
    }

    fn sample_status() -> DacStatus {
        DacStatus {
            protocol: 1,
            light_engine_state: DacStatus::LIGHT_ENGINE_WARMUP,
            playback_state: DacStatus::PLAYBACK_PLAYING,
            source: DacStatus::SOURCE_ILDA_PLAYBACK_SD,
            light_engine_flags: 0x1234,
            playback_flags: 0x5678,
            source_flags: 0x9abc,
            buffer_fullness: 1799,
            point_rate: 30_000,
            point_count: 123_456,
        }
    }

    #[test]
    fn test_dac_status_roundtrip() {
        let status = sample_status();
        let bytes = encode(status);
        assert_eq!(bytes.len(), DacStatus::SIZE_BYTES);
        assert_eq!(bytes.len(), 20);
        // Verify little-endian ordering of the first multi-byte field.
        assert_eq!(&bytes[4..6], &0x1234u16.to_le_bytes());
        assert_eq!(decode::<DacStatus>(&bytes), status);
    }

    #[test]
    fn test_dac_response_roundtrip() {
        let response = DacResponse {
            response: DacResponse::ACK,
            command: command::Begin::START_BYTE,
            dac_status: sample_status(),
        };
        let bytes = encode(response);
        assert_eq!(bytes.len(), DacResponse::SIZE_BYTES);
        assert_eq!(bytes.len(), 22);
        assert_eq!(bytes[0], DacResponse::ACK);
        assert_eq!(bytes[1], command::Begin::START_BYTE);
        assert_eq!(decode::<DacResponse>(&bytes), response);
    }

    #[test]
    fn test_dac_broadcast_roundtrip() {
        let broadcast = DacBroadcast {
            mac_address: [0x00, 0x11, 0x22, 0x33, 0x44, 0x55],
            hw_revision: 3,
            sw_revision: 4,
            buffer_capacity: 1800,
            max_point_rate: 100_000,
            dac_status: sample_status(),
        };
        let bytes = encode(broadcast);
        assert_eq!(bytes.len(), DacBroadcast::SIZE_BYTES);
        assert_eq!(bytes.len(), 36);
        assert_eq!(&bytes[0..6], &broadcast.mac_address);
        assert_eq!(decode::<DacBroadcast>(&bytes), broadcast);
    }

    #[test]
    fn test_dac_point_roundtrip() {
        let point = DacPoint {
            control: 0x8000,
            x: -12345,
            y: 12345,
            r: 65535,
            g: 32768,
            b: 1,
            i: 4321,
            u1: 7,
            u2: 9,
        };
        let bytes = encode(point);
        assert_eq!(bytes.len(), DacPoint::SIZE_BYTES);
        assert_eq!(bytes.len(), 18);
        assert_eq!(decode::<DacPoint>(&bytes), point);
    }

    #[test]
    fn test_unit_command_roundtrips() {
        use command::{ClearEmergencyStop, Ping, PrepareStream, Stop};

        let prepare = encode(PrepareStream);
        assert_eq!(prepare, vec![PrepareStream::START_BYTE]);
        assert_eq!(decode::<PrepareStream>(&prepare), PrepareStream);

        let stop = encode(Stop);
        assert_eq!(stop, vec![Stop::START_BYTE]);
        assert_eq!(decode::<Stop>(&stop), Stop);

        let ping = encode(Ping);
        assert_eq!(ping, vec![Ping::START_BYTE]);
        assert_eq!(decode::<Ping>(&ping), Ping);

        let clear = encode(ClearEmergencyStop);
        assert_eq!(clear, vec![ClearEmergencyStop::START_BYTE]);
        assert_eq!(decode::<ClearEmergencyStop>(&clear), ClearEmergencyStop);
    }

    #[test]
    fn test_unit_command_rejects_wrong_byte() {
        use command::PrepareStream;
        // A byte that is not the PrepareStream start byte must fail to parse.
        let err = PrepareStream::read_from_bytes(&[0x00u8][..]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_begin_and_update_roundtrip() {
        let begin = command::Begin {
            low_water_mark: 1000,
            point_rate: 30_000,
        };
        let bytes = encode(begin);
        assert_eq!(bytes.len(), command::Begin::SIZE_BYTES);
        assert_eq!(bytes[0], command::Begin::START_BYTE);
        assert_eq!(decode::<command::Begin>(&bytes), begin);

        let update = command::Update {
            low_water_mark: 2000,
            point_rate: 40_000,
        };
        let bytes = encode(update);
        assert_eq!(bytes.len(), command::Update::SIZE_BYTES);
        assert_eq!(bytes[0], command::Update::START_BYTE);
        assert_eq!(decode::<command::Update>(&bytes), update);
    }

    #[test]
    fn test_point_rate_roundtrip() {
        let point_rate = command::PointRate(50_000);
        let bytes = encode(point_rate);
        assert_eq!(bytes.len(), command::PointRate::SIZE_BYTES);
        assert_eq!(bytes[0], command::PointRate::START_BYTE);
        assert_eq!(decode::<command::PointRate>(&bytes), point_rate);
    }

    #[test]
    fn test_data_command_roundtrip() {
        use std::borrow::Cow;
        let points = vec![
            DacPoint {
                control: 0,
                x: 1,
                y: 2,
                r: 3,
                g: 4,
                b: 5,
                i: 6,
                u1: 0,
                u2: 0,
            },
            DacPoint {
                control: 0,
                x: -1,
                y: -2,
                r: 7,
                g: 8,
                b: 9,
                i: 10,
                u1: 0,
                u2: 0,
            },
        ];
        let data = command::Data {
            points: Cow::Owned(points.clone()),
        };
        let bytes = encode(&data);
        // start byte (1) + count (2) + 2 points * 18 bytes.
        assert_eq!(bytes.len(), 1 + 2 + 2 * DacPoint::SIZE_BYTES);
        assert_eq!(bytes[0], command::Data::START_BYTE);
        assert_eq!(&bytes[1..3], &2u16.to_le_bytes());

        let decoded = decode::<command::Data<'static>>(&bytes);
        assert_eq!(decoded.points.as_ref(), points.as_slice());
    }

    #[test]
    fn test_emergency_stop_accepts_both_start_bytes() {
        use command::{EmergencyStop, EmergencyStopAlt};

        let primary = encode(EmergencyStop);
        assert_eq!(primary, vec![EmergencyStop::START_BYTE]);
        assert_eq!(EmergencyStop::START_BYTE, 0x00);

        let alt = encode(EmergencyStopAlt);
        assert_eq!(alt, vec![EmergencyStopAlt::START_BYTE]);
        assert_eq!(EmergencyStopAlt::START_BYTE, 0xff);

        // Both the primary (0x00) and alternate (0xff) bytes decode to EmergencyStop.
        assert_eq!(decode::<EmergencyStop>(&primary), EmergencyStop);
        assert_eq!(decode::<EmergencyStop>(&alt), EmergencyStop);

        // Any other byte is rejected.
        assert!(EmergencyStop::read_from_bytes(&[0x42u8][..]).is_err());
    }

    #[test]
    fn test_command_start_bytes_match_spec() {
        assert_eq!(command::PrepareStream::START_BYTE, b'p');
        assert_eq!(command::Begin::START_BYTE, b'b');
        assert_eq!(command::Update::START_BYTE, b'u');
        assert_eq!(command::PointRate::START_BYTE, 0x74);
        assert_eq!(command::Data::START_BYTE, b'd');
        assert_eq!(command::Stop::START_BYTE, b's');
        assert_eq!(command::ClearEmergencyStop::START_BYTE, b'c');
        assert_eq!(command::Ping::START_BYTE, b'?');
    }

    #[test]
    fn test_data_command_rejects_too_many_points() {
        use std::borrow::Cow;
        // More than u16::MAX points cannot be encoded.
        let points = vec![
            DacPoint {
                control: 0,
                x: 0,
                y: 0,
                r: 0,
                g: 0,
                b: 0,
                i: 0,
                u1: 0,
                u2: 0,
            };
            u16::MAX as usize + 1
        ];
        let data = command::Data {
            points: Cow::Owned(points),
        };
        let mut buf = Vec::new();
        assert!(buf.write_bytes(&data).is_err());
    }
}
