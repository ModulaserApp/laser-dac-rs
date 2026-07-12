//! USB streaming interface for LaserCube/LaserDock DAC communication.

use crate::protocols::lasercube_usb::dac::{DeviceInfo, DeviceStatus, FirmwareVersion};
use crate::protocols::lasercube_usb::error::{Error, Result};
use crate::protocols::lasercube_usb::protocol::{
    Sample, CMD_CLEAR_RINGBUFFER, CMD_GET_BULK_PACKET_SAMPLE_COUNT, CMD_GET_DAC_RATE,
    CMD_GET_MAX_DAC_RATE, CMD_GET_MAX_DAC_VALUE, CMD_GET_MIN_DAC_VALUE, CMD_GET_OUTPUT,
    CMD_GET_RINGBUFFER_EMPTY_SAMPLE_COUNT, CMD_GET_RINGBUFFER_SAMPLE_COUNT,
    CMD_GET_SAMPLE_ELEMENT_COUNT, CMD_GET_VERSION_MAJOR, CMD_GET_VERSION_MINOR, CMD_RUNNER_MODE,
    CMD_SET_DAC_RATE, CMD_SET_OUTPUT, CONTROL_INTERFACE, CONTROL_PACKET_SIZE, CONTROL_TIMEOUT,
    DATA_ALT_SETTING, DATA_INTERFACE, ENDPOINT_CONTROL_IN, ENDPOINT_CONTROL_OUT, ENDPOINT_DATA_OUT,
    RUNNER_MODE_SUB_ENABLE, RUNNER_MODE_SUB_RUN, SAMPLE_SIZE_BYTES,
};
use crate::protocols::usb_transfer::UsbEndpoints;
use rusb::UsbContext;
use std::time::Duration;

/// Timeout for data endpoint transfers. Zero means infinite/blocking,
/// matching the reference C implementation for natural backpressure.
const DATA_TIMEOUT: Duration = Duration::ZERO;

/// Default X flip. Matches the reference laserdocklib: flip X, not Y. Flipping
/// both would mirror output relative to the reference.
const DEFAULT_FLIP_X: bool = true;
/// Default Y flip (see [`DEFAULT_FLIP_X`]).
const DEFAULT_FLIP_Y: bool = false;

/// A bidirectional USB communication stream with a LaserCube/LaserDock DAC.
///
/// Generic over [`UsbEndpoints`] rather than a concrete `rusb` handle so the
/// streaming logic (chunking, flip, rate clamp, control round-trips) can be
/// exercised against a fake device in tests. Production code uses
/// `Stream<rusb::DeviceHandle<rusb::Context>>`, built via [`Stream::open`].
pub struct Stream<H: UsbEndpoints> {
    /// USB endpoint transport (real `rusb::DeviceHandle` in production).
    handle: H,
    /// Device information and status.
    info: DeviceInfo,
    /// Device initialization status.
    status: DeviceStatus,
    /// Whether X coordinates should be flipped.
    flip_x: bool,
    /// Whether Y coordinates should be flipped.
    flip_y: bool,
    /// Reusable byte buffer for USB data transfers (avoids per-send allocation).
    send_buffer: Vec<u8>,
}

impl<T: UsbContext> Stream<rusb::DeviceHandle<T>> {
    /// Open a stream to a LaserCube/LaserDock USB device.
    ///
    /// This claims the necessary interfaces and initializes the device. The
    /// device-opening steps (`open`/`claim_interface`/`set_alternate_setting`)
    /// are inherent to the concrete `rusb` handle, so this constructor stays
    /// concrete; everything after it runs through the [`UsbEndpoints`] seam.
    pub fn open(device: rusb::Device<T>) -> Result<Self> {
        let handle = device.open()?;

        // Claim control interface
        handle.claim_interface(CONTROL_INTERFACE)?;

        // Claim data interface with alternate setting
        handle.claim_interface(DATA_INTERFACE)?;
        handle.set_alternate_setting(DATA_INTERFACE, DATA_ALT_SETTING)?;

        // Clear any stuck runner mode from a previous session
        Self::clear_runner_mode(&handle);

        let mut stream = Stream {
            handle,
            info: DeviceInfo::default(),
            status: DeviceStatus::Unknown,
            flip_x: DEFAULT_FLIP_X,
            flip_y: DEFAULT_FLIP_Y,
            send_buffer: Vec::new(),
        };

        // Read device information
        stream.refresh_device_info()?;
        stream.status = DeviceStatus::Initialized;

        Ok(stream)
    }
}

impl<H: UsbEndpoints> Stream<H> {
    /// Clear any stuck runner mode state from a previous session.
    ///
    /// Sends the toggle sequence: enable(1), enable(0), run(1), run(0).
    /// Errors are ignored since older firmware may not support this command.
    fn clear_runner_mode(handle: &H) {
        let packets: [[u8; 3]; 4] = [
            [CMD_RUNNER_MODE, RUNNER_MODE_SUB_ENABLE, 1],
            [CMD_RUNNER_MODE, RUNNER_MODE_SUB_ENABLE, 0],
            [CMD_RUNNER_MODE, RUNNER_MODE_SUB_RUN, 1],
            [CMD_RUNNER_MODE, RUNNER_MODE_SUB_RUN, 0],
        ];
        let mut discard = [0u8; CONTROL_PACKET_SIZE];
        for packet in &packets {
            let _ = handle.write_bulk(ENDPOINT_CONTROL_OUT, packet, CONTROL_TIMEOUT);
            let _ = handle.read_bulk(ENDPOINT_CONTROL_IN, &mut discard, CONTROL_TIMEOUT);
        }
    }

    /// Refresh device information from the hardware.
    pub fn refresh_device_info(&mut self) -> Result<()> {
        self.info.firmware_version = FirmwareVersion {
            major: self.get_u32(CMD_GET_VERSION_MAJOR)?,
            minor: self.get_u32(CMD_GET_VERSION_MINOR)?,
        };
        self.info.max_dac_rate = self.get_u32(CMD_GET_MAX_DAC_RATE)?;
        self.info.min_dac_value = self.get_u32(CMD_GET_MIN_DAC_VALUE)?;
        self.info.max_dac_value = self.get_u32(CMD_GET_MAX_DAC_VALUE)?;
        self.info.sample_element_count = self.get_u32(CMD_GET_SAMPLE_ELEMENT_COUNT)?;
        self.info.bulk_packet_sample_count = self.get_u32(CMD_GET_BULK_PACKET_SAMPLE_COUNT)?;
        self.info.current_rate = self.get_u32(CMD_GET_DAC_RATE)?;
        self.info.ringbuffer_capacity = self.get_u32(CMD_GET_RINGBUFFER_SAMPLE_COUNT)?;
        self.info.ringbuffer_free_space = self.get_u32(CMD_GET_RINGBUFFER_EMPTY_SAMPLE_COUNT)?;

        self.info.output_enabled = self.get_u8(CMD_GET_OUTPUT)? == 1;

        Ok(())
    }

    /// Get the device information.
    pub fn info(&self) -> &DeviceInfo {
        &self.info
    }

    /// Get the device status.
    pub fn status(&self) -> DeviceStatus {
        self.status
    }

    /// Enable laser output.
    pub fn enable_output(&mut self) -> Result<()> {
        self.set_u8(CMD_SET_OUTPUT, 0x01)?;
        self.info.output_enabled = true;
        Ok(())
    }

    /// Disable laser output.
    pub fn disable_output(&mut self) -> Result<()> {
        self.set_u8(CMD_SET_OUTPUT, 0x00)?;
        self.info.output_enabled = false;
        Ok(())
    }

    /// Check if output is enabled.
    pub fn output_enabled(&self) -> bool {
        self.info.output_enabled
    }

    /// Set the DAC sample rate in Hz.
    pub fn set_rate(&mut self, rate: u32) -> Result<()> {
        self.set_u32(CMD_SET_DAC_RATE, rate)?;
        self.info.current_rate = rate;
        Ok(())
    }

    /// Get the current DAC sample rate in Hz.
    pub fn rate(&self) -> u32 {
        self.info.current_rate
    }

    /// Clear the ring buffer.
    pub fn clear_ringbuffer(&mut self) -> Result<()> {
        self.set_u8(CMD_CLEAR_RINGBUFFER, 0x00)?;
        self.info.ringbuffer_free_space = self.info.ringbuffer_capacity;
        Ok(())
    }

    /// Get the free space in the ring buffer.
    pub fn ringbuffer_free_space(&mut self) -> Result<u32> {
        let space = self.get_u32(CMD_GET_RINGBUFFER_EMPTY_SAMPLE_COUNT)?;
        self.info.ringbuffer_free_space = space;
        Ok(space)
    }

    /// Send samples to the DAC.
    ///
    /// Samples are sent in chunks of `bulk_packet_sample_count` to match
    /// the device's expected USB bulk transfer size.
    pub fn send_samples(&mut self, samples: &[Sample]) -> Result<()> {
        if self.status != DeviceStatus::Initialized {
            return Err(Error::DeviceNotOpened);
        }

        self.send_buffer.clear();
        self.send_buffer.reserve(samples.len() * SAMPLE_SIZE_BYTES);
        for sample in samples {
            let mut s = *sample;
            if self.flip_x {
                s.flip_x();
            }
            if self.flip_y {
                s.flip_y();
            }
            self.send_buffer.extend_from_slice(&s.to_bytes());
        }

        let chunk_bytes = self.info.bulk_packet_sample_count as usize * SAMPLE_SIZE_BYTES;
        if chunk_bytes == 0 {
            return Self::send_data_to_endpoint(&self.handle, &self.send_buffer);
        }

        for chunk in self.send_buffer.chunks(chunk_bytes) {
            Self::send_data_to_endpoint(&self.handle, chunk)?;
        }
        Ok(())
    }

    /// The rate this stream would program the device to for a requested `rate`,
    /// i.e. `rate` clamped to the device's maximum DAC rate. Returned by
    /// [`Self::write_frame`] so callers can drive their buffer model at the same
    /// rate the hardware actually runs, not the (possibly higher) requested one.
    pub fn effective_rate(&self, rate: u32) -> u32 {
        if self.info.max_dac_rate > 0 {
            rate.min(self.info.max_dac_rate)
        } else {
            rate
        }
    }

    /// Write a frame of samples at the specified rate.
    ///
    /// The rate is clamped to the device's maximum DAC rate; the effective
    /// (clamped) rate that was programmed is returned so the caller's buffer
    /// estimate can track the real drain rate rather than the requested one.
    pub fn write_frame(&mut self, samples: &[Sample], rate: u32) -> Result<u32> {
        let rate = self.effective_rate(rate);
        if self.info.current_rate != rate {
            self.set_rate(rate)?;
        }
        self.send_samples(samples)?;
        Ok(rate)
    }

    /// Stop output and clear the buffer.
    pub fn stop(&mut self) -> Result<()> {
        self.disable_output()?;
        self.clear_ringbuffer()?;
        Ok(())
    }

    // Low-level USB communication methods

    /// Get a u8 value using a control command.
    fn get_u8(&self, command: u8) -> Result<u8> {
        let mut packet = [0u8; CONTROL_PACKET_SIZE];
        packet[0] = command;

        self.send_control(&packet[..1])?;
        let response = self.receive_control()?;

        if response[1] != 0 {
            return Err(Error::InvalidResponse);
        }

        Ok(response[2])
    }

    /// Set a u8 value using a control command.
    fn set_u8(&self, command: u8, value: u8) -> Result<()> {
        let packet = [command, value];

        self.send_control(&packet)?;
        let response = self.receive_control()?;

        if response[1] != 0 {
            return Err(Error::InvalidResponse);
        }

        Ok(())
    }

    /// Get a u32 value using a control command.
    fn get_u32(&self, command: u8) -> Result<u32> {
        self.send_control(&[command])?;
        let response = self.receive_control()?;

        if response[1] != 0 {
            return Err(Error::InvalidResponse);
        }

        let value = u32::from_le_bytes([response[2], response[3], response[4], response[5]]);
        Ok(value)
    }

    /// Set a u32 value using a control command.
    fn set_u32(&self, command: u8, value: u32) -> Result<()> {
        let mut packet = [0u8; 5];
        packet[0] = command;
        packet[1..5].copy_from_slice(&value.to_le_bytes());

        self.send_control(&packet)?;
        let response = self.receive_control()?;

        if response[1] != 0 {
            return Err(Error::InvalidResponse);
        }

        Ok(())
    }

    /// Send a control packet to the device.
    fn send_control(&self, data: &[u8]) -> Result<usize> {
        let transferred = self
            .handle
            .write_bulk(ENDPOINT_CONTROL_OUT, data, CONTROL_TIMEOUT)?;

        if transferred != data.len() {
            return Err(Error::Usb(rusb::Error::Io));
        }

        Ok(transferred)
    }

    /// Receive a control response from the device.
    fn receive_control(&self) -> Result<[u8; CONTROL_PACKET_SIZE]> {
        let mut buffer = [0u8; CONTROL_PACKET_SIZE];
        let transferred =
            self.handle
                .read_bulk(ENDPOINT_CONTROL_IN, &mut buffer, CONTROL_TIMEOUT)?;

        if transferred != CONTROL_PACKET_SIZE {
            return Err(Error::InvalidResponse);
        }

        Ok(buffer)
    }

    /// Send data to the data endpoint.
    ///
    /// Uses an infinite timeout so the transfer blocks until the device accepts
    /// the data, providing natural backpressure (matching the reference implementation).
    fn send_data_to_endpoint(handle: &H, data: &[u8]) -> Result<()> {
        let transferred = handle.write_bulk(ENDPOINT_DATA_OUT, data, DATA_TIMEOUT)?;

        if transferred != data.len() {
            return Err(Error::Usb(rusb::Error::Io));
        }

        Ok(())
    }
}

impl<H: UsbEndpoints> Drop for Stream<H> {
    fn drop(&mut self) {
        // Best effort to stop output when dropping
        let _ = self.stop();

        // Release interfaces
        let _ = self.handle.release_interface(DATA_INTERFACE);
        let _ = self.handle.release_interface(CONTROL_INTERFACE);
    }
}

#[cfg(test)]
impl<H: UsbEndpoints> Stream<H> {
    /// Build a stream over an arbitrary [`UsbEndpoints`] transport without
    /// touching real hardware, for tests. Bypasses the `open()` init handshake;
    /// callers supply the [`DeviceInfo`] the device would have reported.
    pub(crate) fn from_parts(
        handle: H,
        info: DeviceInfo,
        status: DeviceStatus,
        flip_x: bool,
        flip_y: bool,
    ) -> Self {
        Stream {
            handle,
            info,
            status,
            flip_x,
            flip_y,
            send_buffer: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::lasercube_usb::protocol::MAX_COORDINATE_VALUE;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Fake USB endpoint transport that records every write and scripts control
    /// reads, so `Stream` can be driven without hardware — the USB analogue of
    /// the network transport's `FakeSocket`.
    #[derive(Clone, Default)]
    struct FakeUsb {
        inner: Arc<Mutex<FakeInner>>,
    }

    #[derive(Default)]
    struct FakeInner {
        /// Every `write_bulk`, recorded as `(endpoint, bytes)`.
        writes: Vec<(u8, Vec<u8>)>,
        /// Scripted responses for `ENDPOINT_CONTROL_IN` reads. When empty, a
        /// zeroed 64-byte buffer is returned (a valid "status OK, value 0").
        control_reads: VecDeque<[u8; CONTROL_PACKET_SIZE]>,
        /// If set, the next data-endpoint write reports this many bytes instead
        /// of the full length (forces a short-write error).
        short_data_write: Option<usize>,
    }

    impl FakeUsb {
        fn writes_to(&self, endpoint: u8) -> Vec<Vec<u8>> {
            self.inner
                .lock()
                .unwrap()
                .writes
                .iter()
                .filter(|(ep, _)| *ep == endpoint)
                .map(|(_, b)| b.clone())
                .collect()
        }
    }

    impl UsbEndpoints for FakeUsb {
        fn write_bulk(&self, endpoint: u8, buf: &[u8], _t: Duration) -> rusb::Result<usize> {
            let mut inner = self.inner.lock().unwrap();
            inner.writes.push((endpoint, buf.to_vec()));
            if endpoint == ENDPOINT_DATA_OUT {
                if let Some(n) = inner.short_data_write.take() {
                    return Ok(n);
                }
            }
            Ok(buf.len())
        }

        fn read_bulk(&self, endpoint: u8, buf: &mut [u8], _t: Duration) -> rusb::Result<usize> {
            if endpoint == ENDPOINT_CONTROL_IN {
                let resp = self
                    .inner
                    .lock()
                    .unwrap()
                    .control_reads
                    .pop_front()
                    .unwrap_or([0u8; CONTROL_PACKET_SIZE]);
                let n = buf.len().min(CONTROL_PACKET_SIZE);
                buf[..n].copy_from_slice(&resp[..n]);
                return Ok(CONTROL_PACKET_SIZE);
            }
            Ok(0)
        }

        fn write_interrupt(&self, _e: u8, buf: &[u8], _t: Duration) -> rusb::Result<usize> {
            Ok(buf.len())
        }
        fn read_interrupt(&self, _e: u8, _buf: &mut [u8], _t: Duration) -> rusb::Result<usize> {
            Ok(0)
        }
        fn clear_halt(&self, _e: u8) -> rusb::Result<()> {
            Ok(())
        }
        fn release_interface(&self, _i: u8) -> rusb::Result<()> {
            Ok(())
        }
    }

    fn info_with(
        bulk_packet_sample_count: u32,
        max_dac_rate: u32,
        current_rate: u32,
    ) -> DeviceInfo {
        DeviceInfo {
            bulk_packet_sample_count,
            max_dac_rate,
            current_rate,
            ..DeviceInfo::default()
        }
    }

    /// Build an Initialized stream over a fresh `FakeUsb`, returning both.
    fn init_stream(info: DeviceInfo) -> (Stream<FakeUsb>, FakeUsb) {
        let fake = FakeUsb::default();
        let stream = Stream::from_parts(
            fake.clone(),
            info,
            DeviceStatus::Initialized,
            DEFAULT_FLIP_X,
            DEFAULT_FLIP_Y,
        );
        (stream, fake)
    }

    fn sample_at(x: u16, y: u16) -> Sample {
        Sample::new(x, y, 10, 20, 30)
    }

    #[test]
    fn send_samples_chunks_by_bulk_packet_sample_count() {
        // 64 samples/packet => 512-byte chunks. 512 samples => exactly 8 chunks.
        let (mut stream, fake) = init_stream(info_with(64, 0, 0));
        let samples: Vec<Sample> = (0..512).map(|i| sample_at(i % 4095, 0)).collect();
        stream.send_samples(&samples).unwrap();

        let data = fake.writes_to(ENDPOINT_DATA_OUT);
        assert_eq!(data.len(), 8, "expected 8 chunks");
        assert!(data.iter().all(|c| c.len() == 512), "each chunk 512 bytes");
    }

    #[test]
    fn send_samples_chunks_with_remainder() {
        // 100 samples * 8 bytes = 800 bytes => 512 + 288.
        let (mut stream, fake) = init_stream(info_with(64, 0, 0));
        let samples: Vec<Sample> = (0..100).map(|_| sample_at(0, 0)).collect();
        stream.send_samples(&samples).unwrap();

        let sizes: Vec<usize> = fake
            .writes_to(ENDPOINT_DATA_OUT)
            .iter()
            .map(Vec::len)
            .collect();
        assert_eq!(sizes, vec![512, 288]);
    }

    #[test]
    fn send_samples_zero_packet_count_sends_single_transfer() {
        // Fallback: a device reporting bulk_packet_sample_count == 0 gets one
        // transfer of the whole buffer rather than a divide-by-zero chunk size.
        let (mut stream, fake) = init_stream(info_with(0, 0, 0));
        let samples: Vec<Sample> = (0..10).map(|_| sample_at(0, 0)).collect();
        stream.send_samples(&samples).unwrap();

        let data = fake.writes_to(ENDPOINT_DATA_OUT);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].len(), 80);
    }

    #[test]
    fn send_samples_applies_flip_x_not_flip_y() {
        // `init_stream` builds with DEFAULT_FLIP_X/DEFAULT_FLIP_Y, so this also
        // pins the reference-matching defaults: a silent flip regression fails.
        let (mut stream, fake) = init_stream(info_with(64, 0, 0));
        stream.send_samples(&[sample_at(1000, 3000)]).unwrap();

        let data = fake.writes_to(ENDPOINT_DATA_OUT);
        let bytes = &data[0];
        let x = u16::from_le_bytes([bytes[4], bytes[5]]);
        let y = u16::from_le_bytes([bytes[6], bytes[7]]);
        assert_eq!(x, MAX_COORDINATE_VALUE - 1000, "x flipped");
        assert_eq!(y, 3000, "y not flipped");
    }

    #[test]
    fn send_samples_requires_initialized_status() {
        let fake = FakeUsb::default();
        let mut stream = Stream::from_parts(
            fake,
            info_with(64, 0, 0),
            DeviceStatus::Unknown,
            DEFAULT_FLIP_X,
            DEFAULT_FLIP_Y,
        );
        assert!(matches!(
            stream.send_samples(&[sample_at(0, 0)]),
            Err(Error::DeviceNotOpened)
        ));
    }

    #[test]
    fn send_samples_short_write_is_io_error() {
        let (mut stream, fake) = init_stream(info_with(64, 0, 0));
        fake.inner.lock().unwrap().short_data_write = Some(4);
        assert!(matches!(
            stream.send_samples(&[sample_at(0, 0)]),
            Err(Error::Usb(rusb::Error::Io))
        ));
    }

    #[test]
    fn write_frame_clamps_rate_and_reports_effective_rate() {
        // The regression guard for the estimator/clamp divergence: a request
        // above the device max is clamped on the wire AND the clamped value is
        // returned so the caller drives its buffer model at the real rate.
        let (mut stream, fake) = init_stream(info_with(64, 30_000, 0));
        let effective = stream.write_frame(&[], 40_000).unwrap();
        assert_eq!(effective, 30_000, "returned rate is clamped");

        let ctrl = fake.writes_to(ENDPOINT_CONTROL_OUT);
        let set_rate = ctrl
            .iter()
            .find(|p| p.first() == Some(&CMD_SET_DAC_RATE))
            .expect("SET_DAC_RATE issued");
        let programmed = u32::from_le_bytes([set_rate[1], set_rate[2], set_rate[3], set_rate[4]]);
        assert_eq!(programmed, 30_000, "device programmed to clamped rate");
    }

    #[test]
    fn write_frame_does_not_clamp_when_max_rate_unknown() {
        let (mut stream, _fake) = init_stream(info_with(64, 0, 0));
        assert_eq!(stream.write_frame(&[], 40_000).unwrap(), 40_000);
    }

    #[test]
    fn write_frame_skips_set_rate_when_unchanged() {
        let (mut stream, fake) = init_stream(info_with(64, 30_000, 30_000));
        stream.write_frame(&[], 30_000).unwrap();
        assert!(
            fake.writes_to(ENDPOINT_CONTROL_OUT)
                .iter()
                .all(|p| p.first() != Some(&CMD_SET_DAC_RATE)),
            "no SET_DAC_RATE when already at target"
        );
    }
}
