//! LaserCube USB DAC streaming backend implementation.

use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::lasercube_usb::dac::Stream;
use crate::protocols::lasercube_usb::protocol::Sample as LasercubeUsbSample;
use crate::protocols::lasercube_usb::{discover_dacs, rusb};
use crate::types::{DacCapabilities, DacType, LaserPoint};

/// LaserCube USB DAC backend (LaserDock).
pub struct LasercubeUsbBackend {
    device: Option<rusb::Device<rusb::Context>>,
    stream: Option<Stream<rusb::Context>>,
    caps: DacCapabilities,
}

impl LasercubeUsbBackend {
    pub fn new(device: rusb::Device<rusb::Context>) -> Self {
        Self {
            device: Some(device),
            stream: None,
            caps: super::default_capabilities(),
        }
    }

    pub fn from_stream(stream: Stream<rusb::Context>) -> Self {
        Self {
            device: None,
            stream: Some(stream),
            caps: super::default_capabilities(),
        }
    }

    pub fn discover_devices() -> Result<Vec<rusb::Device<rusb::Context>>> {
        discover_dacs().map_err(Error::backend)
    }
}

impl StreamBackend for LasercubeUsbBackend {
    fn dac_type(&self) -> DacType {
        DacType::LasercubeUsb
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        if self.stream.is_some() {
            return Ok(());
        }

        let device = self
            .device
            .take()
            .ok_or_else(|| Error::disconnected("No device available"))?;

        let mut stream = Stream::open(device).map_err(Error::backend)?;

        stream.enable_output().map_err(Error::backend)?;

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

        let samples: Vec<LasercubeUsbSample> = points.iter().map(|p| p.into()).collect();

        stream.write_frame(&samples, pps).map_err(Error::backend)?;

        Ok(WriteOutcome::Written)
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(stream) = &mut self.stream {
            stream.stop().map_err(Error::backend)?;
        }
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        let Some(stream) = &mut self.stream else {
            return Ok(());
        };
        if open {
            stream.enable_output().map_err(Error::backend)
        } else {
            stream.disable_output().map_err(Error::backend)
        }
    }
}
