//! IDN Simulator - A debug tool for testing without hardware.
//!
//! Acts as a virtual IDN laser DAC that can be discovered by the laser-dac crate
//! and renders received laser points in an egui window.

mod protocol_handler;
mod renderer;
mod server;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::Instant;

use clap::Parser;
use eframe::egui;

use protocol_handler::RenderPoint;
use renderer::RenderSettings;
use server::{run_server, ServerEvent, SimulatorServerConfig};

/// Shared settings between UI and server.
#[derive(Clone)]
pub struct SimulatorSettings {
    // Visualization
    pub point_size: f32,
    pub show_grid: bool,
    pub show_blanking: bool,
    pub invert_y: bool,

    // Device status flags (for scan response)
    pub status_malfunction: bool,
    pub status_offline: bool,
    pub status_excluded: bool,
    pub status_occupied: bool,

    // Connection
    pub link_timeout_ms: u32,
    pub simulated_latency_ms: u32,
    pub force_disconnect: bool,

    // ACK error injection
    pub ack_error_code: u8,
}

impl Default for SimulatorSettings {
    fn default() -> Self {
        Self {
            point_size: 2.0,
            show_grid: true,
            show_blanking: false,
            invert_y: false,
            status_malfunction: false,
            status_offline: false,
            status_excluded: false,
            status_occupied: false,
            link_timeout_ms: 1000,
            simulated_latency_ms: 0,
            force_disconnect: false,
            ack_error_code: 0x00,
        }
    }
}

/// ACK error code options for the dropdown.
#[derive(Clone, Copy, PartialEq)]
pub enum AckErrorOption {
    Success,
    EmptyClose,
    SessionsOccupied,
    GroupExcluded,
    InvalidPayload,
    ProcessingError,
}

impl AckErrorOption {
    fn code(&self) -> u8 {
        match self {
            AckErrorOption::Success => 0x00,
            AckErrorOption::EmptyClose => 0xEB,
            AckErrorOption::SessionsOccupied => 0xEC,
            AckErrorOption::GroupExcluded => 0xED,
            AckErrorOption::InvalidPayload => 0xEE,
            AckErrorOption::ProcessingError => 0xEF,
        }
    }

    fn label(&self) -> &'static str {
        match self {
            AckErrorOption::Success => "Success (0x00)",
            AckErrorOption::EmptyClose => "Empty close (0xEB)",
            AckErrorOption::SessionsOccupied => "Sessions occupied (0xEC)",
            AckErrorOption::GroupExcluded => "Group excluded (0xED)",
            AckErrorOption::InvalidPayload => "Invalid payload (0xEE)",
            AckErrorOption::ProcessingError => "Processing error (0xEF)",
        }
    }
}

#[derive(Parser)]
#[command(
    name = "idn-simulator",
    about = "IDN laser DAC simulator for debugging"
)]
struct Args {
    /// Hostname to advertise in scan responses
    #[arg(short = 'n', long, default_value = "IDN-Simulator")]
    hostname: String,

    /// Service name to advertise
    #[arg(short = 's', long, default_value = "Simulator Laser")]
    service_name: String,

    /// UDP port to listen on
    #[arg(short, long, default_value_t = 7255)]
    port: u16,

    /// Test mode: render hardcoded test lines instead of network data
    #[arg(long)]
    test: bool,
}

/// Generate test points for debugging rendering.
/// Line 1: top-left area, red, ~5% width
/// Line 2: bottom-right area, green, ~2% width
fn generate_test_points() -> Vec<RenderPoint> {
    vec![
        // Line 1: top-left, red (from -0.9,-0.9 to -0.85,-0.85) - 5% of 2.0 range
        RenderPoint {
            x: -0.9,
            y: 0.9,
            r: 1.0,
            g: 0.0,
            b: 0.0,
            intensity: 1.0,
        },
        RenderPoint {
            x: -0.8,
            y: 0.8,
            r: 1.0,
            g: 0.0,
            b: 0.0,
            intensity: 1.0,
        },
        // Blank move
        RenderPoint {
            x: -0.8,
            y: 0.8,
            r: 0.0,
            g: 0.0,
            b: 0.0,
            intensity: 0.0,
        },
        RenderPoint {
            x: 0.88,
            y: -0.88,
            r: 0.0,
            g: 0.0,
            b: 0.0,
            intensity: 0.0,
        },
        // Line 2: bottom-right, green (from 0.88,-0.88 to 0.92,-0.92) - 2% of 2.0 range
        RenderPoint {
            x: 0.88,
            y: -0.88,
            r: 0.0,
            g: 1.0,
            b: 0.0,
            intensity: 1.0,
        },
        RenderPoint {
            x: 0.92,
            y: -0.92,
            r: 0.0,
            g: 1.0,
            b: 0.0,
            intensity: 1.0,
        },
    ]
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = Args::parse();

    if args.test {
        // Test mode: just render test points, no server
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([800.0, 600.0])
                .with_title("IDN Simulator - TEST MODE"),
            ..Default::default()
        };

        return eframe::run_native(
            "IDN Simulator",
            options,
            Box::new(|_cc| Ok(Box::new(TestApp::new()))),
        );
    }

    let (event_tx, event_rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let settings = Arc::new(RwLock::new(SimulatorSettings::default()));

    // Start UDP server thread
    let server_running = Arc::clone(&running);
    let server_settings = Arc::clone(&settings);
    let server_config = SimulatorServerConfig {
        hostname: args.hostname.clone(),
        service_name: args.service_name.clone(),
        port: args.port,
    };

    let server_handle = thread::spawn(move || {
        if let Err(e) = run_server(server_config, server_running, server_settings, event_tx) {
            log::error!("Failed to start server: {}", e);
        }
    });

    // Run egui app
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([800.0, 600.0])
            .with_title(format!("IDN Simulator - {}", args.hostname)),
        ..Default::default()
    };

    let result = eframe::run_native(
        "IDN Simulator",
        options,
        Box::new(|_cc| Ok(Box::new(SimulatorApp::new(event_rx, running, settings)))),
    );

    // Wait for server thread to finish
    let _ = server_handle.join();

    result
}

/// Test mode app - renders hardcoded test points.
struct TestApp {
    test_points: Vec<RenderPoint>,
}

impl TestApp {
    fn new() -> Self {
        Self {
            test_points: generate_test_points(),
        }
    }
}

impl eframe::App for TestApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::TopBottomPanel::bottom("stats").show(ctx, |ui| {
            ui.label("TEST MODE - Showing hardcoded test lines");
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                renderer::render_laser_canvas(ui, &self.test_points, &RenderSettings::default());
            });
    }
}

struct SimulatorApp {
    event_rx: mpsc::Receiver<ServerEvent>,
    running: Arc<AtomicBool>,
    settings: Arc<RwLock<SimulatorSettings>>,
    current_frame: Vec<RenderPoint>,
    /// Buffer for accumulating points from multiple packets
    accumulator: Vec<RenderPoint>,
    /// Timestamp of last received packet
    last_packet_time: Option<Instant>,
    frames_received: u64,
    client_address: Option<SocketAddr>,
    /// Local copy of ACK error selection for UI
    ack_error_selection: AckErrorOption,
}

/// Time window for accumulating packets into a single frame (ms).
/// Packets arriving within this window are considered part of the same frame.
/// This needs to be short enough to handle 60fps (16ms) but long enough
/// to catch all packets from a multi-packet frame.
const FRAME_ACCUMULATION_WINDOW_MS: u64 = 8;

impl SimulatorApp {
    fn new(
        event_rx: mpsc::Receiver<ServerEvent>,
        running: Arc<AtomicBool>,
        settings: Arc<RwLock<SimulatorSettings>>,
    ) -> Self {
        Self {
            event_rx,
            running,
            settings,
            current_frame: Vec::new(),
            accumulator: Vec::new(),
            last_packet_time: None,
            frames_received: 0,
            client_address: None,
            ack_error_selection: AckErrorOption::Success,
        }
    }
}

impl eframe::App for SimulatorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let now = Instant::now();

        // Check if we should finalize the accumulated frame
        // (no new packets for a while means the frame is complete)
        if let Some(last_time) = self.last_packet_time {
            if now.duration_since(last_time).as_millis() as u64 > FRAME_ACCUMULATION_WINDOW_MS
                && !self.accumulator.is_empty()
            {
                // Finalize the accumulated frame
                self.current_frame = std::mem::take(&mut self.accumulator);
                self.frames_received += 1;
                self.last_packet_time = None;
            }
        }

        // Process incoming events
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                ServerEvent::Frame(points) => {
                    // Accumulate points from this packet
                    self.accumulator.extend(points);
                    self.last_packet_time = Some(now);
                }
                ServerEvent::ClientConnected(addr) => {
                    self.client_address = Some(addr);
                }
                ServerEvent::ClientDisconnected => {
                    self.client_address = None;
                }
            }
        }

        // Request continuous repaints for smooth rendering
        ctx.request_repaint();

        // Get render settings for the canvas
        let render_settings = {
            let settings = self.settings.read().unwrap();
            RenderSettings {
                point_size: settings.point_size,
                show_grid: settings.show_grid,
                show_blanking: settings.show_blanking,
                invert_y: settings.invert_y,
            }
        };

        // Left side panel with controls
        egui::SidePanel::left("controls")
            .default_width(180.0)
            .show(ctx, |ui| {
                ui.heading("Visualization");
                ui.add_space(4.0);

                {
                    let mut settings = self.settings.write().unwrap();

                    ui.horizontal(|ui| {
                        ui.label("Line size:");
                        ui.add(
                            egui::DragValue::new(&mut settings.point_size)
                                .speed(0.1)
                                .range(1.0..=10.0),
                        );
                    });

                    ui.checkbox(&mut settings.show_grid, "Show grid");
                    ui.checkbox(&mut settings.show_blanking, "Show blanking");
                    ui.checkbox(&mut settings.invert_y, "Invert Y axis");
                }

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Device Status");
                ui.add_space(4.0);

                {
                    let mut settings = self.settings.write().unwrap();

                    ui.checkbox(&mut settings.status_malfunction, "Malfunction")
                        .on_hover_text("Sets malfunction flag in scan response");
                    ui.checkbox(&mut settings.status_offline, "Offline")
                        .on_hover_text("Device becomes invisible to discovery");
                    ui.checkbox(&mut settings.status_excluded, "Excluded")
                        .on_hover_text("Rejects all real-time messages");
                    ui.checkbox(&mut settings.status_occupied, "Occupied")
                        .on_hover_text("Rejects new clients while one is connected");
                }

                ui.add_space(8.0);
                ui.separator();
                ui.heading("Connection");
                ui.add_space(4.0);

                {
                    let mut settings = self.settings.write().unwrap();

                    ui.horizontal(|ui| {
                        ui.label("Timeout (ms):");
                        ui.add(
                            egui::DragValue::new(&mut settings.link_timeout_ms)
                                .speed(10)
                                .range(100..=5000),
                        );
                    });

                    ui.horizontal(|ui| {
                        ui.label("Latency (ms):");
                        ui.add(
                            egui::DragValue::new(&mut settings.simulated_latency_ms)
                                .speed(1)
                                .range(0..=200),
                        );
                    });

                    if ui.button("Disconnect client").clicked() {
                        settings.force_disconnect = true;
                    }
                }

                ui.add_space(8.0);
                ui.separator();
                ui.heading("ACK Injection");
                ui.add_space(4.0);

                egui::ComboBox::from_label("")
                    .selected_text(self.ack_error_selection.label())
                    .show_ui(ui, |ui| {
                        let options = [
                            AckErrorOption::Success,
                            AckErrorOption::EmptyClose,
                            AckErrorOption::SessionsOccupied,
                            AckErrorOption::GroupExcluded,
                            AckErrorOption::InvalidPayload,
                            AckErrorOption::ProcessingError,
                        ];
                        for option in options {
                            if ui
                                .selectable_value(
                                    &mut self.ack_error_selection,
                                    option,
                                    option.label(),
                                )
                                .clicked()
                            {
                                let mut settings = self.settings.write().unwrap();
                                settings.ack_error_code = option.code();
                            }
                        }
                    });
            });

        // Stats panel at bottom
        egui::TopBottomPanel::bottom("stats").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label(format!("Frames: {}", self.frames_received));
                ui.separator();
                ui.label(format!("Points: {}", self.current_frame.len()));
                if let Some(addr) = &self.client_address {
                    ui.separator();
                    ui.label(format!("Client: {}", addr));
                }
            });
        });

        // Main canvas
        egui::CentralPanel::default()
            .frame(egui::Frame::none())
            .show(ctx, |ui| {
                renderer::render_laser_canvas(ui, &self.current_frame, &render_settings);
            });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.running.store(false, Ordering::SeqCst);
    }
}
