//! L1 微基准 —— SECS-II 编解码纯内存性能测试
//!
//! 测试策略：
//!   - 所有负载均在内存中构造，不涉及任何 I/O 或异步调度
//!   - 使用 `criterion::black_box` 阻止编译器消除死代码
//!   - 三个维度：encode-only, decode-only, roundtrip
//!   - throughput 组：以 bytes/s 为单位衡量大 payload 的编码带宽
//!
//! 运行方法：
//!   cargo bench --bench secs2_bench
//!
//! 生成 HTML 报告（自动存放在 target/criterion/）：
//!   cargo bench --bench secs2_bench -- --output-format=bencher
//! 
//! # 全量跑（每个 group 默认 5s 预热 + 5s 采样，HTML 报告在 target/criterion/）
//！ cargo bench --bench secs2_bench

//！ # 只看某一 group（快速验证）
//！ cargo bench --bench secs2_bench -- secs2/roundtrip

//！ # 缩短时间快速冒烟
//！ cargo bench --bench secs2_bench -- --warm-up-time 1 --measurement-time 2

#[path = "payload_gen.rs"]
mod payload_gen;

use criterion::{
    BenchmarkId, Criterion, Throughput, criterion_group, criterion_main,
};
use secs_rust::secs2::{Secs2, encode};
use std::hint::black_box;

// ═══════════════════════════════════════════════════════════════════════════
// 辅助：将 pprof profiler 特性包含进来（可选，需要 pprof feature flag）
// ═══════════════════════════════════════════════════════════════════════════

// ═══════════════════════════════════════════════════════════════════════════
// Group 1: encode
//
// 对所有 mixed_payloads 做 encode，测量每次调用的纳秒开销
// ═══════════════════════════════════════════════════════════════════════════
fn bench_encode(c: &mut Criterion) {
    let mut group = c.benchmark_group("secs2/encode");

    for (label, payload) in payload_gen::mixed_payloads() {
        group.bench_with_input(
            BenchmarkId::new("encode", label),
            &payload,
            |b, data| {
                b.iter(|| {
                    let _ = black_box(encode(black_box(data)).unwrap());
                });
            },
        );
    }

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// Group 2: decode
//
// 预先将所有 payload 编码为字节，benchmark 阶段只测 decode 开销，
// 与 encode 基准分离，便于单独归因
// ═══════════════════════════════════════════════════════════════════════════
fn bench_decode(c: &mut Criterion) {
    let mut group = c.benchmark_group("secs2/decode");

    // 提前序列化，测量时不含 encode 开销
    let encoded_payloads: Vec<(&'static str, Vec<u8>)> = payload_gen::mixed_payloads()
        .into_iter()
        .map(|(label, data)| {
            let bytes = encode(&data).expect("pre-encode failed");
            (label, bytes)
        })
        .collect();

    for (label, bytes) in &encoded_payloads {
        group.bench_with_input(
            BenchmarkId::new("decode", *label),
            bytes,
            |b, raw| {
                b.iter(|| {
                    let _ = black_box(Secs2::decode(black_box(raw)).unwrap());
                });
            },
        );
    }

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// Group 3: roundtrip（encode + decode 串联）
//
// 模拟最常见的"收一条消息，复制再发出"场景，测量完整往返开销
// ═══════════════════════════════════════════════════════════════════════════
fn bench_roundtrip(c: &mut Criterion) {
    let mut group = c.benchmark_group("secs2/roundtrip");

    for (label, payload) in payload_gen::mixed_payloads() {
        group.bench_with_input(
            BenchmarkId::new("roundtrip", label),
            &payload,
            |b, data| {
                b.iter(|| {
                    let bytes = encode(black_box(data)).unwrap();
                    let _ = black_box(Secs2::decode(black_box(&bytes)).unwrap());
                });
            },
        );
    }

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// Group 4: throughput（bytes/s）
//
// 针对大 payload 报告编码带宽，方便与其他协议实现横向比较
// ═══════════════════════════════════════════════════════════════════════════
fn bench_encode_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("secs2/encode_throughput");

    // 只取大报文场景衡量带宽
    let large_cases = [
        ("binary_8k", payload_gen::large_binary()),
        ("u1_4k", payload_gen::large_u1_array()),
        ("f4_4k", payload_gen::large_f4_array()),
        ("d8_4k", payload_gen::large_d8_array()),
        ("wide_list_256", payload_gen::wide_list()),
    ];

    for (label, payload) in &large_cases {
        // 先编码一次，以实际编码后字节数作为 throughput 基准
        let encoded_len = encode(payload).unwrap().len() as u64;
        group.throughput(Throughput::Bytes(encoded_len));
        group.bench_with_input(
            BenchmarkId::new("throughput", *label),
            payload,
            |b, data| {
                b.iter(|| {
                    let _ = black_box(encode(black_box(data)).unwrap());
                });
            },
        );
    }

    group.finish();
}

fn bench_decode_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("secs2/decode_throughput");

    let large_cases = [
        ("binary_8k", payload_gen::large_binary()),
        ("u1_4k", payload_gen::large_u1_array()),
        ("f4_4k", payload_gen::large_f4_array()),
        ("d8_4k", payload_gen::large_d8_array()),
        ("wide_list_256", payload_gen::wide_list()),
    ];

    let encoded: Vec<(&str, Vec<u8>)> = large_cases
        .iter()
        .map(|(label, data)| (*label, encode(data).unwrap()))
        .collect();

    for (label, bytes) in &encoded {
        group.throughput(Throughput::Bytes(bytes.len() as u64));
        group.bench_with_input(
            BenchmarkId::new("throughput", *label),
            bytes,
            |b, raw| {
                b.iter(|| {
                    let _ = black_box(Secs2::decode(black_box(raw)).unwrap());
                });
            },
        );
    }

    group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// Group 5: 嵌套深度扫描
//
// 系统性测试递归深度从 1 到 32 时 encode/decode 的变化曲线，
// 用于发现潜在的栈溢出风险或复杂度异常
// ═══════════════════════════════════════════════════════════════════════════
fn bench_nested_depth_scan(c: &mut Criterion) {
    let depths = [1_usize, 2, 4, 8, 16, 32];

    let mut enc_group = c.benchmark_group("secs2/nested_encode_depth");
    for &d in &depths {
        let payload = payload_gen::nested_list(d);
        enc_group.bench_with_input(BenchmarkId::from_parameter(d), &payload, |b, data| {
            b.iter(|| {
                let _ = black_box(encode(black_box(data)).unwrap());
            });
        });
    }
    enc_group.finish();

    let mut dec_group = c.benchmark_group("secs2/nested_decode_depth");
    for &d in &depths {
        let bytes = encode(&payload_gen::nested_list(d)).unwrap();
        dec_group.bench_with_input(BenchmarkId::from_parameter(d), &bytes, |b, raw| {
            b.iter(|| {
                let _ = black_box(Secs2::decode(black_box(raw)).unwrap());
            });
        });
    }
    dec_group.finish();
}

// ═══════════════════════════════════════════════════════════════════════════
// 注册所有 benchmark group
// ═══════════════════════════════════════════════════════════════════════════
criterion_group!(
    secs2_benches,
    bench_encode,
    bench_decode,
    bench_roundtrip,
    bench_encode_throughput,
    bench_decode_throughput,
    bench_nested_depth_scan,
);
criterion_main!(secs2_benches);
