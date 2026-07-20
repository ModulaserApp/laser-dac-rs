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
        if let Some(dac) = self.dac.take() {
            // Leaks the USB handle for an open DAC; an idle DAC drops normally.
            dac.leak_handle();
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

    /// Returns `true` if the error is a USB access-permission failure — on Linux
    /// the device node needs a udev rule before a non-root user can claim it.
    fn is_access_error(e: &HeliosDacError) -> bool {
        matches!(e, HeliosDacError::UsbError(rusb::Error::Access))
    }

    /// Classify a Helios DAC error into the appropriate streaming error type.
    ///
    /// Fatal USB errors (`NoDevice`, `Io`, `Pipe`) indicate the device was
    /// physically disconnected and are mapped to `Error::Disconnected` so the
    /// stream layer can enter the reconnection path instead of calling
    /// `disconnect()` on a dead handle.
    fn map_err(e: HeliosDacError) -> Error {
        if Self::is_access_error(&e) {
            Error::usb_permission_denied("helios")
        } else if Self::is_fatal_usb_error(&e) {
            Error::disconnected(format!("USB device error: {e}"))
        } else {
            Error::backend(std::io::Error::other(e.to_string()))
        }
    }

    fn map_err_with_context(context: impl Into<String>, e: HeliosDacError) -> Error {
        let context = context.into();
        if Self::is_access_error(&e) {
            Error::usb_permission_denied(context)
        } else if Self::is_fatal_usb_error(&e) {
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

    /// Handle a status-poll error. A fatal USB error flags a disconnect and
    /// leaks the (dead) handle so `Drop` does not close it; a non-fatal error is
    /// counted and, once it exceeds [`STATUS_ERROR_LIMIT`] consecutive failures,
    /// closes the handle so the reconnect path can reopen the DAC. Extracted from
    /// `is_ready_for_frame` so the state machine is unit-testable without a live
    /// device.
    fn on_status_error(&mut self, location: &str, e: &HeliosDacError) {
        if Self::is_fatal_usb_error(e) {
            log::warn!("helios: fatal status poll failure at {location}: {e}");
            self.fatal_disconnect = true;
            self.leak_handle();
        } else {
            self.mark_status_error(location, e);
            if self.status_error_count >= STATUS_ERROR_LIMIT {
                log::warn!(
                    "helios: closing backend after {} consecutive status poll failures at {location}: {e}",
                    self.status_error_count
                );
                self.close_handle();
            }
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

    /// Truncate to the device maximum, clamp pps into range, and encode into
    /// `self.frame_buffer`. Returns `(clamped_pps, encoded_points, byte_len)`.
    ///
    /// Oversized frames are truncated (the SDK refuses larger frames and
    /// firmware behavior is undefined) and pps is clamped so the u16 wire field
    /// never wraps; both are rate-limited-logged rather than erroring into a
    /// reconnect loop. Touches no USB device, so the clamp/encode logic is
    /// unit-testable without a connected DAC.
    fn prepare_frame_buffer(&mut self, pps: u32, points: &[LaserPoint]) -> (u32, usize, usize) {
        let max_points = self.caps.max_points_per_chunk;
        let points = if points.len() > max_points {
            self.note_oversize_frame(points.len(), max_points);
            &points[..max_points]
        } else {
            points
        };

        let pps = self.clamp_pps(pps);

        self.point_buffer.clear();
        self.point_buffer
            .extend(points.iter().map(HeliosPoint::from));

        encode_frame_into(
            pps,
            &self.point_buffer,
            WriteFrameFlags::SINGLE_MODE,
            &mut self.frame_buffer,
        );

        (pps, points.len(), self.frame_buffer.len())
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
        matches!(&self.dac, Some(HeliosDac::Open { .. }))
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
        // Always leak an open USB handle at final teardown rather than closing
        // it. `libusb_close()` can segfault on macOS for a handle whose device
        // was surprise-removed, and `Drop` cannot reliably tell a still-present
        // device from one unplugged while idle: an idle unplug is never observed
        // by any USB op, so `fatal_disconnect` stays false even though closing
        // would hit the exact crash this leak works around. Interface release for
        // reopening the DAC in the same process is handled by `disconnect()` /
        // `connect()`, which run on a live backend *before* `Drop` — so leaking
        // here costs only a bounded ~200-byte handle the OS reclaims at process
        // exit. `close_handle()` remains the right choice on those live paths.
        if !self.fatal_disconnect {
            // Reaching teardown with an open handle and no observed disconnect is
            // the ambiguous case above; log it so an idle-unplug leak is visible.
            log::debug!("helios: leaking USB handle at teardown (no observed disconnect)");
        }
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
                self.on_status_error(&location, &e);
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
        let (pps, num_points, byte_len) = self.prepare_frame_buffer(pps, points);

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
                    "helios write_frame bulk transfer (pps={pps}, points={num_points}, bytes={byte_len}, timeout_ms={})",
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

    // -------------------------------------------------------------------------
    // Pure error-classification tests (no device needed).
    // -------------------------------------------------------------------------

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

    #[test]
    fn map_err_usb_access_is_permission_denied() {
        // A permission failure (no udev rule on Linux) must surface as a distinct
        // PermissionDenied error, not a generic backend/disconnect, so consumers
        // can guide the user to grant device access.
        let err = HeliosBackend::map_err(HeliosDacError::UsbError(rusb::Error::Access));
        assert!(err.is_permission_denied());
        assert!(!err.is_disconnected());
        assert!(!matches!(err, Error::Backend(_)));
    }

    #[test]
    fn map_err_ctx_usb_access_is_permission_denied_and_not_fatal() {
        let mut backend = HeliosBackend::new(0);
        let err = backend.map_err_ctx(
            "helios connect",
            HeliosDacError::UsbError(rusb::Error::Access),
        );
        assert!(err.is_permission_denied());
        // Access is not a physical disconnect, so Drop must not be flagged to leak.
        assert!(!backend.fatal_disconnect);
    }

    #[test]
    fn map_err_ctx_fatal_error_flags_fatal_disconnect() {
        let mut backend = HeliosBackend::new(0);
        let err = backend.map_err_ctx("ctx", HeliosDacError::UsbError(rusb::Error::NoDevice));
        assert!(err.is_disconnected());
        assert!(
            backend.fatal_disconnect,
            "a fatal USB error mapped with context must flag Drop to leak the handle"
        );
    }

    #[test]
    fn map_err_ctx_nonfatal_error_leaves_fatal_disconnect_clear() {
        let mut backend = HeliosBackend::new(0);
        let err = backend.map_err_ctx("ctx", HeliosDacError::UsbError(rusb::Error::Timeout));
        assert!(!err.is_disconnected());
        assert!(!backend.fatal_disconnect);
    }

    // -------------------------------------------------------------------------
    // Status-poll state machine (`on_status_error`), driven directly so the
    // fatal/non-fatal classification, counting, and disconnect flagging are
    // covered without a live device.
    // -------------------------------------------------------------------------

    fn assert_fatal_status_error(e: rusb::Error) {
        let mut backend = HeliosBackend::new(0);
        assert!(!backend.fatal_disconnect);
        backend.on_status_error("loc", &HeliosDacError::UsbError(e));
        assert!(
            backend.fatal_disconnect,
            "fatal USB error {e:?} must set fatal_disconnect"
        );
    }

    #[test]
    fn status_error_no_device_flags_fatal_disconnect() {
        assert_fatal_status_error(rusb::Error::NoDevice);
    }

    #[test]
    fn status_error_io_flags_fatal_disconnect() {
        assert_fatal_status_error(rusb::Error::Io);
    }

    #[test]
    fn status_error_pipe_flags_fatal_disconnect() {
        assert_fatal_status_error(rusb::Error::Pipe);
    }

    #[test]
    fn status_error_timeout_is_nonfatal_and_counted() {
        let mut backend = HeliosBackend::new(0);
        backend.on_status_error("loc", &HeliosDacError::UsbError(rusb::Error::Timeout));
        assert!(
            !backend.fatal_disconnect,
            "a timeout is non-fatal and must not flag a fatal disconnect"
        );
        assert_eq!(backend.status_error_count, 1);
    }

    #[test]
    fn repeated_nonfatal_status_errors_reach_close_limit() {
        let mut backend = HeliosBackend::new(0);
        for _ in 0..STATUS_ERROR_LIMIT {
            backend.on_status_error("loc", &HeliosDacError::UsbError(rusb::Error::Timeout));
        }
        assert_eq!(backend.status_error_count, STATUS_ERROR_LIMIT);
        assert!(!backend.fatal_disconnect, "timeouts stay non-fatal");
        // At the limit the handle is closed so the reconnect path can run.
        assert!(!backend.is_connected());
    }

    #[test]
    fn successful_status_poll_resets_nonfatal_error_count() {
        let mut backend = HeliosBackend::new(0);
        backend.on_status_error("loc", &HeliosDacError::UsbError(rusb::Error::Timeout));
        assert_eq!(backend.status_error_count, 1);
        backend.mark_status_ok();
        assert_eq!(backend.status_error_count, 0);
    }

    // -------------------------------------------------------------------------
    // Frame preparation (truncation + pps clamp + encode), driven without a
    // device via `prepare_frame_buffer`.
    // -------------------------------------------------------------------------

    #[test]
    fn oversized_frame_is_truncated_to_device_maximum() {
        let mut backend = HeliosBackend::new(0);
        let max = backend.caps.max_points_per_chunk;

        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535); max + 500];
        let (_pps, num_points, byte_len) = backend.prepare_frame_buffer(30_000, &points);

        // Encoded to exactly `max` points (no transfer-size workaround at 4095).
        assert_eq!(num_points, max);
        assert_eq!(backend.point_buffer.len(), max);
        assert_eq!(byte_len, max * 7 + 5);
        let footer = &backend.frame_buffer[backend.frame_buffer.len() - 5..];
        assert_eq!(u16::from_le_bytes([footer[2], footer[3]]) as usize, max);
        assert_eq!(backend.oversize_clamp_count, 1);
    }

    #[test]
    fn pps_above_caps_is_clamped_before_encoding() {
        let mut backend = HeliosBackend::new(0);
        let pps_max = backend.caps.pps_max;

        // A single point avoids the transfer-size workaround, so the encoded
        // pps equals the clamped value exactly.
        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];
        let (pps, _num, _bytes) = backend.prepare_frame_buffer(10_000_000, &points);
        assert_eq!(pps, pps_max);

        let footer = &backend.frame_buffer[backend.frame_buffer.len() - 5..];
        assert_eq!(u16::from_le_bytes([footer[0], footer[1]]) as u32, pps_max);
        assert_eq!(backend.pps_clamp_count, 1);
    }

    #[test]
    fn pps_below_caps_is_clamped_up() {
        let mut backend = HeliosBackend::new(0);
        let pps_min = backend.caps.pps_min;

        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];
        let (pps, _num, _bytes) = backend.prepare_frame_buffer(1, &points);
        assert_eq!(pps, pps_min);

        let footer = &backend.frame_buffer[backend.frame_buffer.len() - 5..];
        assert_eq!(u16::from_le_bytes([footer[0], footer[1]]) as u32, pps_min);
        assert_eq!(backend.pps_clamp_count, 1);
    }

    // -------------------------------------------------------------------------
    // Disconnected backend rejects writes.
    // -------------------------------------------------------------------------

    #[test]
    fn write_frame_ready_without_device_is_disconnected() {
        let mut backend = HeliosBackend::new(0);
        let points = vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)];
        let err = backend.write_frame_ready(30_000, &points).unwrap_err();
        assert!(err.is_disconnected());
    }

    #[test]
    fn is_connected_is_false_without_a_device() {
        let backend = HeliosBackend::new(0);
        assert!(!backend.is_connected());
    }

    // -------------------------------------------------------------------------
    // Public backend surface driven end-to-end through a fake `UsbEndpoints`.
    //
    // These restore the integration coverage lost when the old `MockOpen`-based
    // tests were deleted: they build a `HeliosDac::from_endpoints(fake)` (no real
    // `rusb::Device`), wrap it in a backend via `from_dac`, and drive
    // `is_ready_for_frame` / `write_frame` through the real call path — status
    // parsing, the `on_status_error` state machine, frame encode + bulk write,
    // and fatal-disconnect flagging — reusing the single fake in `native`.
    // -------------------------------------------------------------------------

    use crate::protocols::helios::native::test_support::{FakeUsb, WriteScript};

    fn one_point() -> Vec<LaserPoint> {
        vec![LaserPoint::new(0.0, 0.0, 65535, 0, 0, 65535)]
    }

    /// Wrap a fake transport in an open backend. The fake is cloned so callers
    /// keep a handle to inspect recorded writes after `from_dac` takes the DAC.
    fn backend_over(fake: &FakeUsb) -> HeliosBackend {
        HeliosBackend::from_dac(HeliosDac::from_endpoints(fake.clone(), Some(1)))
    }

    // (1) is_ready_for_frame happy path: Ready -> true, NotReady -> false.

    #[test]
    fn is_ready_for_frame_true_when_status_ready() {
        let fake = FakeUsb::default();
        fake.script_int_in([Ok(vec![0x83, 1])]); // CONTROL_GET_STATUS -> Ready
        let mut backend = backend_over(&fake);
        assert!(backend.is_ready_for_frame());
        assert_eq!(
            backend.status_error_count, 0,
            "no error recorded on success"
        );
    }

    #[test]
    fn is_ready_for_frame_false_when_status_not_ready() {
        let fake = FakeUsb::default();
        fake.script_int_in([Ok(vec![0x83, 0])]); // CONTROL_GET_STATUS -> NotReady
        let mut backend = backend_over(&fake);
        assert!(!backend.is_ready_for_frame());
        assert_eq!(backend.status_error_count, 0);
    }

    // (2) is_ready_for_frame error branch wires through to on_status_error.

    #[test]
    fn is_ready_for_frame_nonfatal_error_counts_via_on_status_error() {
        let fake = FakeUsb::default();
        fake.script_int_in([Err(rusb::Error::Timeout)]); // status poll fails (non-fatal)
        let mut backend = backend_over(&fake);
        assert!(!backend.is_ready_for_frame());
        // Reached on_status_error: non-fatal timeout is counted, not disconnected.
        assert_eq!(backend.status_error_count, 1);
        assert!(!backend.fatal_disconnect);
    }

    #[test]
    fn is_ready_for_frame_fatal_error_flags_disconnect_via_on_status_error() {
        let fake = FakeUsb::default();
        fake.script_int_in([Err(rusb::Error::NoDevice)]); // fatal: device gone
        let mut backend = backend_over(&fake);
        assert!(!backend.is_ready_for_frame());
        // Reached on_status_error's fatal branch through the real status path.
        assert!(backend.fatal_disconnect);
    }

    // (3) public write_frame success path: Ready -> write_frame_ready -> Written.

    #[test]
    fn write_frame_ready_status_writes_frame_and_reports_written() {
        let fake = FakeUsb::default();
        fake.script_int_in([Ok(vec![0x83, 1])]); // status poll -> Ready
        let mut backend = backend_over(&fake);

        let outcome = backend.write_frame(30_000, &one_point()).unwrap();
        assert_eq!(outcome, WriteOutcome::Written);
        assert_eq!(
            fake.bulk_writes().len(),
            1,
            "exactly one bulk-OUT frame transfer"
        );
        // The wire buffer carries the single encoded point (7 bytes) + 5 footer.
        assert_eq!(fake.bulk_writes()[0].len(), 7 + 5);
    }

    #[test]
    fn write_frame_not_ready_status_would_block_without_writing() {
        let fake = FakeUsb::default();
        fake.script_int_in([Ok(vec![0x83, 0])]); // status poll -> NotReady
        let mut backend = backend_over(&fake);

        let outcome = backend.write_frame(30_000, &one_point()).unwrap();
        assert_eq!(outcome, WriteOutcome::WouldBlock);
        assert!(
            fake.bulk_writes().is_empty(),
            "no frame written when not ready"
        );
    }

    // (4) fatal error during the write sets fatal_disconnect via the real path.

    #[test]
    fn write_frame_fatal_bulk_error_flags_fatal_disconnect() {
        let fake = FakeUsb::default();
        fake.script_int_in([Ok(vec![0x83, 1])]); // status poll -> Ready
        fake.script_bulk_out([WriteScript::Err(rusb::Error::Io)]); // fatal on frame write
        let mut backend = backend_over(&fake);

        let err = backend.write_frame(30_000, &one_point()).unwrap_err();
        assert!(err.is_disconnected());
        assert!(
            backend.fatal_disconnect,
            "a fatal USB error during the write must flag Drop to leak the handle"
        );
    }
}
