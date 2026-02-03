//! Core mock IDN server implementation.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Instant;

use crate::behavior::ServerBehavior;
use crate::config::ServerConfig;
use crate::constants::*;
use crate::packet_builder::*;

/// A mock IDN server with pluggable behavior.
pub struct MockIdnServer<B: ServerBehavior> {
    socket: UdpSocket,
    config: ServerConfig,
    behavior: B,
    running: Arc<AtomicBool>,
    // Client tracking
    last_client: Option<SocketAddr>,
    last_activity: Option<Instant>,
    /// Client that was force-disconnected (ignore packets from them temporarily)
    disconnected_client: Option<(SocketAddr, Instant)>,
}

impl<B: ServerBehavior> MockIdnServer<B> {
    /// Create a new mock server with the given configuration and behavior.
    pub fn new(config: ServerConfig, behavior: B) -> io::Result<Self> {
        let socket = UdpSocket::bind(config.bind_address)?;
        socket.set_read_timeout(Some(config.read_timeout))?;

        log::info!("Mock IDN server listening on {}", socket.local_addr()?);

        Ok(Self {
            socket,
            config,
            behavior,
            running: Arc::new(AtomicBool::new(true)),
            last_client: None,
            last_activity: None,
            disconnected_client: None,
        })
    }

    /// Get the server's local address.
    pub fn addr(&self) -> SocketAddr {
        self.socket.local_addr().unwrap()
    }

    /// Get a handle to control the running flag.
    pub fn running_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.running)
    }

    /// Start the server in a background thread and return a handle.
    pub fn spawn(self) -> ServerHandle {
        let addr = self.addr();
        let running = self.running_handle();

        let handle = thread::spawn(move || {
            self.run();
        });

        ServerHandle {
            addr,
            running,
            handle: Some(handle),
        }
    }

    /// Run the server loop (blocking).
    pub fn run(mut self) {
        let mut buf = [0u8; 2048];

        while self.running.load(Ordering::SeqCst) {
            // Check for force disconnect
            if self.behavior.should_force_disconnect() {
                if let Some(client) = self.last_client.take() {
                    log::info!("Force disconnecting client: {}", client);
                    self.disconnected_client = Some((client, Instant::now()));
                    self.last_activity = None;
                    self.behavior.on_client_disconnected();
                }
            }

            // Clear disconnected client after 3 seconds
            if let Some((_, disconnect_time)) = self.disconnected_client {
                if disconnect_time.elapsed() > std::time::Duration::from_secs(3) {
                    self.disconnected_client = None;
                }
            }

            // Check for link timeout
            if let Some(last_time) = self.last_activity {
                if last_time.elapsed() > self.config.link_timeout && self.last_client.is_some() {
                    log::info!("Client timed out");
                    self.last_client = None;
                    self.last_activity = None;
                    self.behavior.on_client_disconnected();
                }
            }

            // Receive packet
            let (len, src) = match self.socket.recv_from(&mut buf) {
                Ok(result) => result,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue
                }
                Err(e) => {
                    log::error!("Socket error: {}", e);
                    break;
                }
            };

            if len < 4 {
                continue;
            }

            // Apply simulated latency
            let latency = self.behavior.get_simulated_latency();
            if !latency.is_zero() {
                thread::sleep(latency);
            }

            // Ignore packets from force-disconnected client
            if let Some((disconnected_addr, _)) = self.disconnected_client {
                if src == disconnected_addr {
                    continue;
                }
            }

            // Notify behavior of packet receipt (before any filtering)
            self.behavior.on_packet_received(&buf[..len]);

            let command = buf[0];
            let flags = buf[1];
            let sequence = u16::from_be_bytes([buf[2], buf[3]]);

            // Check if we should respond
            if !self.behavior.should_respond(command) {
                log::debug!("Ignoring command 0x{:02X} (should_respond=false)", command);
                continue;
            }

            match command {
                IDNCMD_SCAN_REQUEST => {
                    log::debug!("Received SCAN_REQUEST from {}", src);
                    let response = build_scan_response(
                        flags,
                        sequence,
                        &self.config.unit_id,
                        &self.config.hostname,
                        self.config.protocol_version,
                        self.behavior.get_status_byte(),
                    );
                    let _ = self.socket.send_to(&response, src);
                }
                IDNCMD_SERVICEMAP_REQUEST => {
                    log::debug!("Received SERVICEMAP_REQUEST from {}", src);
                    let response = build_servicemap_response(
                        flags,
                        sequence,
                        &self.config.services,
                        &self.config.relays,
                    );
                    let _ = self.socket.send_to(&response, src);
                }
                IDNCMD_PING_REQUEST => {
                    log::trace!("Received PING_REQUEST from {}", src);
                    // Copy the payload (everything after the 4-byte header)
                    let payload = &buf[4..len];
                    let response = build_ping_response(flags, sequence, payload);
                    let _ = self.socket.send_to(&response, src);
                }
                IDNCMD_RT_CNLMSG | IDNCMD_RT_CNLMSG_ACKREQ => {
                    // Check if excluded
                    if self.behavior.is_excluded() {
                        if command == IDNCMD_RT_CNLMSG_ACKREQ {
                            let response = build_ack_response(flags, sequence, 0xED);
                            let _ = self.socket.send_to(&response, src);
                        }
                        continue;
                    }

                    // Check if occupied by another client
                    if self.behavior.is_occupied()
                        && self.last_client.is_some()
                        && self.last_client != Some(src)
                    {
                        if command == IDNCMD_RT_CNLMSG_ACKREQ {
                            let response = build_ack_response(flags, sequence, 0xEC);
                            let _ = self.socket.send_to(&response, src);
                        }
                        continue;
                    }

                    // Track client connection
                    if self.last_client != Some(src) {
                        log::info!("Client connected: {}", src);
                        self.last_client = Some(src);
                        self.behavior.on_client_connected(src);
                    }
                    self.last_activity = Some(Instant::now());

                    // Forward frame data to behavior
                    self.behavior.on_frame_received(&buf[..len]);

                    // Send ACK if requested
                    if command == IDNCMD_RT_CNLMSG_ACKREQ {
                        let response = build_ack_response(
                            flags,
                            sequence,
                            self.behavior.get_ack_result_code(),
                        );
                        let _ = self.socket.send_to(&response, src);
                    }
                }
                IDNCMD_RT_CNLMSG_CLOSE => {
                    log::debug!("Received RT_CNLMSG_CLOSE from {}", src);
                    // No response needed for close without ack
                }
                IDNCMD_RT_CNLMSG_CLOSE_ACKREQ => {
                    log::debug!("Received RT_CNLMSG_CLOSE_ACKREQ from {}", src);
                    let response =
                        build_ack_response(flags, sequence, self.behavior.get_ack_result_code());
                    let _ = self.socket.send_to(&response, src);
                }
                IDNCMD_UNIT_PARAMS_REQUEST => {
                    log::debug!("Received UNIT_PARAMS_REQUEST from {}", src);
                    // Parse service_id and param_id from the request
                    let service_id = if len > 4 { buf[4] } else { 0 };
                    let param_id = if len > 7 {
                        u16::from_be_bytes([buf[6], buf[7]])
                    } else {
                        0
                    };
                    let response = build_parameter_response(
                        flags,
                        sequence,
                        IDNCMD_UNIT_PARAMS_RESPONSE,
                        0, // success
                        service_id,
                        param_id,
                        0x12345678, // dummy value
                    );
                    let _ = self.socket.send_to(&response, src);
                }
                IDNCMD_SERVICE_PARAMS_REQUEST => {
                    log::debug!("Received SERVICE_PARAMS_REQUEST from {}", src);
                    let service_id = if len > 4 { buf[4] } else { 0 };
                    let param_id = if len > 7 {
                        u16::from_be_bytes([buf[6], buf[7]])
                    } else {
                        0
                    };
                    let response = build_parameter_response(
                        flags,
                        sequence,
                        IDNCMD_SERVICE_PARAMS_RESPONSE,
                        0, // success
                        service_id,
                        param_id,
                        0x12345678, // dummy value
                    );
                    let _ = self.socket.send_to(&response, src);
                }
                _ => {
                    log::trace!("Unknown command: 0x{:02X} from {}", command, src);
                }
            }
        }

        log::info!("Mock IDN server stopped");
    }
}

/// Handle for controlling a spawned server.
pub struct ServerHandle {
    /// The server's local address.
    pub addr: SocketAddr,
    running: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ServerHandle {
    /// Stop the server.
    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
