//! High-level abstractions around a LaserCube WiFi DAC.

pub mod buffer_estimator;
pub mod stream;

pub use self::buffer_estimator::BufferEstimator;
pub use self::stream::Stream;

use crate::protocols::lasercube_wifi::protocol::{self, DeviceInfo};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::ops;

/// A discovered LaserCube DAC with its network address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Addressed {
    /// The network address from which the DAC was discovered.
    pub source_addr: SocketAddr,
    /// The DAC's state and capabilities.
    pub dac: Dac,
}

/// A LaserCube DAC's state and capabilities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dac {
    /// Protocol version.
    pub version: u8,
    /// Maximum buffer capacity for points.
    pub max_buffer_space: u16,
    /// The DAC's IP address.
    pub ip_addr: IpAddr,
    /// Current status of the DAC.
    pub status: Status,
}

/// Current status of a LaserCube DAC.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Number of free sample slots in the device buffer.
    pub free_buffer_space: u16,
    /// Current playback rate in Hz.
    pub point_rate: u32,
    /// Whether laser output is enabled.
    pub output_enabled: bool,
}

impl Addressed {
    /// Create an `Addressed` DAC from discovery information.
    pub fn from_discovery(info: &DeviceInfo, source_addr: SocketAddr) -> Self {
        let dac = Dac {
            version: info.version,
            max_buffer_space: info.max_buffer_space,
            ip_addr: source_addr.ip(),
            status: Status {
                free_buffer_space: info.max_buffer_space,
                point_rate: 0,
                output_enabled: false,
            },
        };
        Addressed { source_addr, dac }
    }
}

impl Dac {
    /// Update the buffer status.
    pub fn update_buffer_status(&mut self, status: &protocol::BufferStatus) {
        self.status.free_buffer_space = status.free_space;
    }
}

impl ops::Deref for Addressed {
    type Target = Dac;
    fn deref(&self) -> &Self::Target {
        &self.dac
    }
}

impl ops::DerefMut for Addressed {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.dac
    }
}

impl fmt::Display for Dac {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "LaserCube @ {} (v{}, buffer: {})",
            self.ip_addr, self.version, self.max_buffer_space
        )
    }
}
