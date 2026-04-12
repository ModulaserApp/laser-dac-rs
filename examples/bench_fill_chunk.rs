//! Microbenchmark for PresentationEngine::fill_chunk throughput.
//!
//! Outputs JSON to stdout: {"ns_per_point": <f64>}
//!
//! Run with:
//!   cargo run --release --features testutils --example bench_fill_chunk

use laser_dac::presentation::PresentationEngine;
use laser_dac::{Frame, LaserPoint, TransitionPlan};
use std::time::Instant;

/// Number of points in the test frame (typical laser frame size).
const FRAME_POINTS: usize = 500;

/// Output buffer size (typical DAC chunk request).
const CHUNK_SIZE: usize = 4096;

/// Total points to stream through fill_chunk (enough for stable timing).
const TOTAL_POINTS: usize = 10_000_000;

/// Number of warmup points before measurement (prime caches, branch predictors).
const WARMUP_POINTS: usize = 500_000;

fn make_frame(n: usize) -> Frame {
    let points: Vec<LaserPoint> = (0..n)
        .map(|i| {
            let t = i as f32 / n as f32 * std::f32::consts::TAU;
            LaserPoint::new(t.cos(), t.sin(), 65535, 0, 0, 65535)
        })
        .collect();
    Frame::new(points)
}

fn main() {
    // Use a no-op transition to isolate fill_chunk's core loop performance.
    // default_transition involves allocation which would dominate the measurement.
    let transition_fn: laser_dac::TransitionFn =
        Box::new(|_from: &LaserPoint, _to: &LaserPoint| TransitionPlan::Coalesce);

    let mut engine = PresentationEngine::new(transition_fn);
    let frame = make_frame(FRAME_POINTS);
    engine.set_pending(frame);

    let mut buffer = vec![LaserPoint::default(); CHUNK_SIZE];

    // Warmup
    let mut warmup_remaining = WARMUP_POINTS;
    while warmup_remaining > 0 {
        let n = engine.fill_chunk(&mut buffer, CHUNK_SIZE);
        warmup_remaining = warmup_remaining.saturating_sub(n);
    }

    // Measure
    let mut total_written = 0usize;
    let start = Instant::now();
    while total_written < TOTAL_POINTS {
        let n = engine.fill_chunk(&mut buffer, CHUNK_SIZE);
        total_written += n;
    }
    let elapsed = start.elapsed();

    let ns_per_point = elapsed.as_nanos() as f64 / total_written as f64;

    // JSON to stdout — nothing else
    println!("{{\"ns_per_point\": {:.3}}}", ns_per_point);
}
