//! Source-labeled LaserCube network connection profiles.

use std::time::Duration;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConnectionType {
    EthernetServer,
    WifiServer,
    EthernetClient,
    WifiClient,
    Unknown(u8),
}

impl ConnectionType {
    pub fn from_status_byte(raw: u8) -> Self {
        match raw {
            0 => Self::EthernetServer,
            1 => Self::WifiServer,
            2 => Self::EthernetClient,
            3 => Self::WifiClient,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProfileSource {
    DesktopNetworkDefault,
    UnknownConservative,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ConnectionProfile {
    pub source: ProfileSource,
    pub connection_type: ConnectionType,
    pub source_chunk_samples: usize,
    pub remote_buffer_cutoff: usize,
    pub wait_connect_sleep: Duration,
    pub wait_buffer_sleep: Duration,
    pub post_send_sleep: Duration,
    pub max_udp_samples_per_packet: usize,
    pub configured_packets_per_transfer: usize,
    pub buffer_total: usize,
}

impl ConnectionProfile {
    pub fn for_connection(connection_type: ConnectionType, buffer_total: usize) -> Self {
        match connection_type {
            ConnectionType::WifiServer => {
                Self::desktop(connection_type, 600, 2000, 12, 4, 10, 140, 20, buffer_total)
            }
            ConnectionType::EthernetServer
            | ConnectionType::EthernetClient
            | ConnectionType::WifiClient => {
                Self::desktop(connection_type, 700, 1800, 12, 6, 4, 80, 20, buffer_total)
            }
            ConnectionType::Unknown(_) => Self::unknown_conservative(buffer_total),
        }
    }

    pub fn unknown_conservative(buffer_total: usize) -> Self {
        Self {
            source: ProfileSource::UnknownConservative,
            connection_type: ConnectionType::Unknown(0),
            source_chunk_samples: 700,
            remote_buffer_cutoff: 1800.min(buffer_total),
            wait_connect_sleep: Duration::from_millis(12),
            wait_buffer_sleep: Duration::from_millis(6),
            post_send_sleep: Duration::from_millis(4),
            max_udp_samples_per_packet: 80,
            configured_packets_per_transfer: 20,
            buffer_total,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn desktop(
        connection_type: ConnectionType,
        source_chunk_samples: usize,
        remote_buffer_cutoff: usize,
        wait_connect_sleep_ms: u64,
        wait_buffer_sleep_ms: u64,
        post_send_sleep_ms: u64,
        max_udp_samples_per_packet: usize,
        configured_packets_per_transfer: usize,
        buffer_total: usize,
    ) -> Self {
        Self {
            source: ProfileSource::DesktopNetworkDefault,
            connection_type,
            source_chunk_samples,
            remote_buffer_cutoff: remote_buffer_cutoff.min(buffer_total),
            wait_connect_sleep: Duration::from_millis(wait_connect_sleep_ms),
            wait_buffer_sleep: Duration::from_millis(wait_buffer_sleep_ms),
            post_send_sleep: Duration::from_millis(post_send_sleep_ms),
            max_udp_samples_per_packet,
            configured_packets_per_transfer,
            buffer_total,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_desktop_network_profiles() {
        assert_eq!(
            ConnectionProfile::for_connection(ConnectionType::EthernetServer, 6000)
                .max_udp_samples_per_packet,
            80
        );
        assert_eq!(
            ConnectionProfile::for_connection(ConnectionType::WifiServer, 6000)
                .max_udp_samples_per_packet,
            140
        );
        assert_eq!(
            ConnectionProfile::for_connection(ConnectionType::WifiClient, 6000)
                .max_udp_samples_per_packet,
            80
        );
    }

    #[test]
    fn unknown_uses_conservative_80_sample_packets() {
        let profile = ConnectionProfile::for_connection(ConnectionType::Unknown(99), 6000);
        assert_eq!(profile.source, ProfileSource::UnknownConservative);
        assert_eq!(profile.max_udp_samples_per_packet, 80);
    }

    #[test]
    fn full_info_connection_type_is_zero_based() {
        assert_eq!(
            ConnectionType::from_status_byte(0),
            ConnectionType::EthernetServer
        );
        assert_eq!(
            ConnectionType::from_status_byte(1),
            ConnectionType::WifiServer
        );
        assert_eq!(
            ConnectionType::from_status_byte(2),
            ConnectionType::EthernetClient
        );
        assert_eq!(
            ConnectionType::from_status_byte(3),
            ConnectionType::WifiClient
        );
        assert_eq!(
            ConnectionType::from_status_byte(4),
            ConnectionType::Unknown(4)
        );
    }
}
