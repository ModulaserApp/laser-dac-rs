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
    /// Physical USB location of this backend's device, recorded so a fallback
    /// re-enumeration in `connect()` can re-match the same unit rather than
    /// blindly picking device index 0.
    usb_location: Option<String>,
    caps: DacCapabilities,
    status_error_count: u32,
    /// Set once a fatal (physical-disconnect) USB error is observed. Gates the
    /// handle leak in `Drop`: a physically-gone device is leaked (macOS
    /// anti-segfault); a still-present device is closed normally.
    fatal_disconnect: bool,
    /// Rate-limit counter for oversized-frame truncation warnings.
    oversize_clamp_count: u32,
    /// Rate-limit counter for out-of-range pps clamp logs.
    pps_clamp_count: u32,
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
            usb_location: None,
            caps: super::default_capabilities(),
            status_error_count: 0,
            fatal_disconnect: false,
            oversize_clamp_count: 0,
            pps_clamp_count: 0,
            point_buffer: Vec::new(),
            frame_buffer: Vec::new(),
        }
    }

    /// Create a backend from an already-discovered DAC.
    pub fn from_dac(dac: HeliosDac) -> Self {
        let usb_location = Some(dac.usb_location());
        Self {
            dac: Some(dac),
            device_index: 0,
            usb_location,
            caps: super::default_capabilities(),
            status_error_count: 0,
            fatal_disconnect: false,
            oversize_clamp_count: 0,
            pps_clamp_count: 0,
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

    /// Map a Helios error with context, recording a fatal (physical-disconnect)
    /// error so `Drop` leaks rather than closes the handle.
    fn map_err_ctx(&mut self, context: impl Into<String>, e: HeliosDacError) -> Error {
        if Self::is_fatal_usb_error(&e) {
            self.fatal_disconnect = true;
        }
        Self::map_err_with_context(context, e)
    }

    /// Rate-limited warning when an oversized frame is truncated to the device
    /// maximum. Keeps output alive rather than erroring into a disconnect loop.
    fn note_oversize_frame(&mut self, requested: usize, max: usize) {
        self.oversize_clamp_count += 1;
        if self.oversize_clamp_count == 1 || self.oversize_clamp_count.is_multiple_of(256) {
            log::warn!(
                "helios: frame of {requested} points exceeds device maximum {max}; truncating (occurrence {})",
                self.oversize_clamp_count
            );
        }
    }

    /// Clamp `pps` into the device's advertised range. The wire encoder packs
    /// pps into a u16, so an out-of-range rate would otherwise wrap to garbage.
    /// Logs (rate-limited) when a clamp actually changes the value.
    fn clamp_pps(&mut self, pps: u32) -> u32 {
        let clamped = pps.clamp(self.caps.pps_min, self.caps.pps_max);
        if clamped != pps {
            self.pps_clamp_count += 1;
            if self.pps_clamp_count == 1 || self.pps_clamp_count.is_multiple_of(256) {
                log::debug!(
                    "helios: pps {pps} out of range [{}, {}]; clamping to {clamped} (occurrence {})",
                    self.caps.pps_min,
                    self.caps.pps_max,
                    self.pps_clamp_count
                );
            }
        }
        clamped
    }

    /// The firmware version probed at open time, if connected. For diagnostics.
    pub fn firmware_version(&self) -> Option<u32> {
        self.dac
            .as_ref()
            .and_then(HeliosDac::probed_firmware_version)
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
        // A fresh connect attempt: clear stale disconnect/error state.
        self.status_error_count = 0;
        self.fatal_disconnect = false;

        if let Some(dac) = self.dac.take() {
            let opened = dac.open().map_err(Self::map_err)?;
            self.usb_location = Some(opened.usb_location());
            self.dac = Some(opened);
            return Ok(());
        }

        let controller = HeliosDacController::new().map_err(Self::map_err)?;
        let dacs = controller.list_devices().map_err(Self::map_err)?;

        // Prefer re-matching the previously connected physical USB location so a
        // fallback re-enumeration re-binds the same unit instead of hardcoding
        // device index 0 (which could grab a different projector in a multi-DAC
        // rig). Fall back to the configured device index when no location is
        // stored yet (fresh `new(index)` backend).
        let dac = if let Some(target) = self.usb_location.clone() {
            dacs.into_iter()
                .find(|d| d.usb_location() == target)
                .ok_or_else(|| {
                    Error::disconnected(format!("Helios device at USB location {target} not found"))
                })?
        } else {
            let mut dacs = dacs;
            if self.device_index >= dacs.len() {
                return Err(Error::disconnected(format!(
                    "Device index {} out of range (found {} devices)",
                    self.device_index,
                    dacs.len()
                )));
            }
            dacs.remove(self.device_index)
        };

        let opened = dac.open().map_err(Self::map_err)?;
        self.usb_location = Some(opened.usb_location());
        self.dac = Some(opened);
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
        let result = match &self.dac {
            Some(dac) => dac.stop(),
            None => return Ok(()),
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(self.map_err_ctx("helios stop", e)),
        }
    }

    fn set_shutter(&mut self, open: bool) -> Result<()> {
        let result = match &self.dac {
            Some(dac) => dac.set_shutter(open),
            None => return Ok(()),
        };
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(self.map_err_ctx("helios set_shutter", e)),
        }
    }
}

impl Drop for HeliosBackend {
    fn drop(&mut self) {
        if self.fatal_disconnect {
            // The device was physically disconnected: `libusb_close()` can
            // segfault on macOS for a handle whose device is no longer present,
            // so leak the claimed handle instead (a bounded ~200-byte leak).
            self.leak_handle();
        } else {
            // Graceful teardown of a still-present device: close normally so the
            // claimed interface is released and the DAC can be reopened in the
            // same process (avoids LIBUSB_ERROR_BUSY). The macOS close segfault
            // only affects handles whose device has been unplugged — that case
            // is handled by the branch above — so closing a present device here
            // is safe on all platforms.
            self.close_handle();
        }
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
                    self.fatal_disconnect = true;
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
        let status = {
            let dac = self
                .dac
                .as_ref()
                .ok_or_else(|| Error::disconnected("Not connected"))?;
            dac.status()
        };

        match status {
            Ok(DeviceStatus::Ready) => {}
            Ok(DeviceStatus::NotReady) => return Ok(WriteOutcome::WouldBlock),
            Err(e) => return Err(self.map_err_ctx("helios write_frame status poll", e)),
        }

        self.write_frame_ready(pps, points)
    }

    fn write_frame_ready(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        // Truncate oversized frames to the device maximum. The SDK refuses
        // larger frames and firmware behavior is undefined; keep output alive
        // with a rate-limited warning rather than erroring into reconnect.
        let max_points = self.caps.max_points_per_chunk;
        let points = if points.len() > max_points {
            self.note_oversize_frame(points.len(), max_points);
            &points[..max_points]
        } else {
            points
        };

        // Clamp pps into the device range so the u16 wire field never wraps.
        let pps = self.clamp_pps(pps);

        if self.dac.is_none() {
            return Err(Error::disconnected("Not connected"));
        }

        self.point_buffer.clear();
        self.point_buffer
            .extend(points.iter().map(HeliosPoint::from));

        encode_frame_into(
            pps,
            &self.point_buffer,
            WriteFrameFlags::SINGLE_MODE,
            &mut self.frame_buffer,
        );

        let byte_len = self.frame_buffer.len();
        let write_result = {
            let dac = self
                .dac
                .as_mut()
                .ok_or_else(|| Error::disconnected("Not connected"))?;
            dac.write_frame_buffer(&self.frame_buffer)
        };

        match write_result {
            Ok(()) => Ok(WriteOutcome::Written),
            Err(e) => {
                let context = format!(
                    "helios write_frame bulk transfer (pps={pps}, points={}, bytes={byte_len}, timeout_ms={})",
                    points.len(),
                    bulk_transfer_timeout(byte_len).as_millis()
                );
                Err(self.map_err_ctx(context, e))
            }
        }
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
            usb_location: Some("mock".into()),
            caps: super::super::default_capabilities(),
            status_error_count: 0,
            fatal_disconnect: false,
            oversize_clamp_count: 0,
            pps_clamp_count: 0,
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
    fn oversized_frame_is_truncated_to_device_maximum() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state);
        let max = backend.caps.max_points_per_chunk;

        // A frame larger than the device maximum must not be sent unclamped.
        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535); max + 500];
        let result = backend.write_frame(30_000, &points);
        assert!(matches!(result, Ok(WriteOutcome::Written)));

        // Encoded to exactly `max` points (no transfer-size workaround at 4095).
        assert_eq!(backend.point_buffer.len(), max);
        assert_eq!(backend.frame_buffer.len(), max * 7 + 5);
        let footer = &backend.frame_buffer[backend.frame_buffer.len() - 5..];
        assert_eq!(u16::from_le_bytes([footer[2], footer[3]]) as usize, max);
        assert_eq!(backend.oversize_clamp_count, 1);
    }

    #[test]
    fn pps_above_caps_is_clamped_before_encoding() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state);
        let pps_max = backend.caps.pps_max;

        // A single point avoids the transfer-size workaround, so the encoded
        // pps equals the clamped value exactly.
        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];
        let result = backend.write_frame(10_000_000, &points);
        assert!(matches!(result, Ok(WriteOutcome::Written)));

        let footer = &backend.frame_buffer[backend.frame_buffer.len() - 5..];
        assert_eq!(u16::from_le_bytes([footer[0], footer[1]]) as u32, pps_max);
        assert_eq!(backend.pps_clamp_count, 1);
    }

    #[test]
    fn pps_below_caps_is_clamped_up() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state);
        let pps_min = backend.caps.pps_min;

        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];
        let result = backend.write_frame(1, &points);
        assert!(matches!(result, Ok(WriteOutcome::Written)));

        let footer = &backend.frame_buffer[backend.frame_buffer.len() - 5..];
        assert_eq!(u16::from_le_bytes([footer[0], footer[1]]) as u32, pps_min);
        assert_eq!(backend.pps_clamp_count, 1);
    }

    #[test]
    fn write_frame_returns_disconnected_on_fatal_usb_error() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state.clone());
        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];

        state.lock().unwrap().connected = false;

        let err = backend.write_frame(30_000, &points).unwrap_err();
        assert!(err.is_disconnected());
        // The fatal error must flag Drop to leak (not close) the dead handle.
        assert!(backend.fatal_disconnect);
    }

    #[test]
    fn fatal_status_error_sets_fatal_disconnect_flag() {
        let state = Arc::new(Mutex::new(MockUsbState::new()));
        let mut backend = mock_backend(state.clone());

        assert!(!backend.fatal_disconnect);
        state.lock().unwrap().connected = false;
        assert!(!backend.is_ready_for_frame());
        assert!(
            backend.fatal_disconnect,
            "a fatal (NoDevice) status poll must set fatal_disconnect"
        );
    }

    #[test]
    fn nonfatal_status_error_leaves_fatal_disconnect_clear() {
        let state = Arc::new(Mutex::new(
            MockUsbState::new().with_disconnect_error(|| rusb::Error::Timeout),
        ));
        let mut backend = mock_backend(state.clone());

        state.lock().unwrap().connected = false;
        assert!(!backend.is_ready_for_frame());
        assert!(
            !backend.fatal_disconnect,
            "a non-fatal timeout must not flag a fatal disconnect"
        );
    }
}
