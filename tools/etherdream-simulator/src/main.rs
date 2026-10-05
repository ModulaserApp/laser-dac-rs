//! Ether Dream simulator: a virtual Ether Dream DAC with a GUI.
//!
//! Serves the crate's Ether Dream simulator model on the real ports: TCP 7765
//! for streaming and UDP broadcasts to port 7654 for discovery. Any Ether
//! Dream client on the network, including this crate's discoverer, sees it as
//! hardware. The window renders the points the simulated DAC plays, and lets
//! you switch firmware profiles and inject faults while a client is connected.

mod app;
mod sim;

use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use eframe::egui;
use laser_dac::protocols::ether_dream::FirmwareProfile;

use app::SimulatorApp;
use sim::{SimHandle, SimOptions};

#[derive(Parser)]
#[command(
    name = "etherdream-simulator",
    about = "Ether Dream laser DAC simulator for debugging"
)]
struct Args {
    /// Firmware profile to simulate.
    #[arg(short, long, default_value = "ed2-r331")]
    profile: String,

    /// TCP address to listen on.
    #[arg(long, default_value = "0.0.0.0:7765")]
    bind: SocketAddr,

    /// Where to send discovery broadcasts.
    #[arg(long, default_value = "255.255.255.255:7654")]
    broadcast_target: SocketAddr,

    /// Broadcast interval in milliseconds. Real hardware uses 1000.
    #[arg(long, default_value_t = 1000)]
    broadcast_interval_ms: u64,

    /// Do not send discovery broadcasts.
    #[arg(long)]
    no_broadcast: bool,

    /// List the available profiles and exit.
    #[arg(long)]
    list_profiles: bool,
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let profiles = FirmwareProfile::all();
    if args.list_profiles {
        for p in &profiles {
            println!(
                "{:<12} {:?}, capacity {}, max {} pps",
                p.name, p.provenance, p.buffer_capacity, p.max_point_rate
            );
        }
        return Ok(());
    }
    let Some(profile_index) = profiles.iter().position(|p| p.name == args.profile) else {
        let names: Vec<_> = profiles.iter().map(|p| p.name).collect();
        eprintln!(
            "unknown profile {:?}; available: {}",
            args.profile,
            names.join(", ")
        );
        std::process::exit(2);
    };

    let options = SimOptions {
        bind: args.bind,
        broadcast: (!args.no_broadcast).then_some((
            args.broadcast_target,
            Duration::from_millis(args.broadcast_interval_ms.max(10)),
        )),
    };
    let sim = match SimHandle::start(profiles[profile_index].clone(), options.clone()) {
        Ok(sim) => sim,
        Err(e) => {
            eprintln!("failed to start the simulator on {}: {e}", args.bind);
            std::process::exit(1);
        }
    };
    log::info!(
        "serving {} on {} (broadcasts: {})",
        profiles[profile_index].name,
        sim.addr(),
        match options.broadcast {
            Some((t, _)) => t.to_string(),
            None => "off".into(),
        }
    );

    let native = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1000.0, 680.0])
            .with_title("Ether Dream Simulator"),
        ..Default::default()
    };
    eframe::run_native(
        "Ether Dream Simulator",
        native,
        Box::new(move |_cc| {
            Ok(Box::new(SimulatorApp::new(
                sim,
                options,
                profiles,
                profile_index,
            )))
        }),
    )
}
