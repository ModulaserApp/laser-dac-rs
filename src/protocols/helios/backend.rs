//! Helios DAC streaming backend implementation.

use crate::backend::{StreamBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::helios::{
    DeviceStatus, Frame, HeliosDac, HeliosDacController, Point as HeliosPoint,
};
use crate::types::{caps_for_dac_type, Caps, DacType, LaserPoint};

/// Helios DAC backend (USB).
pub struct HeliosBackend {
    dac: Option<HeliosDac>,
    device_index: usize,
    caps: Caps,
}

impl HeliosBackend {
    /// Create a new Helios backend for the given device index.
    pub fn new(device_index: usize) -> Self {
        Self {
            dac: None,
            device_index,
            caps: caps_for_dac_type(&DacType::Helios),
        }
    }

    /// Create a backend from an already-discovered DAC.
    pub fn from_dac(dac: HeliosDac) -> Self {
        Self {
            dac: Some(dac),
            device_index: 0,
            caps: caps_for_dac_type(&DacType::Helios),
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
}

impl StreamBackend for HeliosBackend {
    fn dac_type(&self) -> DacType {
        DacType::Helios
    }

    fn caps(&self) -> &Caps {
        &self.caps
    }

    fn connect(&mut self) -> Result<()> {
        let map_err = |e: crate::protocols::helios::native::HeliosDacError| {
            Error::backend(std::io::Error::other(e.to_string()))
        };

        if let Some(dac) = self.dac.take() {
            self.dac = Some(dac.open().map_err(map_err)?);
            return Ok(());
        }

        let controller = HeliosDacController::new().map_err(map_err)?;
        let mut dacs = controller.list_devices().map_err(map_err)?;

        if self.device_index >= dacs.len() {
            return Err(Error::disconnected(format!(
                "Device index {} out of range (found {} devices)",
                self.device_index,
                dacs.len()
            )));
        }

        let dac = dacs.remove(self.device_index).open().map_err(map_err)?;
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

    fn try_write_chunk(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let dac = self
            .dac
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        let map_err = |e: crate::protocols::helios::native::HeliosDacError| {
            Error::backend(std::io::Error::other(e.to_string()))
        };

        match dac.status().map_err(map_err)? {
            DeviceStatus::Ready => {}
            DeviceStatus::NotReady => return Ok(WriteOutcome::WouldBlock),
        }

        let helios_points: Vec<HeliosPoint> = points.iter().map(|p| p.into()).collect();
        let helios_frame = Frame::new(pps, helios_points);

        dac.write_frame(helios_frame).map_err(map_err)?;

        Ok(WriteOutcome::Written)
    }

    fn stop(&mut self) -> Result<()> {
        if let Some(dac) = &self.dac {
            dac.stop()
                .map_err(|e| Error::backend(std::io::Error::other(e.to_string())))?;
        }
        Ok(())
    }

    fn set_shutter(&mut self, _open: bool) -> Result<()> {
        // Helios doesn't have explicit shutter control
        Ok(())
    }
}
