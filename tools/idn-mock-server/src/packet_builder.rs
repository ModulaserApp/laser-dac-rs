//! Pure functions for building IDN protocol response packets.

use crate::config::{MockRelay, MockService};
use crate::constants::{
    IDNCMD_PING_RESPONSE, IDNCMD_RT_ACK, IDNCMD_SCAN_RESPONSE, IDNCMD_SERVICEMAP_RESPONSE,
};

/// Build a scan response packet.
pub fn build_scan_response(
    flags: u8,
    sequence: u16,
    unit_id: &[u8; 16],
    hostname: &str,
    protocol_version: u8,
    status: u8,
) -> Vec<u8> {
    let mut response = Vec::with_capacity(44);

    // Packet header (4 bytes)
    response.push(IDNCMD_SCAN_RESPONSE);
    response.push(flags);
    response.extend_from_slice(&sequence.to_be_bytes());

    // ScanResponse (40 bytes)
    response.push(40); // struct_size
    response.push(protocol_version);
    response.push(status);
    response.push(0x00); // reserved
    response.extend_from_slice(unit_id);

    // Hostname (20 bytes, null-padded)
    let mut hostname_bytes = [0u8; 20];
    let name_bytes = hostname.as_bytes();
    let len = name_bytes.len().min(20);
    hostname_bytes[..len].copy_from_slice(&name_bytes[..len]);
    response.extend_from_slice(&hostname_bytes);

    response
}

/// Build a servicemap response packet.
pub fn build_servicemap_response(
    flags: u8,
    sequence: u16,
    services: &[MockService],
    relays: &[MockRelay],
) -> Vec<u8> {
    let relay_count = relays.len() as u8;
    let service_count = services.len() as u8;
    let capacity = 4 + 4 + (relay_count as usize + service_count as usize) * 24;
    let mut response = Vec::with_capacity(capacity);

    // Packet header (4 bytes)
    response.push(IDNCMD_SERVICEMAP_RESPONSE);
    response.push(flags);
    response.extend_from_slice(&sequence.to_be_bytes());

    // ServiceMapResponseHeader (4 bytes)
    response.push(4); // struct_size
    response.push(24); // entry_size
    response.push(relay_count);
    response.push(service_count);

    // Relay entries (24 bytes each)
    for relay in relays {
        response.push(0x00); // service_id (must be 0 for relays)
        response.push(0x00); // service_type (unused for relays)
        response.push(0x00); // flags
        response.push(relay.relay_number);

        // Relay name (20 bytes, null-padded)
        let mut name_bytes = [0u8; 20];
        let name = relay.name.as_bytes();
        let len = name.len().min(20);
        name_bytes[..len].copy_from_slice(&name[..len]);
        response.extend_from_slice(&name_bytes);
    }

    // Service entries (24 bytes each)
    for service in services {
        response.push(service.service_id);
        response.push(service.service_type);
        response.push(service.flags);
        response.push(service.relay_number);

        // Service name (20 bytes, null-padded)
        let mut name_bytes = [0u8; 20];
        let name = service.name.as_bytes();
        let len = name.len().min(20);
        name_bytes[..len].copy_from_slice(&name[..len]);
        response.extend_from_slice(&name_bytes);
    }

    response
}

/// Build a ping response packet.
///
/// Per spec section 3.1.1: "A server SHALL copy the request payload to the
/// response payload. The payload SHALL be an exact copy in content and length."
pub fn build_ping_response(flags: u8, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut response = Vec::with_capacity(4 + payload.len());

    // Packet header (4 bytes)
    response.push(IDNCMD_PING_RESPONSE);
    response.push(flags);
    response.extend_from_slice(&sequence.to_be_bytes());

    // Copy request payload exactly
    response.extend_from_slice(payload);

    response
}

/// Build an ACK response packet.
pub fn build_ack_response(flags: u8, sequence: u16, result_code: u8) -> Vec<u8> {
    build_ack_response_full(flags, sequence, result_code, 0xFF, 0)
}

/// Build an ACK response packet with all fields.
pub fn build_ack_response_full(
    flags: u8,
    sequence: u16,
    result_code: u8,
    link_quality: u8,
    latency_us: u32,
) -> Vec<u8> {
    let mut response = Vec::with_capacity(16);

    // Packet header (4 bytes)
    response.push(IDNCMD_RT_ACK);
    response.push(flags);
    response.extend_from_slice(&sequence.to_be_bytes());

    // Acknowledgement header (12 bytes)
    response.push(12); // struct_size
    response.push(result_code);
    response.push(0x00); // input_event_flags (high byte, network byte order)
    response.push(0x00); // input_event_flags (low byte)
    response.push(0x00); // pipeline_event_flags (high byte, network byte order)
    response.push(0x00); // pipeline_event_flags (low byte)
    response.push(0x00); // status_flags
    response.push(link_quality);
    response.extend_from_slice(&latency_us.to_be_bytes());

    response
}

/// Build a parameter response packet.
///
/// Used for responding to UNIT_PARAMS_REQUEST and SERVICE_PARAMS_REQUEST.
pub fn build_parameter_response(
    flags: u8,
    sequence: u16,
    response_cmd: u8,
    result_code: i8,
    service_id: u8,
    param_id: u16,
    value: u32,
) -> Vec<u8> {
    let mut response = Vec::with_capacity(12);

    // Packet header (4 bytes)
    response.push(response_cmd);
    response.push(flags);
    response.extend_from_slice(&sequence.to_be_bytes());

    // ParameterResponse (8 bytes): service_id, result_code, param_id (BE), value (BE)
    response.push(service_id);
    response.push(result_code as u8);
    response.extend_from_slice(&param_id.to_be_bytes());
    response.extend_from_slice(&value.to_be_bytes());

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::IDNVAL_STYPE_LAPRO;

    #[test]
    fn test_build_scan_response() {
        let unit_id = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let response = build_scan_response(0x00, 0x1234, &unit_id, "TestHost", 0x10, 0x01);

        assert_eq!(response.len(), 44);
        assert_eq!(response[0], IDNCMD_SCAN_RESPONSE);
        assert_eq!(response[1], 0x00); // flags
        assert_eq!(response[2..4], [0x12, 0x34]); // sequence
        assert_eq!(response[4], 40); // struct_size
        assert_eq!(response[5], 0x10); // protocol_version
        assert_eq!(response[6], 0x01); // status
        assert_eq!(response[8..24], unit_id);
        assert_eq!(&response[24..32], b"TestHost");
    }

    #[test]
    fn test_build_servicemap_response() {
        let services = vec![MockService::laser_projector(1, "Laser1")];
        let response = build_servicemap_response(0x00, 0x5678, &services, &[]);

        assert_eq!(response.len(), 32); // 4 header + 4 servicemap header + 24 service
        assert_eq!(response[0], IDNCMD_SERVICEMAP_RESPONSE);
        assert_eq!(response[6], 0); // relay_count
        assert_eq!(response[7], 1); // service_count
        assert_eq!(response[8], 1); // service_id
        assert_eq!(response[9], IDNVAL_STYPE_LAPRO); // service_type
    }

    #[test]
    fn test_build_ping_response() {
        let payload = [0x11, 0x22, 0x33, 0x44];
        let response = build_ping_response(0x01, 0xABCD, &payload);

        assert_eq!(response.len(), 8); // 4-byte header + 4-byte payload
        assert_eq!(response[0], IDNCMD_PING_RESPONSE);
        assert_eq!(response[1], 0x01);
        assert_eq!(response[2..4], [0xAB, 0xCD]);
        // Verify payload is copied exactly
        assert_eq!(&response[4..8], &payload);
    }

    #[test]
    fn test_build_ping_response_empty_payload() {
        let response = build_ping_response(0x00, 0x1234, &[]);

        assert_eq!(response.len(), 4); // header only
        assert_eq!(response[0], IDNCMD_PING_RESPONSE);
    }

    #[test]
    fn test_build_ack_response() {
        let response = build_ack_response(0x00, 0x1111, 0x00);

        assert_eq!(response.len(), 16);
        assert_eq!(response[0], IDNCMD_RT_ACK);
        assert_eq!(response[4], 12); // struct_size
        assert_eq!(response[5], 0x00); // result_code (success)
        assert_eq!(response[11], 0xFF); // link_quality
    }
}
