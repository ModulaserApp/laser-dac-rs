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

use super::protocol_handler::{parse_frame_data, RenderPoint};
use super::SimulatorSettings;

/// Server configuration from command-line arguments.
pub struct SimulatorServerConfig {
    pub hostname: String,
    pub service_name: String,
    pub port: u16,
}

/// Events sent from the server to the main thread.
pub enum ServerEvent {
    Frame(Vec<RenderPoint>),
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
        // Parse and forward frame data
        if let Some(points) = parse_frame_data(raw_data) {
            let _ = self.event_tx.send(ServerEvent::Frame(points));
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

/// Create and run the IDN server with simulator behavior.
pub fn run_server(
    config: SimulatorServerConfig,
    running: Arc<AtomicBool>,
    settings: Arc<RwLock<SimulatorSettings>>,
    event_tx: Sender<ServerEvent>,
) -> io::Result<()> {
    let link_timeout = Duration::from_millis(settings.read().unwrap().link_timeout_ms as u64);

    let server_config = ServerConfig::new(&config.hostname)
        .with_services(vec![
            MockService::laser_projector(1, &config.service_name).with_dsid()
        ])
        .with_bind_address(format!("0.0.0.0:{}", config.port).parse().unwrap())
        .with_link_timeout(link_timeout);

    let behavior = SimulatorBehavior::new(settings, event_tx);

    let server = MockIdnServer::new(server_config, behavior)?;

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
