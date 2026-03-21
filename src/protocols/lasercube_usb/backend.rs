//! LaserCube USB DAC streaming backend implementation.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::lasercube_usb::dac::Stream;
use crate::protocols::lasercube_usb::error::Error as UsbError;
use crate::protocols::lasercube_usb::protocol::Sample as LasercubeUsbSample;
use crate::protocols::lasercube_usb::{rusb, DacController};
use crate::types::{DacCapabilities, DacType, LaserPoint};

/// LaserCube USB DAC backend (LaserDock).
pub struct LasercubeUsbBackend {
    device: Option<rusb::Device<rusb::Context>>,
    stream: Option<Stream<rusb::Context>>,
    caps: DacCapabilities,
    /// Pre-allocated conversion buffer (avoids per-write heap allocation).
    point_buffer: Vec<LasercubeUsbSample>,
}

impl LasercubeUsbBackend {
    pub fn new(device: rusb::Device<rusb::Context>) -> Self {
        Self {
            device: Some(device),
            stream: None,
            caps: super::default_capabilities(),
            point_buffer: Vec::new(),
        }
    }

    pub fn from_stream(stream: Stream<rusb::Context>) -> Self {
        Self {
            device: None,
            stream: Some(stream),
            caps: super::default_capabilities(),
            point_buffer: Vec::new(),
        }
    }

    pub fn discover_devices() -> Result<Vec<rusb::Device<rusb::Context>>> {
        let controller = DacController::new().map_err(Error::backend)?;
        controller.list_devices().map_err(Error::backend)
    }

    /// Handle a stream error by classifying it as fatal (disconnected) or transient.
    ///
    /// Fatal USB errors (`NoDevice`, `Io`, `Pipe`) clear the stream and return
    /// `Error::disconnected`. Transient errors (`Timeout`) return `Error::backend`.
    fn handle_stream_error<R>(&mut self, err: UsbError) -> Result<R> {
        match &err {
            UsbError::Usb(usb_err) => match usb_err {
                rusb::Error::NoDevice | rusb::Error::Io | rusb::Error::Pipe => {
                    self.stream = None;
                    Err(Error::disconnected(format!("USB device error: {err}")))
                }
                rusb::Error::Timeout => Err(Error::backend(err)),
                _ => Err(Error::backend(err)),
            },
            UsbError::DeviceNotOpened => {
                self.stream = None;
                Err(Error::disconnected(format!("{err}")))
            }
            _ => Err(Error::backend(err)),
        }
    }
}

impl DacBackend for LasercubeUsbBackend {
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

        let info = stream.info();
        if info.max_dac_rate > 0 {
            self.caps.pps_max = info.max_dac_rate;
        }

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

    fn stop(&mut self) -> Result<()> {
        let Some(stream) = &mut self.stream else {
            return Ok(());
        };
        match stream.stop() {
            Ok(()) => Ok(()),
            Err(e) => self.handle_stream_error(e),
        }
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        let Some(stream) = &mut self.stream else {
            return Ok(());
        };
        let result = if open {
            stream.enable_output()
        } else {
            stream.disable_output()
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => self.handle_stream_error(e),
        }
    }
}

impl FifoBackend for LasercubeUsbBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        self.point_buffer.clear();
        self.point_buffer
            .extend(points.iter().map(LasercubeUsbSample::from));

        match stream.write_frame(&self.point_buffer, pps) {
            Ok(()) => Ok(WriteOutcome::Written),
            Err(e) => self.handle_stream_error(e),
        }
    }

    fn queued_points(&self) -> Option<u64> {
        None
    }
}
