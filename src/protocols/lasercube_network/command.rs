//! LaserCube network command encoders.

use byteorder::{ByteOrder, LittleEndian};

use super::protocol::{
    CMD_CLEAR_RINGBUFFER, CMD_ENABLE_BUFFER_SIZE_RESPONSE, CMD_GET_FULL_INFO,
    CMD_SET_DAC_BUFFER_THRESHOLD, CMD_SET_OUTPUT, CMD_SET_RATE,
};
use super::status::LaserCubeNetworkStatus;

pub fn get_full_info() -> [u8; 1] {
    [CMD_GET_FULL_INFO]
}

pub fn enable_buffer_size_response(enable: bool) -> [u8; 2] {
    [CMD_ENABLE_BUFFER_SIZE_RESPONSE, u8::from(enable)]
}

pub fn set_output(enable: bool) -> [u8; 2] {
    [CMD_SET_OUTPUT, u8::from(enable)]
}

pub fn set_rate(rate: u32) -> [u8; 5] {
    let mut out = [0; 5];
    out[0] = CMD_SET_RATE;
    LittleEndian::write_u32(&mut out[1..], rate);
    out
}

pub fn clear_ringbuffer() -> [u8; 1] {
    [CMD_CLEAR_RINGBUFFER]
}

pub fn set_dac_buffer_threshold(threshold: u32) -> [u8; 5] {
    let mut out = [0; 5];
    out[0] = CMD_SET_DAC_BUFFER_THRESHOLD;
    LittleEndian::write_u32(&mut out[1..], threshold);
    out
}

pub fn threshold_supported(status: &LaserCubeNetworkStatus) -> bool {
    status.model_number >= 10 || status.firmware_major > 1 || status.firmware_minor > 23
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn encodes_commands_little_endian() {
        assert_eq!(get_full_info(), [0x77]);
        assert_eq!(enable_buffer_size_response(true), [0x78, 0x01]);
        assert_eq!(set_output(false), [0x80, 0x00]);
        assert_eq!(set_rate(30_000), [0x82, 0x30, 0x75, 0x00, 0x00]);
        assert_eq!(clear_ringbuffer(), [0x8D]);
        assert_eq!(
            set_dac_buffer_threshold(1800),
            [0xA0, 0x08, 0x07, 0x00, 0x00]
        );
    }

    #[test]
    fn threshold_guard_is_field_wise() {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(!threshold_supported(&status));
        status.model_number = 10;
        assert!(threshold_supported(&status));
        status.model_number = 0;
        status.firmware_major = 2;
        assert!(threshold_supported(&status));
        status.firmware_major = 0;
        status.firmware_minor = 24;
        assert!(threshold_supported(&status));
    }
}
