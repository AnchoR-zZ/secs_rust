#[path = "data_generator.rs"]
mod data_generator;

use criterion::measurement::WallTime;
use criterion::{
    black_box, criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion,
    Throughput,
};
use futures::future::join_all;
use secs_rust::hsms::{
    communicator::HsmsCommunicator,
    config::{ConnectionMode, HsmsConfig},
    message::{HsmsMessage, HsmsMessageCodec},
    ConnectionState,
};
use secs_rust::util::next_system_bytes;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{mpsc, watch};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::{Decoder, Encoder};

const NETWORK_TIMEOUT: Duration = Duration::from_secs(5);
const NETWORK_BATCH_SIZES: [u64; 2] = [32, 128];
const NETWORK_CONCURRENCY_LEVELS: [usize; 2] = [2, 4];

static NEXT_LOOPBACK_PORT: AtomicU16 = AtomicU16::new(16_000);

struct LoopbackFixture {
    active: HsmsCommunicator,
    passive: HsmsCommunicator,
}

fn criterion_config() -> Criterion {
    Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(200))
        .measurement_time(Duration::from_secs(1))
}

fn build_runtime() -> Runtime {
    Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime should be available for HSMS network benchmarks")
}

fn next_loopback_port() -> u16 {
    NEXT_LOOPBACK_PORT.fetch_add(1, Ordering::Relaxed)
}

fn loopback_config(port: u16, mode: ConnectionMode) -> HsmsConfig {
    HsmsConfig {
        session_id: 0,
        ip: "127.0.0.1".to_string(),
        port,
        mode,
        connect_timeout: Duration::from_secs(2),
        t3: Duration::from_secs(2),
        t5: Duration::from_millis(100),
        t6: Duration::from_secs(2),
        t7: Duration::from_secs(2),
        t8: Duration::from_secs(2),
        linktest: Duration::from_secs(30),
    }
}

async fn wait_hsms_state(
    rx: &mut watch::Receiver<ConnectionState>,
    expected: ConnectionState,
    timeout: Duration,
) {
    let result = tokio::time::timeout(timeout, async {
        loop {
            if *rx.borrow() == expected {
                return;
            }
            rx.changed().await.expect("HSMS state channel should stay open");
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out waiting for HSMS state {expected:?}, current: {:?}",
        *rx.borrow()
    );
}

fn network_request(body_size: usize) -> HsmsMessage {
    HsmsMessage::build_request_data_message(
        0,
        6,
        11,
        next_system_bytes(),
        data_generator::binary(body_size),
    )
}

fn loopback_wire_bytes(body_size: usize) -> u64 {
    let request = network_request(body_size);
    (encode_message_bytes(&request).len() * 2) as u64
}

fn spawn_reply_task(passive_comm: HsmsCommunicator, mut passive_msg_rx: mpsc::Receiver<HsmsMessage>) {
    tokio::spawn(async move {
        while let Some(msg) = passive_msg_rx.recv().await {
            if msg.header.w_bit {
                let reply = msg.build_reply_message(
                    msg.header.stream,
                    msg.header.function + 1,
                    msg.body.clone(),
                );
                let _ = passive_comm.send_reply(reply).await;
            }
        }
    });
}

fn spawn_drain_task(mut inbound_rx: mpsc::Receiver<HsmsMessage>) {
    tokio::spawn(async move {
        while inbound_rx.recv().await.is_some() {}
    });
}

async fn setup_loopback_pair(port: u16) -> LoopbackFixture {
    let passive_config = loopback_config(port, ConnectionMode::Passive);
    let (passive, passive_msg_rx) = HsmsCommunicator::new(passive_config);
    let mut passive_state_rx = passive.state_rx();
    spawn_reply_task(passive.clone(), passive_msg_rx);

    tokio::time::sleep(Duration::from_millis(50)).await;

    let active_config = loopback_config(port, ConnectionMode::Active);
    let (active, active_msg_rx) = HsmsCommunicator::new(active_config);
    let mut active_state_rx = active.state_rx();
    spawn_drain_task(active_msg_rx);

    wait_hsms_state(&mut passive_state_rx, ConnectionState::Selected, NETWORK_TIMEOUT).await;
    wait_hsms_state(&mut active_state_rx, ConnectionState::Selected, NETWORK_TIMEOUT).await;

    LoopbackFixture { active, passive }
}

async fn shutdown_loopback_pair(fixture: LoopbackFixture) {
    let _ = tokio::time::timeout(Duration::from_secs(2), fixture.active.shutdown()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), fixture.passive.shutdown()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
}

fn encode_message_bytes(message: &HsmsMessage) -> Vec<u8> {
    let mut codec = HsmsMessageCodec;
    let mut buffer = BytesMut::new();
    codec
        .encode(message.clone(), &mut buffer)
        .expect("HSMS message should encode");
    buffer.to_vec()
}

fn bench_hsms_case(group: &mut BenchmarkGroup<'_, WallTime>, label: &str, message: &HsmsMessage) {
    let encoded = encode_message_bytes(message);

    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function(BenchmarkId::new("encode_bytes", label), |b| {
        b.iter(|| {
            let mut codec = HsmsMessageCodec;
            let mut buffer = BytesMut::with_capacity(encoded.len());
            codec
                .encode(black_box(message.clone()), &mut buffer)
                .expect("encode should succeed");
            black_box(buffer)
        })
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("encode_messages", label), |b| {
        b.iter(|| {
            let mut codec = HsmsMessageCodec;
            let mut buffer = BytesMut::with_capacity(encoded.len());
            codec
                .encode(black_box(message.clone()), &mut buffer)
                .expect("encode should succeed");
            black_box(buffer)
        })
    });

    group.throughput(Throughput::Bytes(encoded.len() as u64));
    group.bench_function(BenchmarkId::new("decode_bytes", label), |b| {
        b.iter(|| {
            let mut codec = HsmsMessageCodec;
            let mut source = BytesMut::from(black_box(encoded.as_slice()));
            let decoded = codec
                .decode(&mut source)
                .expect("decode should succeed")
                .expect("complete benchmark frame should decode");
            black_box(decoded)
        })
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function(BenchmarkId::new("decode_messages", label), |b| {
        b.iter(|| {
            let mut codec = HsmsMessageCodec;
            let mut source = BytesMut::from(black_box(encoded.as_slice()));
            let decoded = codec
                .decode(&mut source)
                .expect("decode should succeed")
                .expect("complete benchmark frame should decode");
            black_box(decoded)
        })
    });
}

fn bench_data_messages(c: &mut Criterion) {
    // Pure codec benchmarks isolate framing cost from network and runtime scheduling noise.
    let mut group = c.benchmark_group("hsms_codec/data_messages");

    for size in data_generator::HSMS_BODY_SIZES {
        let message = data_generator::hsms_data_message(size);
        bench_hsms_case(&mut group, &format!("data_body_{size}b"), &message);
    }

    group.finish();
}

fn bench_control_messages(c: &mut Criterion) {
    // Control frames benchmark zero-body encode/decode paths separately from SECS-II data payloads.
    let mut group = c.benchmark_group("hsms_codec/control_messages");

    for (name, message) in data_generator::hsms_control_messages() {
        bench_hsms_case(&mut group, name, &message);
    }

    group.finish();
}

fn bench_loopback_connection_latency(c: &mut Criterion) {
    // Connection latency includes TCP connect, HSMS select handshake, and readiness to exchange data.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("hsms_loopback/connection_latency");

    group.measurement_time(Duration::from_secs(2));
    group.throughput(Throughput::Elements(1));
    group.bench_function("connect_and_select", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            runtime.block_on(async {
                for _ in 0..iters {
                    let fixture = setup_loopback_pair(next_loopback_port()).await;
                    shutdown_loopback_pair(fixture).await;
                }
            });
            start.elapsed()
        })
    });

    group.finish();
}

fn bench_loopback_rtt(c: &mut Criterion) {
    // Round-trip latency isolates local socket and manager scheduling effects from pure codec costs.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("hsms_loopback/rtt");

    group.measurement_time(Duration::from_secs(2));

    for size in data_generator::HSMS_BODY_SIZES {
        let fixture = runtime.block_on(setup_loopback_pair(next_loopback_port()));
        let active = fixture.active.clone();
        let bytes_per_round_trip = loopback_wire_bytes(size);
        let label = format!("data_body_{size}b");

        group.throughput(Throughput::Bytes(bytes_per_round_trip));
        group.bench_function(BenchmarkId::new("round_trip_bytes", &label), |b| {
            b.iter_custom(|iters| {
                let start = Instant::now();
                runtime.block_on(async {
                    for _ in 0..iters {
                        let reply = active
                            .send_message_with_reply(network_request(size))
                            .await
                            .expect("loopback RTT request should succeed");
                        black_box(reply);
                    }
                });
                start.elapsed()
            })
        });

        group.throughput(Throughput::Elements(1));
        group.bench_function(BenchmarkId::new("round_trip_messages", &label), |b| {
            b.iter_custom(|iters| {
                let start = Instant::now();
                runtime.block_on(async {
                    for _ in 0..iters {
                        let reply = active
                            .send_message_with_reply(network_request(size))
                            .await
                            .expect("loopback RTT request should succeed");
                        black_box(reply);
                    }
                });
                start.elapsed()
            })
        });

        runtime.block_on(shutdown_loopback_pair(fixture));
    }

    group.finish();
}

fn bench_loopback_batch_throughput(c: &mut Criterion) {
    // Batch tests measure sustained request/reply throughput over a single established loopback connection.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("hsms_loopback/batch_throughput");

    group.measurement_time(Duration::from_secs(2));

    for size in data_generator::HSMS_BODY_SIZES {
        for batch_size in NETWORK_BATCH_SIZES {
            let fixture = runtime.block_on(setup_loopback_pair(next_loopback_port()));
            let active = fixture.active.clone();
            let bytes_per_batch = loopback_wire_bytes(size) * batch_size;
            let label = format!("data_body_{size}b_batch_{batch_size}");

            group.throughput(Throughput::Bytes(bytes_per_batch));
            group.bench_function(BenchmarkId::new("batch_round_trip_bytes", &label), |b| {
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    runtime.block_on(async {
                        for _ in 0..iters {
                            for _ in 0..batch_size {
                                let reply = active
                                    .send_message_with_reply(network_request(size))
                                    .await
                                    .expect("loopback batch request should succeed");
                                black_box(reply);
                            }
                        }
                    });
                    start.elapsed()
                })
            });

            group.throughput(Throughput::Elements(batch_size));
            group.bench_function(BenchmarkId::new("batch_round_trip_messages", &label), |b| {
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    runtime.block_on(async {
                        for _ in 0..iters {
                            for _ in 0..batch_size {
                                let reply = active
                                    .send_message_with_reply(network_request(size))
                                    .await
                                    .expect("loopback batch request should succeed");
                                black_box(reply);
                            }
                        }
                    });
                    start.elapsed()
                })
            });

            runtime.block_on(shutdown_loopback_pair(fixture));
        }
    }

    group.finish();
}

fn bench_loopback_concurrent_throughput(c: &mut Criterion) {
    // Concurrent pairs provide a second-stage view of scheduler and socket contention across loopback sessions.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("hsms_loopback/concurrent_throughput");
    let body_size = 100;
    let bytes_per_round_trip = loopback_wire_bytes(body_size);

    group.measurement_time(Duration::from_secs(2));

    for concurrency in NETWORK_CONCURRENCY_LEVELS {
        let fixtures: Vec<_> = runtime.block_on(async {
            let mut fixtures = Vec::with_capacity(concurrency);
            for _ in 0..concurrency {
                fixtures.push(setup_loopback_pair(next_loopback_port()).await);
            }
            fixtures
        });
        let actives: Vec<_> = fixtures.iter().map(|fixture| fixture.active.clone()).collect();
        let label = format!("pairs_{concurrency}");

        group.throughput(Throughput::Bytes(bytes_per_round_trip * concurrency as u64));
        group.bench_function(BenchmarkId::new("concurrent_round_trip_bytes", &label), |b| {
            b.iter_custom(|iters| {
                let start = Instant::now();
                runtime.block_on(async {
                    for _ in 0..iters {
                        let replies = join_all(
                            actives
                                .iter()
                                .map(|active| active.send_message_with_reply(network_request(body_size))),
                        )
                        .await;

                        for reply in replies {
                            black_box(reply.expect("concurrent loopback request should succeed"));
                        }
                    }
                });
                start.elapsed()
            })
        });

        group.throughput(Throughput::Elements(concurrency as u64));
        group.bench_function(BenchmarkId::new("concurrent_round_trip_messages", &label), |b| {
            b.iter_custom(|iters| {
                let start = Instant::now();
                runtime.block_on(async {
                    for _ in 0..iters {
                        let replies = join_all(
                            actives
                                .iter()
                                .map(|active| active.send_message_with_reply(network_request(body_size))),
                        )
                        .await;

                        for reply in replies {
                            black_box(reply.expect("concurrent loopback request should succeed"));
                        }
                    }
                });
                start.elapsed()
            })
        });

        runtime.block_on(async {
            for fixture in fixtures {
                shutdown_loopback_pair(fixture).await;
            }
        });
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = bench_data_messages,
        bench_control_messages,
        bench_loopback_connection_latency,
        bench_loopback_rtt,
        bench_loopback_batch_throughput,
        bench_loopback_concurrent_throughput
}
criterion_main!(benches);