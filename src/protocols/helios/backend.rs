//! Helios DAC streaming backend implementation.

use crate::backend::{DacBackend, FrameSwapBackend, WriteOutcome};
use crate::error::{Error, Result};
use crate::protocols::helios::{
    DeviceStatus, Frame, HeliosDac, HeliosDacController, HeliosDacError, Point as HeliosPoint,
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
        let controller = HeliosDacController::new().map_err(Self::map_err)?;
        controller.list_devices().map_err(Self::map_err)
    }

    /// Take and leak any open USB device handle to prevent a segfault in
    /// `libusb_close()` on macOS when the device was physically disconnected.
    /// This is a bounded leak (~200 bytes) per disconnect event.
    fn leak_handle(&mut self) {
        if let Some(HeliosDac::Open { handle, .. }) = self.dac.take() {
            std::mem::forget(handle);
        }
    }

    /// Classify a Helios DAC error into the appropriate streaming error type.
    ///
    /// Fatal USB errors (`NoDevice`, `Io`, `Pipe`) indicate the device was
    /// physically disconnected and are mapped to `Error::Disconnected` so the
    /// stream layer can enter the reconnection path instead of calling
    /// `disconnect()` on a dead handle.
    fn map_err(e: HeliosDacError) -> Error {
        match &e {
            HeliosDacError::UsbError(
                rusb::Error::NoDevice | rusb::Error::Io | rusb::Error::Pipe,
            ) => Error::disconnected(format!("USB device error: {e}")),
            _ => Error::backend(std::io::Error::other(e.to_string())),
        }
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
        self.leak_handle();
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

impl Drop for HeliosBackend {
    fn drop(&mut self) {
        self.leak_handle();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_err_usb_no_device_is_disconnected() {
        let err = HeliosBackend::map_err(HeliosDacError::UsbError(rusb::Error::NoDevice));
        assert!(err.is_disconnected());
    }

    #[test]
    fn map_err_usb_io_is_disconnected() {
        let err = HeliosBackend::map_err(HeliosDacError::UsbError(rusb::Error::Io));
        assert!(err.is_disconnected());
    }

    #[test]
    fn map_err_usb_pipe_is_disconnected() {
        let err = HeliosBackend::map_err(HeliosDacError::UsbError(rusb::Error::Pipe));
        assert!(err.is_disconnected());
    }

    #[test]
    fn map_err_usb_timeout_is_backend() {
        let err = HeliosBackend::map_err(HeliosDacError::UsbError(rusb::Error::Timeout));
        assert!(!err.is_disconnected());
        assert!(matches!(err, Error::Backend(_)));
    }

    #[test]
    fn map_err_device_not_opened_is_backend() {
        let err = HeliosBackend::map_err(HeliosDacError::DeviceNotOpened);
        assert!(!err.is_disconnected());
        assert!(matches!(err, Error::Backend(_)));
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
