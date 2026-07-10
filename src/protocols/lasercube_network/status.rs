//! LaserCube network full-info status parsing.

use byteorder::{ByteOrder, LittleEndian};
use std::io;
use std::net::IpAddr;

use super::profiles::ConnectionType;
use super::protocol::CMD_GET_FULL_INFO;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaserCubeNetworkStatus {
    /// Authoritative endpoint address from discovery/source socket metadata.
    pub ip: IpAddr,
    /// IP bytes reported by full-info. Diagnostic only; client mode may report
    /// `0.0.0.0` or another non-authoritative value.
    pub reported_ip: Option<[u8; 4]>,
    pub payload_version: u8,
    pub firmware_major: u8,
    pub firmware_minor: u8,
    pub output_enabled: bool,
    pub interlock_enabled: bool,
    pub temperature_warning: bool,
    pub over_temperature: bool,
    pub packet_errors: u8,
    pub point_rate: u32,
    pub point_rate_max: u32,
    pub buffer_free: u16,
    pub buffer_max: u16,
    pub battery_percent: u8,
    pub temperature_c: i8,
    pub raw_connection_type: u8,
    pub connection_type: ConnectionType,
    pub serial_number: String,
    pub model_number: u8,
    pub model_name: String,
}

impl LaserCubeNetworkStatus {
    pub fn minimal(ip: IpAddr) -> Self {
        Self {
            ip,
            reported_ip: None,
            payload_version: 0,
            firmware_major: 0,
            firmware_minor: 0,
            output_enabled: false,
            interlock_enabled: false,
            temperature_warning: false,
            over_temperature: false,
            packet_errors: 0,
            point_rate: 0,
            point_rate_max: 30_000,
            buffer_free: super::protocol::DEFAULT_BUFFER_CAPACITY,
            buffer_max: super::protocol::DEFAULT_BUFFER_CAPACITY,
            battery_percent: 0,
            temperature_c: 0,
            raw_connection_type: 0,
            connection_type: ConnectionType::Unknown(0),
            serial_number: String::new(),
            model_number: 0,
            model_name: String::new(),
        }
    }

    pub fn parse(buffer: &[u8], source_ip: IpAddr) -> io::Result<Self> {
        // Accept payloads of at least 64 bytes; future firmware may append
        // fields after the known layout. We only read the first 64 bytes.
        if buffer.len() < 64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected full-info response length: {}", buffer.len()),
            ));
        }
        if buffer[0] != CMD_GET_FULL_INFO {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unexpected full-info command: 0x{:02X}", buffer[0]),
            ));
        }
        if buffer[1] != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("full-info command status was nonzero: {}", buffer[1]),
            ));
        }
        let payload_version = buffer[2];
        if payload_version != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported full-info payload version: {}", payload_version),
            ));
        }

        let firmware_major = buffer[3];
        let firmware_minor = buffer[4];
        let flags = buffer[5];
        let new_flag_layout = firmware_major > 0 || firmware_minor >= 13;
        let (interlock_enabled, temperature_warning, over_temperature, packet_errors) =
            if new_flag_layout {
                (
                    flags & 0x02 != 0,
                    flags & 0x04 != 0,
                    flags & 0x08 != 0,
                    (flags >> 4) & 0x0f,
                )
            } else {
                (flags & 0x08 != 0, flags & 0x10 != 0, flags & 0x20 != 0, 0)
            };

        let raw_connection_type = buffer[25];
        let model_name_bytes = &buffer[38..64];
        let model_name_len = model_name_bytes
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(model_name_bytes.len());
        let model_name = String::from_utf8_lossy(&model_name_bytes[..model_name_len]).to_string();

        Ok(Self {
            ip: source_ip,
            reported_ip: Some([buffer[32], buffer[33], buffer[34], buffer[35]]),
            payload_version,
            firmware_major,
            firmware_minor,
            output_enabled: flags & 0x01 != 0,
            interlock_enabled,
            temperature_warning,
            over_temperature,
            packet_errors,
            point_rate: LittleEndian::read_u32(&buffer[10..14]),
            point_rate_max: LittleEndian::read_u32(&buffer[14..18]),
            buffer_free: LittleEndian::read_u16(&buffer[19..21]),
            buffer_max: LittleEndian::read_u16(&buffer[21..23]),
            battery_percent: buffer[23],
            temperature_c: buffer[24] as i8,
            raw_connection_type,
            connection_type: ConnectionType::from_status_byte(raw_connection_type),
            serial_number: hex_upper(&buffer[26..32]),
            model_number: buffer[37],
            model_name,
        })
    }

    pub fn is_plugged_in(&self) -> bool {
        self.battery_percent == 255
    }

    pub fn battery_level(&self) -> f32 {
        if self.battery_percent > 100 {
            1.0
        } else {
            self.battery_percent as f32 / 100.0
        }
    }
}

fn hex_upper(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use byteorder::WriteBytesExt;
    use std::net::{IpAddr, Ipv4Addr};

    fn full_info() -> [u8; 64] {
        let mut data = [0u8; 64];
        data[0] = 0x77;
        data[2] = 0;
        data[3] = 1;
        data[4] = 24;
        data[5] = 0b0011_1111;
        (&mut data[10..14])
            .write_u32::<LittleEndian>(30_000)
            .unwrap();
        (&mut data[14..18])
            .write_u32::<LittleEndian>(45_000)
            .unwrap();
        (&mut data[19..21]).write_u16::<LittleEndian>(5000).unwrap();
        (&mut data[21..23]).write_u16::<LittleEndian>(6000).unwrap();
        data[23] = 85;
        data[24] = 31;
        data[25] = 1;
        data[26..32].copy_from_slice(&[1, 2, 3, 4, 5, 6]);
        data[32..36].copy_from_slice(&[10, 0, 0, 9]);
        data[37] = 10;
        data[38..47].copy_from_slice(b"Ultra Mk2");
        data
    }

    #[test]
    fn parses_full_info_and_uses_source_ip() {
        let source_ip = IpAddr::V4(Ipv4Addr::new(192, 168, 7, 5));
        let status = LaserCubeNetworkStatus::parse(&full_info(), source_ip).unwrap();
        assert_eq!(status.ip, source_ip);
        assert_eq!(status.reported_ip, Some([10, 0, 0, 9]));
        assert_eq!(status.firmware_major, 1);
        assert_eq!(status.firmware_minor, 24);
        assert_eq!(status.packet_errors, 3);
        assert_eq!(status.point_rate, 30_000);
        assert_eq!(status.point_rate_max, 45_000);
        assert_eq!(status.buffer_free, 5000);
        assert_eq!(status.buffer_max, 6000);
        assert_eq!(status.battery_percent, 85);
        assert_eq!(status.battery_level(), 0.85);
        assert!(!status.is_plugged_in());
        assert_eq!(status.connection_type, ConnectionType::WifiServer);
        assert_eq!(status.serial_number, "010203040506");
        assert_eq!(status.model_number, 10);
        assert_eq!(status.model_name, "Ultra Mk2");
    }

    #[test]
    fn rejects_bad_full_info_status_and_length() {
        let mut data = full_info();
        data[1] = 1;
        assert!(LaserCubeNetworkStatus::parse(&data, IpAddr::V4(Ipv4Addr::LOCALHOST)).is_err());
        assert!(
            LaserCubeNetworkStatus::parse(&data[..63], IpAddr::V4(Ipv4Addr::LOCALHOST)).is_err()
        );
    }

    #[test]
    fn accepts_payloads_longer_than_64_bytes() {
        let mut data = full_info().to_vec();
        data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        let status = LaserCubeNetworkStatus::parse(&data, IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert_eq!(status.serial_number, "010203040506");
        assert_eq!(status.model_name, "Ultra Mk2");
    }

    #[test]
    fn handles_battery_plugged_in_sentinel() {
        let mut data = full_info();
        data[23] = 255;
        let status = LaserCubeNetworkStatus::parse(&data, IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert!(status.is_plugged_in());
        assert_eq!(status.battery_level(), 1.0);
    }

    #[test]
    fn old_firmware_uses_legacy_status_bits() {
        let mut data = full_info();
        data[3] = 0;
        data[4] = 12;
        data[5] = 0b0011_1001;
        let status = LaserCubeNetworkStatus::parse(&data, IpAddr::V4(Ipv4Addr::LOCALHOST)).unwrap();
        assert!(status.output_enabled);
        assert!(status.interlock_enabled);
        assert!(status.temperature_warning);
        assert!(status.over_temperature);
        assert_eq!(status.packet_errors, 0);
    }
}
