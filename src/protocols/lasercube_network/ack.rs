//! LaserCube network free-buffer response parsing.

use byteorder::{ByteOrder, LittleEndian};
use std::io;

use super::protocol::CMD_GET_RINGBUFFER_EMPTY;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AckSource {
    Command,
    Data,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BufferAck {
    pub source: AckSource,
    pub packet_sequence: Option<u8>,
    pub free_space: u16,
}

pub fn parse_command_ack(buffer: &[u8]) -> io::Result<BufferAck> {
    parse_ack(buffer, AckSource::Command)
}

pub fn parse_data_ack(buffer: &[u8]) -> io::Result<BufferAck> {
    parse_ack(buffer, AckSource::Data)
}

fn parse_ack(buffer: &[u8], source: AckSource) -> io::Result<BufferAck> {
    if buffer.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "LaserCube buffer ACK too short",
        ));
    }
    if buffer[0] != CMD_GET_RINGBUFFER_EMPTY {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unexpected ACK command byte: 0x{:02X}", buffer[0]),
        ));
    }
    if source == AckSource::Command && buffer[1] != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("command ACK status was nonzero: {}", buffer[1]),
        ));
    }
    Ok(BufferAck {
        source,
        packet_sequence: (source == AckSource::Data).then_some(buffer[1]),
        free_space: LittleEndian::read_u16(&buffer[2..4]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_and_data_ack_semantics() {
        assert_eq!(
            parse_command_ack(&[0x8A, 0x00, 0x34, 0x12]).unwrap(),
            BufferAck {
                source: AckSource::Command,
                packet_sequence: None,
                free_space: 0x1234,
            }
        );
        assert_eq!(
            parse_data_ack(&[0x8A, 0x7F, 0x78, 0x56]).unwrap(),
            BufferAck {
                source: AckSource::Data,
                packet_sequence: Some(0x7F),
                free_space: 0x5678,
            }
        );
    }

    #[test]
    fn rejects_nonzero_command_status() {
        assert!(parse_command_ack(&[0x8A, 0x01, 0x00, 0x00]).is_err());
    }
}
