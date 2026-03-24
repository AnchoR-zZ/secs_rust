#[path = "data_generator.rs"]
mod data_generator;

use criterion::measurement::WallTime;
use criterion::{
    black_box, criterion_group, criterion_main, BenchmarkGroup, BenchmarkId, Criterion,
    Throughput,
};
use futures::future::join_all;
use secs_rust::gem::communicator::GemCommunicator;
use secs_rust::gem::config::{GemConfig, GemRole};
use secs_rust::gem::gem_state::{
    DeviceState, GemControl, StateEvent, StateMachineConfig,
};
use secs_rust::gem::message as gem_message;
use secs_rust::hsms::config::{ConnectionMode, HsmsConfig};
use secs_rust::hsms::message::{HsmsMessage, HsmsMessageCodec};
use secs_rust::util::next_system_bytes;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{mpsc, watch};
use tokio_util::bytes::BytesMut;
use tokio_util::codec::Encoder;

const GEM_NETWORK_TIMEOUT: Duration = Duration::from_secs(5);
const GEM_LOOPBACK_BATCH_SIZES: [u64; 2] = [16, 64];

static NEXT_GEM_LOOPBACK_PORT: AtomicU16 = AtomicU16::new(17_000);

// Note: GemLoopbackFixture does not implement Drop because shutdown() is async.
// Benchmark processes are short-lived; the OS reclaims ports on exit.
struct GemLoopbackFixture {
    equipment: GemCommunicator,
    host: GemCommunicator,
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
        .expect("tokio runtime should be available for GEM loopback benchmarks")
}

fn next_gem_loopback_port() -> u16 {
    NEXT_GEM_LOOPBACK_PORT.fetch_add(1, Ordering::Relaxed)
}

fn hsms_loopback_config(port: u16, mode: ConnectionMode) -> HsmsConfig {
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

fn gem_loopback_config(port: u16, role: GemRole, mode: ConnectionMode) -> GemConfig {
    GemConfig::new(
        role,
        hsms_loopback_config(port, mode),
        None,
        Some("SECS-SIMULATOR".to_string()),
        Some("1.0.0".to_string()),
    )
}

fn encode_wire_len(message: &HsmsMessage) -> usize {
    let mut codec = HsmsMessageCodec;
    let mut buffer = BytesMut::new();
    codec
        .encode(message.clone(), &mut buffer)
        .expect("GEM loopback message should encode to HSMS wire bytes");
    buffer.len()
}

fn gem_passthrough_request(body_size: usize) -> HsmsMessage {
    HsmsMessage::build_request_data_message(
        0,
        2,
        17,
        next_system_bytes(),
        data_generator::binary(body_size),
    )
}

fn gem_message_wire_bytes(body_size: usize) -> u64 {
    let request = gem_passthrough_request(body_size);
    let reply = request.build_reply_message(2, 18, data_generator::binary(body_size));
    (encode_wire_len(&request) + encode_wire_len(&reply)) as u64
}

fn gem_control_wire_bytes(build_request: fn(u16) -> HsmsMessage, build_reply: fn(&HsmsMessage, u8) -> HsmsMessage) -> u64 {
    let request = build_request(0);
    let reply = build_reply(&request, 0);
    (encode_wire_len(&request) + encode_wire_len(&reply)) as u64
}

async fn wait_gem_state(
    rx: &mut watch::Receiver<DeviceState>,
    predicate: impl Fn(&DeviceState) -> bool,
    timeout: Duration,
) {
    let result = tokio::time::timeout(timeout, async {
        loop {
            if predicate(&rx.borrow()) {
                return;
            }
            rx.changed().await.expect("GEM state channel should stay open");
        }
    })
    .await;

    assert!(
        result.is_ok(),
        "timed out waiting for GEM state, current: {:?}",
        *rx.borrow()
    );
}

fn spawn_equipment_passthrough_reply_task(
    equipment: GemCommunicator,
    mut inbound_rx: mpsc::Receiver<HsmsMessage>,
) {
    tokio::spawn(async move {
        while let Some(msg) = inbound_rx.recv().await {
            if msg.header.w_bit {
                let reply = msg.build_reply_message(
                    msg.header.stream,
                    msg.header.function + 1,
                    msg.body.clone(),
                );
                let _ = equipment.send_reply(reply).await;
            }
        }
    });
}

fn spawn_drain_task(mut inbound_rx: mpsc::Receiver<HsmsMessage>) {
    tokio::spawn(async move {
        while inbound_rx.recv().await.is_some() {}
    });
}

async fn setup_gem_loopback_pair(port: u16) -> GemLoopbackFixture {
    let equipment_config = gem_loopback_config(port, GemRole::Equipment, ConnectionMode::Passive);
    let (equipment, equipment_msg_rx) = GemCommunicator::new(equipment_config);
    let mut equipment_state_rx = equipment.state_rx();
    spawn_equipment_passthrough_reply_task(equipment.clone(), equipment_msg_rx);

    tokio::time::sleep(Duration::from_millis(50)).await;

    let host_config = gem_loopback_config(port, GemRole::Host, ConnectionMode::Active);
    let (host, host_msg_rx) = GemCommunicator::new(host_config);
    let mut host_state_rx = host.state_rx();
    spawn_drain_task(host_msg_rx);

    wait_gem_state(&mut equipment_state_rx, |state| state.is_selected(), GEM_NETWORK_TIMEOUT).await;
    wait_gem_state(&mut host_state_rx, |state| state.is_selected(), GEM_NETWORK_TIMEOUT).await;

    GemLoopbackFixture { equipment, host }
}

async fn promote_equipment_online(fixture: &GemLoopbackFixture) {
    fixture
        .equipment
        .operator_online()
        .await
        .expect("equipment operator_online should succeed for GEM network benchmarks");

    let mut equipment_state_rx = fixture.equipment.state_rx();
    wait_gem_state(&mut equipment_state_rx, |state| state.is_online(), GEM_NETWORK_TIMEOUT).await;
}

async fn shutdown_gem_loopback_pair(fixture: GemLoopbackFixture) {
    let _ = tokio::time::timeout(Duration::from_secs(2), fixture.equipment.shutdown()).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), fixture.host.shutdown()).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
}

fn bench_builder<F>(group: &mut BenchmarkGroup<'_, WallTime>, label: &str, mut build: F)
where
    F: FnMut() -> HsmsMessage,
{
    group.throughput(Throughput::Elements(1));
    group.bench_function(label, |b| b.iter(|| black_box(build())));
}

fn bench_state_sequence(
    group: &mut BenchmarkGroup<'_, WallTime>,
    label: &str,
    initial_state: DeviceState,
    config: StateMachineConfig,
    events: Vec<StateEvent>,
) {
    group.throughput(Throughput::Elements(events.len() as u64));
    group.bench_function(BenchmarkId::new("device_state_on_event", label), |b| {
        b.iter(|| {
            let mut state = initial_state.clone();
            for event in &events {
                state = state.on_event(black_box(event.clone()), black_box(&config));
            }
            black_box(state)
        })
    });
}

fn bench_control_sequence(
    group: &mut BenchmarkGroup<'_, WallTime>,
    label: &str,
    config: StateMachineConfig,
    events: Vec<StateEvent>,
) {
    group.throughput(Throughput::Elements(events.len() as u64));
    group.bench_function(BenchmarkId::new("gem_control_handle_event", label), |b| {
        b.iter(|| {
            let mut control = GemControl::new(config.clone());
            for event in &events {
                control.handle_event(black_box(event.clone()));
            }
            black_box(control.state.clone())
        })
    });
}

fn bench_message_builders(c: &mut Criterion) {
    // Builder benchmarks isolate message construction cost from later codec and transport overhead.
    let mut group = c.benchmark_group("gem/message_builders");

    bench_builder(&mut group, "build_s1f1", || gem_message::build_s1f1(0x1001));
    bench_builder(&mut group, "build_s1f15", || gem_message::build_s1f15(0x1001));
    bench_builder(&mut group, "build_s1f17", || gem_message::build_s1f17(0x1001));

    for (label, mdln, softrev) in data_generator::gem_identity_variants() {
        let s1f1_req = gem_message::build_s1f1(0x1001);
        let s1f13_req = gem_message::build_s1f13(0x1001, mdln, softrev);

        bench_builder(&mut group, &format!("build_s1f2_reply/{label}"), || {
            gem_message::build_s1f2_reply(&s1f1_req, mdln, softrev)
        });
        bench_builder(&mut group, &format!("build_s1f13/{label}"), || {
            gem_message::build_s1f13(0x1001, mdln, softrev)
        });
        bench_builder(&mut group, &format!("build_s1f14_reply/{label}"), || {
            gem_message::build_s1f14_reply(&s1f13_req, 0, mdln, softrev)
        });
    }

    let s1f15_req = gem_message::build_s1f15(0x1001);
    let s1f17_req = gem_message::build_s1f17(0x1001);

    bench_builder(&mut group, "build_s1f16_reply/accepted", || {
        gem_message::build_s1f16_reply(&s1f15_req, 0)
    });
    bench_builder(&mut group, "build_s1f18_reply/accepted", || {
        gem_message::build_s1f18_reply(&s1f17_req, 0)
    });

    group.finish();
}

fn bench_gem_control_new(c: &mut Criterion) {
    // Construction benchmark measures the fixed setup cost of the lightweight control wrapper.
    let mut group = c.benchmark_group("gem/control_new");

    for (label, config) in data_generator::gem_representative_configs() {
        group.throughput(Throughput::Elements(1));
        group.bench_function(label, |b| {
            b.iter(|| black_box(GemControl::new(black_box(config.clone()))))
        });
    }

    group.finish();
}

fn bench_state_machine(c: &mut Criterion) {
    // State-machine sequences cover nominal flows, recovery flows, ignored events, and high-frequency churn.
    let mut group = c.benchmark_group("gem/state_machine");

    for (label, state, config, events) in data_generator::gem_transition_scenarios() {
        bench_state_sequence(&mut group, label, state.clone(), config.clone(), events.clone());
        bench_control_sequence(&mut group, label, config, events);
    }

    for (label, state, config, event) in data_generator::gem_invalid_transition_cases() {
        bench_state_sequence(&mut group, label, state.clone(), config.clone(), vec![event.clone()]);
        bench_control_sequence(&mut group, label, config, vec![event]);
    }

    let high_frequency_events = data_generator::gem_high_frequency_events();
    bench_state_sequence(
        &mut group,
        "high_frequency_1000_transitions",
        DeviceState::NotConnected,
        StateMachineConfig::default(),
        high_frequency_events.clone(),
    );
    bench_control_sequence(
        &mut group,
        "high_frequency_1000_transitions",
        StateMachineConfig::default(),
        high_frequency_events,
    );

    group.finish();
}

fn bench_gem_loopback_connection(c: &mut Criterion) {
    // Connection benchmark covers TCP connect, HSMS select, and GEM selected/offline readiness.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("gem_loopback/connection_latency");

    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));
    group.bench_function("connect_and_select", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            runtime.block_on(async {
                for _ in 0..iters {
                    let fixture = setup_gem_loopback_pair(next_gem_loopback_port()).await;
                    shutdown_gem_loopback_pair(fixture).await;
                }
            });
            start.elapsed()
        })
    });

    group.finish();
}

fn bench_gem_loopback_online_transition(c: &mut Criterion) {
    // Operator-online latency includes S1F1/S1F2 handshake and transition into OnLine/Local.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("gem_loopback/operator_online_latency");

    group.measurement_time(Duration::from_secs(3));
    group.throughput(Throughput::Elements(1));
    group.bench_function("equipment_operator_online", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            runtime.block_on(async {
                for _ in 0..iters {
                    let fixture = setup_gem_loopback_pair(next_gem_loopback_port()).await;
                    fixture
                        .equipment
                        .operator_online()
                        .await
                        .expect("operator_online should succeed");

                    let mut equipment_state_rx = fixture.equipment.state_rx();
                    wait_gem_state(&mut equipment_state_rx, |state| state.is_online(), GEM_NETWORK_TIMEOUT).await;

                    shutdown_gem_loopback_pair(fixture).await;
                }
            });
            start.elapsed()
        })
    });

    group.finish();
}

fn bench_gem_loopback_control_rtt(c: &mut Criterion) {
    // GEM control message RTT measures host-driven offline/online commands over a live GEM session.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("gem_loopback/control_rtt");

    group.measurement_time(Duration::from_secs(3));

    let offline_bytes = gem_control_wire_bytes(gem_message::build_s1f15, gem_message::build_s1f16_reply);
    let online_bytes = gem_control_wire_bytes(gem_message::build_s1f17, gem_message::build_s1f18_reply);

    // Each offline iteration sends S1F15 (go offline) then S1F17 (recover online) — a full cycle.
    // Each online iteration calls operator_online, then S1F15 (go offline), then S1F17 (go online).
    // Throughput reflects the total wire bytes of the full cycle per iteration.
    let offline_cycle_bytes = offline_bytes + online_bytes;
    let online_cycle_bytes = offline_bytes + online_bytes;

    let offline_fixture = runtime.block_on(async {
        let fixture = setup_gem_loopback_pair(next_gem_loopback_port()).await;
        promote_equipment_online(&fixture).await;
        fixture
    });

    group.throughput(Throughput::Bytes(offline_cycle_bytes));
    group.bench_function("host_offline_recover_cycle_bytes", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            runtime.block_on(async {
                for _ in 0..iters {
                    let reply = offline_fixture
                        .host
                        .send_message_with_reply(gem_message::build_s1f15(0))
                        .await
                        .expect("S1F15/S1F16 round trip should succeed");
                    black_box(reply);

                    offline_fixture
                        .host
                        .send_message_with_reply(gem_message::build_s1f17(0))
                        .await
                        .expect("S1F17/S1F18 recovery should succeed");
                }
            });
            start.elapsed()
        })
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function("host_offline_recover_cycle_messages", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            runtime.block_on(async {
                for _ in 0..iters {
                    let reply = offline_fixture
                        .host
                        .send_message_with_reply(gem_message::build_s1f15(0))
                        .await
                        .expect("S1F15/S1F16 round trip should succeed");
                    black_box(reply);

                    offline_fixture
                        .host
                        .send_message_with_reply(gem_message::build_s1f17(0))
                        .await
                        .expect("S1F17/S1F18 recovery should succeed");
                }
            });
            start.elapsed()
        })
    });

    runtime.block_on(shutdown_gem_loopback_pair(offline_fixture));

    let online_fixture = runtime.block_on(async {
        let fixture = setup_gem_loopback_pair(next_gem_loopback_port()).await;
        fixture
    });

    group.throughput(Throughput::Bytes(online_cycle_bytes));
    group.bench_function("host_online_recover_cycle_bytes", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            runtime.block_on(async {
                for _ in 0..iters {
                    online_fixture
                        .equipment
                        .operator_online()
                        .await
                        .expect("operator_online should succeed before S1F17 benchmark");

                    let reply = online_fixture
                        .host
                        .send_message_with_reply(gem_message::build_s1f15(0))
                        .await
                        .expect("S1F15/S1F16 pre-step should succeed");
                    black_box(reply);

                    let reply = online_fixture
                        .host
                        .send_message_with_reply(gem_message::build_s1f17(0))
                        .await
                        .expect("S1F17/S1F18 round trip should succeed");
                    black_box(reply);
                }
            });
            start.elapsed()
        })
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function("host_online_recover_cycle_messages", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            runtime.block_on(async {
                for _ in 0..iters {
                    online_fixture
                        .equipment
                        .operator_online()
                        .await
                        .expect("operator_online should succeed before S1F17 benchmark");

                    let reply = online_fixture
                        .host
                        .send_message_with_reply(gem_message::build_s1f15(0))
                        .await
                        .expect("S1F15/S1F16 pre-step should succeed");
                    black_box(reply);

                    let reply = online_fixture
                        .host
                        .send_message_with_reply(gem_message::build_s1f17(0))
                        .await
                        .expect("S1F17/S1F18 round trip should succeed");
                    black_box(reply);
                }
            });
            start.elapsed()
        })
    });

    runtime.block_on(shutdown_gem_loopback_pair(online_fixture));

    group.finish();
}

fn bench_gem_loopback_passthrough(c: &mut Criterion) {
    // Passthrough RTT benchmarks non-GEM traffic over an already-online GEM session.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("gem_loopback/passthrough_rtt");

    group.measurement_time(Duration::from_secs(3));

    for size in data_generator::HSMS_BODY_SIZES {
        let fixture = runtime.block_on(async {
            let fixture = setup_gem_loopback_pair(next_gem_loopback_port()).await;
            promote_equipment_online(&fixture).await;
            fixture
        });
        let bytes_per_round_trip = gem_message_wire_bytes(size);
        let label = format!("data_body_{size}b");

        group.throughput(Throughput::Bytes(bytes_per_round_trip));
        group.bench_function(BenchmarkId::new("round_trip_bytes", &label), |b| {
            b.iter_custom(|iters| {
                let start = Instant::now();
                runtime.block_on(async {
                    for _ in 0..iters {
                        let reply = fixture
                            .host
                            .send_message_with_reply(gem_passthrough_request(size))
                            .await
                            .expect("GEM passthrough round trip should succeed");
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
                        let reply = fixture
                            .host
                            .send_message_with_reply(gem_passthrough_request(size))
                            .await
                            .expect("GEM passthrough round trip should succeed");
                        black_box(reply);
                    }
                });
                start.elapsed()
            })
        });

        runtime.block_on(shutdown_gem_loopback_pair(fixture));
    }

    group.finish();
}

fn bench_gem_loopback_batch_throughput(c: &mut Criterion) {
    // Batch throughput measures concurrent non-GEM traffic while GEM remains online and stable.
    let runtime = build_runtime();
    let mut group = c.benchmark_group("gem_loopback/batch_throughput");

    group.measurement_time(Duration::from_secs(3));

    for size in data_generator::HSMS_BODY_SIZES {
        for batch_size in GEM_LOOPBACK_BATCH_SIZES {
            let fixture = runtime.block_on(async {
                let fixture = setup_gem_loopback_pair(next_gem_loopback_port()).await;
                promote_equipment_online(&fixture).await;
                fixture
            });
            let bytes_per_batch = gem_message_wire_bytes(size) * batch_size;
            let label = format!("data_body_{size}b_batch_{batch_size}");

            group.throughput(Throughput::Bytes(bytes_per_batch));
            group.bench_function(BenchmarkId::new("batch_concurrent_bytes", &label), |b| {
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    runtime.block_on(async {
                        for _ in 0..iters {
                            let futures: Vec<_> = (0..batch_size)
                                .map(|_| fixture.host.send_message_with_reply(gem_passthrough_request(size)))
                                .collect();
                            let replies = join_all(futures).await;
                            for reply in replies {
                                black_box(reply.expect("GEM batch passthrough request should succeed"));
                            }
                        }
                    });
                    start.elapsed()
                })
            });

            group.throughput(Throughput::Elements(batch_size));
            group.bench_function(BenchmarkId::new("batch_concurrent_messages", &label), |b| {
                b.iter_custom(|iters| {
                    let start = Instant::now();
                    runtime.block_on(async {
                        for _ in 0..iters {
                            let futures: Vec<_> = (0..batch_size)
                                .map(|_| fixture.host.send_message_with_reply(gem_passthrough_request(size)))
                                .collect();
                            let replies = join_all(futures).await;
                            for reply in replies {
                                black_box(reply.expect("GEM batch passthrough request should succeed"));
                            }
                        }
                    });
                    start.elapsed()
                })
            });

            runtime.block_on(shutdown_gem_loopback_pair(fixture));
        }
    }

    group.finish();
}

criterion_group! {
    name = benches;
    config = criterion_config();
    targets = bench_message_builders,
        bench_gem_control_new,
        bench_state_machine,
        bench_gem_loopback_connection,
        bench_gem_loopback_online_transition,
        bench_gem_loopback_control_rtt,
        bench_gem_loopback_passthrough,
        bench_gem_loopback_batch_throughput
}
criterion_main!(benches);