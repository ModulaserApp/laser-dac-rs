//! USB streaming interface for LaserCube/LaserDock DAC communication.

use crate::protocols::lasercube_usb::dac::{DeviceInfo, DeviceStatus, FirmwareVersion};
use crate::protocols::lasercube_usb::error::{Error, Result};
use crate::protocols::lasercube_usb::protocol::{
    Sample, CMD_CLEAR_RINGBUFFER, CMD_GET_BULK_PACKET_SAMPLE_COUNT, CMD_GET_DAC_RATE,
    CMD_GET_MAX_DAC_RATE, CMD_GET_MAX_DAC_VALUE, CMD_GET_MIN_DAC_VALUE, CMD_GET_OUTPUT,
    CMD_GET_RINGBUFFER_EMPTY_SAMPLE_COUNT, CMD_GET_RINGBUFFER_SAMPLE_COUNT,
    CMD_GET_SAMPLE_ELEMENT_COUNT, CMD_GET_VERSION_MAJOR, CMD_GET_VERSION_MINOR, CMD_RUNNER_MODE,
    CMD_SET_DAC_RATE, CMD_SET_OUTPUT, CONTROL_INTERFACE, CONTROL_PACKET_SIZE, DATA_ALT_SETTING,
    DATA_INTERFACE, ENDPOINT_CONTROL_IN, ENDPOINT_CONTROL_OUT, ENDPOINT_DATA_OUT,
    RUNNER_MODE_SUB_ENABLE, RUNNER_MODE_SUB_RUN, SAMPLE_SIZE_BYTES,
};
use rusb::{DeviceHandle, UsbContext};
use std::time::Duration;

/// Timeout for control endpoint transfers.
const CONTROL_TIMEOUT: Duration = Duration::from_millis(1000);

/// Timeout for data endpoint transfers. Zero means infinite/blocking,
/// matching the reference C implementation for natural backpressure.
const DATA_TIMEOUT: Duration = Duration::ZERO;

/// A bidirectional USB communication stream with a LaserCube/LaserDock DAC.
pub struct Stream<T: UsbContext> {
    /// USB device handle for control operations.
    handle: DeviceHandle<T>,
    /// Device information and status.
    info: DeviceInfo,
    /// Device initialization status.
    status: DeviceStatus,
    /// Whether X coordinates should be flipped.
    flip_x: bool,
    /// Whether Y coordinates should be flipped.
    flip_y: bool,
}

impl<T: UsbContext> Stream<T> {
    /// Clear any stuck runner mode state from a previous session.
    ///
    /// Sends the toggle sequence: enable(1), enable(0), run(1), run(0).
    /// Errors are ignored since older firmware may not support this command.
    fn clear_runner_mode(handle: &DeviceHandle<T>) {
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

    /// Open a stream to a LaserCube/LaserDock USB device.
    ///
    /// This claims the necessary interfaces and initializes the device.
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
            flip_x: true,
            flip_y: true,
        };

        // Read device information
        stream.refresh_device_info()?;
        stream.status = DeviceStatus::Initialized;

        Ok(stream)
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

    /// Set whether to flip X coordinates.
    pub fn set_flip_x(&mut self, flip: bool) {
        self.flip_x = flip;
    }

    /// Set whether to flip Y coordinates.
    pub fn set_flip_y(&mut self, flip: bool) {
        self.flip_y = flip;
    }

    /// Send samples to the DAC.
    ///
    /// Samples are sent in chunks of `bulk_packet_sample_count` to match
    /// the device's expected USB bulk transfer size.
    pub fn send_samples(&mut self, samples: &[Sample]) -> Result<()> {
        if self.status != DeviceStatus::Initialized {
            return Err(Error::DeviceNotOpened);
        }

        let buffer: Vec<u8> = samples
            .iter()
            .flat_map(|sample| {
                let mut s = *sample;
                if self.flip_x {
                    s.flip_x();
                }
                if self.flip_y {
                    s.flip_y();
                }
                s.to_bytes()
            })
            .collect();

        let chunk_bytes = self.info.bulk_packet_sample_count as usize * SAMPLE_SIZE_BYTES;
        if chunk_bytes == 0 {
            return self.send_data(&buffer);
        }

        for chunk in buffer.chunks(chunk_bytes) {
            self.send_data(chunk)?;
        }
        Ok(())
    }

    /// Write a frame of samples at the specified rate.
    ///
    /// The rate is clamped to the device's maximum DAC rate.
    pub fn write_frame(&mut self, samples: &[Sample], rate: u32) -> Result<()> {
        let rate = if self.info.max_dac_rate > 0 {
            rate.min(self.info.max_dac_rate)
        } else {
            rate
        };
        if self.info.current_rate != rate {
            self.set_rate(rate)?;
        }
        self.send_samples(samples)
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
    fn send_data(&self, data: &[u8]) -> Result<()> {
        let transferred = self
            .handle
            .write_bulk(ENDPOINT_DATA_OUT, data, DATA_TIMEOUT)?;

        if transferred != data.len() {
            return Err(Error::Usb(rusb::Error::Io));
        }

        Ok(())
    }
}

impl<T: UsbContext> Drop for Stream<T> {
    fn drop(&mut self) {
        // Best effort to stop output when dropping
        let _ = self.stop();

        // Release interfaces
        let _ = self.handle.release_interface(DATA_INTERFACE);
        let _ = self.handle.release_interface(CONTROL_INTERFACE);
    }
}
