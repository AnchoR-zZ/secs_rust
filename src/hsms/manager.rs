use crate::hsms::config::{ConnectionMode, HsmsConfig};
use crate::hsms::message::HsmsMessage;
use crate::hsms::session::HsmsSession;
use crate::hsms::{ConnectionState, HsmsCommand};
use crate::util::SystemBytesGenerator;
use std::sync::Arc;
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
    system_bytes: Arc<SystemBytesGenerator>,
}

impl HsmsManager {
    /// 创建新的 ConnectionManager 实例
    pub fn new(
        config: HsmsConfig,
        from_communicator_cmd_rx: mpsc::Receiver<HsmsCommand>,
        to_communicator_inbound_msg_tx: mpsc::Sender<HsmsMessage>,
        to_communicator_state_tx: watch::Sender<ConnectionState>,
        system_bytes: Arc<SystemBytesGenerator>,
    ) -> Self {
        HsmsManager {
            config,
            from_communicator_cmd_rx,
            to_communicator_inbound_msg_tx,
            to_communicator_state_tx,
            system_bytes,
        }
    }

    pub async fn run(mut self) {
        let passive_listener = if self.config.mode == ConnectionMode::Passive {
            match Self::bind_with_retry(&self.config, &mut self.from_communicator_cmd_rx).await {
                Ok(listener) => Some(listener),
                Err(ShutdownError) => {
                    tracing::info!("Manager shutdown requested during bind");
                    let _ = self.to_communicator_state_tx.send(ConnectionState::NotConnected);
                    return;
                }
            }
        } else {
            None
        };

        loop {
            let _ = self
                .to_communicator_state_tx
                .send(ConnectionState::NotConnected);

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
                    match Self::accept_connection(
                        passive_listener.as_ref().unwrap(),
                        &mut self.from_communicator_cmd_rx,
                    ).await {
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

            let session = HsmsSession::new(
                stream,
                self.to_communicator_inbound_msg_tx.clone(),
                self.to_communicator_state_tx.clone(),
                self.config.clone(),
                Arc::clone(&self.system_bytes),
            );

            let should_shutdown = session.run(&mut self.from_communicator_cmd_rx).await;

            if should_shutdown {
                tracing::info!("Manager shutdown requested");
                let _ = self
                    .to_communicator_state_tx
                    .send(ConnectionState::NotConnected);
                return;
            }
        }
    }

    /// Passive 模式：绑定端口（带重试），返回 TcpListener
    async fn bind_with_retry(
        config: &HsmsConfig,
        cmd_rx: &mut mpsc::Receiver<HsmsCommand>,
    ) -> Result<TcpListener, ShutdownError> {
        let addr_str = format!("{}:{}", config.ip, config.port);
        tracing::debug!("Passive mode: binding on {}", addr_str);

        loop {
            match TcpListener::bind(&addr_str).await {
                Ok(listener) => {
                    tracing::info!("Passive mode: bound on {}", addr_str);
                    return Ok(listener);
                }
                Err(e) => {
                    tracing::warn!("Bind failed on {}: {}, retrying in 5s", addr_str, e);
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
                        cmd = cmd_rx.recv() => {
                            if let Some(HsmsCommand::Shutdown { reply_tx }) = cmd {
                                tracing::info!("Shutdown command received during bind retry");
                                let _ = reply_tx.send(Ok(()));
                                return Err(ShutdownError);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Passive 模式：从已有 listener 接受连接
    async fn accept_connection(
        listener: &TcpListener,
        cmd_rx: &mut mpsc::Receiver<HsmsCommand>,
    ) -> Result<TcpStream, ShutdownError> {
        loop {
            let accept_result = tokio::select! {
                result = listener.accept() => result,
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(HsmsCommand::Shutdown { reply_tx }) => {
                            tracing::info!("Shutdown command received during accept");
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

}
