//! Ether Dream laser DAC protocol implementation.
//!
//! Ether Dream is a network-based laser DAC that uses TCP for streaming
//! and UDP for device discovery.
//!
//! # Example
//!
//! ```no_run
//! use laser_dac::protocols::ether_dream::{recv_dac_broadcasts, dac::stream};
//! use std::time::Duration;
//!
//! fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Listen for DAC broadcasts
//!     let mut broadcasts = recv_dac_broadcasts()?;
//!     broadcasts.set_timeout(Some(Duration::from_secs(2)))?;
//!
//!     if let Ok((broadcast, src_addr)) = broadcasts.next_broadcast() {
//!         println!("Found DAC: {:?}", broadcast);
//!
//!         // Connect to the DAC
//!         let mut stream = stream::connect(&broadcast, src_addr.ip())?;
//!
//!         // Prepare for streaming
//!         stream.queue_commands()
//!             .prepare_stream()
//!             .submit()?;
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! # Connecting without a broadcast
//!
//! A DAC on a known address can be reached without waiting for its UDP
//! broadcast: use [`dac::stream::connect_to`] for the low-level stream, or
//! [`EtherDreamBackend::with_address`] for the backend. Without a broadcast,
//! the buffer capacity and maximum point rate fall back to 1799 points, the
//! smallest known ring (ED1), and 100 000 pps, which every known Ether Dream
//! advertises.
//!
//! # Buffering
//!
//! Streams default to an 80 ms target buffer
//! ([`StreamConfig::ETHER_DREAM_DEFAULT_TARGET_BUFFER`](crate::StreamConfig::ETHER_DREAM_DEFAULT_TARGET_BUFFER)),
//! measured to cover the host stalls seen on an ED2. The backend caps the
//! target at 80 % of the DAC's ring
//! ([`FifoBackend::target_buffer_ceiling`](crate::FifoBackend::target_buffer_ceiling)),
//! so at high rates the target stays reachable and steady writes always fit.
//! Writes are admitted against the reported fullness decayed from when the
//! reply arrived, an upper bound, so an oversized chunk is never sent into a
//! full ring.
//!
//! # Firmware profiles and the simulator
//!
//! [`FirmwareProfile`] records what each known firmware advertises and how it
//! reacts to edge cases. With the `testutils` feature, the `sim` module
//! provides `EtherDreamModel`, a pure state machine, and `SimServer`, which
//! serves the model over real TCP and UDP sockets. Tests use them to run the
//! production code against every profile in [`FirmwareProfile::all`].

pub mod backend;
pub mod dac;
mod discovery;
pub mod profile;
pub mod protocol;
#[cfg(test)]
mod replay;
#[cfg(any(test, feature = "testutils"))]
pub mod sim;

pub use self::protocol::{
    DacBroadcast, DacPoint, DacResponse, DacStatus, ReadBytes, SizeBytes, WriteBytes,
};
pub use backend::EtherDreamBackend;
pub use discovery::EtherDreamDiscoverer;
pub use profile::{FirmwareProfile, Provenance};

use crate::device::{DacCapabilities, OutputModel};
use std::{io, net};

/// Returns the default capabilities for Ether Dream DACs.
pub fn default_capabilities() -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max: 100_000,
        max_points_per_chunk: 1799,
        output_model: OutputModel::NetworkFifo,
    }
}

/// An iterator that listens and waits for broadcast messages from DACs on the network and yields
/// them as they are received on the inner UDP socket.
pub struct RecvDacBroadcasts {
    udp_socket: net::UdpSocket,
    buffer: [u8; RecvDacBroadcasts::BUFFER_LEN],
}

impl RecvDacBroadcasts {
    /// The size of the inner buffer used to receive broadcast messages.
    pub const BUFFER_LEN: usize = protocol::DacBroadcast::SIZE_BYTES;
}

/// Produces a `RecvDacBroadcasts` instance that listens and waits for broadcast messages from DACs
/// on the network and yields them as they are received on the inner UDP socket.
///
/// The socket is bound with `SO_REUSEADDR` (and `SO_REUSEPORT` on unix) so that
/// other Ether Dream software sharing port 7654 on the same host does not make
/// the DAC invisible to this discoverer.
pub fn recv_dac_broadcasts() -> io::Result<RecvDacBroadcasts> {
    use socket2::{Domain, Protocol, Socket, Type};

    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;
    #[cfg(unix)]
    socket.set_reuse_port(true)?;

    let broadcast_addr = net::SocketAddrV4::new([0, 0, 0, 0].into(), protocol::BROADCAST_PORT);
    socket.bind(&socket2::SockAddr::from(broadcast_addr))?;

    let udp_socket = net::UdpSocket::from(socket);
    Ok(RecvDacBroadcasts {
        udp_socket,
        buffer: [0; RecvDacBroadcasts::BUFFER_LEN],
    })
}

impl RecvDacBroadcasts {
    /// Attempt to read the next broadcast.
    pub fn next_broadcast(&mut self) -> io::Result<(protocol::DacBroadcast, net::SocketAddr)> {
        let (len, src_addr) = self.udp_socket.recv_from(&mut self.buffer)?;
        if len < protocol::DacBroadcast::SIZE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!(
                    "received {} bytes, expected at least {}",
                    len,
                    protocol::DacBroadcast::SIZE_BYTES
                ),
            ));
        }
        let mut bytes = &self.buffer[..len];
        let dac_broadcast = bytes.read_bytes::<protocol::DacBroadcast>()?;
        Ok((dac_broadcast, src_addr))
    }

    /// Set the timeout for the inner UDP socket used for reading broadcasts.
    pub fn set_timeout(&self, duration: Option<std::time::Duration>) -> io::Result<()> {
        self.udp_socket.set_read_timeout(duration)
    }

    /// Moves the inner UDP socket into or out of nonblocking mode.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        self.udp_socket.set_nonblocking(nonblocking)
    }
}

impl Iterator for RecvDacBroadcasts {
    type Item = io::Result<(protocol::DacBroadcast, net::SocketAddr)>;
    fn next(&mut self) -> Option<Self::Item> {
        Some(self.next_broadcast())
    }
}
