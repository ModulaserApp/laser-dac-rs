//! LaserCube USB DAC streaming backend implementation.

use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::lasercube_usb::dac::Stream;
use crate::protocols::lasercube_usb::error::Error as UsbError;
use crate::protocols::lasercube_usb::protocol::Sample as LasercubeUsbSample;
use crate::protocols::lasercube_usb::{discover_dacs, rusb};
use crate::types::{DacCapabilities, DacType, LaserPoint};

/// LaserCube USB DAC backend (LaserDock).
pub struct LasercubeUsbBackend {
    device: Option<rusb::Device<rusb::Context>>,
    stream: Option<Stream<rusb::Context>>,
    caps: DacCapabilities,
    /// Cached ring buffer capacity from device info.
    ringbuffer_capacity: u32,
    /// Last known ring buffer free space.
    last_free_space: u32,
}

impl LasercubeUsbBackend {
    pub fn new(device: rusb::Device<rusb::Context>) -> Self {
        Self {
            device: Some(device),
            stream: None,
            caps: super::default_capabilities(),
            ringbuffer_capacity: 0,
            last_free_space: 0,
        }
    }

    pub fn from_stream(stream: Stream<rusb::Context>) -> Self {
        let info = stream.info();
        let ringbuffer_capacity = info.ringbuffer_capacity;
        let last_free_space = info.ringbuffer_free_space;
        Self {
            device: None,
            stream: Some(stream),
            caps: super::default_capabilities(),
            ringbuffer_capacity,
            last_free_space,
        }
    }

    pub fn discover_devices() -> Result<Vec<rusb::Device<rusb::Context>>> {
        discover_dacs().map_err(Error::backend)
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

        let info = stream.info();
        self.ringbuffer_capacity = info.ringbuffer_capacity;
        self.last_free_space = info.ringbuffer_free_space;
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

    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        // Query ring buffer free space for backpressure
        let free_space = match stream.ringbuffer_free_space() {
            Ok(space) => space,
            Err(e) => return self.handle_stream_error(e),
        };
        self.last_free_space = free_space;

        if (free_space as usize) < points.len() {
            return Ok(WriteOutcome::WouldBlock);
        }

        let samples: Vec<LasercubeUsbSample> = points.iter().map(|p| p.into()).collect();

        // Re-borrow stream after the free space check
        let stream = self.stream.as_mut().unwrap();
        match stream.write_frame(&samples, pps) {
            Ok(()) => Ok(WriteOutcome::Written),
            Err(e) => self.handle_stream_error(e),
        }
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

    fn queued_points(&self) -> Option<u64> {
        if self.ringbuffer_capacity == 0 {
            return None;
        }
        Some((self.ringbuffer_capacity - self.last_free_space) as u64)
    }
}
