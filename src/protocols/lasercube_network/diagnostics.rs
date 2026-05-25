//! Diagnostic snapshots for LaserCube network output.

use std::time::{Duration, Instant};

use super::profiles::{ConnectionProfile, ConnectionType};

#[derive(Clone, Debug)]
pub struct LaserCubeNetworkDiagnostics {
    pub profile: ConnectionProfile,
    pub connection_type: ConnectionType,
    pub host_queue_len: usize,
    pub host_queue_capacity: usize,
    pub device_free_estimate: usize,
    pub device_buffered_estimate: usize,
    pub packets_sent: u64,
    pub samples_sent: u64,
    pub acks_received: u64,
    pub send_errors: u64,
    pub packet_errors: u8,
    pub last_ack_age: Option<Duration>,
}

impl LaserCubeNetworkDiagnostics {
    pub(crate) fn last_ack_age(now: Instant, last_ack: Option<Instant>) -> Option<Duration> {
        last_ack.map(|t| now.saturating_duration_since(t))
    }
}
