//! Helios USB communication.

use std::time::Duration;

use super::{DeviceStatus, Frame};
use crate::protocols::usb_transfer::UsbEndpoints;
use rusb::{Context, Device, UsbContext};
use thiserror::Error;

type Result<T> = std::result::Result<T, HeliosDacError>;

const SDK_VERSION: u8 = 6;

const HELIOS_VID: u16 = 0x1209;
const HELIOS_PID: u16 = 0xE500;

/// Maximum points per frame accepted by the Helios firmware/SDK. Frames larger
/// than this are refused by the official SDK and have undefined firmware
/// behavior, so the encoder hard-caps to this length.
pub const HELIOS_MAX_POINTS: usize = 4095;

// USB endpoints
const ENDPOINT_BULK_OUT: u8 = 0x02;
const ENDPOINT_INT_OUT: u8 = 0x06;
const ENDPOINT_INT_IN: u8 = 0x83;

// Init timing — mirrors the official Helios C++ SDK. After
// `set_alternate_setting`, some devices (notably on macOS) need ~100ms before
// the first interrupt transfer succeeds; a 16ms first-attempt write usually
// times out, then succeeds on retry. See `open()` for details.
const INIT_SETTLE: Duration = Duration::from_millis(100);
const INIT_DRAIN_TIMEOUT: Duration = Duration::from_millis(5);
const INIT_INTERRUPT_TIMEOUT: Duration = Duration::from_millis(32);
const INIT_FW_OUT_ATTEMPTS: usize = 2;
const INIT_FW_IN_ATTEMPTS: usize = 3;

// Interrupt control bytes
const CONTROL_STOP: u8 = 0x01;
const CONTROL_SET_SHUTTER: u8 = 0x02;
const CONTROL_GET_STATUS: u8 = 0x03;
const CONTROL_GET_FIRMWARE_VERSION: u8 = 0x04;
const CONTROL_GET_NAME: u8 = 0x05;
const CONTROL_SEND_SDK_VERSION: u8 = 0x07;

/// Controller for discovering and managing Helios DACs.
pub struct HeliosDacController {
    context: rusb::Context,
}

impl HeliosDacController {
    /// Create a new Helios DAC controller.
    pub fn new() -> Result<Self> {
        Ok(HeliosDacController {
            context: rusb::Context::new()?,
        })
    }

    /// List all connected Helios DAC devices.
    pub fn list_devices(&self) -> Result<Vec<HeliosDac>> {
        let dacs = self
            .context
            .devices()?
            .iter()
            .filter_map(|device| {
                let descriptor = device.device_descriptor().ok()?;
                (descriptor.vendor_id() == HELIOS_VID && descriptor.product_id() == HELIOS_PID)
                    .then(|| device.into())
            })
            .collect();
        Ok(dacs)
    }
}

/// USB transfer engine for an open Helios DAC, generic over the [`UsbEndpoints`]
/// seam.
///
/// Owns the USB handle and every request/response operation the device path
/// uses: bulk frame writes (with STALL/`clear_halt` recovery and short-write
/// detection), interrupt control round-trips, and the init firmware probe. When
/// stored in an open [`HeliosDac`] it is boxed as
/// `HeliosComm<Box<dyn UsbEndpoints + Send>>` (the concrete `rusb::DeviceHandle`
/// in production); tests inject a fake `UsbEndpoints` to exercise this logic
/// without hardware — the USB analogue of the LaserCube stream's fake device.
struct HeliosComm<H: UsbEndpoints> {
    /// USB endpoint transport (real `rusb::DeviceHandle` in production).
    handle: H,
    /// Firmware version probed during [`HeliosDac::open`], for diagnostics.
    firmware_version: Option<u32>,
}

impl<H: UsbEndpoints> HeliosComm<H> {
    /// Firmware-version probe used during init. Retries the OUT command up to
    /// `INIT_FW_OUT_ATTEMPTS` times and the IN response up to
    /// `INIT_FW_IN_ATTEMPTS` per OUT, all with `INIT_INTERRUPT_TIMEOUT`.
    fn probe_firmware_version(&self) -> Result<u32> {
        let ctrl_buffer = [CONTROL_GET_FIRMWARE_VERSION, 0];
        let mut last_err: Option<rusb::Error> = None;

        for out_attempt in 1..=INIT_FW_OUT_ATTEMPTS {
            match self.handle.write_interrupt(
                ENDPOINT_INT_OUT,
                &ctrl_buffer,
                INIT_INTERRUPT_TIMEOUT,
            ) {
                Ok(2) => {}
                Ok(_) => {
                    last_err = Some(rusb::Error::Io);
                    log::debug!(
                        "helios: firmware probe OUT short-write (attempt {out_attempt}/{INIT_FW_OUT_ATTEMPTS})"
                    );
                    continue;
                }
                Err(e) => {
                    last_err = Some(e);
                    log::debug!(
                        "helios: firmware probe OUT failed (attempt {out_attempt}/{INIT_FW_OUT_ATTEMPTS}): {e:?}"
                    );
                    continue;
                }
            }

            for in_attempt in 1..=INIT_FW_IN_ATTEMPTS {
                let mut buf = [0u8; 32];
                match self
                    .handle
                    .read_interrupt(ENDPOINT_INT_IN, &mut buf, INIT_INTERRUPT_TIMEOUT)
                {
                    Ok(size) => match &buf[0..size] {
                        [0x84, b0, b1, b2, b3, ..] => {
                            return Ok(u32::from_le_bytes([*b0, *b1, *b2, *b3]));
                        }
                        _ => {
                            log::debug!(
                                "helios: firmware probe IN unexpected reply (out {out_attempt}/{INIT_FW_OUT_ATTEMPTS}, in {in_attempt}/{INIT_FW_IN_ATTEMPTS}): {:?}",
                                &buf[0..size]
                            );
                            continue;
                        }
                    },
                    Err(e) => {
                        last_err = Some(e);
                        log::debug!(
                            "helios: firmware probe IN failed (out {out_attempt}/{INIT_FW_OUT_ATTEMPTS}, in {in_attempt}/{INIT_FW_IN_ATTEMPTS}): {e:?}"
                        );
                    }
                }
            }
        }

        Err(HeliosDacError::UsbError(
            last_err.unwrap_or(rusb::Error::Timeout),
        ))
    }

    /// Drain any stale interrupt-IN responses so the next request/response pair
    /// resyncs. Mirrors the drain step in [`HeliosDac::open`]; called after an
    /// unexpected reply so a lingering response does not corrupt later reads.
    fn drain_stale_responses(&self) {
        let mut drain_buf = [0u8; 32];
        while self
            .handle
            .read_interrupt(ENDPOINT_INT_IN, &mut drain_buf, INIT_DRAIN_TIMEOUT)
            .is_ok()
        {}
    }

    /// Encode and output a frame to the DAC.
    fn write_frame(&self, frame: Frame) -> Result<()> {
        let frame_buffer = encode_frame(frame);
        self.write_bulk_all(&frame_buffer)
    }

    /// Write a pre-encoded frame buffer to the DAC.
    fn write_frame_buffer(&self, buf: &[u8]) -> Result<()> {
        self.write_bulk_all(buf)
    }

    /// Send a full frame buffer over the bulk-OUT endpoint.
    ///
    /// A partial transfer (fewer bytes accepted than submitted) yields a
    /// truncated, corrupted frame on the device, so it is reported as an I/O
    /// error rather than `Ok(())`. On a `Pipe` (endpoint STALL) the halt is
    /// cleared once and the write retried before the error escalates.
    fn write_bulk_all(&self, buf: &[u8]) -> Result<()> {
        let timeout = bulk_transfer_timeout(buf.len());

        match self.handle.write_bulk(ENDPOINT_BULK_OUT, buf, timeout) {
            Ok(n) if n == buf.len() => Ok(()),
            Ok(n) => {
                log::debug!("helios: bulk-OUT short-write ({n}/{} bytes)", buf.len());
                Err(HeliosDacError::UsbError(rusb::Error::Io))
            }
            Err(rusb::Error::Pipe) => {
                // STALL: try clearing the halt condition once before treating
                // the endpoint as dead.
                log::debug!("helios: bulk-OUT stalled (Pipe); clearing halt and retrying once");
                self.handle.clear_halt(ENDPOINT_BULK_OUT)?;
                match self.handle.write_bulk(ENDPOINT_BULK_OUT, buf, timeout)? {
                    n if n == buf.len() => Ok(()),
                    n => {
                        log::debug!(
                            "helios: bulk-OUT short-write after clear_halt ({n}/{} bytes)",
                            buf.len()
                        );
                        Err(HeliosDacError::UsbError(rusb::Error::Io))
                    }
                }
            }
            Err(e) => Err(HeliosDacError::UsbError(e)),
        }
    }

    /// Gets name of DAC.
    fn name(&self) -> Result<String> {
        let ctrl_buffer = [CONTROL_GET_NAME, 0];
        let (buffer, _) = self.call_control(&ctrl_buffer)?;

        match buffer {
            [0x85, bytes @ ..] => {
                let null_byte_position = bytes.iter().position(|b| *b == 0u8).unwrap_or(31); // max length is 30 chars
                let (bytes_until_null, _) = bytes.split_at(null_byte_position);
                let name = String::from_utf8(bytes_until_null.to_vec())?;

                Ok(name)
            }
            _ => {
                self.drain_stale_responses();
                Err(HeliosDacError::InvalidDeviceResult)
            }
        }
    }

    /// Query the device firmware version over the control endpoint.
    fn read_firmware_version(&self) -> Result<u32> {
        let ctrl_buffer = [CONTROL_GET_FIRMWARE_VERSION, 0];
        let (buffer, size) = self.call_control(&ctrl_buffer)?;

        match &buffer[0..size] {
            [0x84, b0, b1, b2, b3, ..] => Ok(u32::from_le_bytes([*b0, *b1, *b2, *b3])),
            _ => {
                self.drain_stale_responses();
                Err(HeliosDacError::InvalidDeviceResult)
            }
        }
    }

    fn send_sdk_version(&self) -> Result<()> {
        let ctrl_buffer = [CONTROL_SEND_SDK_VERSION, SDK_VERSION];
        self.send_control(&ctrl_buffer)
    }

    /// Get device status (ready or not ready).
    fn status(&self) -> Result<DeviceStatus> {
        let ctrl_buffer = [CONTROL_GET_STATUS, 0];
        let (buffer, size) = self.call_control(&ctrl_buffer)?;

        match &buffer[0..size] {
            [0x83, 0] => Ok(DeviceStatus::NotReady),
            [0x83, 1] => Ok(DeviceStatus::Ready),
            _ => {
                self.drain_stale_responses();
                Err(HeliosDacError::InvalidDeviceResult)
            }
        }
    }

    /// Stops output of DAC.
    fn stop(&self) -> Result<()> {
        let ctrl_buffer = [CONTROL_STOP, 0];
        self.send_control(&ctrl_buffer)
    }

    /// Opens or closes the hardware shutter.
    fn set_shutter(&self, open: bool) -> Result<()> {
        let ctrl_buffer = [CONTROL_SET_SHUTTER, open as u8];
        self.send_control(&ctrl_buffer)
    }

    fn call_control(&self, buffer: &[u8]) -> Result<([u8; 32], usize)> {
        self.send_control(buffer)?;
        self.read_response()
    }

    fn send_control(&self, buffer: &[u8]) -> Result<()> {
        let written_length =
            self.handle
                .write_interrupt(ENDPOINT_INT_OUT, buffer, Duration::from_millis(16))?;
        if written_length != buffer.len() {
            // A short interrupt-OUT write must not panic the output thread;
            // surface it as an I/O error like the firmware-probe path does.
            log::debug!(
                "helios: control OUT short-write ({written_length}/{} bytes)",
                buffer.len()
            );
            return Err(HeliosDacError::UsbError(rusb::Error::Io));
        }
        Ok(())
    }

    fn read_response(&self) -> Result<([u8; 32], usize)> {
        let mut buffer: [u8; 32] = [0; 32];
        let size =
            self.handle
                .read_interrupt(ENDPOINT_INT_IN, &mut buffer, Duration::from_millis(32))?;
        Ok((buffer, size))
    }

    /// Leak the underlying USB handle instead of dropping it. Used by the
    /// backend's fatal-disconnect path to avoid the macOS `libusb_close()`
    /// segfault on a physically-removed device (see `HeliosBackend::Drop`).
    fn leak_handle(self) {
        let HeliosComm { handle, .. } = self;
        std::mem::forget(handle);
    }
}

/// A Helios DAC device.
///
/// Can be in either Idle (not opened) or Open state.
pub enum HeliosDac {
    /// Device is idle (not opened).
    Idle(rusb::Device<rusb::Context>),
    /// Device is open and ready for communication.
    ///
    /// The `comm` field carries the internal (`HeliosComm`) transfer engine, so
    /// the variant allows the private-interface exposure that the pub enum would
    /// otherwise flag — external code cannot do anything useful with it since all
    /// `HeliosComm` methods are private.
    #[allow(private_interfaces)]
    Open {
        /// Physical USB device backing this handle. Always `Some` in production
        /// (set by [`open`](Self::open)); `None` only for a test-injected DAC
        /// built over a fake [`UsbEndpoints`], which has no real `rusb::Device`.
        device: Option<rusb::Device<rusb::Context>>,
        /// USB transfer engine bound to the open handle. Boxed behind
        /// `dyn UsbEndpoints + Send` so the same variant can carry the real
        /// `rusb` handle in production or a fake transport in tests, without
        /// leaking a type parameter into the public `HeliosDac` surface. The
        /// `+ Send` keeps `HeliosDac` `Send`, which the backend trait requires.
        comm: HeliosComm<Box<dyn UsbEndpoints + Send>>,
    },
}

impl HeliosDac {
    /// Open the device for communication.
    ///
    /// The init sequence mirrors the official Helios C++ SDK: claim the
    /// interface, select alt setting 1, sleep 100ms for the device to settle,
    /// drain any stale interrupt-IN bytes, then probe firmware with retries.
    /// Without this, the first connection attempt after enumeration usually
    /// times out on macOS — the device isn't ready to answer interrupt
    /// transfers immediately after `set_alternate_setting`.
    pub fn open(self) -> Result<Self> {
        match self {
            HeliosDac::Idle(device) => {
                let handle = device.open()?;
                handle.claim_interface(0)?;
                handle.set_alternate_setting(0, 1)?;

                std::thread::sleep(INIT_SETTLE);

                let mut comm: HeliosComm<Box<dyn UsbEndpoints + Send>> = HeliosComm {
                    handle: Box::new(handle),
                    firmware_version: None,
                };

                // Drain any lingering IN packets from a previous session.
                comm.drain_stale_responses();

                let fw = comm.probe_firmware_version()?;
                comm.firmware_version = Some(fw);
                log::debug!("helios: connected, firmware version {fw}");
                comm.send_sdk_version()?;

                Ok(HeliosDac::Open {
                    device: Some(device),
                    comm,
                })
            }
            open => Ok(open),
        }
    }

    /// Borrow the open transfer engine, or error if the device is not opened.
    fn comm(&self) -> Result<&HeliosComm<Box<dyn UsbEndpoints + Send>>> {
        match self {
            HeliosDac::Open { comm, .. } => Ok(comm),
            _ => Err(HeliosDacError::DeviceNotOpened),
        }
    }

    /// Best-effort physical USB location, stable across re-enumeration when
    /// the DAC stays on the same hub/port path.
    pub fn usb_location(&self) -> String {
        match self {
            HeliosDac::Idle(device) => format_usb_location(device),
            HeliosDac::Open {
                device: Some(device),
                ..
            } => format_usb_location(device),
            // Only a test-injected DAC (fake transport) has no real device.
            HeliosDac::Open { device: None, .. } => "test:fake".to_string(),
        }
    }

    /// Best-effort read of the device name for a stable identity, without
    /// disturbing the `Idle`/`Open` state used for a later [`open`](Self::open).
    ///
    /// Opens a temporary handle, claims the interface, reads the name via
    /// `CONTROL_GET_NAME`, then closes. The temporary handle is a plain
    /// [`HeliosComm`] (not routed through the backend's `Drop`), so it closes
    /// normally — safe because the device is physically present during a scan.
    ///
    /// Returns an error (leaving the caller to fall back to a port-path id)
    /// when the device is busy — e.g. already claimed by a live session in
    /// this process, where `claim_interface` fails before any transfer runs.
    pub(crate) fn read_name(&self) -> Result<String> {
        let device = match self {
            HeliosDac::Idle(device) => device.clone(),
            HeliosDac::Open {
                device: Some(device),
                ..
            } => device.clone(),
            // A test-injected DAC has no real device to reopen for a name probe.
            HeliosDac::Open { device: None, .. } => return Err(HeliosDacError::DeviceNotOpened),
        };

        let handle = device.open()?;
        handle.claim_interface(0)?;
        handle.set_alternate_setting(0, 1)?;
        std::thread::sleep(INIT_SETTLE);

        // Route through a temporary comm so the existing `name()` control flow
        // is reused. Dropped at end of scope, closing the handle normally.
        let comm = HeliosComm {
            handle,
            firmware_version: None,
        };

        // Drain any lingering IN packets before issuing the name request.
        comm.drain_stale_responses();

        comm.name()
    }

    /// The firmware version probed during [`HeliosDac::open`], if the device is
    /// open. Returns `None` for idle devices. Exposed for diagnostics.
    pub fn probed_firmware_version(&self) -> Option<u32> {
        match self {
            HeliosDac::Open { comm, .. } => comm.firmware_version,
            _ => None,
        }
    }

    /// Consume the DAC and, if open, leak the USB handle instead of closing it.
    /// Used by the backend's fatal-disconnect path (see `HeliosBackend::Drop`).
    pub(crate) fn leak_handle(self) {
        if let HeliosDac::Open { comm, .. } = self {
            comm.leak_handle();
        }
    }

    /// Writes and outputs a frame to the DAC.
    pub fn write_frame(&mut self, frame: Frame) -> Result<()> {
        self.comm()?.write_frame(frame)
    }

    /// Write a pre-encoded frame buffer to the DAC.
    ///
    /// Use [`encode_frame_into`] to build the buffer, then call this to
    /// send it. Avoids allocating a new buffer per frame.
    pub fn write_frame_buffer(&mut self, buf: &[u8]) -> Result<()> {
        self.comm()?.write_frame_buffer(buf)
    }

    /// Gets name of DAC.
    pub fn name(&self) -> Result<String> {
        self.comm()?.name()
    }

    /// Get firmware version.
    pub fn firmware_version(&self) -> Result<u32> {
        self.comm()?.read_firmware_version()
    }

    /// Get device status (ready or not ready).
    pub fn status(&self) -> Result<DeviceStatus> {
        self.comm()?.status()
    }

    /// Stops output of DAC.
    pub fn stop(&self) -> Result<()> {
        self.comm()?.stop()
    }

    /// Opens or closes the hardware shutter.
    pub fn set_shutter(&self, open: bool) -> Result<()> {
        self.comm()?.set_shutter(open)
    }
}

impl From<rusb::Device<rusb::Context>> for HeliosDac {
    fn from(device: Device<Context>) -> Self {
        HeliosDac::Idle(device)
    }
}

fn format_usb_location<T: UsbContext>(device: &Device<T>) -> String {
    let bus = device.bus_number();
    match device.port_numbers() {
        Ok(ports) if !ports.is_empty() => {
            let path = ports
                .iter()
                .map(u8::to_string)
                .collect::<Vec<_>>()
                .join(".");
            format!("{bus}:{path}")
        }
        _ => format!("{}:{}", bus, device.address()),
    }
}

fn encode_frame(frame: Frame) -> Vec<u8> {
    let mut buf = Vec::with_capacity(frame.points.len() * 7 + 5);
    encode_frame_into(frame.pps, &frame.points, frame.flags, &mut buf);
    buf
}

/// Encode a Helios frame into a reusable byte buffer.
///
/// Clears `buf` and writes the wire-format bytes for the given points,
/// PPS, and flags. Use with [`HeliosDac::write_frame_buffer`] to avoid
/// per-frame heap allocation.
pub fn encode_frame_into(
    pps: u32,
    points: &[super::Point],
    flags: super::WriteFrameFlags,
    buf: &mut Vec<u8>,
) {
    // Hard-cap to the firmware/SDK maximum; frames larger than this are
    // refused by the SDK and undefined on the device. Callers should truncate
    // earlier (with a warning) — this is a defensive backstop.
    let requested_points = points.len().min(HELIOS_MAX_POINTS);
    let (pps_actual, num_of_points_actual) = adjusted_frame_params(pps, requested_points);

    buf.clear();
    buf.reserve(num_of_points_actual * 7 + 5);
    for point in points.iter().take(num_of_points_actual) {
        buf.extend_from_slice(&[
            (point.coordinate.x >> 4) as u8,
            ((point.coordinate.x & 0x0F) << 4) as u8 | (point.coordinate.y >> 8) as u8,
            (point.coordinate.y & 0xFF) as u8,
            point.color.r,
            point.color.g,
            point.color.b,
            point.intensity,
        ]);
    }
    buf.push((pps_actual & 0xFF) as u8);
    buf.push((pps_actual >> 8) as u8);
    buf.push((num_of_points_actual & 0xFF) as u8);
    buf.push((num_of_points_actual >> 8) as u8);
    buf.push(flags.bits());
}

fn adjusted_frame_params(requested_pps: u32, requested_points: usize) -> (u32, usize) {
    if requested_points >= 45 && (requested_points - 45).is_multiple_of(64) {
        let actual_points = requested_points - 1;
        let pps_actual = (((requested_pps as u64) * (actual_points as u64))
            + (requested_points as u64 / 2))
            / (requested_points as u64);
        log::debug!(
            "helios transfer-size workaround applied: requested_points={}, actual_points={}, requested_pps={}, actual_pps={}",
            requested_points,
            actual_points,
            requested_pps,
            pps_actual
        );
        (pps_actual as u32, actual_points)
    } else {
        (requested_pps, requested_points)
    }
}

/// Compute the USB bulk transfer timeout for a frame buffer.
///
/// Matches the Helios C++ SDK formula: `8 + (bufferSize >> 5)` ms.
/// The constant 8ms base ensures a minimum timeout even for tiny frames.
pub(crate) fn bulk_transfer_timeout(buffer_len: usize) -> Duration {
    Duration::from_millis((8 + (buffer_len >> 5)) as u64)
}

/// Errors that can occur when communicating with a Helios DAC.
#[derive(Error, Debug)]
pub enum HeliosDacError {
    #[error("device is not opened")]
    DeviceNotOpened,
    #[error("usb connection error: {0}")]
    UsbError(#[from] rusb::Error),
    #[error("usb device answered with invalid data")]
    InvalidDeviceResult,
    #[error("could not parse string: {0}")]
    Utf8Error(#[from] std::string::FromUtf8Error),
}

#[cfg(test)]
impl<H: UsbEndpoints> HeliosComm<H> {
    /// Build a comm over an arbitrary [`UsbEndpoints`] transport without
    /// touching real hardware, for tests. Bypasses the `open()` init handshake.
    fn from_parts(handle: H, firmware_version: Option<u32>) -> Self {
        HeliosComm {
            handle,
            firmware_version,
        }
    }
}

#[cfg(test)]
impl HeliosDac {
    /// Build an `Open` DAC over an arbitrary [`UsbEndpoints`] transport (a fake
    /// in tests), with no backing `rusb::Device`. Bypasses the `open()` init
    /// handshake so the backend's public frame/status path can be exercised
    /// end-to-end without hardware. The USB analogue of `Stream::from_parts`.
    pub(crate) fn from_endpoints(
        handle: impl UsbEndpoints + Send + 'static,
        firmware_version: Option<u32>,
    ) -> Self {
        HeliosDac::Open {
            device: None,
            comm: HeliosComm {
                handle: Box::new(handle),
                firmware_version,
            },
        }
    }
}

/// Shared test-only fake `UsbEndpoints` transport. Lives outside `mod tests` so
/// both this module's comm tests and `backend.rs`'s integration tests drive the
/// same fake through the seam (the finding requires one fake, not two).
#[cfg(test)]
pub(crate) mod test_support {
    use super::ENDPOINT_BULK_OUT;
    use crate::protocols::usb_transfer::UsbEndpoints;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// A scripted outcome for a single write on a fake endpoint.
    #[derive(Clone)]
    pub(crate) enum WriteScript {
        /// Report the full buffer length as transferred (success).
        Full,
        /// Report `n` bytes transferred (a short write).
        Short(usize),
        /// Fail the transfer with the given `rusb` error.
        Err(rusb::Error),
    }

    impl WriteScript {
        fn apply(&self, len: usize) -> rusb::Result<usize> {
            match self {
                WriteScript::Full => Ok(len),
                WriteScript::Short(n) => Ok(*n),
                WriteScript::Err(e) => Err(*e),
            }
        }
    }

    /// Fake USB endpoint transport for Helios: records every write and scripts
    /// bulk-OUT / interrupt-OUT outcomes and interrupt-IN reads, so `HeliosComm`
    /// (and, through it, `HeliosDac`/`HeliosBackend`) can be driven without
    /// hardware. The USB analogue of the network transports' `FakeSocket`.
    #[derive(Clone, Default)]
    pub(crate) struct FakeUsb {
        pub(crate) inner: Arc<Mutex<FakeInner>>,
    }

    #[derive(Default)]
    pub(crate) struct FakeInner {
        /// Every write, recorded as `(endpoint, bytes)`.
        writes: Vec<(u8, Vec<u8>)>,
        /// Scripted outcomes for bulk-OUT writes (front = next). Default: `Full`.
        bulk_out: VecDeque<WriteScript>,
        /// Scripted outcomes for interrupt-OUT writes (front = next). Default: `Full`.
        int_out: VecDeque<WriteScript>,
        /// Scripted interrupt-IN reads (front = next). Empty => `Err(Timeout)`,
        /// which is what real hardware returns when nothing is pending (and what
        /// terminates the drain loop).
        pub(crate) int_in: VecDeque<rusb::Result<Vec<u8>>>,
        /// Endpoints passed to `clear_halt`, in call order.
        clear_halt_calls: Vec<u8>,
    }

    impl FakeUsb {
        pub(crate) fn writes_to(&self, endpoint: u8) -> Vec<Vec<u8>> {
            self.inner
                .lock()
                .unwrap()
                .writes
                .iter()
                .filter(|(ep, _)| *ep == endpoint)
                .map(|(_, b)| b.clone())
                .collect()
        }

        /// Bytes written to the bulk-OUT (frame) endpoint, in order.
        pub(crate) fn bulk_writes(&self) -> Vec<Vec<u8>> {
            self.writes_to(ENDPOINT_BULK_OUT)
        }

        pub(crate) fn clear_halt_calls(&self) -> Vec<u8> {
            self.inner.lock().unwrap().clear_halt_calls.clone()
        }

        pub(crate) fn script_bulk_out(&self, scripts: impl IntoIterator<Item = WriteScript>) {
            self.inner.lock().unwrap().bulk_out.extend(scripts);
        }

        pub(crate) fn script_int_out(&self, scripts: impl IntoIterator<Item = WriteScript>) {
            self.inner.lock().unwrap().int_out.extend(scripts);
        }

        pub(crate) fn script_int_in(&self, reads: impl IntoIterator<Item = rusb::Result<Vec<u8>>>) {
            self.inner.lock().unwrap().int_in.extend(reads);
        }
    }

    impl UsbEndpoints for FakeUsb {
        fn write_bulk(&self, endpoint: u8, buf: &[u8], _t: Duration) -> rusb::Result<usize> {
            let mut inner = self.inner.lock().unwrap();
            inner.writes.push((endpoint, buf.to_vec()));
            let script = inner.bulk_out.pop_front().unwrap_or(WriteScript::Full);
            script.apply(buf.len())
        }

        fn read_bulk(&self, _e: u8, _buf: &mut [u8], _t: Duration) -> rusb::Result<usize> {
            Ok(0)
        }

        fn write_interrupt(&self, endpoint: u8, buf: &[u8], _t: Duration) -> rusb::Result<usize> {
            let mut inner = self.inner.lock().unwrap();
            inner.writes.push((endpoint, buf.to_vec()));
            let script = inner.int_out.pop_front().unwrap_or(WriteScript::Full);
            script.apply(buf.len())
        }

        fn read_interrupt(&self, _e: u8, buf: &mut [u8], _t: Duration) -> rusb::Result<usize> {
            let reply = self
                .inner
                .lock()
                .unwrap()
                .int_in
                .pop_front()
                // Nothing pending: mirror hardware and terminate the drain loop.
                .unwrap_or(Err(rusb::Error::Timeout))?;
            let n = reply.len().min(buf.len());
            buf[..n].copy_from_slice(&reply[..n]);
            Ok(n)
        }

        fn clear_halt(&self, endpoint: u8) -> rusb::Result<()> {
            self.inner.lock().unwrap().clear_halt_calls.push(endpoint);
            Ok(())
        }

        fn release_interface(&self, _i: u8) -> rusb::Result<()> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{FakeUsb, WriteScript};
    use super::*;
    use crate::protocols::helios::{Color, Coordinate, Point, WriteFrameFlags};

    fn test_point() -> Point {
        Point {
            coordinate: Coordinate { x: 0x123, y: 0x456 },
            color: Color::new(0x11, 0x22, 0x33),
            intensity: 0x44,
        }
    }

    #[test]
    fn test_bulk_transfer_timeout_matches_sdk() {
        // C++ SDK: 8 + (frameBufferSize >> 5) ms
        // Must have an 8ms minimum floor for any buffer size.

        // 1 point = 12 bytes → 8 + 0 = 8ms
        assert_eq!(bulk_transfer_timeout(12), Duration::from_millis(8));

        // 10 points = 75 bytes → 8 + 2 = 10ms
        assert_eq!(bulk_transfer_timeout(75), Duration::from_millis(10));

        // 100 points = 710 bytes → 8 + 22 = 30ms
        assert_eq!(bulk_transfer_timeout(710), Duration::from_millis(30));

        // 4095 points = 28670 bytes → 8 + 895 = 903ms
        assert_eq!(bulk_transfer_timeout(28670), Duration::from_millis(903));

        // Empty buffer → still 8ms minimum
        assert_eq!(bulk_transfer_timeout(0), Duration::from_millis(8));
    }

    #[test]
    fn test_adjusted_frame_params_leaves_small_frames_unchanged() {
        assert_eq!(adjusted_frame_params(30_000, 1), (30_000, 1));
        assert_eq!(adjusted_frame_params(30_000, 44), (30_000, 44));
    }

    #[test]
    fn test_adjusted_frame_params_applies_problem_size_workaround() {
        assert_eq!(adjusted_frame_params(30_000, 45), (29_333, 44));
        assert_eq!(adjusted_frame_params(30_000, 109), (29_725, 108));
    }

    #[test]
    fn test_encode_frame_truncates_problematic_transfer_payload() {
        let points = vec![test_point(); 109];
        let buffer = encode_frame(Frame::new_with_flags(
            30_000,
            points,
            WriteFrameFlags::SINGLE_MODE,
        ));

        assert_eq!(buffer.len(), 108 * 7 + 5);

        let footer = &buffer[buffer.len() - 5..];
        assert_eq!(u16::from_le_bytes([footer[0], footer[1]]), 29_725);
        assert_eq!(u16::from_le_bytes([footer[2], footer[3]]), 108);
        assert_eq!(footer[4], WriteFrameFlags::SINGLE_MODE.bits());
    }

    // =========================================================================
    // Fake-device comm tests
    //
    // These drive `HeliosComm` through the `UsbEndpoints` seam with a scripted
    // fake (see `super::test_support`), exercising the frame/control transfer
    // logic (encode, STALL recovery, short-write detection, control parsing,
    // firmware probe) without hardware — the paths that were previously
    // hardware-only behind the `MockOpen` branch.
    // =========================================================================

    fn comm() -> (HeliosComm<FakeUsb>, FakeUsb) {
        let fake = FakeUsb::default();
        (HeliosComm::from_parts(fake.clone(), None), fake)
    }

    // --- (a) write_frame encodes then bulk-writes the encoded bytes ----------

    #[test]
    fn write_frame_bulk_writes_exactly_encode_frame_output() {
        let (comm, fake) = comm();
        let frame = Frame::new(30_000, vec![test_point(); 3]);
        let expected = encode_frame(frame.clone());

        comm.write_frame(frame).unwrap();

        let bulk = fake.writes_to(ENDPOINT_BULK_OUT);
        assert_eq!(bulk.len(), 1, "one bulk-OUT transfer");
        assert_eq!(
            bulk[0], expected,
            "bytes on the wire == encode_frame(frame)"
        );
    }

    // --- (b) STALL recovery: Pipe -> clear_halt once -> retry succeeds -------

    #[test]
    fn write_bulk_all_recovers_from_single_stall() {
        let (comm, fake) = comm();
        fake.script_bulk_out([WriteScript::Err(rusb::Error::Pipe), WriteScript::Full]);

        comm.write_frame_buffer(&[0u8; 12]).unwrap();

        assert_eq!(
            fake.clear_halt_calls(),
            vec![ENDPOINT_BULK_OUT],
            "halt cleared exactly once on the stalled endpoint"
        );
        assert_eq!(
            fake.writes_to(ENDPOINT_BULK_OUT).len(),
            2,
            "wrote, then retried"
        );
    }

    // --- (c) STALL then short-write on the retry -> Io -----------------------

    #[test]
    fn write_bulk_all_stall_then_short_retry_is_io_error() {
        let (comm, fake) = comm();
        fake.script_bulk_out([WriteScript::Err(rusb::Error::Pipe), WriteScript::Short(3)]);

        let err = comm.write_frame_buffer(&[0u8; 12]).unwrap_err();
        assert!(matches!(err, HeliosDacError::UsbError(rusb::Error::Io)));
        assert_eq!(fake.clear_halt_calls(), vec![ENDPOINT_BULK_OUT]);
    }

    // --- (d) STALL then Pipe again on the retry -> Pipe propagates -----------

    #[test]
    fn write_bulk_all_stall_then_stall_propagates_pipe() {
        let (comm, fake) = comm();
        fake.script_bulk_out([
            WriteScript::Err(rusb::Error::Pipe),
            WriteScript::Err(rusb::Error::Pipe),
        ]);

        let err = comm.write_frame_buffer(&[0u8; 12]).unwrap_err();
        assert!(matches!(err, HeliosDacError::UsbError(rusb::Error::Pipe)));
    }

    // --- (e) plain short-write (no stall) -> Io ------------------------------

    #[test]
    fn write_bulk_all_short_write_is_io_error() {
        let (comm, fake) = comm();
        fake.script_bulk_out([WriteScript::Short(3)]);

        let err = comm.write_frame_buffer(&[0u8; 12]).unwrap_err();
        assert!(matches!(err, HeliosDacError::UsbError(rusb::Error::Io)));
        assert!(
            fake.clear_halt_calls().is_empty(),
            "no clear_halt without a stall"
        );
    }

    // --- (f) send_control short interrupt-write -> Io ------------------------

    #[test]
    fn send_control_short_interrupt_write_is_io_error() {
        let (comm, fake) = comm();
        // `stop()` sends a 2-byte control packet; report only 1 byte written.
        fake.script_int_out([WriteScript::Short(1)]);

        let err = comm.stop().unwrap_err();
        assert!(matches!(err, HeliosDacError::UsbError(rusb::Error::Io)));
    }

    // --- (g) probe_firmware_version: retries then parses / exhausts ----------

    #[test]
    fn probe_firmware_version_retries_out_and_in_then_parses() {
        let (comm, fake) = comm();
        // First OUT short-writes (retried); second OUT succeeds.
        fake.script_int_out([WriteScript::Short(1)]);
        // First IN reply is unexpected (retried); second IN is a valid 0x84.
        fake.script_int_in([
            Ok(vec![0x99, 0, 0, 0, 0]),
            Ok(vec![0x84, 0x2A, 0x00, 0x00, 0x00]),
        ]);

        assert_eq!(comm.probe_firmware_version().unwrap(), 42);
    }

    #[test]
    fn probe_firmware_version_exhausts_to_timeout() {
        let (comm, _fake) = comm();
        // OUTs succeed (default Full) but no IN reply ever arrives, so every IN
        // read times out and the probe exhausts its attempts.
        let err = comm.probe_firmware_version().unwrap_err();
        assert!(matches!(
            err,
            HeliosDacError::UsbError(rusb::Error::Timeout)
        ));
    }

    // --- (h) control replies: name / status / firmware parse + drain ---------

    #[test]
    fn name_parses_scripted_reply() {
        let (comm, fake) = comm();
        fake.script_int_in([Ok(vec![0x85, b'H', b'i', 0, 0, 0])]);
        assert_eq!(comm.name().unwrap(), "Hi");
    }

    #[test]
    fn read_firmware_version_parses_scripted_reply() {
        let (comm, fake) = comm();
        fake.script_int_in([Ok(vec![0x84, 0x07, 0x00, 0x00, 0x00])]);
        assert_eq!(comm.read_firmware_version().unwrap(), 7);
    }

    #[test]
    fn status_parses_ready_and_not_ready() {
        let (ready, fake_ready) = comm();
        fake_ready.script_int_in([Ok(vec![0x83, 1])]);
        assert!(matches!(ready.status().unwrap(), DeviceStatus::Ready));

        let (not_ready, fake_nr) = comm();
        fake_nr.script_int_in([Ok(vec![0x83, 0])]);
        assert!(matches!(
            not_ready.status().unwrap(),
            DeviceStatus::NotReady
        ));
    }

    #[test]
    fn status_malformed_reply_errors_and_drains() {
        let (comm, fake) = comm();
        // One malformed reply for the response read, then a lingering reply that
        // the drain must consume before the next request/response pair.
        fake.script_int_in([Ok(vec![0x83, 9]), Ok(vec![0xAB, 0xCD])]);

        let err = comm.status().unwrap_err();
        assert!(matches!(err, HeliosDacError::InvalidDeviceResult));
        // Drain consumed the lingering reply too (queue emptied to timeout).
        assert!(
            fake.inner.lock().unwrap().int_in.is_empty(),
            "drain_stale_responses drained the lingering IN reply"
        );
    }

    // --- leak_handle consumes without closing (smoke) ------------------------

    #[test]
    fn leak_handle_does_not_panic() {
        let (comm, _fake) = comm();
        comm.leak_handle();
    }
}
