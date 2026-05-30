//! LaserCube network UDP sample packet construction.

use std::io;

use super::protocol::{
    Point, CMD_SAMPLE_DATA, DATA_HEADER_SIZE, MAX_UDP_SAMPLES_PER_PACKET, POINT_SIZE_BYTES,
};

pub fn encode_sample_packet(
    packet_sequence: u8,
    transfer_sequence: u8,
    points: &[Point],
    out: &mut Vec<u8>,
) -> io::Result<()> {
    if points.len() > MAX_UDP_SAMPLES_PER_PACKET {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many LaserCube samples for one UDP packet",
        ));
    }
    out.clear();
    out.reserve(DATA_HEADER_SIZE + points.len() * POINT_SIZE_BYTES);
    out.extend_from_slice(&[CMD_SAMPLE_DATA, 0x00, packet_sequence, transfer_sequence]);
    for point in points {
        point.write_to(&mut *out)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_golden_sample_packet() {
        let points = [Point {
            x: 0x0102,
            y: 0x0304,
            r: 0x0506,
            g: 0x0708,
            b: 0x090A,
        }];
        let mut out = Vec::new();
        encode_sample_packet(0x11, 0x22, &points, &mut out).unwrap();
        assert_eq!(
            out,
            vec![
                0xA9, 0x00, 0x11, 0x22, 0x02, 0x01, 0x04, 0x03, 0x06, 0x05, 0x08, 0x07, 0x0A, 0x09,
            ]
        );
    }

    #[test]
    fn rejects_packets_above_wire_limit() {
        let points = vec![Point::blank(); MAX_UDP_SAMPLES_PER_PACKET + 1];
        let mut out = Vec::new();
        assert!(encode_sample_packet(0, 0, &points, &mut out).is_err());
    }
}
