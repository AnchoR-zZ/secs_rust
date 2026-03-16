use secs_rust::hsms::{
    communicator::HsmsCommunicator,
    config::{ConnectionMode, HsmsConfig},
    message::HsmsMessage,
    ConnectionState,
};
use secs_rust::secs2::Secs2;
use std::time::Duration;

/// 辅助函数：等待 HSMS 状态满足条件，带超时
async fn wait_hsms_state(
    rx: &mut tokio::sync::watch::Receiver<ConnectionState>,
    pred: impl Fn(&ConnectionState) -> bool,
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
        "Timed out waiting for HSMS state: {}, current: {:?}",
        description,
        *rx.borrow()
    );
}

/// 测试 HSMS 完整状态机转换生命周期（单方法模拟真实流程）
///
/// 模拟 HSMS 从连接建立到完整运行的真实生命周期：
///   NotConnected → NotSelected → Selected (连接建立)
///   → 数据消息往返验证 (S1F1/S1F2)
///   → Selected → NotSelected (Deselect)
///   → NotSelected → NotConnected (Separate) → 自动重连 → Selected
///   → 重连后数据消息验证 (S2F17/S2F18)
///   → Selected → NotConnected (直接 Separate) → 自动重连 → Selected
///   → 二次重连后数据消息验证 (S1F3/S1F4)
///   → Shutdown 优雅退出
#[tokio::test]
async fn test_hsms_full_state_lifecycle() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_line_number(true)
        .try_init();

    let port = 15100;
    let timeout = Duration::from_secs(5);
    let reconnect_timeout = Duration::from_secs(10);

    // ── 建立 Passive 端 ──────────────────────────────────────────
    let passive_config = HsmsConfig {
        mode: ConnectionMode::Passive,
        ip: "127.0.0.1".to_string(),
        port,
        t3: Duration::from_secs(3),
        t5: Duration::from_secs(1),
        t6: Duration::from_secs(3),
        ..Default::default()
    };

    let (passive_comm, mut passive_msg_rx) = HsmsCommunicator::new(passive_config);
    let mut passive_state_rx = passive_comm.state_rx();

    // Passive 端自动回复收到的数据消息（跨重连持续工作）
    let passive_reply_comm = passive_comm.clone();
    tokio::spawn(async move {
        while let Some(msg) = passive_msg_rx.recv().await {
            tracing::debug!(
                "Passive passthrough: S{}F{}",
                msg.header.stream,
                msg.header.function
            );
            if msg.header.w_bit {
                let reply = msg.build_reply_message(
                    msg.header.stream,
                    msg.header.function + 1,
                    Secs2::ASCII("REPLY".to_string()),
                );
                let _ = passive_reply_comm.send_reply(reply).await;
            }
        }
    });

    // 初始状态：NotConnected
    assert_eq!(
        passive_comm.state(),
        ConnectionState::NotConnected,
        "Passive 端初始状态应为 NotConnected"
    );

    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── 建立 Active 端 ──────────────────────────────────────────
    let active_config = HsmsConfig {
        mode: ConnectionMode::Active,
        ip: "127.0.0.1".to_string(),
        port,
        t3: Duration::from_secs(3),
        t5: Duration::from_secs(1),
        t6: Duration::from_secs(3),
        ..Default::default()
    };

    let (active_comm, mut active_msg_rx) = HsmsCommunicator::new(active_config);
    let mut active_state_rx = active_comm.state_rx();

    // 消耗 Active 端收到的透传消息
    tokio::spawn(async move {
        while let Some(_msg) = active_msg_rx.recv().await {}
    });

    // ── 阶段 1: 等待双端 Selected ────────────────────────────────
    // 转换: NotConnected → NotSelected → Selected
    wait_hsms_state(
        &mut passive_state_rx,
        |s| *s == ConnectionState::Selected,
        "Passive Selected",
        timeout,
    )
    .await;
    wait_hsms_state(
        &mut active_state_rx,
        |s| *s == ConnectionState::Selected,
        "Active Selected",
        timeout,
    )
    .await;

    assert_eq!(
        passive_comm.state(),
        ConnectionState::Selected,
        "Passive 端应为 Selected"
    );
    assert_eq!(
        active_comm.state(),
        ConnectionState::Selected,
        "Active 端应为 Selected"
    );

    // ── 阶段 2: 数据消息往返验证 ──────────────────────────────────
    // Active 发送 S1F1, Passive 自动回复 S1F2
    let req = HsmsMessage::build_request_data_message(
        0,
        1,
        1,
        secs_rust::util::next_system_bytes(),
        Secs2::ASCII("HELLO".to_string()),
    );

    let reply = tokio::time::timeout(timeout, active_comm.send_message_with_reply(req))
        .await
        .expect("等待数据回复超时")
        .expect("send_message_with_reply 失败");

    assert_eq!(reply.header.stream, 1, "回复 stream 应为 1");
    assert_eq!(reply.header.function, 2, "回复 function 应为 2");
    match &reply.body {
        Secs2::ASCII(s) => assert_eq!(s, "REPLY", "回复内容应为 REPLY"),
        other => panic!("回复 body 应为 ASCII, 实际: {:?}", other),
    }

    // ── 阶段 3: Deselect (Selected → NotSelected) ────────────────
    active_comm
        .send_not_select()
        .await
        .expect("send_not_select 应成功");

    wait_hsms_state(
        &mut active_state_rx,
        |s| *s == ConnectionState::NotSelected,
        "Active NotSelected (Deselect)",
        timeout,
    )
    .await;
    wait_hsms_state(
        &mut passive_state_rx,
        |s| *s == ConnectionState::NotSelected,
        "Passive NotSelected (Deselect)",
        timeout,
    )
    .await;

    assert_eq!(
        active_comm.state(),
        ConnectionState::NotSelected,
        "Deselect 后 Active 端应为 NotSelected"
    );
    assert_eq!(
        passive_comm.state(),
        ConnectionState::NotSelected,
        "Deselect 后 Passive 端应为 NotSelected"
    );

    // ── 阶段 4: Separate 与自动重连 ──────────────────────────────
    // 从 NotSelected 状态 Separate → NotConnected → 管理器自动重连 → Selected
    active_comm
        .send_not_connect()
        .await
        .expect("send_not_connect 应成功");

    // Active 端在 send_not_connect 返回前已设为 NotConnected，等待自动重连至 Selected
    wait_hsms_state(
        &mut active_state_rx,
        |s| *s == ConnectionState::Selected,
        "Active Selected (首次重连)",
        reconnect_timeout,
    )
    .await;
    // Active Selected 意味着 Select 握手完成，Passive 端也应为 Selected
    wait_hsms_state(
        &mut passive_state_rx,
        |s| *s == ConnectionState::Selected,
        "Passive Selected (首次重连)",
        reconnect_timeout,
    )
    .await;

    assert_eq!(
        active_comm.state(),
        ConnectionState::Selected,
        "首次重连后 Active 端应为 Selected"
    );
    assert_eq!(
        passive_comm.state(),
        ConnectionState::Selected,
        "首次重连后 Passive 端应为 Selected"
    );

    // ── 阶段 5: 重连后数据消息验证 ────────────────────────────────
    // 验证新会话可正常透传数据消息
    let req2 = HsmsMessage::build_request_data_message(
        0,
        2,
        17,
        secs_rust::util::next_system_bytes(),
        Secs2::ASCII("RECONNECT_TEST".to_string()),
    );

    let reply2 = tokio::time::timeout(timeout, active_comm.send_message_with_reply(req2))
        .await
        .expect("重连后等待数据回复超时")
        .expect("重连后 send_message_with_reply 失败");

    assert_eq!(reply2.header.stream, 2, "重连回复 stream 应为 2");
    assert_eq!(reply2.header.function, 18, "重连回复 function 应为 18");
    match &reply2.body {
        Secs2::ASCII(s) => assert_eq!(s, "REPLY", "重连回复内容应为 REPLY"),
        other => panic!("重连回复 body 应为 ASCII, 实际: {:?}", other),
    }

    // ── 阶段 6: 从 Selected 直接 Separate ────────────────────────
    // Selected → NotConnected → 管理器自动重连 → Selected
    active_comm
        .send_not_connect()
        .await
        .expect("二次 send_not_connect 应成功");

    // Active 端在 send_not_connect 返回前已设为 NotConnected，安全等待 Selected
    wait_hsms_state(
        &mut active_state_rx,
        |s| *s == ConnectionState::Selected,
        "Active Selected (二次重连)",
        reconnect_timeout,
    )
    .await;

    assert_eq!(
        active_comm.state(),
        ConnectionState::Selected,
        "二次重连后 Active 端应为 Selected"
    );
    assert_eq!(
        passive_comm.state(),
        ConnectionState::Selected,
        "二次重连后 Passive 端应为 Selected"
    );

    // ── 阶段 7: 二次重连后数据消息验证 ────────────────────────────
    let req3 = HsmsMessage::build_request_data_message(
        0,
        1,
        3,
        secs_rust::util::next_system_bytes(),
        Secs2::ASCII("SECOND_RECONNECT".to_string()),
    );

    let reply3 = tokio::time::timeout(timeout, active_comm.send_message_with_reply(req3))
        .await
        .expect("二次重连后等待数据回复超时")
        .expect("二次重连后 send_message_with_reply 失败");

    assert_eq!(reply3.header.stream, 1, "二次重连回复 stream 应为 1");
    assert_eq!(reply3.header.function, 4, "二次重连回复 function 应为 4");
    match &reply3.body {
        Secs2::ASCII(s) => assert_eq!(s, "REPLY", "二次重连回复内容应为 REPLY"),
        other => panic!("二次重连回复 body 应为 ASCII, 实际: {:?}", other),
    }

    // ── 阶段 8: Shutdown 优雅退出 ─────────────────────────────────
    let passive_shutdown =
        tokio::time::timeout(Duration::from_secs(3), passive_comm.shutdown()).await;
    assert!(passive_shutdown.is_ok(), "Passive Shutdown 超时");
    assert!(
        passive_shutdown.unwrap().is_ok(),
        "Passive Shutdown 返回错误"
    );

    let active_shutdown =
        tokio::time::timeout(Duration::from_secs(3), active_comm.shutdown()).await;
    assert!(active_shutdown.is_ok(), "Active Shutdown 超时");
    assert!(
        active_shutdown.unwrap().is_ok(),
        "Active Shutdown 返回错误"
    );
}
