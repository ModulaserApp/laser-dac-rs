//! Microbenchmark for default_transition throughput.
//!
//! Outputs JSON to stdout: {"ns_per_transition": <f64>}
//!
//! Run with:
//!   cargo run --release --example bench_transition

use laser_dac::{default_transition, LaserPoint};
use std::hint::black_box;
use std::time::Instant;

const PPS: u32 = 30_000;
const TOTAL_CALLS: usize = 200_000;
const WARMUP_CALLS: usize = 20_000;

fn main() {
    let transition_fn = default_transition(PPS);

    // 100 point pairs at evenly spaced L∞ distances (0.02 .. 2.0)
    let pairs: Vec<(LaserPoint, LaserPoint)> = (0..100)
        .map(|i| {
            let t = (i as f32 + 1.0) / 100.0; // 0.01 .. 1.0
            let from = LaserPoint::new(-t, -t, 65535, 0, 0, 65535);
            let to = LaserPoint::new(t, t, 0, 65535, 0, 65535);
            (from, to)
        })
        .collect();

    // Warmup
    for i in 0..WARMUP_CALLS {
        let (ref from, ref to) = pairs[i % pairs.len()];
        let _ = black_box((transition_fn)(from, to));
    }

    // Measure
    let start = Instant::now();
    for i in 0..TOTAL_CALLS {
        let (ref from, ref to) = pairs[i % pairs.len()];
        let _ = black_box((transition_fn)(from, to));
    }
    let elapsed = start.elapsed();

    let ns_per_transition = elapsed.as_nanos() as f64 / TOTAL_CALLS as f64;
    println!("{{\"ns_per_transition\": {:.3}}}", ns_per_transition);
}
