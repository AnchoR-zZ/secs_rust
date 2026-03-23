#[path = "data_generator.rs"]
mod data_generator;

use criterion::measurement::WallTime;
use criterion::{
    black_box, criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion,
    Throughput,
};
use secs_rust::secs2::{self, Secs2};
use std::time::Duration;

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(200))
        .measurement_time(Duration::from_secs(1))
}

fn bench_secs2_case(group: &mut BenchmarkGroup<'_, WallTime>, label: &str, data: &Secs2) {
    let encoded = secs2::encode(data).expect("SECS-II test case should encode");

    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function(BenchmarkId::new("encode_bytes", label), |b| {
        b.iter(|| black_box(secs2::encode(black_box(data)).expect("encode should succeed")))
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("encode_messages", label), |b| {
        b.iter(|| black_box(secs2::encode(black_box(data)).expect("encode should succeed")))
    });

    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function(BenchmarkId::new("decode_bytes", label), |b| {
        b.iter(|| {
            let decoded = Secs2::decode(black_box(encoded.as_slice()))
                .expect("decode should succeed")
                .expect("benchmark payload should not decode to None");
            black_box(decoded)
        })
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("decode_messages", label), |b| {
        b.iter(|| {
            let decoded = Secs2::decode(black_box(encoded.as_slice()))
                .expect("decode should succeed")
                .expect("benchmark payload should not decode to None");
            black_box(decoded)
        })
    });
}

fn bench_scalar_types(c: &mut Criterion) {
    // Scalar payloads highlight cache boundaries and per-element encode/decode overhead.
    for (name, factory) in data_generator::secs2_scalar_factories() {
        let mut group = c.benchmark_group(format!("secs2_scalar/{name}"));

        for size in data_generator::SCALAR_SIZES {
            let payload = factory(size);
            bench_secs2_case(&mut group, &size.to_string(), &payload);
        }

        group.finish();
    }
}

fn bench_nested_lists(c: &mut Criterion) {
    // Nested LIST cases stress recursive traversal and header construction depth.
    let mut group = c.benchmark_group("secs2_nested_list");

    for depth in data_generator::LIST_DEPTHS {
        let payload = data_generator::nested_list(depth);
        bench_secs2_case(&mut group, &format!("depth_{depth}"), &payload);
    }

    group.finish();
}

fn bench_real_world_structures(c: &mut Criterion) {
    // Mixed structures simulate realistic SECS/GEM traffic rather than synthetic flat arrays.
    let mut group = c.benchmark_group("secs2_real_world");

    for (name, payload) in data_generator::secs2_real_world_cases() {
        bench_secs2_case(&mut group, name, &payload);
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = bench_scalar_types, bench_nested_lists, bench_real_world_structures
}
criterion_main!(benches);