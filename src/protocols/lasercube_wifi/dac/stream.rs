//! UDP streaming interface for LaserCube WiFi DAC communication.

use crate::protocols::lasercube_wifi::dac::buffer_estimator::BufferEstimator;
use crate::protocols::lasercube_wifi::dac::Addressed;
use crate::protocols::lasercube_wifi::error::CommunicationError;
use crate::protocols::lasercube_wifi::protocol::{
    command, BufferStatus, Point, WriteBytes, CMD_PORT, DATA_HEADER_SIZE, DATA_PORT,
    MAX_POINTS_PER_PACKET, POINT_SIZE_BYTES,
};
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

/// Default command socket receive timeout.
const CMD_TIMEOUT: Duration = Duration::from_millis(500);

/// Delay between sending chunks of a frame (microseconds).
const CHUNK_DELAY_US: u64 = 10;

/// Number of times to repeat commands for reliability.
const COMMAND_REPEAT_COUNT: usize = 2;

/// Initial warmup packets required before normal operation.
const REQUIRED_WARMUP_PACKETS: usize = 300;

/// A bi-directional communication stream with a LaserCube DAC.
pub struct Stream {
    /// The DAC we're connected to.
    dac: Addressed,
    /// UDP socket for commands.
    cmd_socket: UdpSocket,
    /// UDP socket for data.
    data_socket: UdpSocket,
    /// Message number counter (increments per packet).
    message_number: u8,
    /// Frame number counter (increments per frame).
    frame_number: u8,
    /// Current playback rate in Hz.
    current_rate: u32,
    /// Whether the stream has been initialized.
    initialized: bool,
    /// Reusable buffer for sending packets.
    send_buffer: Vec<u8>,
    /// Reusable buffer for receiving responses.
    recv_buffer: [u8; 1500],
    /// Dual-estimate buffer tracker.
    buffer_estimator: BufferEstimator,
}

impl Stream {
    /// Connect to a LaserCube DAC.
    ///
    /// This creates the UDP sockets and initializes the device for streaming.
    pub fn connect(dac: &Addressed) -> Result<Self, CommunicationError> {
        Self::connect_with_timeout(dac, CMD_TIMEOUT)
    }

    /// Connect to a LaserCube DAC with a custom timeout.
    pub fn connect_with_timeout(
        dac: &Addressed,
        timeout: Duration,
    ) -> Result<Self, CommunicationError> {
        // Create command socket
        let cmd_socket = UdpSocket::bind("0.0.0.0:0")?;
        cmd_socket.set_read_timeout(Some(timeout))?;
        let cmd_addr = SocketAddr::new(dac.ip_addr, CMD_PORT);
        cmd_socket.connect(cmd_addr)?;

        // Create data socket
        let data_socket = UdpSocket::bind("0.0.0.0:0")?;
        data_socket.set_read_timeout(Some(Duration::from_micros(1000)))?;
        let data_addr = SocketAddr::new(dac.ip_addr, DATA_PORT);
        data_socket.connect(data_addr)?;

        let mut stream = Stream {
            buffer_estimator: BufferEstimator::new(dac.max_buffer_space, 0),
            dac: dac.clone(),
            cmd_socket,
            data_socket,
            message_number: 0,
            frame_number: 0,
            current_rate: 0,
            initialized: false,
            send_buffer: Vec::with_capacity(
                DATA_HEADER_SIZE + MAX_POINTS_PER_PACKET * POINT_SIZE_BYTES,
            ),
            recv_buffer: [0u8; 1500],
        };

        // Initialize the device
        stream.initialize()?;

        Ok(stream)
    }

    /// Initialize the device for streaming.
    fn initialize(&mut self) -> Result<(), CommunicationError> {
        // Enable buffer size responses on data packets
        self.send_command_repeated(&command::enable_buffer_size_response(true))?;

        // Clear the ring buffer
        self.send_command_repeated(&command::clear_ringbuffer())?;

        // Enable output
        self.send_command_repeated(&command::set_output(true))?;
        self.dac.status.output_enabled = true;

        // Set initial rate (30 kHz is a safe default)
        let initial_rate = 30000;
        self.send_command_repeated(&command::set_rate(initial_rate))?;
        self.current_rate = initial_rate;
        self.dac.status.point_rate = initial_rate;

        // Perform warmup
        self.warmup()?;

        self.initialized = true;
        Ok(())
    }

    /// Perform the warmup sequence to pre-fill the buffer.
    fn warmup(&mut self) -> Result<(), CommunicationError> {
        let blank_points = [Point::blank(); MAX_POINTS_PER_PACKET];

        for _ in 0..REQUIRED_WARMUP_PACKETS {
            self.send_points_internal(&blank_points)?;
            self.receive_buffer_status()?;
        }

        Ok(())
    }

    /// Send a command to the command socket, repeating for reliability.
    fn send_command_repeated(&mut self, cmd: &[u8]) -> Result<(), CommunicationError> {
        for _ in 0..COMMAND_REPEAT_COUNT {
            self.cmd_socket.send(cmd)?;
        }
        Ok(())
    }

    /// Receive and process a buffer status response from the data socket.
    ///
    /// Updates the buffer estimator with ACK data for dual-estimate tracking.
    fn receive_buffer_status(&mut self) -> Result<Option<BufferStatus>, CommunicationError> {
        match self.data_socket.recv(&mut self.recv_buffer) {
            Ok(len) if len >= 4 => {
                if let Ok(status) = BufferStatus::from_response(&self.recv_buffer[..len]) {
                    let now = Instant::now();
                    self.buffer_estimator.record_ack(
                        status.message_number,
                        status.free_space,
                        now,
                    );
                    self.dac.status.free_buffer_space = status.free_space;
                    return Ok(Some(status));
                }
            }
            Ok(_) => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(e.into()),
        }
        Ok(None)
    }

    /// Try to receive buffer status responses (non-blocking).
    fn try_receive_buffer_status(&mut self) {
        // Try to receive up to 2 responses
        for _ in 0..2 {
            if self.receive_buffer_status().is_err() {
                break;
            }
        }
    }

    /// Send points to the device (internal implementation).
    fn send_points_internal(&mut self, points: &[Point]) -> Result<(), CommunicationError> {
        // Split into chunks of MAX_POINTS_PER_PACKET
        for chunk in points.chunks(MAX_POINTS_PER_PACKET) {
            // Build the packet
            self.send_buffer.clear();

            // Add header
            let header = command::sample_data_header(self.message_number, self.frame_number);
            self.send_buffer.extend_from_slice(&header);

            // Add points
            for point in chunk {
                self.send_buffer.write_bytes(point)?;
            }

            // Send the packet
            self.data_socket.send(&self.send_buffer)?;

            // Record the send in the buffer estimator
            let now = Instant::now();
            self.buffer_estimator
                .record_send(self.message_number, chunk.len() as u16, now);

            // Update counters
            self.message_number = self.message_number.wrapping_add(1);

            // Small delay between chunks
            if points.len() > MAX_POINTS_PER_PACKET {
                std::thread::sleep(Duration::from_micros(CHUNK_DELAY_US));
            }

            // Try to receive buffer status responses
            self.try_receive_buffer_status();
        }

        Ok(())
    }

    /// Borrow the inner DAC to examine its state.
    pub fn dac(&self) -> &Addressed {
        &self.dac
    }

    /// Get the current free buffer space on the device.
    pub fn free_buffer_space(&self) -> u16 {
        self.dac.status.free_buffer_space
    }

    /// Get the estimated number of points currently in the device buffer.
    pub fn estimated_buffer_fullness(&self) -> u16 {
        self.buffer_estimator.estimated_buffer_fullness(Instant::now())
    }

    /// Get the current playback rate in Hz.
    pub fn point_rate(&self) -> u32 {
        self.current_rate
    }

    /// Set the playback rate in Hz.
    ///
    /// Only sends the command if the rate differs by more than 3 Hz from the current rate.
    pub fn set_rate(&mut self, rate: u32) -> Result<(), CommunicationError> {
        if (rate as i64 - self.current_rate as i64).unsigned_abs() > 3 {
            self.send_command_repeated(&command::set_rate(rate))?;
            self.current_rate = rate;
            self.dac.status.point_rate = rate;
            self.buffer_estimator.set_point_rate(rate);
        }
        Ok(())
    }

    /// Enable or disable laser output.
    pub fn set_output(&mut self, enabled: bool) -> Result<(), CommunicationError> {
        self.send_command_repeated(&command::set_output(enabled))?;
        self.dac.status.output_enabled = enabled;
        Ok(())
    }

    /// Write a frame of points to the DAC.
    ///
    /// This method handles:
    /// - Splitting large frames into 140-point packets
    /// - Buffer management and flow control
    /// - Frame/message numbering
    ///
    /// The `rate` parameter sets the playback rate for this frame.
    pub fn write_frame(&mut self, points: &[Point], rate: u32) -> Result<(), CommunicationError> {
        if !self.initialized {
            return Err(CommunicationError::NotInitialized);
        }

        // Update rate if needed
        self.set_rate(rate)?;

        if !self.buffer_estimator.can_send(Instant::now()) {
            // Try to receive status updates to get fresh buffer info
            self.try_receive_buffer_status();
        }

        // Send the points
        self.send_points_internal(points)?;

        // Increment frame number after sending all chunks
        self.frame_number = self.frame_number.wrapping_add(1);

        Ok(())
    }

    /// Stop output and clear the buffer.
    pub fn stop(&mut self) -> Result<(), CommunicationError> {
        self.send_command_repeated(&command::set_output(false))?;
        self.dac.status.output_enabled = false;
        self.send_command_repeated(&command::clear_ringbuffer())?;
        self.dac.status.free_buffer_space = self.dac.max_buffer_space;
        self.buffer_estimator.reset();
        Ok(())
    }

    /// Set the read timeout for the command socket.
    pub fn set_cmd_timeout(&self, timeout: Option<Duration>) -> Result<(), CommunicationError> {
        self.cmd_socket.set_read_timeout(timeout)?;
        Ok(())
    }

    /// Set the read timeout for the data socket.
    pub fn set_data_timeout(&self, timeout: Option<Duration>) -> Result<(), CommunicationError> {
        self.data_socket.set_read_timeout(timeout)?;
        Ok(())
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        // Best effort to stop output when dropping the stream
        let _ = self.stop();
    }
}

/// Connect to a LaserCube DAC.
///
/// This is a convenience function that calls `Stream::connect`.
pub fn connect(dac: &Addressed) -> Result<Stream, CommunicationError> {
    Stream::connect(dac)
}

/// Connect to a LaserCube DAC with a custom timeout.
///
/// This is a convenience function that calls `Stream::connect_with_timeout`.
pub fn connect_timeout(dac: &Addressed, timeout: Duration) -> Result<Stream, CommunicationError> {
    Stream::connect_with_timeout(dac, timeout)
}

/// Calculate buffer usage from max and free space.
///
/// Returns the number of points currently in the device buffer.
#[inline]
pub fn calculate_buffer_used(max_buffer_space: u16, free_buffer_space: u16) -> u16 {
    max_buffer_space.saturating_sub(free_buffer_space)
}
