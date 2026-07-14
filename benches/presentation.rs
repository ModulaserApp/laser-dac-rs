mod common;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use laser_dac::benchmark_support::{
    ColorDelayBenchmark, PresentationBenchmark, SlicePipelineBenchmark, TransitionWorkload,
};
use laser_dac::{default_transition, Frame, LaserPoint, TransitionPlan};

const PPS: u32 = 30_000;
const FRAME_POINTS: usize = 500;
const CHUNK_POINTS: usize = 4_096;

fn benchmark_fifo(c: &mut Criterion) {
    let frame = Frame::new(common::artwork_points(FRAME_POINTS, 0.0));
    let mut output = vec![LaserPoint::default(); CHUNK_POINTS];
    let mut group = c.benchmark_group("presentation/fifo");
    group.throughput(Throughput::Elements(CHUNK_POINTS as u64));

    let mut steady = PresentationBenchmark::new(frame.clone(), TransitionWorkload::Coalesce);
    assert_eq!(steady.fill_chunk(&mut output), CHUNK_POINTS);
    group.bench_function("coalesced_steady_state", |b| {
        b.iter(|| {
            let n = steady.fill_chunk(black_box(&mut output));
            black_box(&output[..n]);
        });
    });

    let mut transitions =
        PresentationBenchmark::new(frame, TransitionWorkload::Default { pps: PPS });
    assert_eq!(transitions.fill_chunk(&mut output), CHUNK_POINTS);
    group.bench_function("default_self_loop_transition", |b| {
        b.iter(|| {
            let n = transitions.fill_chunk(black_box(&mut output));
            black_box(&output[..n]);
        });
    });

    group.finish();
}

/// Worst case by construction: a new frame is queued on every call, whereas
/// content normally arrives at frame rate while chunks are pulled far more
/// often. Isolates promotion cost rather than modelling a duty cycle.
fn benchmark_pending_frame_promotion(c: &mut Criterion) {
    let frames = [
        Frame::new(common::artwork_points(FRAME_POINTS, 0.0)),
        Frame::new(common::artwork_points(FRAME_POINTS, 0.7)),
    ];
    let mut fixture =
        PresentationBenchmark::new(frames[0].clone(), TransitionWorkload::Default { pps: PPS });
    let mut output = vec![LaserPoint::default(); FRAME_POINTS + 128];
    assert_eq!(fixture.fill_chunk(&mut output), output.len());

    c.bench_function("presentation/fifo/pending_frame_promotion", |b| {
        let mut next = 0usize;
        b.iter(|| {
            next ^= 1;
            fixture.queue_frame(frames[next].clone());
            let n = fixture.fill_chunk(black_box(&mut output));
            black_box(&output[..n]);
        });
    });
}

/// Engine-level frame-swap composition. This is only half of what a frame-swap
/// DAC runs per frame; `slice_pipeline/frame_swap_color_delay` covers the rest.
fn benchmark_frame_swap(c: &mut Criterion) {
    let mut group = c.benchmark_group("presentation/frame_swap");

    for points in [FRAME_POINTS, 4_095] {
        let frames = [
            Frame::new(common::artwork_points(points, 0.0)),
            Frame::new(common::artwork_points(points, 0.7)),
        ];
        let mut fixture =
            PresentationBenchmark::new(frames[0].clone(), TransitionWorkload::Default { pps: PPS });
        fixture.set_frame_capacity(Some(4_095));
        assert!(!fixture.compose_hardware_frame().is_empty());

        group.throughput(Throughput::Elements(points as u64));
        group.bench_with_input(BenchmarkId::new("frame_change", points), &points, |b, _| {
            let mut next = 0usize;
            b.iter(|| {
                next ^= 1;
                fixture.queue_frame(frames[next].clone());
                black_box(fixture.compose_hardware_frame());
            });
        });
    }

    group.finish();
}

/// FIFO pipeline path used by Ether Dream, IDN, LaserCube and AVB, running the
/// production transition so composition, blanking and colour delay are all
/// represented.
fn benchmark_slice_pipeline_fifo(c: &mut Criterion) {
    let frame = Frame::new(common::artwork_points(FRAME_POINTS, 0.0));
    let mut group = c.benchmark_group("presentation/slice_pipeline");
    group.throughput(Throughput::Elements(CHUNK_POINTS as u64));

    for delay in [0usize, 5, 32] {
        let mut fixture = SlicePipelineBenchmark::new(
            frame.clone(),
            TransitionWorkload::Default { pps: PPS },
            delay,
            CHUNK_POINTS,
        );
        assert_eq!(
            fixture.produce_fifo_chunk(CHUNK_POINTS, PPS).len(),
            CHUNK_POINTS
        );
        group.bench_with_input(
            BenchmarkId::new("fifo_color_delay", delay),
            &delay,
            |b, _| {
                b.iter(|| {
                    black_box(fixture.produce_fifo_chunk(CHUNK_POINTS, PPS));
                });
            },
        );
    }

    group.finish();
}

/// Frame-swap pipeline path used by Helios (`OutputModel::UsbFrameSwap`): the
/// composed frame plus the copy, blanking and colour-delay work the engine-level
/// benchmark does not reach. A frame is queued per call because a frame-swap DAC
/// repeats the frame in hardware and is only written when content changes.
fn benchmark_slice_pipeline_frame_swap(c: &mut Criterion) {
    let mut group = c.benchmark_group("presentation/slice_pipeline");

    for points in [FRAME_POINTS, 4_095] {
        let frames = [
            Frame::new(common::artwork_points(points, 0.0)),
            Frame::new(common::artwork_points(points, 0.7)),
        ];

        for delay in [0usize, 5] {
            let mut fixture = SlicePipelineBenchmark::new(
                frames[0].clone(),
                TransitionWorkload::Default { pps: PPS },
                delay,
                points,
            );
            assert!(!fixture.produce_frame_swap(PPS).is_empty());

            group.throughput(Throughput::Elements(points as u64));
            group.bench_with_input(
                BenchmarkId::new("frame_swap_color_delay", format!("{points}_delay{delay}")),
                &delay,
                |b, _| {
                    let mut next = 0usize;
                    b.iter(|| {
                        next ^= 1;
                        fixture.queue_frame(frames[next].clone());
                        black_box(fixture.produce_frame_swap(PPS));
                    });
                },
            );
        }
    }

    group.finish();
}

/// The color delay in isolation, with no composition underneath it.
///
/// The `slice_pipeline` groups run the delay fused with composition, so the only
/// way to price the delay there is to subtract the `delay0` case from the
/// `delay5` case — two separately-sampled numbers whose difference carries both
/// their errors. This group measures `ColorDelayLine::apply` directly, so a
/// change to the delay shows up as a change to this number.
///
/// `delay0` is the early return, and it is here as the floor the other cases are
/// read against, not as a workload.
fn benchmark_color_delay(c: &mut Criterion) {
    let points = common::artwork_points(CHUNK_POINTS, 0.0);
    let mut group = c.benchmark_group("presentation/color_delay");
    group.throughput(Throughput::Elements(CHUNK_POINTS as u64));

    for delay in [0usize, 5, 32] {
        let mut fixture = ColorDelayBenchmark::new(delay, &points);
        assert_eq!(fixture.apply().len(), CHUNK_POINTS);

        group.bench_with_input(BenchmarkId::new("apply", delay), &delay, |b, _| {
            b.iter(|| {
                black_box(fixture.apply());
            });
        });
    }

    group.finish();
}

fn benchmark_default_transition(c: &mut Criterion) {
    let transition = default_transition(PPS);
    let cases = [
        (
            "short",
            LaserPoint::new(-0.01, -0.01, 65535, 0, 0, 65535),
            LaserPoint::new(0.01, 0.01, 0, 65535, 0, 65535),
        ),
        (
            "full_scale",
            LaserPoint::new(-1.0, -1.0, 65535, 0, 0, 65535),
            LaserPoint::new(1.0, 1.0, 0, 65535, 0, 65535),
        ),
    ];

    let mut group = c.benchmark_group("presentation/default_transition");
    for (name, from, to) in cases {
        let expected_len = match transition(&from, &to) {
            TransitionPlan::Transition(points) => points.len(),
            TransitionPlan::Coalesce => 0,
        };
        assert!(expected_len > 0);
        group.throughput(Throughput::Elements(expected_len as u64));
        group.bench_function(name, |b| {
            b.iter(|| {
                black_box(transition(black_box(&from), black_box(&to)));
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    benchmark_fifo,
    benchmark_pending_frame_promotion,
    benchmark_frame_swap,
    benchmark_slice_pipeline_fifo,
    benchmark_slice_pipeline_frame_swap,
    benchmark_color_delay,
    benchmark_default_transition,
);
criterion_main!(benches);
