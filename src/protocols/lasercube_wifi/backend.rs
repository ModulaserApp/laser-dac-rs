//! LaserCube WiFi DAC streaming backend implementation.

use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::lasercube_wifi::dac::{stream, Addressed};
use crate::protocols::lasercube_wifi::protocol::{DeviceInfo, Point as LasercubePoint};
use crate::types::{caps_for_dac_type, Caps, DacType, LaserPoint};
use std::net::SocketAddr;

/// LaserCube WiFi DAC backend.
pub struct LasercubeWifiBackend {
    addressed: Addressed,
    stream: Option<stream::Stream>,
    caps: Caps,
}

impl LasercubeWifiBackend {
    pub fn new(addressed: Addressed) -> Self {
        Self {
            addressed,
            stream: None,
            caps: caps_for_dac_type(&DacType::LasercubeWifi),
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

    fn caps(&self) -> &Caps {
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

        let lc_points: Vec<LasercubePoint> = points.iter().map(|p| p.into()).collect();

        stream
            .write_frame(&lc_points, pps)
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
}
