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

pub mod backend;
pub mod dac;
pub mod protocol;

pub use self::protocol::{
    DacBroadcast, DacPoint, DacResponse, DacStatus, ReadBytes, SizeBytes, WriteBytes,
};
pub use backend::EtherDreamBackend;

use crate::types::{DacCapabilities, OutputModel};
use std::{io, net};

/// Returns the default capabilities for Ether Dream DACs.
pub fn default_capabilities() -> DacCapabilities {
    DacCapabilities {
        pps_min: 1,
        pps_max: 100_000,
        max_points_per_chunk: 1799,
        prefers_constant_pps: true,
        can_estimate_queue: true,
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
pub fn recv_dac_broadcasts() -> io::Result<RecvDacBroadcasts> {
    let broadcast_addr = net::SocketAddrV4::new([0, 0, 0, 0].into(), protocol::BROADCAST_PORT);
    let udp_socket = net::UdpSocket::bind(broadcast_addr)?;
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
