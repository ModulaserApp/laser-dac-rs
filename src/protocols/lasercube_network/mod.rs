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
use protocol::DEFAULT_BUFFER_CAPACITY;

/// Returns capabilities for a LaserCube network DAC using the selected profile.
pub(crate) fn capabilities_for_profile(profile: ConnectionProfile) -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max: 30_000,
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
