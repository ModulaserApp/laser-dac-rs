use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::buffer_estimate::BufferEstimator;

use super::super::diagnostics::LaserCubeNetworkDiagnostics;
use super::super::error::CommunicationError;
use super::super::profiles::{ConnectionProfile, ConnectionType};
use super::super::protocol::DEFAULT_POINT_RATE;
use super::super::status::LaserCubeNetworkStatus;
use super::COMMS_STALE_TIMEOUT;

#[derive(Debug)]
pub(super) struct TransportState {
    pub(super) profile: ConnectionProfile,
    pub(super) connection_type: ConnectionType,
    pub(super) status: LaserCubeNetworkStatus,
    pub(super) host_queue_len: usize,
    pub(super) host_queue_capacity: usize,
    pub(super) free_estimate: usize,
    pub(super) buffer_total: usize,
    pub(super) point_rate: u32,
    pub(super) last_estimate: Instant,
    pub(super) packets_sent: u64,
    pub(super) samples_sent: u64,
    pub(super) acks_received: u64,
    pub(super) command_successes: u64,
    pub(super) command_failures: u64,
    pub(super) send_errors: u64,
    pub(super) last_data_ack_sequence: Option<u8>,
    pub(super) last_ack_free_space: Option<u16>,
    pub(super) last_ack_rtt: Option<Duration>,
    pub(super) packet_errors: u8,
    pub(super) last_ack: Option<Instant>,
    pub(super) last_full_info: Option<Instant>,
    pub(super) last_comms: Option<Instant>,
    pub(super) connected: bool,
}

#[derive(Clone, Debug)]
pub struct SharedTransportState {
    pub(super) inner: Arc<Mutex<TransportState>>,
}

#[derive(Debug)]
pub(super) struct HostQueueReservation {
    state: SharedTransportState,
    points: usize,
    committed: bool,
}

impl HostQueueReservation {
    pub(super) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for HostQueueReservation {
    fn drop(&mut self) {
        if !self.committed {
            self.state.release_host_points(self.points);
        }
    }
}

/// Device buffer capacity from the reported `buffer_max`, falling back to the
/// default only when the device reports 0 (unknown). Reporting a smaller real
/// capacity must not be inflated to the default, or the pacer would over-fill.
pub(super) fn buffer_total_from_max(buffer_max: u16) -> usize {
    if buffer_max > 0 {
        buffer_max as usize
    } else {
        super::super::protocol::DEFAULT_BUFFER_CAPACITY as usize
    }
}

impl SharedTransportState {
    pub(super) fn new(status: &LaserCubeNetworkStatus, profile: ConnectionProfile) -> Self {
        let now = Instant::now();
        let buffer_total = buffer_total_from_max(status.buffer_max);
        let host_queue_capacity = (buffer_total * 2).max(profile.max_udp_samples_per_packet * 8);
        let point_rate = if status.point_rate > 0 {
            super::super::clamp_point_rate(status, status.point_rate)
        } else {
            super::super::clamp_point_rate(status, DEFAULT_POINT_RATE)
        };
        Self {
            inner: Arc::new(Mutex::new(TransportState {
                profile,
                connection_type: status.connection_type,
                status: status.clone(),
                host_queue_len: 0,
                host_queue_capacity,
                free_estimate: status.buffer_free.min(status.buffer_max) as usize,
                buffer_total,
                point_rate,
                last_estimate: now,
                packets_sent: 0,
                samples_sent: 0,
                acks_received: 0,
                command_successes: 0,
                command_failures: 0,
                send_errors: 0,
                last_data_ack_sequence: None,
                last_ack_free_space: None,
                last_ack_rtt: None,
                packet_errors: status.packet_errors,
                last_ack: None,
                last_full_info: Some(now),
                last_comms: Some(now),
                connected: true,
            })),
        }
    }

    pub fn disconnected(profile: ConnectionProfile) -> Self {
        let now = Instant::now();
        Self {
            inner: Arc::new(Mutex::new(TransportState {
                profile,
                connection_type: ConnectionType::Unknown(0),
                status: LaserCubeNetworkStatus::minimal(
                    "0.0.0.0".parse().expect("valid default IP"),
                ),
                host_queue_len: 0,
                host_queue_capacity: 0,
                free_estimate: profile.buffer_total,
                buffer_total: profile.buffer_total,
                point_rate: DEFAULT_POINT_RATE,
                last_estimate: now,
                packets_sent: 0,
                samples_sent: 0,
                acks_received: 0,
                command_successes: 0,
                command_failures: 0,
                send_errors: 0,
                last_data_ack_sequence: None,
                last_ack_free_space: None,
                last_ack_rtt: None,
                packet_errors: 0,
                last_ack: None,
                last_full_info: None,
                last_comms: None,
                connected: false,
            })),
        }
    }

    pub(super) fn reserve_host_points(
        &self,
        points: usize,
    ) -> Result<HostQueueReservation, CommunicationError> {
        let mut state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        if state.host_queue_len.saturating_add(points) > state.host_queue_capacity {
            return Err(CommunicationError::QueueFull);
        }
        state.host_queue_len += points;
        Ok(HostQueueReservation {
            state: self.clone(),
            points,
            committed: false,
        })
    }

    fn release_host_points(&self, points: usize) {
        let mut state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.host_queue_len = state.host_queue_len.saturating_sub(points);
    }

    pub(super) fn clear_host_queue(&self) {
        let mut state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.host_queue_len = 0;
    }

    pub(super) fn mark_disconnected(&self) {
        let mut state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.connected = false;
    }

    pub fn diagnostics(&self) -> LaserCubeNetworkDiagnostics {
        let state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        let now = Instant::now();
        let free = decayed_free(&state, now);
        let last_comms_age = LaserCubeNetworkDiagnostics::age(now, state.last_comms);
        LaserCubeNetworkDiagnostics {
            profile: state.profile,
            connection_type: state.connection_type,
            status: state.status.clone(),
            connected: state.connected,
            communication_stale: communication_stale(&state, now),
            host_queue_len: state.host_queue_len,
            host_queue_capacity: state.host_queue_capacity,
            device_free_estimate: free,
            device_buffered_estimate: state.buffer_total.saturating_sub(free),
            packets_sent: state.packets_sent,
            samples_sent: state.samples_sent,
            acks_received: state.acks_received,
            command_successes: state.command_successes,
            command_failures: state.command_failures,
            send_errors: state.send_errors,
            point_rate: state.point_rate,
            last_data_ack_sequence: state.last_data_ack_sequence,
            last_ack_free_space: state.last_ack_free_space,
            last_ack_rtt: state.last_ack_rtt,
            packet_errors: state.packet_errors,
            last_ack_age: LaserCubeNetworkDiagnostics::age(now, state.last_ack),
            last_full_info_age: LaserCubeNetworkDiagnostics::age(now, state.last_full_info),
            last_comms_age,
        }
    }

    pub(crate) fn is_usable(&self) -> bool {
        let state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        state.connected && !communication_stale(&state, Instant::now())
    }
}

impl BufferEstimator for SharedTransportState {
    fn estimated_fullness(&self, now: Instant, _pps: u32) -> u64 {
        let state = self
            .inner
            .lock()
            .expect("LaserCube transport state poisoned");
        let free = decayed_free(&state, now);
        let device_buffered = state.buffer_total.saturating_sub(free);
        device_buffered.saturating_add(state.host_queue_len) as u64
    }
}

pub(super) fn communication_stale(state: &TransportState, now: Instant) -> bool {
    match state.last_comms {
        Some(last_comms) => now.saturating_duration_since(last_comms) > COMMS_STALE_TIMEOUT,
        None => state.connected,
    }
}

pub(super) fn decayed_free(state: &TransportState, now: Instant) -> usize {
    let elapsed = now.saturating_duration_since(state.last_estimate);
    let drained = (elapsed.as_secs_f64() * state.point_rate as f64) as usize;
    state
        .free_estimate
        .saturating_add(drained)
        .min(state.buffer_total)
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};
    use std::time::{Duration, Instant};

    use super::super::COMMS_STALE_TIMEOUT;
    use super::*;

    fn state_with_host_queue(host_queue_len: usize) -> SharedTransportState {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        status.buffer_max = 6000;
        status.buffer_free = 5000;
        let profile = ConnectionProfile::unknown_conservative(6000);
        let state = SharedTransportState::new(&status, profile);
        state.reserve_host_points(host_queue_len).unwrap().commit();
        state
    }

    #[test]
    fn estimator_includes_host_queue() {
        let state = state_with_host_queue(320);
        let now = state.inner.lock().unwrap().last_estimate;
        assert_eq!(state.estimated_fullness(now, 30_000), 1320);
    }

    #[test]
    fn bounded_queue_rejects_over_capacity() {
        let state = state_with_host_queue(0);
        let capacity = state.diagnostics().host_queue_capacity;
        let _reservation = state.reserve_host_points(capacity).unwrap();
        assert!(state.reserve_host_points(1).is_err());
    }

    #[test]
    fn diagnostics_report_comms_stale_after_timeout() {
        let state = state_with_host_queue(0);
        {
            let mut inner = state.inner.lock().unwrap();
            inner.last_comms =
                Some(Instant::now() - COMMS_STALE_TIMEOUT - Duration::from_millis(1));
        }
        let diagnostics = state.diagnostics();
        assert!(diagnostics.communication_stale);
        assert!(diagnostics.last_comms_age >= Some(COMMS_STALE_TIMEOUT));
        assert!(!state.is_usable());
    }

    #[test]
    fn transport_state_is_usable_while_comms_are_fresh() {
        let state = state_with_host_queue(0);
        assert!(state.is_usable());
    }

    #[test]
    fn diagnostics_include_latest_status_snapshot() {
        let state = state_with_host_queue(0);
        {
            let mut inner = state.inner.lock().unwrap();
            inner.status.firmware_major = 1;
            inner.status.firmware_minor = 24;
            inner.status.battery_percent = 255;
            inner.status.temperature_c = 42;
            inner.status.interlock_enabled = true;
            inner.command_successes = 2;
            inner.command_failures = 1;
            inner.point_rate = 40_000;
            inner.last_data_ack_sequence = Some(42);
            inner.last_ack_free_space = Some(1234);
            inner.last_ack_rtt = Some(Duration::from_millis(7));
        }
        let diagnostics = state.diagnostics();
        assert_eq!(diagnostics.status.firmware_major, 1);
        assert_eq!(diagnostics.status.firmware_minor, 24);
        assert!(diagnostics.status.is_plugged_in());
        assert_eq!(diagnostics.status.temperature_c, 42);
        assert!(diagnostics.status.interlock_enabled);
        assert_eq!(diagnostics.command_successes, 2);
        assert_eq!(diagnostics.command_failures, 1);
        assert_eq!(diagnostics.point_rate, 40_000);
        assert_eq!(diagnostics.last_data_ack_sequence, Some(42));
        assert_eq!(diagnostics.last_ack_free_space, Some(1234));
        assert_eq!(diagnostics.last_ack_rtt, Some(Duration::from_millis(7)));
    }
}
