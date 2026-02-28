//! IDN Simulator - A debug tool for testing without hardware.
//!
//! Acts as a virtual IDN laser DAC that can be discovered by the laser-dac crate
//! and renders received laser points in an egui window.

mod app;
mod fps_estimator;
mod persistence_buffer;
mod protocol_handler;
mod renderer;
mod server;
mod settings;
mod timing;

use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::{Arc, RwLock};
use std::thread;

use clap::Parser;
use eframe::egui;

use app::{SimulatorApp, TestApp};
use protocol_handler::RenderPoint;
use server::{run_server, SimulatorServerConfig};
use settings::SimulatorSettings;

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
            Box::new(|_cc| Ok(Box::new(TestApp::new(generate_test_points())))),
        );
    }

    let (event_tx, event_rx) = mpsc::channel();
    let running = Arc::new(AtomicBool::new(true));
    let settings = Arc::new(RwLock::new(SimulatorSettings::default()));

    // Start UDP server thread
    let server_running = Arc::clone(&running);
    let server_settings = Arc::clone(&settings);
    let hostname = args.hostname.clone();
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
        Box::new(|_cc| {
            Ok(Box::new(SimulatorApp::new(
                event_rx, running, settings, hostname,
            )))
        }),
    );

    // Wait for server thread to finish
    let _ = server_handle.join();

    result
}
