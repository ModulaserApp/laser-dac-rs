mod common;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use laser_dac::benchmark_support::AvbResamplerBenchmark;

const INPUT_POINTS: usize = 1_024;

/// `(pps, sample_rate)`. The last case runs PPS above the device rate, which the
/// AVB backend handles by decimating rather than rejecting.
const AVB_RATES: [(u32, u32); 3] = [(30_000, 48_000), (30_000, 96_000), (60_000, 48_000)];

/// The AVB write path: `LaserPoint` conversion plus a 6-channel (XYRGBI)
/// Catmull-Rom resample, with phase carried across chunks as in the backend.
fn benchmark_avb_resampling(c: &mut Criterion) {
    let input = common::artwork_points(INPUT_POINTS, 0.0);
    let mut group = c.benchmark_group("resampling/avb_stream");
    // Counted in input points, which is what the backend is handed per write.
    // Upsampling cases therefore produce more output per counted element.
    group.throughput(Throughput::Elements(INPUT_POINTS as u64));

    for (pps, sample_rate) in AVB_RATES {
        let output_capacity =
            ((INPUT_POINTS as u64 * sample_rate as u64).div_ceil(pps as u64) + 4) as usize;
        let mut fixture = AvbResamplerBenchmark::new(pps, sample_rate, output_capacity);
        let produced = fixture.process(&input, pps, sample_rate);
        assert!(produced > 0);
        assert!(produced <= output_capacity);

        group.bench_with_input(
            BenchmarkId::new(format!("{pps}_to_{sample_rate}"), INPUT_POINTS),
            &(pps, sample_rate),
            |b, &(pps, sample_rate)| {
                b.iter(|| {
                    black_box(fixture.process(black_box(&input), pps, sample_rate));
                });
            },
        );
    }

    group.finish();
}

/// The oscilloscope backend's 2-channel (XY only) resample. Only built when the
/// `oscilloscope` feature is on, since that backend is a debug tool rather than
/// a shipped output path.
fn benchmark_oscilloscope_resampling(c: &mut Criterion) {
    #[cfg(feature = "oscilloscope")]
    {
        use laser_dac::benchmark_support::StereoResamplerBenchmark;

        let input = common::stereo_samples(INPUT_POINTS);
        let mut group = c.benchmark_group("resampling/oscilloscope_stereo");
        group.throughput(Throughput::Elements(INPUT_POINTS as u64));

        for (from, to) in [(30_000u32, 48_000u32), (30_000, 96_000)] {
            let output_capacity =
                ((INPUT_POINTS as u64 * to as u64).div_ceil(from as u64) + 4) as usize;
            let mut fixture = StereoResamplerBenchmark::new(from, to, output_capacity);
            let initial = fixture.process(&input);
            assert!(!initial.is_empty());
            assert!(initial.len() <= output_capacity);

            group.bench_with_input(
                BenchmarkId::new(format!("{from}_to_{to}"), INPUT_POINTS),
                &(from, to),
                |b, _| {
                    b.iter(|| {
                        black_box(fixture.process(black_box(&input)));
                    });
                },
            );
        }

        group.finish();
    }

    #[cfg(not(feature = "oscilloscope"))]
    let _ = c;
}

criterion_group!(
    benches,
    benchmark_avb_resampling,
    benchmark_oscilloscope_resampling,
);
criterion_main!(benches);
