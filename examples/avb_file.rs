//! AVB file validation example.
//!
//! Generates a 6-channel WAV file using AVB channel mapping:
//! 1: X, 2: Y, 3: R, 4: G, 5: B, 6: Intensity.
//!
//! This is useful for validating channel mapping without AVB hardware.
//! Inspect the file in Audacity or any multichannel editor.
//!
//! Run with:
//! `cargo run --example avb_file -- circle --seconds 10 --output avb-validation.wav`

mod common;

use clap::Parser;
use common::{fill_points, Shape};
use hound::{SampleFormat, WavSpec, WavWriter};
use laser_dac::{ChunkRequest, ChunkResult, LaserPoint, StreamInstant};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(about = "Generate AVB-mapped 6-channel WAV test files")]
struct Args {
    /// Shape/pattern to render
    #[arg(value_enum, default_value_t = Shape::Triangle)]
    shape: Shape,

    /// Points per chunk
    #[arg(short, long, default_value_t = 512)]
    chunk_points: usize,

    /// Point/sample rate (Hz)
    #[arg(long, default_value_t = 48_000)]
    pps: u32,

    /// Duration in seconds
    #[arg(long, default_value_t = 5.0)]
    seconds: f32,

    /// Output WAV path
    #[arg(long, default_value = "avb-validation.wav")]
    output: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = Args::parse();

    if args.chunk_points == 0 {
        return Err("chunk_points must be > 0".into());
    }
    if args.pps == 0 {
        return Err("pps must be > 0".into());
    }
    if args.seconds <= 0.0 {
        return Err("seconds must be > 0".into());
    }

    let total_points = (args.seconds as f64 * args.pps as f64).round() as u64;
    if total_points == 0 {
        return Err("requested duration produced 0 samples".into());
    }

    let spec = WavSpec {
        channels: 6,
        sample_rate: args.pps,
        bits_per_sample: 32,
        sample_format: SampleFormat::Float,
    };
    let mut writer = WavWriter::create(&args.output, spec)?;

    let mut current = 0_u64;
    while current < total_points {
        let remaining = total_points - current;
        let n_points = remaining.min(args.chunk_points as u64) as usize;

        let req = ChunkRequest {
            start: StreamInstant::new(current),
            pps: args.pps,
            min_points: 0,
            target_points: n_points,
            buffered_points: 0,
            buffered: Duration::ZERO,
            device_queued_points: None,
        };

        let mut points = vec![LaserPoint::default(); n_points];
        let written = match fill_points(args.shape, &req, &mut points) {
            ChunkResult::Filled(n) => n.min(points.len()),
            ChunkResult::Starved | ChunkResult::End => n_points,
        };

        for point in &points[..written] {
            let [x, y, r, g, b, i] = point_to_avb_samples(point);
            writer.write_sample(x)?;
            writer.write_sample(y)?;
            writer.write_sample(r)?;
            writer.write_sample(g)?;
            writer.write_sample(b)?;
            writer.write_sample(i)?;
        }

        current += written as u64;
    }

    writer.finalize()?;

    println!("Wrote {}", args.output.display());
    println!("Shape: {}", args.shape.name());
    println!("Rate: {} Hz", args.pps);
    println!("Duration: {:.2}s", args.seconds);
    println!("Channels: 1=X, 2=Y, 3=R, 4=G, 5=B, 6=I");
    println!("Inspect in Audacity: RGB/I should drop to 0 during blanking.");

    Ok(())
}

fn point_to_avb_samples(point: &LaserPoint) -> [f32; 6] {
    [
        point.x.clamp(-1.0, 1.0),
        point.y.clamp(-1.0, 1.0),
        u16_to_unit(point.r),
        u16_to_unit(point.g),
        u16_to_unit(point.b),
        u16_to_unit(point.intensity),
    ]
}

fn u16_to_unit(value: u16) -> f32 {
    value as f32 / u16::MAX as f32
}
