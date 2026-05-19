//! UDP server implementing the IDN protocol for the simulator.

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use laser_dac::receiver::{
    IdnServer, ReceivedChunk, ServerBehavior, ServerConfig, Service, IDNFLG_STATUS_EXCLUDED,
    IDNFLG_STATUS_MALFUNCTION, IDNFLG_STATUS_OCCUPIED, IDNFLG_STATUS_OFFLINE,
    IDNFLG_STATUS_REALTIME,
};

use crate::protocol_handler::{parsed_chunk_from_received, ParsedChunk};
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

/// Snapshot of the subset of `SimulatorSettings` that gets read on the hot
/// per-packet path. Refreshed once per `on_packet_received` so the read-side
/// `ServerBehavior` methods don't contend with the UI thread.
#[derive(Clone, Copy)]
struct SettingsSnapshot {
    status_offline: bool,
    status_malfunction: bool,
    status_excluded: bool,
    status_occupied: bool,
    ack_error_code: u8,
    simulated_latency_ms: u32,
}

impl SettingsSnapshot {
    fn from_settings(s: &SimulatorSettings) -> Self {
        Self {
            status_offline: s.status_offline,
            status_malfunction: s.status_malfunction,
            status_excluded: s.status_excluded,
            status_occupied: s.status_occupied,
            ack_error_code: s.ack_error_code,
            simulated_latency_ms: s.simulated_latency_ms,
        }
    }
}

/// Behavior implementation for the simulator.
///
/// This wraps the shared settings and event channel to integrate with
/// the egui UI.
pub struct SimulatorBehavior {
    settings: Arc<RwLock<SimulatorSettings>>,
    snapshot: RwLock<SettingsSnapshot>,
    event_tx: Sender<ServerEvent>,
}

impl SimulatorBehavior {
    pub fn new(settings: Arc<RwLock<SimulatorSettings>>, event_tx: Sender<ServerEvent>) -> Self {
        let snapshot = SettingsSnapshot::from_settings(&settings.read().unwrap());
        Self {
            settings,
            snapshot: RwLock::new(snapshot),
            event_tx,
        }
    }
}

impl ServerBehavior for SimulatorBehavior {
    fn on_packet_received(&mut self, _raw_data: &[u8]) {
        // Refresh the per-packet snapshot from the shared settings once, so
        // the read-side methods below avoid contending with the UI thread.
        let s = self.settings.read().unwrap();
        *self.snapshot.write().unwrap() = SettingsSnapshot::from_settings(&s);
    }

    fn on_chunk_received(&mut self, chunk: ReceivedChunk<'_>) {
        let _ = self
            .event_tx
            .send(ServerEvent::Chunk(parsed_chunk_from_received(chunk)));
    }

    fn should_respond(&self, _command: u8) -> bool {
        // If offline, don't respond to any commands (device is "invisible")
        !self.snapshot.read().unwrap().status_offline
    }

    fn get_status_byte(&self) -> u8 {
        let snapshot = self.snapshot.read().unwrap();
        let mut status = IDNFLG_STATUS_REALTIME; // Always realtime capable
        if snapshot.status_malfunction {
            status |= IDNFLG_STATUS_MALFUNCTION;
        }
        if snapshot.status_offline {
            status |= IDNFLG_STATUS_OFFLINE;
        }
        if snapshot.status_excluded {
            status |= IDNFLG_STATUS_EXCLUDED;
        }
        if snapshot.status_occupied {
            status |= IDNFLG_STATUS_OCCUPIED;
        }
        status
    }

    fn get_ack_result_code(&self) -> u8 {
        self.snapshot.read().unwrap().ack_error_code
    }

    fn get_simulated_latency(&self) -> Duration {
        let ms = self.snapshot.read().unwrap().simulated_latency_ms;
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
        self.snapshot.read().unwrap().status_occupied
    }

    fn is_excluded(&self) -> bool {
        self.snapshot.read().unwrap().status_excluded
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
            Service::laser_projector(1, &config.service_name).with_dsid()
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
            match IdnServer::new(server_config, behavior) {
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
