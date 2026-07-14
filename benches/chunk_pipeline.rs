//! One driver iteration's CPU per DAC: produce a slice, then convert and encode
//! it the way that DAC's backend does on the driver thread.
//!
//! This is the most production-shaped target in the suite. `presentation` stops
//! at the slice and `protocol_encoding` starts from one; neither corresponds to
//! a unit of work the driver actually performs, and composing their numbers
//! misses the copies and per-chunk allocations that only exist where the two
//! halves meet.
//!
//! Read these numbers **per chunk**. No `Throughput` is set on purpose: a chunk
//! is what has to fit inside the driver's cadence, and per-point framing hides
//! fixed cost — which is most of the story at IDN's 179 points.

mod common;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use laser_dac::benchmark_support::{ChunkPipelineBenchmark, ChunkTarget, TransitionWorkload};
use laser_dac::Frame;

const PPS: u32 = 30_000;

/// A realistic authored frame. Chunk sizes are a property of the DAC, not of
/// the content, so every target draws from the same frame.
const FRAME_POINTS: usize = 500;

/// Helios composes into a hardware frame rather than pulling chunks, so its
/// size comes from `max_points_per_chunk` used as a frame capacity.
const HELIOS_FRAME_CAPACITY: usize = 4_095;

/// `(name, target, chunk_points)`, each at that DAC's real chunk size.
///
/// These are `max_points_per_chunk` from each protocol's `default_capabilities`
/// — the deadline-relevant worst case, since the FIFO adapter clamps its
/// request to it. Ether Dream also gets its ~5 ms quantum floor at 30 kHz (150
/// points), which is the *other* end of its range; the gap between the two is
/// how much of its per-chunk cost is fixed rather than per-point.
const FIFO_CASES: [(&str, ChunkTarget, usize); 5] = [
    ("idn", ChunkTarget::Idn, 179),
    ("ether_dream", ChunkTarget::EtherDream, 1_799),
    ("ether_dream_quantum", ChunkTarget::EtherDream, 150),
    // LaserCube network sizes per connection profile: 80 on Ethernet and as a
    // Wi-Fi client, 140 as a Wi-Fi server.
    ("lasercube_network", ChunkTarget::LaserCubeNetwork, 80),
    ("lasercube_network_wifi", ChunkTarget::LaserCubeNetwork, 140),
];

/// FIFO steady state: chunks are pulled far more often than content arrives, so
/// no frame is queued per iteration. The frame loops, and each wrap drives a
/// real `default_transition` across the seam.
fn benchmark_fifo_chunk(c: &mut Criterion) {
    let frame = Frame::new(common::artwork_points(FRAME_POINTS, 0.0));
    let mut group = c.benchmark_group("chunk_pipeline/fifo");

    for (name, target, chunk_points) in FIFO_CASES {
        let mut fixture = ChunkPipelineBenchmark::new(
            frame.clone(),
            TransitionWorkload::Default { pps: PPS },
            0,
            chunk_points,
            target,
        );
        assert!(!fixture.is_frame_swap());
        assert!(fixture.step(chunk_points, PPS) > 0);

        group.bench_with_input(
            BenchmarkId::new(name, chunk_points),
            &chunk_points,
            |b, &chunk_points| {
                b.iter(|| {
                    black_box(fixture.step(black_box(chunk_points), PPS));
                });
            },
        );
    }

    // LaserCube USB pulls the largest FIFO chunk of any DAC.
    let chunk_points = 4_096;
    let mut fixture = ChunkPipelineBenchmark::new(
        frame,
        TransitionWorkload::Default { pps: PPS },
        0,
        chunk_points,
        ChunkTarget::LaserCubeUsb,
    );
    assert!(fixture.step(chunk_points, PPS) > 0);
    group.bench_with_input(
        BenchmarkId::new("lasercube_usb", chunk_points),
        &chunk_points,
        |b, &chunk_points| {
            b.iter(|| {
                black_box(fixture.step(black_box(chunk_points), PPS));
            });
        },
    );

    group.finish();
}

/// Frame-swap steady state: the DAC repeats the frame in hardware and is only
/// written when content changes, so a frame is queued per iteration.
fn benchmark_frame_swap_chunk(c: &mut Criterion) {
    let frames = [
        Frame::new(common::artwork_points(FRAME_POINTS, 0.0)),
        Frame::new(common::artwork_points(FRAME_POINTS, 0.7)),
    ];
    let mut fixture = ChunkPipelineBenchmark::new(
        frames[0].clone(),
        TransitionWorkload::Default { pps: PPS },
        0,
        HELIOS_FRAME_CAPACITY,
        ChunkTarget::Helios,
    );
    assert!(fixture.is_frame_swap());
    assert!(fixture.step(0, PPS) > 0);

    let mut group = c.benchmark_group("chunk_pipeline/frame_swap");
    group.bench_function(BenchmarkId::new("helios", FRAME_POINTS), |b| {
        let mut next = 0usize;
        b.iter(|| {
            next ^= 1;
            fixture.queue_frame(frames[next].clone());
            black_box(fixture.step(0, PPS));
        });
    });
    group.finish();
}

criterion_group!(benches, benchmark_fifo_chunk, benchmark_frame_swap_chunk);
criterion_main!(benches);
