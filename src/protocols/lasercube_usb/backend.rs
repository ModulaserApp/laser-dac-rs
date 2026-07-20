//! LaserCube USB DAC streaming backend implementation.

use crate::backend::{DacBackend, FifoBackend, WriteOutcome};
use crate::buffer_estimate::{BufferEstimator, SoftwareDecayEstimator};
use crate::device::{DacCapabilities, DacType};
use crate::error::{Error, Result};
use crate::point::LaserPoint;
use crate::protocols::lasercube_usb::dac::Stream;
use crate::protocols::lasercube_usb::error::Error as UsbError;
use crate::protocols::lasercube_usb::protocol::Sample as LaserCubeUsbSample;
use crate::protocols::lasercube_usb::{rusb, DacController};
use std::time::Instant;

/// LaserCube USB DAC backend (LaserDock).
pub struct LaserCubeUsbBackend {
    device: Option<rusb::Device<rusb::Context>>,
    stream: Option<Stream<rusb::DeviceHandle<rusb::Context>>>,
    caps: DacCapabilities,
    /// Pre-allocated conversion buffer (avoids per-write heap allocation).
    point_buffer: Vec<LaserCubeUsbSample>,
    /// Software-only buffer estimator. Driven by `record_send` from inside
    /// `try_write_points`; not yet consulted by the adapter (Phase 1).
    estimator: SoftwareDecayEstimator,
}

impl LaserCubeUsbBackend {
    pub fn new(device: rusb::Device<rusb::Context>) -> Self {
        Self {
            device: Some(device),
            stream: None,
            caps: super::default_capabilities(),
            point_buffer: Vec::new(),
            estimator: SoftwareDecayEstimator::new(),
        }
    }

    pub fn from_stream(stream: Stream<rusb::DeviceHandle<rusb::Context>>) -> Self {
        Self {
            device: None,
            stream: Some(stream),
            caps: super::default_capabilities(),
            point_buffer: Vec::new(),
            estimator: SoftwareDecayEstimator::new(),
        }
    }

    pub fn discover_devices() -> Result<Vec<rusb::Device<rusb::Context>>> {
        let controller = DacController::new().map_err(Error::backend)?;
        controller.list_devices().map_err(Error::backend)
    }

    /// Handle a stream error by classifying it as fatal (disconnected) or
    /// transient. Fatal errors clear the stream and report `disconnected`;
    /// transient errors are surfaced as a backend error with the stream intact.
    fn handle_stream_error<R>(&mut self, err: UsbError) -> Result<R> {
        match classify_stream_error(&err) {
            StreamErrorClass::Fatal => {
                self.stream = None;
                Err(Error::disconnected(format!("USB device error: {err}")))
            }
            StreamErrorClass::Transient => Err(Error::backend(err)),
        }
    }
}

/// Whether a USB stream error means the device is gone (fatal) or the operation
/// can be retried against the same stream (transient).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamErrorClass {
    Fatal,
    Transient,
}

/// Classify a USB stream error. `NoDevice`/`Io`/`Pipe` and a not-opened stream
/// are fatal (the device is unusable); everything else — notably `Timeout` — is
/// transient. Pure and hardware-free so it can be unit-tested exhaustively.
fn classify_stream_error(err: &UsbError) -> StreamErrorClass {
    match err {
        UsbError::Usb(rusb::Error::NoDevice | rusb::Error::Io | rusb::Error::Pipe) => {
            StreamErrorClass::Fatal
        }
        UsbError::DeviceNotOpened => StreamErrorClass::Fatal,
        _ => StreamErrorClass::Transient,
    }
}

/// Map an error from opening the USB stream into the public error type.
///
/// A `rusb::Error::Access` means the OS denied access to the device — on Linux
/// the LaserCube/LaserDock needs a udev rule granting the user access to the
/// device node. Surfacing it as [`Error::PermissionDenied`] (rather than a
/// generic backend error) lets consumers guide the user to the fix. Every other
/// open failure stays a backend error.
fn map_open_error(err: UsbError) -> Error {
    if matches!(err, UsbError::Usb(rusb::Error::Access)) {
        Error::usb_permission_denied("laser-cube usb")
    } else {
        Error::backend(err)
    }
}

impl DacBackend for LaserCubeUsbBackend {
    fn dac_type(&self) -> DacType {
        DacType::LaserCubeUsb
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

        let mut stream = Stream::open(device).map_err(map_open_error)?;

        stream.enable_output().map_err(map_open_error)?;

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

impl FifoBackend for LaserCubeUsbBackend {
    fn try_write_points(&mut self, pps: u32, points: &[LaserPoint]) -> Result<WriteOutcome> {
        let stream = self
            .stream
            .as_mut()
            .ok_or_else(|| Error::disconnected("Not connected"))?;

        self.point_buffer.clear();
        self.point_buffer
            .extend(points.iter().map(LaserCubeUsbSample::from));

        let n = self.point_buffer.len();
        match stream.write_frame(&self.point_buffer, pps) {
            // `write_frame` clamps the requested pps to the device max and
            // returns the rate it actually programmed. Feed the estimator that
            // effective rate, not the caller's request — otherwise, whenever
            // `pps > max_dac_rate`, the software buffer model drains faster than
            // the hardware and can blank the tail of a frame early.
            Ok(effective_pps) => {
                self.estimator
                    .record_send(Instant::now(), n as u64, effective_pps);
                Ok(WriteOutcome::Written)
            }
            Err(e) => self.handle_stream_error(e),
        }
    }

    fn estimator(&self) -> &dyn BufferEstimator {
        &self.estimator
    }

    fn reset_device_buffer(&mut self) -> Result<()> {
        let Some(stream) = &mut self.stream else {
            return Ok(());
        };
        match stream.clear_ringbuffer() {
            Ok(()) => {
                // The hardware ring is now empty; drop the software estimate so
                // it agrees with the device rather than replaying pre-disarm
                // fullness.
                self.estimator.reset(Instant::now());
                Ok(())
            }
            Err(e) => self.handle_stream_error(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{classify_stream_error, map_open_error, StreamErrorClass};
    use crate::error::Error;
    use crate::protocols::lasercube_usb::error::Error as UsbError;
    use crate::protocols::lasercube_usb::rusb;

    #[test]
    fn classify_stream_error_covers_every_variant() {
        use StreamErrorClass::{Fatal, Transient};
        let cases = [
            (UsbError::Usb(rusb::Error::NoDevice), Fatal),
            (UsbError::Usb(rusb::Error::Io), Fatal),
            (UsbError::Usb(rusb::Error::Pipe), Fatal),
            (UsbError::DeviceNotOpened, Fatal),
            (UsbError::Usb(rusb::Error::Timeout), Transient),
            (UsbError::Usb(rusb::Error::Busy), Transient),
            (UsbError::Usb(rusb::Error::Access), Transient),
            (UsbError::Usb(rusb::Error::Other), Transient),
            (UsbError::InvalidResponse, Transient),
        ];
        for (err, expected) in cases {
            assert_eq!(
                classify_stream_error(&err),
                expected,
                "classification of {err:?}"
            );
        }
    }

    #[test]
    fn map_open_error_access_is_permission_denied() {
        // A denied open (no udev rule on Linux) surfaces as PermissionDenied so
        // the consumer can guide the user, not a generic backend error.
        let err = map_open_error(UsbError::Usb(rusb::Error::Access));
        assert!(err.is_permission_denied());
        assert!(!matches!(err, Error::Backend(_)));
    }

    #[test]
    fn map_open_error_other_stays_backend() {
        let err = map_open_error(UsbError::Usb(rusb::Error::Timeout));
        assert!(!err.is_permission_denied());
        assert!(matches!(err, Error::Backend(_)));
    }
}
