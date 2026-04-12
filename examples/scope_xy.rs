//! Oscilloscope XY output via audio interface.
//!
//! Sends laser shapes to a stereo audio output (L=X, R=Y) for display
//! on an oscilloscope in XY mode.
//!
//! Run with: `cargo run --example scope_xy --features oscilloscope -- [shape] [--device <name>]`
//!
//! Shapes: triangle, circle, orbiting-circle, orientation, test-pattern
//!
//! **Note:** A DC-coupled audio interface is required for accurate DC
//! representation. AC-coupled interfaces will high-pass filter the signal,
//! causing the image to drift.

mod common;

use clap::Parser;
use common::{make_producer, Shape};
use laser_dac::{
    list_devices, open_device, ChunkRequest, ChunkResult, DacType, LaserPoint, Result, StreamConfig,
};

#[derive(Parser)]
#[command(about = "Send shapes to an oscilloscope in XY mode")]
struct Args {
    /// Shape to display
    #[arg(value_enum, default_value_t = Shape::Circle)]
    shape: Shape,

    /// Number of points per frame (detail level for static shapes)
    #[arg(short, long, default_value_t = 200)]
    points: usize,

    /// Geometry scale around center (0,0); range: (0, 10]
    #[arg(long, default_value_t = 0.8, value_parser = parse_scale)]
    scale: f32,

    /// Audio device name (substring match). If omitted, lists devices and uses the first one.
    #[arg(short, long)]
    device: Option<String>,
}

fn parse_scale(value: &str) -> std::result::Result<f32, String> {
    let scale: f32 = value
        .parse()
        .map_err(|_| format!("invalid scale '{value}': expected a float in (0, 10]"))?;
    if !scale.is_finite() || scale <= 0.0 || scale > 10.0 {
        return Err("scale must be finite and in (0, 10]".to_string());
    }
    Ok(scale)
}

fn main() -> Result<()> {
    env_logger::init();
    let args = Args::parse();

    println!("Scanning for oscilloscope devices...\n");
    let devices = list_devices()?;

    let audio_devices: Vec<_> = devices
        .iter()
        .filter(|d| d.kind == DacType::Oscilloscope)
        .collect();

    if audio_devices.is_empty() {
        println!("No oscilloscope devices found.");
        println!(
            "Make sure the `oscilloscope` feature is enabled and an audio device is connected."
        );
        return Ok(());
    }

    println!("Found {} oscilloscope device(s):", audio_devices.len());
    for (i, dev) in audio_devices.iter().enumerate() {
        println!(
            "  [{}] {} (sample rate: {} Hz)",
            i, dev.name, dev.caps.pps_max
        );
    }
    println!();

    // Select device: by --device substring, or first available
    let selected = if let Some(ref pattern) = args.device {
        audio_devices
            .iter()
            .find(|d| d.name.to_lowercase().contains(&pattern.to_lowercase()))
            .copied()
            .ok_or_else(|| {
                laser_dac::Error::invalid_config(format!(
                    "No oscilloscope device matching '{}'. See list above.",
                    pattern
                ))
            })?
    } else {
        audio_devices[0]
    };

    println!("Using: {} ({})", selected.name, selected.id);

    let device = open_device(&selected.id)?;
    let sample_rate = device.caps().pps_max;

    println!("Sample rate: {} Hz", sample_rate);
    println!("Shape: {}", args.shape.name());
    println!();

    // For oscilloscope, PPS must equal the sample rate
    let config = StreamConfig::new(sample_rate);
    let (stream, info) = device.start_stream(config)?;

    println!("Streaming {} to {}...", args.shape.name(), info.name);
    println!("Connect audio L/R outputs to oscilloscope X/Y inputs.");
    println!("Press Ctrl+C to stop.\n");

    // Arm the output (unmute)
    stream.control().arm()?;

    // Ctrl+C handler
    let control = stream.control().clone();
    ctrlc::set_handler(move || {
        let _ = control.stop();
    })
    .expect("failed to set Ctrl+C handler");

    let mut producer = make_producer(args.shape, args.points, args.scale);

    let exit = stream.run(
        move |req: &ChunkRequest, buffer: &mut [LaserPoint]| -> ChunkResult {
            producer(req, buffer)
        },
        |err| {
            eprintln!("Stream error: {}", err);
        },
    )?;

    println!("\nStream ended: {:?}", exit);
    Ok(())
}
