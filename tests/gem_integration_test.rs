use secs_rust::gem::{
    communicator::GemCommunicator,
    config::{GemConfig, GemRole},
    state::{GemOfflineState, GemOnlineState, ControlState},
    DeviceState,
};
use secs_rust::hsms::config::{ConnectionMode, HsmsConfig};
use secs_rust::hsms::message::HsmsMessage;
use secs_rust::secs2::Secs2;
use std::time::Duration;

async fn wait_gem_state(
    rx: &mut tokio::sync::watch::Receiver<DeviceState>,
    pred: impl Fn(&DeviceState) -> bool,
    description: &str,
    timeout: Duration,
) {
    let result = tokio::time::timeout(timeout, async {
        loop {
            if pred(&rx.borrow()) {
                return;
            }
            rx.changed().await.unwrap();
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "Timed out waiting for GEM state: {}, current: {:?}",
        description,
        *rx.borrow()
    );
}

fn assert_control_states(
    eq_comm: &GemCommunicator,
    host_comm: &GemCommunicator,
    expected_eq_control: ControlState,
    expected_host_control: ControlState,
    context: &str,
) {
    let eq_state = eq_comm.state();
    let host_state = host_comm.state();
    assert_eq!(
        eq_state.control_state(),
        Some(&expected_eq_control),
        "{}: equipment control state mismatch (full state: {:?})",
        context,
        eq_state
    );
    assert_eq!(
        host_state.control_state(),
        Some(&expected_host_control),
        "{}: host control state mismatch (full state: {:?})",
        context,
        host_state
    );
}

#[tokio::test]
async fn test_gem_full_state_lifecycle() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_line_number(true)
        .try_init();

    let port = 15200;
    let timeout = Duration::from_secs(5);

    // ── 建立设备端 (Passive / Equipment) ───────────────────────────
    let eq_hsms = HsmsConfig {
        mode: ConnectionMode::Passive,
        ip: "127.0.0.1".to_string(),
        port,
        t3: Duration::from_secs(3),
        t6: Duration::from_secs(3),
        ..Default::default()
    };

    let eq_config = GemConfig {
        role: GemRole::Equipment,
        hsms_config: eq_hsms,
        ..Default::default()
    };

    let (eq_comm, mut eq_msg_rx) = GemCommunicator::new(eq_config);
    let mut eq_state_rx = eq_comm.state_rx();

    assert_eq!(
        eq_comm.state(),
        DeviceState::NotConnected,
        "设备初始状态应为 NotConnected"
    );

    let eq_reply_comm = eq_comm.clone();
    tokio::spawn(async move {
        while let Some(msg) = eq_msg_rx.recv().await {
            tracing::debug!(
                "Equipment passthrough: S{}F{}",
                msg.header.stream,
                msg.header.function
            );
            if msg.header.w_bit {
                let reply = msg.build_reply_message(
                    msg.header.stream,
                    msg.header.function + 1,
                    Secs2::ASCII("EQ_REPLY".to_string()),
                );
                let _ = eq_reply_comm.send_reply(reply).await;
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── 建立主机端 (Active / Host) ─────────────────────────────────
    let host_hsms = HsmsConfig {
        mode: ConnectionMode::Active,
        ip: "127.0.0.1".to_string(),
        port,
        t3: Duration::from_secs(3),
        t6: Duration::from_secs(3),
        ..Default::default()
    };

    let host_config = GemConfig {
        role: GemRole::Host,
        hsms_config: host_hsms,
        ..Default::default()
    };

    let (host_comm, mut host_msg_rx) = GemCommunicator::new(host_config);
    let mut host_state_rx = host_comm.state_rx();

    assert_eq!(eq_comm.state(), DeviceState::NotConnected);
    assert_eq!(host_comm.state(), DeviceState::NotConnected);

    tokio::spawn(async move {
        while let Some(_msg) = host_msg_rx.recv().await {}
    });

    // ── 阶段 1: 等待双端 Selected ──────────────────────────────────
    wait_gem_state(
        &mut eq_state_rx,
        |s| s.is_selected(),
        "Equipment Selected",
        timeout,
    )
    .await;
    wait_gem_state(
        &mut host_state_rx,
        |s| s.is_selected(),
        "Host Selected",
        timeout,
    )
    .await;

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "双端进入 Selected 后",
    );

    // ── 阶段 2: 操作员上线 ─────────────────────────────────────────
    eq_comm
        .operator_online()
        .await
        .expect("operator_online 应成功");

    wait_gem_state(
        &mut eq_state_rx,
        |s| s.is_online(),
        "Equipment OnLine",
        timeout,
    )
    .await;

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OnlineState(GemOnlineState::Local),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "第一次 operator_online 后",
    );

    // ── 阶段 2.5: 数据消息透传验证 ────────────────────────────────
    let req = HsmsMessage::build_request_data_message(
        0,
        2,
        17,
        secs_rust::util::next_system_bytes(),
        Secs2::EMPTY,
    );

    let reply = tokio::time::timeout(timeout, host_comm.send_message_with_reply(req))
        .await
        .expect("等待透传回复超时")
        .expect("send_message_with_reply 失败");

    assert_eq!(reply.header.stream, 2, "透传回复 stream 应为 2");
    assert_eq!(reply.header.function, 18, "透传回复 function 应为 18");
    match &reply.body {
        Secs2::ASCII(s) => assert_eq!(s, "EQ_REPLY", "透传回复内容应为 EQ_REPLY"),
        other => panic!("透传回复 body 应为 ASCII, 实际: {:?}", other),
    }

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OnlineState(GemOnlineState::Local),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "透传消息往返后",
    );

    // ── 阶段 3: Local ↔ Remote 切换 ───────────────────────────────
    eq_comm
        .set_remote()
        .await
        .expect("set_remote 应成功");

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OnlineState(GemOnlineState::Remote),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "set_remote 后",
    );

    eq_comm
        .set_local()
        .await
        .expect("set_local 应成功");

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OnlineState(GemOnlineState::Local),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "set_local 后",
    );

    // ── 阶段 4: 主机请求离线 (S1F15/S1F16) ────────────────────────
    let s1f15 = secs_rust::gem::message::build_s1f15();
    let s1f16 = host_comm
        .send_message_with_reply(s1f15)
        .await
        .expect("S1F15 发送应成功");

    assert_eq!(s1f16.header.stream, 1, "S1F16 reply stream 应为 1");
    assert_eq!(s1f16.header.function, 16, "S1F16 reply function 应为 16");
    match &s1f16.body {
        Secs2::BINARY(v) => assert_eq!(v[0], 0, "OFLACK 应为 0 (接受)"),
        other => panic!("S1F16 body 应为 BINARY, 实际: {:?}", other),
    }

    wait_gem_state(
        &mut eq_state_rx,
        |s| {
            matches!(
                s.control_state(),
                Some(ControlState::OffLineState(GemOfflineState::HostOffline))
            )
        },
        "Equipment HostOffline",
        timeout,
    )
    .await;

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OffLineState(GemOfflineState::HostOffline),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "第一次 S1F15 后",
    );

    // ── 阶段 5: 主机请求上线 (S1F17/S1F18) ────────────────────────
    let s1f17 = secs_rust::gem::message::build_s1f17();
    let s1f18 = host_comm
        .send_message_with_reply(s1f17)
        .await
        .expect("S1F17 发送应成功");

    assert_eq!(s1f18.header.stream, 1, "S1F18 reply stream 应为 1");
    assert_eq!(s1f18.header.function, 18, "S1F18 reply function 应为 18");
    match &s1f18.body {
        Secs2::BINARY(v) => assert_eq!(v[0], 0, "ONLACK 应为 0 (接受)"),
        other => panic!("S1F18 body 应为 BINARY, 实际: {:?}", other),
    }

    wait_gem_state(
        &mut eq_state_rx,
        |s| s.is_online(),
        "Equipment OnLine again",
        timeout,
    )
    .await;

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OnlineState(GemOnlineState::Local),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "第一次 S1F17 后",
    );

    // ── 阶段 6: 操作员主动离线 ─────────────────────────────────────
    eq_comm
        .operator_offline()
        .await
        .expect("operator_offline 应成功");

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "第一次 operator_offline 后",
    );

    // ── 阶段 7: 再次上线后由主机 S1F15 → HostOffline → 操作员离线 ─
    eq_comm
        .operator_online()
        .await
        .expect("二次 operator_online 应成功");

    wait_gem_state(
        &mut eq_state_rx,
        |s| s.is_online(),
        "Equipment OnLine (second time)",
        timeout,
    )
    .await;

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OnlineState(GemOnlineState::Local),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "第二次 operator_online 后",
    );

    let s1f15_2 = secs_rust::gem::message::build_s1f15();
    let _ = host_comm
        .send_message_with_reply(s1f15_2)
        .await
        .expect("二次 S1F15 应成功");

    wait_gem_state(
        &mut eq_state_rx,
        |s| {
            matches!(
                s.control_state(),
                Some(ControlState::OffLineState(GemOfflineState::HostOffline))
            )
        },
        "Equipment HostOffline (second time)",
        timeout,
    )
    .await;

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OffLineState(GemOfflineState::HostOffline),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "第二次 S1F15 后",
    );

    // 转换 #12: HostOffline 状态下操作员主动离线 → EquipmentOffLine
    eq_comm
        .operator_offline()
        .await
        .expect("从 HostOffline 操作员离线应成功");

    assert_control_states(
        &eq_comm,
        &host_comm,
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        ControlState::OffLineState(GemOfflineState::EquipmentOffLine),
        "第二次 operator_offline 后",
    );

    // ── 阶段 8: Shutdown 优雅退出 ─────────────────────────────────
    let eq_shutdown = tokio::time::timeout(Duration::from_secs(3), eq_comm.shutdown()).await;
    assert!(eq_shutdown.is_ok(), "Equipment Shutdown 超时");
    assert!(eq_shutdown.unwrap().is_ok(), "Equipment Shutdown 返回错误");

    let host_shutdown = tokio::time::timeout(Duration::from_secs(3), host_comm.shutdown()).await;
    assert!(host_shutdown.is_ok(), "Host Shutdown 超时");
    assert!(host_shutdown.unwrap().is_ok(), "Host Shutdown 返回错误");
}
