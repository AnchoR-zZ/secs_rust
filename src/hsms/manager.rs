use crate::hsms::config::{ConnectionMode, HsmsConfig};
use crate::hsms::message::HsmsMessage;
use crate::hsms::session::HsmsSession;
use crate::hsms::{ConnectionState, HsmsCommand};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};

/// 错误类型：表示收到 Shutdown 命令
#[derive(Debug, Clone, Copy)]
struct ShutdownError;

pub struct HsmsManager {
    config: HsmsConfig,
    // 接收来自 Communicator 的命令
    from_communicator_cmd_rx: mpsc::Receiver<HsmsCommand>,
    // 发送入站消息（从 Session 收到）给 Communicator
    to_communicator_inbound_msg_tx: mpsc::Sender<HsmsMessage>,
    // 更新连接状态
    to_communicator_state_tx: watch::Sender<ConnectionState>,
}

impl HsmsManager {
    /// 创建新的 ConnectionManager 实例
    pub fn new(
        config: HsmsConfig,
        from_communicator_cmd_rx: mpsc::Receiver<HsmsCommand>,
        to_communicator_inbound_msg_tx: mpsc::Sender<HsmsMessage>,
        to_communicator_state_tx: watch::Sender<ConnectionState>,
    ) -> Self {
        HsmsManager {
            config,
            from_communicator_cmd_rx,
            to_communicator_inbound_msg_tx,
            to_communicator_state_tx,
        }
    }

    /// 主循环：永不退出，直到显式 Shutdown
    pub async fn run(mut self) {
        loop {
            // 1. 更新状态为NotConnected
            let _ = self
                .to_communicator_state_tx
                .send(ConnectionState::NotConnected);

            // 2. 获取 TCP Stream - 现在返回 Result
            let mode = self.config.mode;
            let stream = match mode {
                ConnectionMode::Active => {
                    match Self::connect_loop_inner(&self.config, &mut self.from_communicator_cmd_rx).await {
                        Ok(stream) => stream,
                        Err(ShutdownError) => {
                            tracing::info!("Manager shutdown requested during connect");
                            let _ = self.to_communicator_state_tx.send(ConnectionState::NotConnected);
                            return;
                        }
                    }
                }
                ConnectionMode::Passive => {
                    match Self::accept_loop_inner(&self.config, &mut self.from_communicator_cmd_rx).await {
                        Ok(stream) => stream,
                        Err(ShutdownError) => {
                            tracing::info!("Manager shutdown requested during accept");
                            let _ = self.to_communicator_state_tx.send(ConnectionState::NotConnected);
                            return;
                        }
                    }
                }
            };

            let _ = self
                .to_communicator_state_tx
                .send(ConnectionState::NotSelected);

            // 3. 连接成功，准备启动 Session
            let session = HsmsSession::new(
                stream,
                self.to_communicator_inbound_msg_tx.clone(), // Session 把收到的数据写入这里
                self.to_communicator_state_tx.clone(),       // Session 更新状态
                self.config.clone(),
            );

            // 4. 直接在当前 Task 运行 Session Loop，复用 self.cmd_rx
            // Session 结束时 (TCP断开或收到Shutdown)，run() 返回，循环回到头部重新连接
            let should_shutdown = session.run(&mut self.from_communicator_cmd_rx).await;

            // 5. 如果 session 返回 true，表示需要 shutdown
            if should_shutdown {
                tracing::info!("Manager shutdown requested");
                let _ = self
                    .to_communicator_state_tx
                    .send(ConnectionState::NotConnected);
                return;
            }

            // 6. Session 结束 (TCP 断开)，循环回到头部，重新连接
        }
    }

    /// Active 模式：循环尝试连接，包含 T5 重试逻辑
    /// 同时监听命令通道，如果收到 Shutdown 命令则返回 Err
    async fn connect_loop_inner(
        config: &HsmsConfig,
        cmd_rx: &mut mpsc::Receiver<HsmsCommand>,
    ) -> Result<TcpStream, ShutdownError> {
        let addr_str = format!("{}:{}", config.ip, config.port);

        loop {
            // 使用 tokio::select! 同时等待连接尝试和命令通道
            let connect_result = tokio::select! {
                // 尝试连接
                result = tokio::time::timeout(config.connect_timeout, TcpStream::connect(&addr_str)) => {
                    result
                }
                // 检查命令通道
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(HsmsCommand::Shutdown { reply_tx }) => {
                            tracing::info!("Shutdown command received during connection loop");
                            let _ = reply_tx.send(Ok(()));
                            return Err(ShutdownError);
                        }
                        Some(_) => {
                            // 其他命令在连接阶段忽略，继续尝试连接
                            tracing::warn!("Ignoring non-shutdown command during connection");
                            continue;
                        }
                        None => {
                            tracing::error!("Command channel closed unexpectedly");
                            return Err(ShutdownError);
                        }
                    }
                }
            };

            // 处理连接结果
            match connect_result {
                Ok(Ok(stream)) => {
                    return Ok(stream);
                }
                Ok(Err(_)) | Err(_) => {
                    // 连接失败或超时，等待T5时间后重试（期间也要监听命令）
                    tokio::select! {
                        _ = tokio::time::sleep(config.t5) => {}
                        cmd = cmd_rx.recv() => {
                            if let Some(HsmsCommand::Shutdown { reply_tx }) = cmd {
                                tracing::info!("Shutdown command received during T5 wait");
                                let _ = reply_tx.send(Ok(()));
                                return Err(ShutdownError);
                            }
                        }
                    };
                }
            }
        }
    }

    /// Passive 模式：监听端口，接受连接
    /// 同时监听命令通道，如果收到 Shutdown 命令则返回 Err
    async fn accept_loop_inner(
        config: &HsmsConfig,
        cmd_rx: &mut mpsc::Receiver<HsmsCommand>,
    ) -> Result<TcpStream, ShutdownError> {
        let addr_str = format!("{}:{}", config.ip, config.port);
        tracing::debug!("Passive mode: listening on {}", addr_str);

        // 先绑定监听器（只绑定一次），绑定失败时重试
        let listener = loop {
            match TcpListener::bind(&addr_str).await {
                Ok(listener) => break listener,
                Err(e) => {
                    tracing::warn!("Failed to bind on {}: {}, retrying in 5s", addr_str, e);
                    // 绑定失败，等待5秒后重试（期间监听 Shutdown 命令）
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        cmd = cmd_rx.recv() => {
                            if let Some(HsmsCommand::Shutdown { reply_tx }) = cmd {
                                tracing::info!("Shutdown command received during bind retry wait");
                                let _ = reply_tx.send(Ok(()));
                                return Err(ShutdownError);
                            }
                        }
                    }
                }
            }
        };

        // 绑定成功后，循环 accept（不再重新绑定）
        loop {
            let accept_result = tokio::select! {
                result = listener.accept() => { result }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(HsmsCommand::Shutdown { reply_tx }) => {
                            tracing::info!("Shutdown command received during accept loop");
                            let _ = reply_tx.send(Ok(()));
                            return Err(ShutdownError);
                        }
                        _ => continue,
                    }
                }
            };

            match accept_result {
                Ok((stream, addr)) => {
                    tracing::info!("Accepted connection from {}", addr);
                    return Ok(stream);
                }
                Err(e) => {
                    tracing::warn!("Accept failed: {}, retrying", e);
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
    }
}
