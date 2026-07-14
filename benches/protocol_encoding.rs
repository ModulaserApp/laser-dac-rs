mod common;

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use laser_dac::benchmark_support::ProtocolEncoderBenchmark;

const PPS: u32 = 30_000;

fn benchmark_protocol_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("protocol/convert_and_encode");

    for points in [500usize, 4_095] {
        let input = common::artwork_points(points, 0.0);
        let mut fixture = ProtocolEncoderBenchmark::new(points);
        let expected = fixture.encode_helios(&input, PPS).len();
        assert_eq!(expected, points * 7 + 5);
        group.throughput(Throughput::Elements(points as u64));
        group.bench_with_input(BenchmarkId::new("helios", points), &points, |b, _| {
            b.iter(|| {
                black_box(fixture.encode_helios(black_box(&input), PPS));
            });
        });
    }

    for points in [500usize, 1_799] {
        let input = common::artwork_points(points, 0.0);
        let mut fixture = ProtocolEncoderBenchmark::new(points);
        let expected = fixture.encode_ether_dream(&input).len();
        assert_eq!(expected, 3 + points * 18);
        group.throughput(Throughput::Elements(points as u64));
        group.bench_with_input(BenchmarkId::new("ether_dream", points), &points, |b, _| {
            b.iter(|| {
                black_box(fixture.encode_ether_dream(black_box(&input)));
            });
        });
    }

    for points in [64usize, 179] {
        let input = common::artwork_points(points, 0.0);
        let mut fixture = ProtocolEncoderBenchmark::new(points);
        let expected = fixture.encode_idn_xyrgbi(&input).len();
        assert_eq!(expected, points * 8);
        group.throughput(Throughput::Elements(points as u64));
        group.bench_with_input(BenchmarkId::new("idn_xyrgbi", points), &points, |b, _| {
            b.iter(|| {
                black_box(fixture.encode_idn_xyrgbi(black_box(&input)));
            });
        });
    }

    for points in [64usize, 140] {
        let input = common::artwork_points(points, 0.0);
        let mut fixture = ProtocolEncoderBenchmark::new(points);
        let expected = fixture.encode_lasercube_network(&input).len();
        assert_eq!(expected, 4 + points * 10);
        group.throughput(Throughput::Elements(points as u64));
        group.bench_with_input(
            BenchmarkId::new("lasercube_network", points),
            &points,
            |b, _| {
                b.iter(|| {
                    black_box(fixture.encode_lasercube_network(black_box(&input)));
                });
            },
        );
    }

    for points in [500usize, 4_096] {
        let input = common::artwork_points(points, 0.0);
        let mut fixture = ProtocolEncoderBenchmark::new(points);
        let expected = fixture.encode_lasercube_usb(&input).len();
        assert_eq!(expected, points * 8);
        group.throughput(Throughput::Elements(points as u64));
        group.bench_with_input(
            BenchmarkId::new("lasercube_usb", points),
            &points,
            |b, _| {
                b.iter(|| {
                    black_box(fixture.encode_lasercube_usb(black_box(&input)));
                });
            },
        );
    }

    group.finish();
}

criterion_group!(benches, benchmark_protocol_encoding);
criterion_main!(benches);
