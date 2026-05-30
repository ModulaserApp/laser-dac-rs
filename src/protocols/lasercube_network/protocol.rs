//! Shared LaserCube network wire constants and point conversion.

use byteorder::{LittleEndian, WriteBytesExt};
use std::io;

use crate::point::LaserPoint;

pub const ALIVE_PORT: u16 = 45456;
pub const CMD_PORT: u16 = 45457;
pub const DATA_PORT: u16 = 45458;

pub const CMD_ALIVE: u8 = 0x27;
pub const CMD_GET_FULL_INFO: u8 = 0x77;
pub const CMD_ENABLE_BUFFER_SIZE_RESPONSE: u8 = 0x78;
pub const CMD_SET_OUTPUT: u8 = 0x80;
pub const CMD_SET_RATE: u8 = 0x82;
pub const CMD_GET_RINGBUFFER_EMPTY: u8 = 0x8A;
pub const CMD_SET_DAC_BUFFER_THRESHOLD: u8 = 0xA0;
pub const CMD_SAMPLE_DATA: u8 = 0xA9;

pub const DEFAULT_POINT_RATE: u32 = 30_000;
pub const DEFAULT_BUFFER_CAPACITY: u16 = 6000;
pub const DATA_HEADER_SIZE: usize = 4;
pub const POINT_SIZE_BYTES: usize = 10;
pub const MAX_UDP_SAMPLES_PER_PACKET: usize = 140;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Point {
    pub x: u16,
    pub y: u16,
    pub r: u16,
    pub g: u16,
    pub b: u16,
}

impl Point {
    #[cfg(test)]
    pub const CENTER: u16 = 2047;

    #[cfg(test)]
    pub fn blank() -> Self {
        Self {
            x: Self::CENTER,
            y: Self::CENTER,
            r: 0,
            g: 0,
            b: 0,
        }
    }

    pub fn write_to<W: WriteBytesExt>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u16::<LittleEndian>(self.x)?;
        writer.write_u16::<LittleEndian>(self.y)?;
        writer.write_u16::<LittleEndian>(self.r)?;
        writer.write_u16::<LittleEndian>(self.g)?;
        writer.write_u16::<LittleEndian>(self.b)?;
        Ok(())
    }
}

impl From<&LaserPoint> for Point {
    fn from(p: &LaserPoint) -> Self {
        Self {
            x: LaserPoint::coord_to_u12(p.x),
            y: LaserPoint::coord_to_u12_inverted(p.y),
            r: LaserPoint::color_to_u12(p.r),
            g: LaserPoint::color_to_u12(p.g),
            b: LaserPoint::color_to_u12(p.b),
        }
    }
}
