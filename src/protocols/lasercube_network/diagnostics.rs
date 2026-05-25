//! Diagnostic snapshots for LaserCube network output.

use std::time::{Duration, Instant};

use super::profiles::{ConnectionProfile, ConnectionType};
use super::status::LaserCubeNetworkStatus;

#[derive(Clone, Debug)]
pub struct LaserCubeNetworkDiagnostics {
    pub profile: ConnectionProfile,
    pub connection_type: ConnectionType,
    pub status: LaserCubeNetworkStatus,
    pub connected: bool,
    pub communication_stale: bool,
    pub host_queue_len: usize,
    pub host_queue_capacity: usize,
    pub device_free_estimate: usize,
    pub device_buffered_estimate: usize,
    pub packets_sent: u64,
    pub samples_sent: u64,
    pub acks_received: u64,
    pub command_successes: u64,
    pub command_failures: u64,
    pub send_errors: u64,
    pub packet_errors: u8,
    pub last_ack_age: Option<Duration>,
    pub last_full_info_age: Option<Duration>,
    pub last_comms_age: Option<Duration>,
}

impl LaserCubeNetworkDiagnostics {
    pub(crate) fn age(now: Instant, timestamp: Option<Instant>) -> Option<Duration> {
        timestamp.map(|t| now.saturating_duration_since(t))
    }
}
