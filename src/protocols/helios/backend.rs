//! Helios DAC streaming backend implementation.

use crate::backend::{DacBackend, FrameSwapBackend, WriteOutcome};
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;
use crate::protocols::helios::{
    bulk_transfer_timeout, encode_frame_into, DeviceStatus, HeliosDac, HeliosDacController,
    HeliosDacError, Point as HeliosPoint, WriteFrameFlags,
};

const STATUS_ERROR_LIMIT: u32 = 50;

/// Helios DAC backend (USB).
pub struct HeliosBackend {
    dac: Option<HeliosDac>,
    device_index: usize,
    caps: DacCapabilities,
    status_error_count: u32,
    /// Pre-allocated buffer for LaserPoint → HeliosPoint conversion.
    point_buffer: Vec<HeliosPoint>,
    /// Pre-allocated buffer for wire-format frame encoding.
    frame_buffer: Vec<u8>,
}

impl HeliosBackend {
    /// Create a new Helios backend for the given device index.
    pub fn new(device_index: usize) -> Self {
        Self {
            dac: None,
            device_index,
            caps: super::default_capabilities(),
            status_error_count: 0,
            point_buffer: Vec::new(),
            frame_buffer: Vec::new(),
        }
    }

    /// Create a backend from an already-discovered DAC.
    pub fn from_dac(dac: HeliosDac) -> Self {
        Self {
            dac: Some(dac),
            device_index: 0,
            caps: super::default_capabilities(),
            status_error_count: 0,
            point_buffer: Vec::new(),
            frame_buffer: Vec::new(),
        }
    }

    /// Discover all Helios DACs on the system.
    pub fn discover() -> Result<Vec<HeliosDac>> {
        let controller = HeliosDacController::new().map_err(Self::map_err)?;
        controller.list_devices().map_err(Self::map_err)
    }

    /// Drop any open USB device handle for an orderly disconnect.
    ///
    /// Non-fatal transport errors, including USB timeouts, need to release the
    /// claimed interface so the reconnect path can open the DAC again in the
    /// same process.
    fn close_handle(&mut self) {
        self.dac = None;
    }

    /// Take and leak any open USB device handle to prevent a segfault in
    /// `libusb_close()` on macOS when the device was physically disconnected.
    /// This is a bounded leak (~200 bytes) per fatal disconnect event.
    fn leak_handle(&mut self) {
        match self.dac.take() {
            Some(HeliosDac::Open { handle, .. }) => {
                std::mem::forget(handle);
            }
            #[cfg(test)]
            Some(HeliosDac::MockOpen(_)) => {
                // No real handle to leak — just clearing self.dac to None is enough.
            }
            _ => {}
        }
    }

    /// Returns `true` if the given Helios error indicates the USB device was
    /// physically disconnected (fatal, unrecoverable without reconnection).
    fn is_fatal_usb_error(e: &HeliosDacError) -> bool {
        matches!(
            e,
            HeliosDacError::UsbError(rusb::Error::NoDevice | rusb::Error::Io | rusb::Error::Pipe)
        )
    }

    /// Classify a Helios DAC error into the appropriate streaming error type.
    ///
    /// Fatal USB errors (`NoDevice`, `Io`, `Pipe`) indicate the device was
    /// physically disconnected and are mapped to `Error::Disconnected` so the
    /// stream layer can enter the reconnection path instead of calling
    /// `disconnect()` on a dead handle.
    fn map_err(e: HeliosDacError) -> Error {
        if Self::is_fatal_usb_error(&e) {
            Error::disconnected(format!("USB device error: {e}"))
        } else {
            Error::backend(std::io::Error::other(e.to_string()))
        }
    }

    fn map_err_with_context(context: impl Into<String>, e: HeliosDacError) -> Error {
        let context = context.into();
        if Self::is_fatal_usb_error(&e) {
            Error::disconnected(format!("{context}: USB device error: {e}"))
        } else {
            Error::backend(std::io::Error::other(format!("{context}: {e}")))
        }
    }

    fn mark_status_ok(&mut self) {
        if self.status_error_count >= 10 {
            log::debug!(
                "helios: status polling recovered after {} failed poll(s)",
                self.status_error_count
            );
        }
        self.status_error_count = 0;
    }

    fn mark_status_error(&mut self, location: &str, e: &HeliosDacError) {
        self.status_error_count += 1;
        if self.status_error_count.is_multiple_of(10) {
            log::debug!(
                "helios: {} consecutive status poll failures at {location}: {e}",
                self.status_error_count
            );
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
        self.close_handle();
        Ok(())
    }

    fn is_connected(&self) -> bool {
        match &self.dac {
            Some(HeliosDac::Open { .. }) => true,
            #[cfg(test)]
            Some(HeliosDac::MockOpen(_)) => true,
            _ => false,
        }
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

impl FrameSwapBackend for HeliosBackend {
    fn frame_capacity(&self) -> usize {
        self.caps.max_points_per_chunk
    }

    fn is_ready_for_frame(&mut self) -> bool {
        let Some(dac) = self.dac.as_mut() else {
            return false;
        };
        let location = dac.usb_location();
        match dac.status() {
            Ok(DeviceStatus::Ready) => {
                self.mark_status_ok();
                true
            }
            Ok(DeviceStatus::NotReady) => {
                self.mark_status_ok();
                false
            }
            Err(e) => {
                if Self::is_fatal_usb_error(&e) {
                    log::warn!("helios: fatal status poll failure at {location}: {e}");
                    self.leak_handle();
                } else {
                    self.mark_status_error(&location, &e);
                    if self.status_error_count >= STATUS_ERROR_LIMIT {
                        log::warn!(
                            "helios: closing backend after {} consecutive status poll failures at {location}: {e}",
                            self.status_error_count
                        );
                        self.close_handle();
                    }
                }
                false
            }
        }
    }

    fn write_frame(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let dac = self
            .dac
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        match dac
            .status()
            .map_err(|e| Self::map_err_with_context("helios write_frame status poll", e))?
        {
            DeviceStatus::Ready => {}
            DeviceStatus::NotReady => {
                return Ok(WriteOutcome::WouldBlock);
            }
        }

        self.write_frame_ready(pps, points)
    }

    fn write_frame_ready(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let dac = self
            .dac
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        self.point_buffer.clear();
        self.point_buffer
            .extend(points.iter().map(HeliosPoint::from));

        encode_frame_into(
            pps,
            &self.point_buffer,
            WriteFrameFlags::SINGLE_MODE,
            &mut self.frame_buffer,
        );

        dac.write_frame_buffer(&self.frame_buffer).map_err(|e| {
            Self::map_err_with_context(
                format!(
                    "helios write_frame bulk transfer (pps={pps}, points={}, bytes={}, timeout_ms={})",
                    points.len(),
                    self.frame_buffer.len(),
                    bulk_transfer_timeout(self.frame_buffer.len()).as_millis()
                ),
                e,
            )
        })?;

        Ok(WriteOutcome::Written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::helios::native::MockUsbState;
    use std::sync::{Arc, Mutex};

    /// Create a `HeliosBackend` backed by a mock USB device.
    fn mock_backend(state: Arc<Mutex<MockUsbState>>) -> HeliosBackend {
        HeliosBackend {
            dac: Some(HeliosDac::MockOpen(state)),
            device_index: 0,
            caps: super::super::default_capabilities(),
            status_error_count: 0,
            point_buffer: Vec::new(),
            frame_buffer: Vec::new(),
        }
    }

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

    // =========================================================================
    // Mock USB device tests
    // =========================================================================

    #[test]
    fn mock_connected_and_ready() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state);

        assert!(backend.is_connected());
        assert!(backend.is_ready_for_frame());
    }

    #[test]
    fn mock_write_frame_succeeds_when_connected() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state);
        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];

        let result = backend.write_frame(30_000, &points);
        assert!(matches!(result, Ok(WriteOutcome::Written)));
    }

    #[test]
    fn fatal_usb_error_in_is_ready_marks_disconnected() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state.clone());

        assert!(backend.is_connected());
        assert!(backend.is_ready_for_frame());

        // Simulate pulling the USB cable.
        state.lock().unwrap().connected = false;

        // is_ready_for_frame should return false AND mark backend disconnected.
        assert!(!backend.is_ready_for_frame());
        assert!(
            !backend.is_connected(),
            "backend must be disconnected after fatal USB error in is_ready_for_frame"
        );
    }

    #[test]
    fn fatal_usb_io_error_in_is_ready_marks_disconnected() {
        let state = Arc::new(Mutex::new(
            MockUsbState::new().with_disconnect_error(|| rusb::Error::Io),
        ));
        let mut backend = mock_backend(state.clone());

        state.lock().unwrap().connected = false;

        assert!(!backend.is_ready_for_frame());
        assert!(
            !backend.is_connected(),
            "Io error should also mark as disconnected"
        );
    }

    #[test]
    fn fatal_usb_pipe_error_in_is_ready_marks_disconnected() {
        let state = Arc::new(Mutex::new(
            MockUsbState::new().with_disconnect_error(|| rusb::Error::Pipe),
        ));
        let mut backend = mock_backend(state.clone());

        state.lock().unwrap().connected = false;

        assert!(!backend.is_ready_for_frame());
        assert!(
            !backend.is_connected(),
            "Pipe error should also mark as disconnected"
        );
    }

    #[test]
    fn nonfatal_usb_error_in_is_ready_stays_connected() {
        let state = Arc::new(Mutex::new(
            MockUsbState::new().with_disconnect_error(|| rusb::Error::Timeout),
        ));
        let mut backend = mock_backend(state.clone());

        state.lock().unwrap().connected = false;

        assert!(!backend.is_ready_for_frame());
        assert!(
            backend.is_connected(),
            "Timeout is non-fatal — backend should remain connected"
        );
    }

    #[test]
    fn repeated_nonfatal_usb_errors_in_is_ready_mark_disconnected() {
        let state = Arc::new(Mutex::new(
            MockUsbState::new().with_disconnect_error(|| rusb::Error::Timeout),
        ));
        let mut backend = mock_backend(state.clone());

        state.lock().unwrap().connected = false;

        for _ in 0..STATUS_ERROR_LIMIT {
            assert!(!backend.is_ready_for_frame());
        }

        assert!(
            !backend.is_connected(),
            "Repeated status timeouts should close the backend so reconnect can run"
        );
    }

    #[test]
    fn successful_status_poll_resets_nonfatal_error_count() {
        let state = Arc::new(Mutex::new(
            MockUsbState::new().with_disconnect_error(|| rusb::Error::Timeout),
        ));
        let mut backend = mock_backend(state.clone());

        state.lock().unwrap().connected = false;
        assert!(!backend.is_ready_for_frame());
        assert_eq!(backend.status_error_count, 1);

        state.lock().unwrap().connected = true;
        assert!(backend.is_ready_for_frame());
        assert_eq!(backend.status_error_count, 0);
    }

    #[test]
    fn write_frame_returns_disconnected_on_fatal_usb_error() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state.clone());
        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];

        state.lock().unwrap().connected = false;

        let err = backend.write_frame(30_000, &points).unwrap_err();
        assert!(err.is_disconnected());
    }
}
