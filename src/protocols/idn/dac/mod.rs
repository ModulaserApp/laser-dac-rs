//! DAC-related types and abstractions for IDN servers and services.

pub mod stream;

use crate::protocols::idn::protocol::{
    ServiceMapEntry, IDNFLG_SCAN_STATUS_EXCLUDED, IDNFLG_SCAN_STATUS_MALFUNCTION,
    IDNFLG_SCAN_STATUS_OCCUPIED, IDNFLG_SCAN_STATUS_OFFLINE, IDNFLG_SCAN_STATUS_REALTIME,
    IDNFLG_SERVICEMAP_DSID, IDNVAL_STYPE_DMX512, IDNVAL_STYPE_LAPRO, IDNVAL_STYPE_UART,
};
use std::net::SocketAddr;

// -------------------------------------------------------------------------------------------------
//  Service Type
// -------------------------------------------------------------------------------------------------

/// The type of service offered by an IDN server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ServiceType {
    /// Standard laser projector (IDNVAL_STYPE_LAPRO)
    LaserProjector,
    /// Generic UART interface
    Uart,
    /// DMX512 interface
    Dmx512,
    /// Unknown service type
    Unknown(u8),
}

impl From<u8> for ServiceType {
    fn from(value: u8) -> Self {
        // Service types 0x80..0xBF are media types (laser projectors)
        // The lower 2 bits may indicate sub-type, so check upper 6 bits
        if (0x80..=0xBF).contains(&value) {
            return ServiceType::LaserProjector;
        }
        match value {
            IDNVAL_STYPE_UART => ServiceType::Uart,
            IDNVAL_STYPE_DMX512 => ServiceType::Dmx512,
            other => ServiceType::Unknown(other),
        }
    }
}

impl From<ServiceType> for u8 {
    fn from(value: ServiceType) -> Self {
        match value {
            ServiceType::LaserProjector => IDNVAL_STYPE_LAPRO,
            ServiceType::Uart => IDNVAL_STYPE_UART,
            ServiceType::Dmx512 => IDNVAL_STYPE_DMX512,
            ServiceType::Unknown(v) => v,
        }
    }
}

// -------------------------------------------------------------------------------------------------
//  Relay Info
// -------------------------------------------------------------------------------------------------

/// Information about a relay device that can route to multiple services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayInfo {
    /// The relay number (1-based)
    pub relay_number: u8,
    /// The relay's name
    pub name: String,
    /// The network address of the relay (if different from main server)
    pub address: Option<SocketAddr>,
}

impl RelayInfo {
    /// Create a new relay info from a service map entry.
    pub fn from_entry(entry: &ServiceMapEntry) -> Self {
        Self {
            relay_number: entry.relay_number,
            name: entry.name_str().to_string(),
            address: None,
        }
    }
}

// -------------------------------------------------------------------------------------------------
//  Service Info
// -------------------------------------------------------------------------------------------------

/// Information about a service offered by an IDN server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    /// The service ID (used when connecting)
    pub service_id: u8,
    /// The type of service
    pub service_type: ServiceType,
    /// The service's name
    pub name: String,
    /// Status flags
    pub flags: u8,
    /// The relay number this service is attached to (0 = root/direct)
    pub relay_number: u8,
}

impl ServiceInfo {
    /// Create a new service info from a service map entry.
    pub fn from_entry(entry: &ServiceMapEntry) -> Self {
        Self {
            service_id: entry.service_id,
            service_type: ServiceType::from(entry.service_type),
            name: entry.name_str().to_string(),
            flags: entry.flags,
            relay_number: entry.relay_number,
        }
    }

    /// Check if this service is a laser projector.
    pub fn is_laser_projector(&self) -> bool {
        matches!(self.service_type, ServiceType::LaserProjector)
    }

    /// Check if this is the default service for its type (DSID flag).
    pub fn is_default(&self) -> bool {
        self.flags & IDNFLG_SERVICEMAP_DSID != 0
    }
}

// -------------------------------------------------------------------------------------------------
//  Server Info
// -------------------------------------------------------------------------------------------------

/// Information about a discovered IDN server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInfo {
    /// The unique 16-byte unit ID
    pub unit_id: [u8; 16],
    /// The server's hostname
    pub hostname: String,
    /// Protocol version (major.minor)
    pub protocol_version: (u8, u8),
    /// Status flags from scan response
    pub status: u8,
    /// Network addresses where the server can be reached
    pub addresses: Vec<SocketAddr>,
    /// Relay devices on this server
    pub relays: Vec<RelayInfo>,
    /// Services offered by this server
    pub services: Vec<ServiceInfo>,
}

impl ServerInfo {
    /// Create a new server info with basic information from a scan response.
    pub fn new(
        unit_id: [u8; 16],
        hostname: String,
        protocol_version: (u8, u8),
        status: u8,
    ) -> Self {
        Self {
            unit_id,
            hostname,
            protocol_version,
            status,
            addresses: Vec::new(),
            relays: Vec::new(),
            services: Vec::new(),
        }
    }

    /// Get the primary network address (first in the list).
    pub fn primary_address(&self) -> Option<&SocketAddr> {
        self.addresses.first()
    }

    /// Find a laser projector service on this server.
    pub fn find_laser_projector(&self) -> Option<&ServiceInfo> {
        self.services.iter().find(|s| s.is_laser_projector())
    }

    /// Find a service by ID.
    pub fn find_service(&self, service_id: u8) -> Option<&ServiceInfo> {
        self.services.iter().find(|s| s.service_id == service_id)
    }

    /// Check if the server has a malfunction.
    pub fn has_malfunction(&self) -> bool {
        self.status & IDNFLG_SCAN_STATUS_MALFUNCTION != 0
    }

    /// Check if the server is offline.
    pub fn is_offline(&self) -> bool {
        self.status & IDNFLG_SCAN_STATUS_OFFLINE != 0
    }

    /// Check if the server is occupied (all sessions in use).
    pub fn is_occupied(&self) -> bool {
        self.status & IDNFLG_SCAN_STATUS_OCCUPIED != 0
    }

    /// Check if the server is excluded from scanning.
    pub fn is_excluded(&self) -> bool {
        self.status & IDNFLG_SCAN_STATUS_EXCLUDED != 0
    }

    /// Check if the server supports realtime streaming.
    pub fn supports_realtime(&self) -> bool {
        self.status & IDNFLG_SCAN_STATUS_REALTIME != 0
    }
}

// -------------------------------------------------------------------------------------------------
//  Addressed DAC
// -------------------------------------------------------------------------------------------------

/// An IDN server with a selected service, ready for streaming.
#[derive(Debug, Clone)]
pub struct Addressed {
    /// The server information
    pub server: ServerInfo,
    /// The selected service
    pub service: ServiceInfo,
    /// The address to connect to
    pub address: SocketAddr,
}

impl Addressed {
    /// Create a new addressed DAC.
    pub fn new(server: ServerInfo, service: ServiceInfo, address: SocketAddr) -> Self {
        Self {
            server,
            service,
            address,
        }
    }

    /// Get the service ID.
    pub fn service_id(&self) -> u8 {
        self.service.service_id
    }

    /// Get the server hostname.
    pub fn hostname(&self) -> &str {
        &self.server.hostname
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_server(status: u8) -> ServerInfo {
        ServerInfo::new([0u8; 16], "test".to_string(), (1, 0), status)
    }

    #[test]
    fn test_is_excluded() {
        // EXCLUDED flag set → true
        let server = make_server(IDNFLG_SCAN_STATUS_EXCLUDED);
        assert!(server.is_excluded());

        // No flags → false
        let server = make_server(0);
        assert!(!server.is_excluded());

        // All flags set (0xFF includes EXCLUDED) → true
        let server = make_server(0xFF);
        assert!(server.is_excluded());
    }
}
