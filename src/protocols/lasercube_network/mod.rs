//! LaserCube network DAC support for WiFi and Ethernet devices.
//!
//! This module owns network discovery, command/status parsing, queueing,
//! pacing, ACK handling, and sample packetization.

mod ack;
mod backend;
mod command;
mod diagnostics;
mod discovery;
mod error;
mod pacing;
mod packetizer;
mod profiles;
mod protocol;
mod status;
mod transport;

pub use backend::LaserCubeNetworkBackend;
pub use diagnostics::LaserCubeNetworkDiagnostics;
pub use discovery::LaserCubeNetworkDiscoverer;
pub use profiles::{ConnectionProfile, ConnectionType, ProfileSource};
pub use status::LaserCubeNetworkStatus;

use crate::device::{DacCapabilities, OutputModel};
use protocol::{DEFAULT_BUFFER_CAPACITY, DEFAULT_POINT_RATE};

fn resolved_point_rate_max(status: &LaserCubeNetworkStatus) -> u32 {
    if status.point_rate_max > 0 {
        status.point_rate_max
    } else {
        DEFAULT_POINT_RATE
    }
}

fn clamp_point_rate(status: &LaserCubeNetworkStatus, point_rate: u32) -> u32 {
    point_rate.max(1).min(resolved_point_rate_max(status))
}

/// Returns capabilities for a LaserCube network DAC using the selected profile.
pub(crate) fn capabilities_for_profile(profile: ConnectionProfile) -> DacCapabilities {
    capabilities_for_profile_and_rate(profile, DEFAULT_POINT_RATE)
}

pub(crate) fn capabilities_for_status(
    profile: ConnectionProfile,
    status: &LaserCubeNetworkStatus,
) -> DacCapabilities {
    capabilities_for_profile_and_rate(profile, resolved_point_rate_max(status))
}

fn capabilities_for_profile_and_rate(profile: ConnectionProfile, pps_max: u32) -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max,
        max_points_per_chunk: profile.max_udp_samples_per_packet,
        output_model: OutputModel::NetworkFifo,
    }
}

/// Returns conservative default capabilities before device status is known.
pub fn default_capabilities() -> DacCapabilities {
    capabilities_for_profile(ConnectionProfile::unknown_conservative(
        DEFAULT_BUFFER_CAPACITY as usize,
    ))
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::*;

    #[test]
    fn capabilities_use_advertised_point_rate_max() {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let profile = ConnectionProfile::unknown_conservative(DEFAULT_BUFFER_CAPACITY as usize);

        status.point_rate_max = 60_000;
        assert_eq!(capabilities_for_status(profile, &status).pps_max, 60_000);

        status.point_rate_max = 40_000;
        assert_eq!(capabilities_for_status(profile, &status).pps_max, 40_000);
    }

    #[test]
    fn point_rate_max_falls_back_to_default_when_unknown() {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        let profile = ConnectionProfile::unknown_conservative(DEFAULT_BUFFER_CAPACITY as usize);

        status.point_rate_max = 0;
        assert_eq!(resolved_point_rate_max(&status), DEFAULT_POINT_RATE);
        assert_eq!(
            capabilities_for_status(profile, &status).pps_max,
            DEFAULT_POINT_RATE
        );
    }

    #[test]
    fn clamps_requested_point_rate_to_device_max() {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        status.point_rate_max = 40_000;

        assert_eq!(clamp_point_rate(&status, 60_000), 40_000);
        assert_eq!(clamp_point_rate(&status, 25_000), 25_000);
        assert_eq!(clamp_point_rate(&status, 0), 1);
    }
}
