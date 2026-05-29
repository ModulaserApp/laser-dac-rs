//! Single-threaded LaserCube network transport worker.

mod handle;
mod state;
mod worker;

use std::io;
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

use super::command;
use super::profiles::ConnectionProfile;
use super::protocol::DEFAULT_POINT_RATE;
use super::status::LaserCubeNetworkStatus;

pub use handle::TransportHandle;
pub use state::SharedTransportState;

pub(super) trait DatagramSocket {
    fn send(&self, buffer: &[u8]) -> io::Result<usize>;
    fn recv(&self, buffer: &mut [u8]) -> io::Result<usize>;
}

impl DatagramSocket for UdpSocket {
    fn send(&self, buffer: &[u8]) -> io::Result<usize> {
        UdpSocket::send(self, buffer)
    }

    fn recv(&self, buffer: &mut [u8]) -> io::Result<usize> {
        UdpSocket::recv(self, buffer)
    }
}

pub(super) const COMMAND_REPEAT_COUNT: usize = 2;
pub(super) const POINT_QUEUE_CAPACITY: usize = 128;
pub(super) const MAX_ACK_DRAIN_PER_LOOP: usize = 32;
pub(super) const MAX_CONTROL_DRAIN_PER_LOOP: usize = 32;
pub(super) const MAX_IDLE_SLEEP: Duration = Duration::from_millis(1);
pub(super) const FULL_INFO_POLL_INACTIVE: Duration = Duration::from_millis(250);
pub(super) const FULL_INFO_POLL_ACTIVE: Duration = Duration::from_millis(2500);
pub(super) const COMMS_STALE_TIMEOUT: Duration = Duration::from_millis(4000);
pub(super) const DIAGNOSTIC_LOG_PERIOD: Duration = Duration::from_secs(1);

#[derive(Clone, Debug)]
pub struct AddressedDevice {
    pub source_addr: SocketAddr,
    pub status: LaserCubeNetworkStatus,
    pub profile: ConnectionProfile,
}

impl AddressedDevice {
    pub fn ip(&self) -> IpAddr {
        self.source_addr.ip()
    }
}

#[derive(Debug)]
enum TransportCommand {
    Enqueue {
        generation: u64,
        point_rate: u32,
        points: Vec<super::protocol::Point>,
        _reservation: state::HostQueueReservation,
    },
}

#[derive(Debug)]
enum PriorityCommand {
    SetOutput { enabled: bool, generation: u64 },
    StopOutput { generation: u64 },
    Shutdown { generation: u64 },
}

pub(super) fn send_repeated<S: DatagramSocket>(socket: &S, cmd: &[u8]) -> io::Result<()> {
    for _ in 0..COMMAND_REPEAT_COUNT {
        socket.send(cmd)?;
    }
    Ok(())
}

pub(super) fn startup_commands(
    status: &LaserCubeNetworkStatus,
    profile: ConnectionProfile,
) -> Vec<Vec<u8>> {
    let mut commands = vec![
        command::set_output(false).to_vec(),
        command::enable_buffer_size_response(true).to_vec(),
        command::set_rate(super::clamp_point_rate(status, DEFAULT_POINT_RATE)).to_vec(),
    ];
    if command::threshold_supported(status) {
        let threshold = profile.remote_buffer_cutoff;
        if threshold < status.buffer_max as usize {
            commands.push(command::set_dac_buffer_threshold(threshold as u32).to_vec());
        }
    }
    commands
}

pub(super) fn would_block(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use super::super::command;
    use super::super::profiles::ConnectionProfile;
    use super::super::status::LaserCubeNetworkStatus;
    use super::*;

    #[test]
    fn startup_commands_do_not_include_warmup_or_clear_ringbuffer() {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        status.firmware_minor = 24;
        let profile = ConnectionProfile::unknown_conservative(6000);
        let commands = startup_commands(&status, profile);
        assert_eq!(commands[0], vec![0x80, 0x00]);
        assert_eq!(commands[1], vec![0x78, 0x01]);
        assert_eq!(commands[2], vec![0x82, 0x30, 0x75, 0x00, 0x00]);
        assert!(commands.iter().all(|cmd| cmd.first() != Some(&0x8D)));
        assert!(commands.iter().all(|cmd| cmd.first() != Some(&0xA9)));
    }

    #[test]
    fn startup_rate_is_clamped_to_advertised_device_max() {
        let mut status = LaserCubeNetworkStatus::minimal(IpAddr::V4(Ipv4Addr::LOCALHOST));
        status.point_rate_max = 20_000;
        let profile = ConnectionProfile::unknown_conservative(6000);
        let commands = startup_commands(&status, profile);
        assert_eq!(commands[2], command::set_rate(20_000).to_vec());
    }
}
