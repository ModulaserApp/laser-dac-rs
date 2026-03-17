//! LaserCube WiFi DAC streaming backend implementation.

use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::lasercube_wifi::dac::{stream, Addressed};
use crate::protocols::lasercube_wifi::protocol::{DeviceInfo, Point as LasercubePoint};
use crate::types::{DacCapabilities, DacType, LaserPoint};
use std::net::SocketAddr;

/// LaserCube WiFi DAC backend.
pub struct LasercubeWifiBackend {
    addressed: Addressed,
    stream: Option<stream::Stream>,
    caps: DacCapabilities,
    /// Pre-allocated conversion buffer (avoids per-write heap allocation).
    point_buffer: Vec<LasercubePoint>,
}

impl LasercubeWifiBackend {
    pub fn new(addressed: Addressed) -> Self {
        let caps = super::capabilities_for_buffer(addressed.max_buffer_space);
        Self {
            addressed,
            stream: None,
            caps,
            point_buffer: Vec::new(),
        }
    }

    pub fn from_discovery(info: &DeviceInfo, source_addr: SocketAddr) -> Self {
        Self::new(Addressed::from_discovery(info, source_addr))
    }
}

impl StreamBackend for LasercubeWifiBackend {
    fn dac_type(&self) -> DacType {
        DacType::LasercubeWifi
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        let stream = stream::connect(&self.addressed).map_err(Error::backend)?;

        self.stream = Some(stream);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            let _ = stream.stop();
        }
        self.stream = None;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        // Update rate before admission check so the estimator uses the correct
        // drain rate — otherwise a PPS decrease could overestimate available space.
        stream.set_rate(pps).map_err(Error::backend)?;

        if points.len() > stream.safe_writable_points() as usize {
            return Ok(WriteOutcome::WouldBlock);
        }

        self.point_buffer.clear();
        self.point_buffer
            .extend(points.iter().map(LasercubePoint::from));

        stream
            .write_frame(&self.point_buffer, pps)
            .map_err(Error::backend)?;

        Ok(WriteOutcome::Written)
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            stream.stop().map_err(Error::backend)?;
        }
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            stream.set_output(open).map_err(Error::backend)?;
        }
        Ok(())
    }

    fn queued_points(&self) -> Option<u64> {
        self.stream
            .as_ref()
            .map(|s| s.estimated_buffer_fullness() as u64)
    }
}
