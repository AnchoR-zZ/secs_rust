use crate::hsms::{ConnectionState, HsmsCommand, HsmsError};
use crate::hsms::config::{ConnectionMode, HsmsConfig};
use crate::hsms::message::{HsmsMessage, HsmsMessageCodec, MessageType};
use crate::hsms::stream_util::MonitoredStream;
use crate::util::next_system_bytes;
use futures::{SinkExt, StreamExt};
use std::collections::HashMap;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{self, Duration, Instant, MissedTickBehavior};
use tokio_util::codec::Framed;

struct PendingReply {
    tx: oneshot::Sender<Result<HsmsMessage, HsmsError>>,
    timeout_at: Instant,
}

pub struct HsmsSession {
    session_id: u16,

    // 经过 Codec 包装的流，可以直接读写 HsmsMessage
    stream: Framed<MonitoredStream, HsmsMessageCodec>,

    // 将收到的有效 Data Message 发送给 Manager (App)
    inbound_tx: mpsc::Sender<HsmsMessage>,

    // 更新状态
    state_tx: watch::Sender<ConnectionState>,
    config: HsmsConfig,

    t3_replies: HashMap<u32, PendingReply>,
    t6_replies: HashMap<u32, PendingReply>,
    current_state: ConnectionState,
}

impl HsmsSession {
    pub fn new(
        stream: TcpStream,
        inbound_tx: mpsc::Sender<HsmsMessage>,
        state_tx: watch::Sender<ConnectionState>,
        config: HsmsConfig,
    ) -> Self {
        if let Err(e) = stream.set_nodelay(true) {
            tracing::warn!("Failed to set TCP nodelay: {}", e);
        }
        let monitored_stream = MonitoredStream::new(stream);
        let framed_stream = Framed::new(monitored_stream, HsmsMessageCodec);
        let _ = state_tx.send_replace(ConnectionState::NotSelected);
        Self {
            session_id: config.session_id,
            stream: framed_stream,
            inbound_tx,
            state_tx,
            config,
            current_state: ConnectionState::NotSelected,
            t3_replies: HashMap::new(),
            t6_replies: HashMap::new(),
        }
    }

    /// Session 主循环：处理 I/O 和 Timer
    /// 返回 true 表示需要 shutdown，false 表示 TCP 断开需要重连
    #[allow(clippy::needless_return)]
    pub async fn run(mut self, outbound_rx: &mut mpsc::Receiver<HsmsCommand>) -> bool {
        // 初始化定时器
        // linktest sender
        let mut linktest_interval = time::interval(self.config.linktest);
        linktest_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        // General Timer Check (T3, T6, etc.) - Check every 1s
        let mut timer_check_interval = time::interval(Duration::from_secs(1));
        timer_check_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        // T8 (Check every 1s)
        let mut t8_check_interval = time::interval(Duration::from_secs(1));
        t8_check_interval.set_missed_tick_behavior(MissedTickBehavior::Delay);

        match self.config.mode {
            ConnectionMode::Active => {
                if let Err(e) = self.send_select_req().await {
                    tracing::error!("Failed to send Select.req: {}", e);
                    return false;
                }
            }
            ConnectionMode::Passive => {
                if let Err(e) = self.handle_passive_connection().await {
                    tracing::error!("Failed to establish passive connection: {}", e);
                    return false;
                }
            }
        }

        // 2. Event Loop
        loop {
            tokio::select! {
                // tcp消息
                result = self.stream.next() => {
                    match result {
                        Some(Ok(msg)) => {
                            if !self.handle_incoming_frame(msg).await {
                                return false; // TCP 断开，需要重连
                            }
                        }
                        Some(Err(_e)) => return false, // IO 错误，断开
                        None => return false, // 流关闭（如 Separate 后），需要重连
                    }
                }

                // App 发送指令
                result = outbound_rx.recv() => {
                    match result {
                        Some(msg) => {
                            if self.handle_outbound_msg(msg).await {
                                return true; // 需要 shutdown
                            }
                        }
                        None => {
                            // 命令通道关闭（所有 Communicator 已 drop），视为 shutdown
                            tracing::info!("Command channel closed, shutting down session");
                            return true;
                        }
                    }
                }

                // linktest send
                _ = linktest_interval.tick() => {
                    // Only send if selected
                    if self.current_state == ConnectionState::Selected {
                        if let Err(e) = self.send_linktest_req().await {
                            tracing::error!("Failed to send Linktest: {}", e);
                        }
                    }
                }

                // Timer checks (T3, T6)
                _ = timer_check_interval.tick() => {
                    self.check_t3_timeout();
                    if let Err(e) = self.check_t6_timeout().await {
                        tracing::error!("Linktest T6 error: {}", e);
                        return true;
                    }
                }

                // T8 check
                _ = t8_check_interval.tick() => {
                    // 只有当缓冲区有数据但不足以构成完整消息时，T8才适用
                    if !self.stream.read_buffer().is_empty() {
                        let last_read = self.stream.get_ref().last_read;
                        if last_read.elapsed() > self.config.t8 {
                             tracing::error!("T8 Timeout: Inter-character timeout exceeded ({:?})", self.config.t8);
                             return true;
                        }
                    }
                }

            }
        }
    }

    /// 处理收到的 HSMS 帧
    async fn handle_incoming_frame(&mut self, msg: HsmsMessage) -> bool {
        tracing::debug!("Received message: {:?}", msg);
        match msg.header.s_type {
            MessageType::Data => {
                let sys_id = msg.header.system_bytes;
                if let Some(entry) = self.t3_replies.remove(&sys_id) {
                    let _ = entry.tx.send(Ok(msg));
                } else if let Err(e) = self.inbound_tx.send(msg).await {
                    tracing::error!("Failed to send inbound message: {}", e);
                }
            }
            MessageType::SelectReq => {
                // 在 Selected 状态下收到 SelectReq，发送 Reject
                let _ = self.send_reject_rsp(&msg, 1).await;
            }
            MessageType::SelectRsp => {
                let sys_id = msg.header.system_bytes;
                if let Some(entry) = self.t3_replies.remove(&sys_id) {
                    if msg.header.function == 0 {
                        self.update_state(ConnectionState::Selected);
                        let _ = entry.tx.send(Ok(msg));
                    } else {
                        let _ = entry.tx.send(Err(HsmsError::Protocol {
                            message: format!("Select.rsp error status: {}", msg.header.function),
                        }));
                        let _ = self.send_reject_rsp(&msg, 0x04).await;
                    }
                } else {
                    tracing::warn!("Unexpected SelectRsp in main loop");
                }
            }
            MessageType::LinktestReq => {
                tracing::debug!("Auto-replying to LinktestReq");
                let rsp = HsmsMessage::linktest_rsp(&msg);
                if let Err(e) = self.stream.send(rsp).await {
                    tracing::error!("Failed to send Linktest.rsp: {}", e);
                }
            }
            MessageType::LinktestRsp => {
                tracing::debug!("Received LinktestRsp: {:?}", msg);
                let sys_id = msg.header.system_bytes;
                if let Some(entry) = self.t6_replies.remove(&sys_id) {
                    let _ = entry.tx.send(Ok(msg));
                    tracing::debug!("Linktest success");
                } else {
                    tracing::warn!("Unexpected LinktestRsp: no pending entry for sys_id {}", sys_id);
                }
            }
            MessageType::SeparateReq => {
                tracing::info!("Received SeparateReq, closing connection");
                self.update_state(ConnectionState::NotConnected);
                return false;
            }
            MessageType::DeselectReq => {
                let rsp = HsmsMessage::deselect_rsp(&msg, 0);
                let _ = self.stream.send(rsp).await;
                self.update_state(ConnectionState::NotSelected);
            }
            MessageType::DeselectRsp => {
                self.update_state(ConnectionState::NotSelected);
                let sys_id = msg.header.system_bytes;
                if let Some(entry) = self.t3_replies.remove(&sys_id) {
                    let _ = entry.tx.send(Ok(msg));
                }
            }
            MessageType::RejectReq => {
                tracing::warn!("Received RejectReq: {:?}", msg);
            }
        }
        true
    }

    /// 处理应用层要发送的消息
    /// 返回 true 表示需要 shutdown，false 表示继续运行
    async fn handle_outbound_msg(&mut self, cmd: HsmsCommand) -> bool {
        match cmd {
            HsmsCommand::Select { reply_tx } => {
                self.handle_select_command(reply_tx).await;
                false
            }
            HsmsCommand::NotSelect { reply_tx } => {
                self.handle_deselect_command(reply_tx).await;
                false
            }
            HsmsCommand::NotConnect { reply_tx } => {
                self.handle_separate_command(reply_tx).await;
                false
            }
            HsmsCommand::SendReply { msg } => {
                if let Err(e) = self.stream.send(msg).await {
                    tracing::error!("Failed to send reply: {}", e);
                }
                false
            }
            HsmsCommand::SendMessage { msg } => {
                if let Err(e) = self.stream.send(msg).await {
                    tracing::error!("Failed to send reply: {}", e);
                }
                false
            }
            HsmsCommand::SendMessageNeedReply { msg, reply_tx } => {
                tracing::debug!("Sending message with reply: {:?}", msg);
                let sys_id = msg.header.system_bytes;
                if let Err(e) = self.stream.send(msg).await {
                    tracing::error!("Failed to send reply: {}", e);
                }
                self.t3_replies.insert(
                    sys_id,
                    PendingReply {
                        tx: reply_tx,
                        timeout_at: Instant::now() + self.config.t3,
                    },
                );
                false
            }
            HsmsCommand::Shutdown { reply_tx } => {
                tracing::info!("Shutdown command received");
                let _ = reply_tx.send(Ok(()));
                true // 返回 true 表示需要 shutdown
            }
        }
    }

    async fn handle_select_command(
        &mut self,
        reply_tx: oneshot::Sender<Result<HsmsMessage, HsmsError>>,
    ) {
        if self.current_state != ConnectionState::NotSelected {
            let _ = reply_tx.send(Err(HsmsError::InvalidState {
                expected: ConnectionState::NotSelected,
                actual: self.current_state,
            }));
            return;
        }

        if self.config.mode != ConnectionMode::Active {
            let _ = reply_tx.send(Err(HsmsError::Protocol {
                message: "Passive mode ignores local Select command".to_string(),
            }));
            return;
        }

        let sys_id = next_system_bytes();
        let req = HsmsMessage::select_req(self.session_id, sys_id);
        if let Err(e) = self.stream.send(req).await {
            tracing::error!("Failed to send Select.req: {}", e);
            let _ = reply_tx.send(Err(HsmsError::Io(e)));
        } else {
            let now = Instant::now();
            self.t3_replies.insert(
                sys_id,
                PendingReply {
                    tx: reply_tx,
                    timeout_at: now + self.config.t6,
                },
            );
        }
    }

    async fn handle_deselect_command(
        &mut self,
        reply_tx: oneshot::Sender<Result<HsmsMessage, HsmsError>>,
    ) {
        if self.current_state != ConnectionState::Selected {
            let _ = reply_tx.send(Err(HsmsError::InvalidState {
                expected: ConnectionState::Selected,
                actual: self.current_state,
            }));
            return;
        }

        let req = HsmsMessage::deselect_req(self.session_id, next_system_bytes());
        let sys_id = req.header.system_bytes;
        if let Err(e) = self.stream.send(req).await {
            tracing::error!("Failed to send Deselect.req: {}", e);
            let _ = reply_tx.send(Err(HsmsError::Io(e)));
        } else {
            let now = Instant::now();
            self.t3_replies.insert(
                sys_id,
                PendingReply {
                    tx: reply_tx,
                    timeout_at: now + self.config.t6,
                },
            );
            self.update_state(ConnectionState::NotSelected);
        }
    }

    async fn handle_separate_command(
        &mut self,
        reply_tx: oneshot::Sender<Result<HsmsMessage, HsmsError>>,
    ) {
        if self.current_state == ConnectionState::NotConnected {
            let _ = reply_tx.send(Err(HsmsError::Protocol {
                message: "Already NotConnected".to_string(),
            }));
            return;
        }

        let sep = HsmsMessage::separate_req(self.session_id, next_system_bytes());
        let sep_msg = sep.clone();
        if let Err(e) = self.stream.send(sep).await {
            tracing::error!("Failed to send Separate.req: {}", e);
        }
        if let Err(e) = self.stream.close().await {
            tracing::warn!("Close stream after Separate.req failed: {}", e);
        }
        self.update_state(ConnectionState::NotConnected);
        let _ = reply_tx.send(Ok(sep_msg));
    }

    async fn check_t6_timeout(&mut self) -> Result<(), HsmsError> {
        if self.current_state != ConnectionState::Selected {
            return Ok(());
        }
        let now = Instant::now();
        let timed_out_keys: Vec<u32> = self
            .t6_replies
            .iter()
            .filter(|(_, v)| v.timeout_at <= now)
            .map(|(k, _)| *k)
            .collect();
        if !timed_out_keys.is_empty() {
            for k in timed_out_keys {
                if let Some(entry) = self.t6_replies.remove(&k) {
                    let _ = entry.tx.send(Err(HsmsError::Timeout {
                        kind: "T6",
                        duration: self.config.t6,
                    }));
                }
            }
            self.update_state(ConnectionState::NotConnected);
            return Err(HsmsError::Timeout {
                kind: "T6",
                duration: self.config.t6,
            });
        }
        Ok(())
    }

    fn check_t3_timeout(&mut self) {
        let now = Instant::now();

        // 收集需要移除的 Key (Rust 中遍历时不能修改 Map，所以分两步)
        let timed_out_keys: Vec<u32> = self
            .t3_replies
            .iter()
            .filter(|(_, v)| v.timeout_at <= now)
            .map(|(k, _)| *k)
            .collect();

        for k in timed_out_keys {
            if let Some(entry) = self.t3_replies.remove(&k) {
                let _ = entry.tx.send(Err(HsmsError::Timeout {
                    kind: "T3",
                    duration: self.config.t3,
                }));
                tracing::warn!("Transaction ID {} timed out", k);
            }
        }
    }

    async fn send_reject_rsp(
        &mut self,
        reject_message: &HsmsMessage,
        reason: u8,
    ) -> Result<(), HsmsError> {
        let rsp = HsmsMessage::new_reject(reject_message, reason);

        self.stream.send(rsp).await.map_err(HsmsError::Io)
    }

    async fn send_linktest_req(&mut self) -> Result<(), HsmsError> {
        let sys_id = next_system_bytes();
        let req = HsmsMessage::linktest_req(sys_id);
        tracing::debug!("Sent LinktestReq: {:?}", req);
        self.stream.send(req).await.map_err(HsmsError::Io)?;
        let now = Instant::now();
        let (tx, _rx) = oneshot::channel();
        self.t6_replies.insert(
            sys_id,
            PendingReply {
                tx,
                timeout_at: now + self.config.t6,
            },
        );
        Ok(())
    }

    /// passive连接
    async fn handle_passive_connection(&mut self) -> Result<(), HsmsError> {
        let start_time = Instant::now();

        loop {
            // 检查是否已超过T7时间
            if start_time.elapsed() > self.config.t7 {
                tracing::warn!(
                    "T7 timeout: No Select.req received within {:?} seconds",
                    self.config.t7
                );
                return Err(HsmsError::Timeout {
                    kind: "T7",
                    duration: self.config.t7,
                });
            }

            // 计算剩余等待时间
            let remaining_time = self.config.t7.saturating_sub(start_time.elapsed());

            match time::timeout(remaining_time, self.stream.next()).await {
                Ok(Some(Ok(msg))) => {
                    tracing::debug!("Received message: {:?}", msg);
                    // 收到消息，检查是否为Select.req
                    if msg.header.s_type == MessageType::SelectReq {
                        // 发送Select.rsp
                        let rsp = HsmsMessage::select_rsp(&msg, 0); // status: 0 = success

                        if let Err(e) = self.stream.send(rsp).await {
                            tracing::error!("Failed to send Select.rsp: {}", e);
                            return Err(HsmsError::Io(e));
                        }

                        self.session_id = msg.header.session_id;
                        self.update_state(ConnectionState::Selected);
                        tracing::info!("HSMS connection established (Passive mode)");
                        break; // 成功建立连接，退出循环
                    } else {
                        tracing::warn!(
                            "Expected Select.req but got {:?}, sending reject message",
                            msg.header.s_type
                        );
                        let _ = self.send_reject_rsp(&msg, 0x04).await;
                        continue;
                    }
                }
                Ok(Some(Err(e))) => {
                    tracing::error!("Stream error while waiting for Select.req: {}", e);
                    return Err(HsmsError::Io(e));
                }
                Ok(None) => {
                    tracing::info!("Connection closed by peer");
                    return Err(HsmsError::Protocol {
                        message: "Connection closed".to_string(),
                    });
                }
                Err(_) => {
                    // 单次等待超时，继续循环检查T7总体超时
                    continue;
                }
            }
        }
        Ok(())
    }

    async fn send_select_req(&mut self) -> Result<(), HsmsError> {
        let sys_id = next_system_bytes();
        tracing::debug!("Sending SelectReq with system_bytes: {:?}", sys_id);
        let req = HsmsMessage::select_req(self.session_id, sys_id);
        self.stream.send(req).await.map_err(HsmsError::Io)?;

        // 等待 Select.rsp，使用 T6 超时
        match time::timeout(self.config.t6, async {
            loop {
                match self.stream.next().await {
                    Some(Ok(msg)) => {
                        tracing::debug!("Received message: {:?}", msg);
                        tracing::debug!("Received system_bytes: {:?}", msg.header.system_bytes);
                        // 检查是否为 Select.rsp 且系统字节匹配
                        if msg.header.s_type == MessageType::SelectRsp
                            && msg.header.system_bytes == sys_id
                        {
                            // 检查状态码 (function 字段)
                            if msg.header.function == 0 {
                                self.update_state(ConnectionState::Selected);
                                return Ok(());
                            } else {
                                return Err(HsmsError::Protocol {
                                    message: format!(
                                        "Select.rsp error status: {}",
                                        msg.header.function
                                    ),
                                });
                            }
                        } else {
                            tracing::warn!(
                                "Expected Select.rsp but got {:?}, sending reject message",
                                msg.header.s_type
                            );
                            let _ = self.send_reject_rsp(&msg, 0x04).await;
                        }
                    }
                    Some(Err(e)) => return Err(HsmsError::Io(e)),
                    None => {
                        return Err(HsmsError::Protocol {
                            message: "Connection closed".to_string(),
                        })
                    }
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(HsmsError::Timeout {
                kind: "T6",
                duration: self.config.t6,
            }),
        }
    }

    fn update_state(&mut self, new_state: ConnectionState) {
        if self.current_state != new_state {
            self.current_state = new_state;
            let _ = self.state_tx.send(new_state);
        }
    }
}
