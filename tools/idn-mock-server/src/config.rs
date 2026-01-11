//! Configuration types for the mock server.

use std::net::SocketAddr;
use std::time::Duration;

use crate::constants::{IDNFLG_SERVICEMAP_DSID, IDNVAL_STYPE_DMX512, IDNVAL_STYPE_LAPRO, IDN_PORT};

/// Configuration for a mock service.
#[derive(Clone, Debug)]
pub struct MockService {
    pub service_id: u8,
    pub service_type: u8,
    pub name: String,
    pub flags: u8,
    pub relay_number: u8,
}

impl MockService {
    /// Create a laser projector service.
    pub fn laser_projector(service_id: u8, name: &str) -> Self {
        Self {
            service_id,
            service_type: IDNVAL_STYPE_LAPRO,
            name: name.to_string(),
            flags: 0,
            relay_number: 0,
        }
    }

    /// Create a DMX512 service.
    pub fn dmx512(service_id: u8, name: &str) -> Self {
        Self {
            service_id,
            service_type: IDNVAL_STYPE_DMX512,
            name: name.to_string(),
            flags: 0,
            relay_number: 0,
        }
    }

    /// Set the relay number this service is attached to.
    pub fn with_relay(mut self, relay_number: u8) -> Self {
        self.relay_number = relay_number;
        self
    }

    /// Set the DSID flag (default service for type).
    pub fn with_dsid(mut self) -> Self {
        self.flags |= IDNFLG_SERVICEMAP_DSID;
        self
    }
}

/// Configuration for a mock relay.
#[derive(Clone, Debug)]
pub struct MockRelay {
    pub relay_number: u8,
    pub name: String,
}

impl MockRelay {
    pub fn new(relay_number: u8, name: &str) -> Self {
        Self {
            relay_number,
            name: name.to_string(),
        }
    }
}

/// Static server configuration (set at construction time).
#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub hostname: String,
    pub unit_id: [u8; 16],
    pub protocol_version: u8,
    pub services: Vec<MockService>,
    pub relays: Vec<MockRelay>,
    pub bind_address: SocketAddr,
    pub read_timeout: Duration,
    pub link_timeout: Duration,
}

impl ServerConfig {
    /// Create a new server configuration with the given hostname.
    ///
    /// Binds to `127.0.0.1:0` by default (ephemeral port for testing).
    pub fn new(hostname: &str) -> Self {
        // Generate unit_id from hostname per spec section 4.1.2:
        // Format: [length][category][identifier bytes...] zero-padded to 16 bytes
        // Using category 0x7F for custom/vendor-specific identifiers
        let mut unit_id = [0u8; 16];
        let bytes = hostname.as_bytes();
        let id_len = bytes.len().min(14); // Max 14 bytes for identifier (16 - length - category)
        unit_id[0] = (id_len + 1) as u8; // Length: category byte + identifier bytes
        unit_id[1] = 0x7F; // Category: vendor-specific/custom
        unit_id[2..2 + id_len].copy_from_slice(&bytes[..id_len]);

        Self {
            hostname: hostname.to_string(),
            unit_id,
            protocol_version: 0x10, // Version 1.0
            services: vec![MockService::laser_projector(1, "Laser1")],
            relays: Vec::new(),
            bind_address: "127.0.0.1:0".parse().unwrap(),
            read_timeout: Duration::from_millis(100),
            link_timeout: Duration::from_millis(1000),
        }
    }

    /// Create a server configuration that binds to the standard IDN port.
    pub fn new_on_standard_port(hostname: &str) -> Self {
        Self::new(hostname).with_bind_address(format!("0.0.0.0:{}", IDN_PORT).parse().unwrap())
    }

    /// Set a custom unit ID.
    pub fn with_unit_id(mut self, unit_id: [u8; 16]) -> Self {
        self.unit_id = unit_id;
        self
    }

    /// Set the protocol version (major.minor packed into single byte).
    pub fn with_protocol_version(mut self, major: u8, minor: u8) -> Self {
        self.protocol_version = (major << 4) | (minor & 0x0F);
        self
    }

    /// Set the services this server provides.
    pub fn with_services(mut self, services: Vec<MockService>) -> Self {
        self.services = services;
        self
    }

    /// Set the relays this server provides.
    pub fn with_relays(mut self, relays: Vec<MockRelay>) -> Self {
        self.relays = relays;
        self
    }

    /// Set the bind address.
    pub fn with_bind_address(mut self, addr: SocketAddr) -> Self {
        self.bind_address = addr;
        self
    }

    /// Set the socket read timeout.
    pub fn with_read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = timeout;
        self
    }

    /// Set the link timeout for client disconnection detection.
    pub fn with_link_timeout(mut self, timeout: Duration) -> Self {
        self.link_timeout = timeout;
        self
    }
}
