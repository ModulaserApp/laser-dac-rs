//! Types and constants that precisely match the Ether Dream protocol specification.

use byteorder::{ReadBytesExt, WriteBytesExt, LE};
use std::io;

use crate::types::LaserPoint;

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
    /// LaserPoint uses f32 coordinates (-1.0 to 1.0) and u8 colors.
    /// Ether Dream uses i16 signed coordinates and u16 colors.
    fn from(p: &LaserPoint) -> Self {
        let x = (p.x.clamp(-1.0, 1.0) * 32767.0) as i16;
        let y = (p.y.clamp(-1.0, 1.0) * 32767.0) as i16;
        let r = (p.r as u16) * 257;
        let g = (p.g as u16) * 257;
        let b = (p.b as u16) * 257;

        DacPoint {
            control: 0,
            x,
            y,
            r,
            g,
            b,
            i: (p.intensity as u16) * 257,
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
            mut n_points: u16,
            points: &mut Vec<DacPoint>,
        ) -> io::Result<()> {
            while n_points > 0 {
                let dac_point = reader.read_bytes::<DacPoint>()?;
                points.push(dac_point);
                n_points -= 1;
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

    impl WriteToBytes for PrepareStream {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)
        }
    }

    impl WriteToBytes for Begin {
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

    impl WriteToBytes for Stop {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)
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

    impl WriteToBytes for ClearEmergencyStop {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)
        }
    }

    impl WriteToBytes for Ping {
        fn write_to_bytes<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
            writer.write_u8(Self::START_BYTE)
        }
    }

    impl ReadFromBytes for PrepareStream {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            let command = reader.read_u8()?;
            if command != Self::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Ok(PrepareStream)
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

    impl ReadFromBytes for Stop {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            if reader.read_u8()? != Self::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Ok(Stop)
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

    impl ReadFromBytes for ClearEmergencyStop {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            if reader.read_u8()? != Self::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Ok(ClearEmergencyStop)
        }
    }

    impl ReadFromBytes for Ping {
        fn read_from_bytes<R: ReadBytesExt>(mut reader: R) -> io::Result<Self> {
            if reader.read_u8()? != Self::START_BYTE {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid command",
                ));
            }
            Ok(Ping)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LaserPoint;

    // ==========================================================================
    // DacPoint Conversion Tests
    // These test the From<&LaserPoint> implementation which handles:
    // - 16-bit signed coordinate conversion (f32 -1..1 to i16 -32767..32767)
    // - Color scaling from u8 (0-255) to u16 (0-65535) using *257
    // - Out-of-range clamping
    // ==========================================================================

    #[test]
    fn test_ether_dream_conversion_center() {
        // Center point (0, 0) should map to (0, 0)
        let laser_point = LaserPoint::new(0.0, 0.0, 128, 64, 32, 200);
        let dac_point: DacPoint = (&laser_point).into();

        assert_eq!(dac_point.x, 0);
        assert_eq!(dac_point.y, 0);
        // Colors: u8 * 257 scales 0-255 to 0-65535
        assert_eq!(dac_point.r, 128 * 257);
        assert_eq!(dac_point.g, 64 * 257);
        assert_eq!(dac_point.b, 32 * 257);
        assert_eq!(dac_point.i, 200 * 257);
    }

    #[test]
    fn test_ether_dream_conversion_boundaries() {
        // Min point (-1, -1) should map to (-32767, -32767)
        let min = LaserPoint::new(-1.0, -1.0, 0, 0, 0, 0);
        let min_dac: DacPoint = (&min).into();
        assert_eq!(min_dac.x, -32767);
        assert_eq!(min_dac.y, -32767);

        // Max point (1, 1) should map to (32767, 32767)
        let max = LaserPoint::new(1.0, 1.0, 255, 255, 255, 255);
        let max_dac: DacPoint = (&max).into();
        assert_eq!(max_dac.x, 32767);
        assert_eq!(max_dac.y, 32767);
    }

    #[test]
    fn test_ether_dream_conversion_clamps_out_of_range() {
        // Out of range values should clamp
        let laser_point = LaserPoint::new(2.0, -3.0, 255, 255, 255, 255);
        let dac_point: DacPoint = (&laser_point).into();

        assert_eq!(dac_point.x, 32767);
        assert_eq!(dac_point.y, -32767);
    }

    #[test]
    fn test_ether_dream_color_scaling_uses_257() {
        // 257 = 0x101, so 0xFF * 257 = 0xFFFF (perfect u8->u16 expansion)
        let laser_point = LaserPoint::new(0.0, 0.0, 0, 127, 255, 1);
        let dac_point: DacPoint = (&laser_point).into();

        assert_eq!(dac_point.r, 0);
        assert_eq!(dac_point.g, 127 * 257); // 32639
        assert_eq!(dac_point.b, 255 * 257); // 65535 (max u16)
        assert_eq!(dac_point.i, 1 * 257); // 257
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

        assert_eq!(dac_point.x, 32767);
        assert_eq!(dac_point.y, -32767);
    }
}
