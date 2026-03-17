//! Helios DAC streaming backend implementation.

use crate::backend::{DacBackend, FrameSwapBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::helios::{
    DeviceStatus, Frame, HeliosDac, HeliosDacController, Point as HeliosPoint,
};
use crate::types::{DacCapabilities, DacType, LaserPoint};

/// Helios DAC backend (USB).
pub struct HeliosBackend {
    dac: Option<HeliosDac>,
    device_index: usize,
    caps: DacCapabilities,
}

impl HeliosBackend {
    /// Create a new Helios backend for the given device index.
    pub fn new(device_index: usize) -> Self {
        Self {
            dac: None,
            device_index,
            caps: super::default_capabilities(),
        }
    }

    /// Create a backend from an already-discovered DAC.
    pub fn from_dac(dac: HeliosDac) -> Self {
        Self {
            dac: Some(dac),
            device_index: 0,
            caps: super::default_capabilities(),
        }
    }

    /// Discover all Helios DACs on the system.
    pub fn discover() -> Result<Vec<HeliosDac>> {
        let controller = HeliosDacController::new()
            .map_err(|e| Error::backend(std::io::Error::other(e.to_string())))?;
        controller
            .list_devices()
            .map_err(|e| Error::backend(std::io::Error::other(e.to_string())))
    }

    fn map_err(e: crate::protocols::helios::native::HeliosDacError) -> Error {
        Error::backend(std::io::Error::other(e.to_string()))
    }
}

impl DacBackend for HeliosBackend {
    fn dac_type(&self) -> DacType {
        DacType::Helios
    }

    fn caps(&self) -> &DacCapabilities {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        if let Some(dac) = self.dac.take() {
            self.dac = Some(dac.open().map_err(Self::map_err)?);
            return Ok(());
        }

        let controller = HeliosDacController::new().map_err(Self::map_err)?;
        let mut dacs = controller.list_devices().map_err(Self::map_err)?;

        if self.device_index >= dacs.len() {
            return Err(Error::disconnected(format!(
                "Device index {} out of range (found {} devices)",
                self.device_index,
                dacs.len()
            )));
        }

        let dac = dacs
            .remove(self.device_index)
            .open()
            .map_err(Self::map_err)?;
        self.dac = Some(dac);
        Ok(())
    }

    fn disconnect(&mut self) -> Result<()> {
        self.dac = None;
        Ok(())
    }

    fn is_connected(&self) -> bool {
        matches!(self.dac, Some(HeliosDac::Open { .. }))
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(dac) = &self.dac {
            dac.stop().map_err(Self::map_err)?;
        }
        Ok(())
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        if let Some(dac) = &self.dac {
            dac.set_shutter(open).map_err(Self::map_err)?;
        }
        Ok(())
    }
}

impl FrameSwapBackend for HeliosBackend {
    fn frame_capacity(&self) -> usize {
        self.caps.max_points_per_chunk
    }

    fn is_ready_for_frame(&mut self) -> bool {
        let Some(dac) = self.dac.as_mut() else {
            return false;
        };
        matches!(dac.status(), Ok(DeviceStatus::Ready))
    }

    fn write_frame(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let dac = self
            .dac
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        match dac.status().map_err(Self::map_err)? {
            DeviceStatus::Ready => {
                log::trace!("helios ready for frame: points={}", points.len());
            }
            DeviceStatus::NotReady => {
                log::trace!(
                    "helios not ready yet: pending frame attempt points={}",
                    points.len()
                );
                return Ok(WriteOutcome::WouldBlock);
            }
        }

        let helios_points: Vec<HeliosPoint> = points.iter().map(|p| p.into()).collect();
        let helios_frame = Frame::new(pps, helios_points);

        if points.len() <= 64 {
            log::debug!(
                "helios writing small frame: points={}, pps={pps}",
                points.len()
            );
        } else {
            log::trace!("helios writing frame: points={}, pps={pps}", points.len());
        }
        dac.write_frame(helios_frame).map_err(Self::map_err)?;

        Ok(WriteOutcome::Written)
    }
}
