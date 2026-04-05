//! UDP server implementing the IDN protocol for the simulator.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use idn_mock_server::{
    MockIdnServer, MockService, ServerBehavior, ServerConfig, IDNFLG_STATUS_EXCLUDED,
    IDNFLG_STATUS_MALFUNCTION, IDNFLG_STATUS_OCCUPIED, IDNFLG_STATUS_OFFLINE,
    IDNFLG_STATUS_REALTIME,
};

use crate::protocol_handler::{parse_frame_data, ParsedChunk};
use crate::settings::SimulatorSettings;

/// Server configuration from command-line arguments.
pub struct SimulatorServerConfig {
    pub hostname: String,
    pub service_name: String,
    pub port: u16,
}

/// Events sent from the server to the main thread.
pub enum ServerEvent {
    /// Server successfully started on the given port.
    Started(u16),
    /// A chunk of points with timing information.
    Chunk(ParsedChunk),
    ClientConnected(SocketAddr),
    ClientDisconnected,
}

/// Behavior implementation for the simulator.
///
/// This wraps the shared settings and event channel to integrate with
/// the egui UI.
pub struct SimulatorBehavior {
    settings: Arc<RwLock<SimulatorSettings>>,
    event_tx: Sender<ServerEvent>,
}

impl SimulatorBehavior {
    pub fn new(settings: Arc<RwLock<SimulatorSettings>>, event_tx: Sender<ServerEvent>) -> Self {
        Self { settings, event_tx }
    }
}

impl ServerBehavior for SimulatorBehavior {
    fn on_frame_received(&mut self, raw_data: &[u8]) {
        // Parse and forward chunk data with timing info
        if let Some(chunk) = parse_frame_data(raw_data) {
            let _ = self.event_tx.send(ServerEvent::Chunk(chunk));
        }
    }

    fn should_respond(&self, _command: u8) -> bool {
        // If offline, don't respond to any commands (device is "invisible")
        !self.settings.read().unwrap().status_offline
    }

    fn get_status_byte(&self) -> u8 {
        let settings = self.settings.read().unwrap();
        let mut status = IDNFLG_STATUS_REALTIME; // Always realtime capable
        if settings.status_malfunction {
            status |= IDNFLG_STATUS_MALFUNCTION;
        }
        if settings.status_offline {
            status |= IDNFLG_STATUS_OFFLINE;
        }
        if settings.status_excluded {
            status |= IDNFLG_STATUS_EXCLUDED;
        }
        if settings.status_occupied {
            status |= IDNFLG_STATUS_OCCUPIED;
        }
        status
    }

    fn get_ack_result_code(&self) -> u8 {
        self.settings.read().unwrap().ack_error_code
    }

    fn get_simulated_latency(&self) -> Duration {
        let ms = self.settings.read().unwrap().simulated_latency_ms;
        Duration::from_millis(ms as u64)
    }

    fn on_client_connected(&mut self, addr: SocketAddr) {
        let _ = self.event_tx.send(ServerEvent::ClientConnected(addr));
    }

    fn on_client_disconnected(&mut self) {
        let _ = self.event_tx.send(ServerEvent::ClientDisconnected);
    }

    fn should_force_disconnect(&mut self) -> bool {
        std::mem::take(&mut self.settings.write().unwrap().force_disconnect)
    }

    fn is_occupied(&self) -> bool {
        self.settings.read().unwrap().status_occupied
    }

    fn is_excluded(&self) -> bool {
        self.settings.read().unwrap().status_excluded
    }
}

/// Maximum number of ports to try when the default is already in use.
const MAX_PORT_ATTEMPTS: u16 = 100;

/// Create and run the IDN server with simulator behavior.
pub fn run_server(
    config: SimulatorServerConfig,
    running: Arc<AtomicBool>,
    settings: Arc<RwLock<SimulatorSettings>>,
    event_tx: Sender<ServerEvent>,
) -> io::Result<()> {
    let link_timeout = Duration::from_millis(settings.read().unwrap().link_timeout_ms as u64);

    let base_config = ServerConfig::new(&config.hostname)
        .with_services(vec![
            MockService::laser_projector(1, &config.service_name).with_dsid()
        ])
        .with_link_timeout(link_timeout);

    // Try binding to the requested port, then increment if already in use.
    let server = {
        let mut last_err = None;
        let mut result = None;
        for offset in 0..MAX_PORT_ATTEMPTS {
            let port = config.port + offset;
            // Generate a unique unit_id per port so discovery doesn't merge instances.
            let mut unit_id = base_config.unit_id;
            unit_id[14] = (port >> 8) as u8;
            unit_id[15] = port as u8;
            let server_config = base_config
                .clone()
                .with_unit_id(unit_id)
                .with_bind_address(format!("0.0.0.0:{}", port).parse().unwrap());
            let behavior = SimulatorBehavior::new(Arc::clone(&settings), event_tx.clone());
            match MockIdnServer::new(server_config, behavior) {
                Ok(server) => {
                    result = Some(server);
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
                    if offset == 0 {
                        log::info!("Port {} in use, trying next available port...", port);
                    }
                    last_err = Some(e);
                }
                Err(e) => return Err(e),
            }
        }
        result.ok_or_else(|| last_err.unwrap())?
    };

    let _ = event_tx.send(ServerEvent::Started(server.addr().port()));

    // Copy the running flag to the server
    let server_running = server.running_handle();
    std::thread::spawn(move || {
        while running.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        server_running.store(false, Ordering::SeqCst);
    });

    server.run();
    Ok(())
}
