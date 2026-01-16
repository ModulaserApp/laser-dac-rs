//! Audio-reactive laser example.
//!
//! This example captures audio from the default input device and renders it to the laser:
//!
//! - **Mono (1 channel)**: Time-domain oscilloscope (X=time, Y=amplitude)
//! - **Stereo (2+ channels)**: XY oscilloscope mode (left=X, right=Y)
//!
//! In both modes, the laser is blanked when the signal is very low to prevent beam dwell.
//!
//! Run with: `cargo run --example audio`
//!
//! Or use the audio shape with any example:
//! `cargo run --example automatic -- audio`
//! `cargo run --example manual -- audio`

mod common;

use common::{create_points, Shape};
use laser_dac::{list_devices, open_device, ChunkRequest, StreamConfig, Result};
use log::info;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    info!("Audio-reactive laser example starting...");

    // Discover and connect to laser
    info!("Scanning for laser DACs...");
    let devices = list_devices()?;

    if devices.is_empty() {
        log::error!("No laser DACs found!");
        return Ok(());
    }

    let device_info = &devices[0];
    info!(
        "Found DAC: {} ({}) - PPS range: {}-{}",
        device_info.name, device_info.kind, device_info.caps.pps_min, device_info.caps.pps_max
    );

    let device = open_device(&device_info.id)?;
    let pps = 30_000u32;
    let config = StreamConfig::new(pps);

    info!("Starting laser stream at {} PPS", pps);
    let (stream, info) = device.start_stream(config)?;

    info!("Connected to: {}", info.name);
    stream.control().arm()?;
    info!("Laser output armed");

    info!("Streaming audio to laser - press Ctrl+C to stop");

    // Track chunks for logging
    let chunk_count = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&chunk_count);

    // Run in callback mode - the stream calls our producer when it needs data
    let exit = stream.run(
        move |req: ChunkRequest| {
            let count = counter.fetch_add(1, Ordering::Relaxed);

            // Use the common create_points with Audio shape
            let points = create_points(Shape::Audio, req.n_points, count as usize);

            if count % 100 == 0 {
                log::debug!("Chunks: {}", count);
            }

            Some(points)
        },
        |err| {
            log::error!("Stream error: {}", err);
        },
    )?;

    info!("Stream ended: {:?}", exit);
    info!("Total chunks: {}", chunk_count.load(Ordering::Relaxed));

    Ok(())
}
