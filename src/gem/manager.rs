//! GEM Manager — GEM 编排层
//!
//! 核心编排器：持有 HsmsCommunicator，监听 HSMS 状态变化和入站消息，
//! 自动驱动 GemControl 状态机，透传非 GEM 消息给上层。

use crate::gem::config::{GemConfig, GemRole};
use crate::gem::gem_state::StateEvent;
use crate::gem::DeviceState;
use crate::gem::session::{GemSession, MessageHandleResult};
use crate::gem::{GemCommand, GemError};
use crate::hsms::communicator::HsmsCommunicator;
use crate::hsms::message::HsmsMessage;
use crate::hsms::ConnectionState;
use tokio::sync::{mpsc, watch};

pub struct GemManager {
    config: GemConfig,
    /// 内部持有的 HSMS 通信器
    hsms_communicator: HsmsCommunicator,
    /// 来自 HSMS 层的入站消息
    hsms_inbound_rx: mpsc::Receiver<HsmsMessage>,
    /// 来自 GemCommunicator 的命令
    from_communicator_cmd_rx: mpsc::Receiver<GemCommand>,
    /// 向上层透传非 GEM 消息
    to_communicator_inbound_msg_tx: mpsc::Sender<HsmsMessage>,
    /// 广播 GEM 状态 (DeviceState 含完整状态层级)
    to_communicator_state_tx: watch::Sender<DeviceState>,
    /// GEM 消息处理器
    session: GemSession,
    /// 上一次的 HSMS ConnectionState，用于检测状态变化
    last_hsms_state: ConnectionState,
}

impl GemManager {
    pub fn new(
        config: GemConfig,
        hsms_communicator: HsmsCommunicator,
        hsms_inbound_rx: mpsc::Receiver<HsmsMessage>,
        from_communicator_cmd_rx: mpsc::Receiver<GemCommand>,
        to_communicator_inbound_msg_tx: mpsc::Sender<HsmsMessage>,
        to_communicator_state_tx: watch::Sender<DeviceState>,
    ) -> Self {
        let session = GemSession::new(
            config.role.clone(),
            config.state_machine_config.clone(),
            hsms_communicator.clone(),
            config.mdln.clone(),
            config.softrev.clone(),
        );

        GemManager {
            config,
            hsms_communicator,
            hsms_inbound_rx,
            from_communicator_cmd_rx,
            to_communicator_inbound_msg_tx,
            to_communicator_state_tx,
            session,
            last_hsms_state: ConnectionState::NotConnected,
        }
    }

    /// 广播当前 GEM 状态
    fn broadcast_state(&self) {
        let _ = self
            .to_communicator_state_tx
            .send(self.session.gem_control.state.clone());
    }

    /// 主循环
    pub async fn run(mut self) {
        let mut hsms_state_rx = self.hsms_communicator.state_rx();

        loop {
            tokio::select! {
                // 1. 监听 HSMS 连接状态变化
                result = hsms_state_rx.changed() => {
                    if result.is_err() {
                        tracing::info!("GEM Manager: HSMS state channel closed, shutting down");
                        break;
                    }
                    let new_hsms_state = *hsms_state_rx.borrow();
                    self.handle_hsms_state_change(new_hsms_state).await;
                }

                // 2. 监听 HSMS 入站消息
                msg = self.hsms_inbound_rx.recv() => {
                    match msg {
                        Some(msg) => {
                            self.handle_inbound_message(msg).await;
                        }
                        None => {
                            tracing::info!("GEM Manager: HSMS inbound channel closed");
                            break;
                        }
                    }
                }

                // 3. 监听 GemCommand
                cmd = self.from_communicator_cmd_rx.recv() => {
                    match cmd {
                        Some(cmd) => {
                            let should_shutdown = self.handle_command(cmd).await;
                            if should_shutdown {
                                break;
                            }
                        }
                        None => {
                            tracing::info!("GEM Manager: command channel closed");
                            break;
                        }
                    }
                }
            }
        }
    }

    /// 处理 HSMS 连接状态变化
    async fn handle_hsms_state_change(&mut self, new_state: ConnectionState) {
        if new_state == self.last_hsms_state {
            return;
        }

        let old_state = self.last_hsms_state;
        self.last_hsms_state = new_state;

        match new_state {
            ConnectionState::NotConnected => {
                tracing::info!("GEM: HSMS disconnected");
                self.session.gem_control.handle_event(StateEvent::SocketDisconnectedEvent);
                self.session.reset();
                self.broadcast_state();
            }

            ConnectionState::NotSelected => {
                if old_state == ConnectionState::NotConnected {
                    tracing::info!("GEM: HSMS connected (Not Selected)");
                    self.session.gem_control.handle_event(StateEvent::SocketConnectedEvent);
                } else {
                    // Selected → NotSelected = Deselect
                    tracing::info!("GEM: HSMS deselected");
                    self.session.gem_control.handle_event(StateEvent::DisSelectEvent);
                    self.session.reset();
                }
                self.broadcast_state();
            }

            ConnectionState::Selected => {
                tracing::info!("GEM: HSMS selected");
                self.session.gem_control.handle_event(StateEvent::SelectEvent);
                self.broadcast_state();

                // 自动发起 S1F13 通信建立
                self.auto_establish_communication().await;
            }
        }
    }

    /// 自动发起 S1F13 通信建立
    async fn auto_establish_communication(&mut self) {
        match self.config.role {
            GemRole::Equipment => {
                // Equipment 模式：主动发送 S1F13
                tracing::info!("GEM: Equipment mode, initiating S1F13");
                self.session.initiate_communication().await;
                self.broadcast_state();

                // 如果配置了初始状态为 OnLine，自动尝试上线
                if self.session.gem_control.state.is_offline() {
                    // 检查是否处于 AttemptOnLine
                    if let DeviceState::Selected(
                        crate::gem::gem_state::GemState::OffLineState(
                            crate::gem::gem_state::GemOfflineState::AttemptOnLine
                        )
                    ) = self.session.gem_control.state {
                        self.session.attempt_online().await;
                        self.broadcast_state();
                    }
                }
            }
            GemRole::Host => {
                // Host 模式：等待 Equipment 发 S1F13，由 handle_inbound_message 自动回复
                tracing::info!("GEM: Host mode, waiting for S1F13 from equipment");
            }
        }
    }

    /// 处理入站消息
    async fn handle_inbound_message(&mut self, msg: HsmsMessage) {
        let result = self.session.handle_inbound_message(msg).await;
        match result {
            MessageHandleResult::Handled => {
                // GEM 消息已处理，广播可能的状态变化
                self.broadcast_state();
            }
            MessageHandleResult::Unhandled(msg) => {
                // 非 GEM 消息，透传给上层
                if self.to_communicator_inbound_msg_tx.send(msg).await.is_err() {
                    tracing::error!("GEM Manager: failed to forward message to app");
                }
            }
        }
    }

    /// 处理 GemCommand，返回 true 表示需要 shutdown
    async fn handle_command(&mut self, cmd: GemCommand) -> bool {
        match cmd {
            GemCommand::OperatorOnline { reply_tx } => {
                tracing::info!("GEM: Operator requests ON-LINE");
                self.session.gem_control.handle_event(StateEvent::OperatorActuatesOnlineEvent);
                self.broadcast_state();

                // 如果进入了 AttemptOnLine，自动发送 S1F1
                if let DeviceState::Selected(
                    crate::gem::gem_state::GemState::OffLineState(
                        crate::gem::gem_state::GemOfflineState::AttemptOnLine
                    )
                ) = self.session.gem_control.state {
                    self.session.attempt_online().await;
                    self.broadcast_state();
                }

                let _ = reply_tx.send(Ok(()));
                false
            }

            GemCommand::OperatorOffline { reply_tx } => {
                tracing::info!("GEM: Operator requests OFF-LINE");
                self.session.gem_control.handle_event(StateEvent::OperatorActuatesOfflineEvent);
                self.broadcast_state();
                let _ = reply_tx.send(Ok(()));
                false
            }

            GemCommand::SetLocal { reply_tx } => {
                tracing::info!("GEM: Set LOCAL");
                self.session.gem_control.handle_event(StateEvent::OperatorSetsLocalEvent);
                self.broadcast_state();
                let _ = reply_tx.send(Ok(()));
                false
            }

            GemCommand::SetRemote { reply_tx } => {
                tracing::info!("GEM: Set REMOTE");
                self.session.gem_control.handle_event(StateEvent::OperatorSetsRemoteEvent);
                self.broadcast_state();
                let _ = reply_tx.send(Ok(()));
                false
            }

            GemCommand::SendMessage { msg } => {
                if let Err(e) = self.hsms_communicator.send_message(msg).await {
                    tracing::error!("GEM: Failed to send message: {}", e);
                }
                false
            }

            GemCommand::SendMessageWithReply { msg, reply_tx } => {
                // Clone sender（O(1)，只复制 mpsc::Sender 和 watch::Receiver），
                // 把等待回复放到独立 task，manager 立即继续处理后续命令（含 Shutdown）。
                let comm = self.hsms_communicator.clone();
                tokio::spawn(async move {
                    let result = comm.send_message_with_reply(msg).await;
                    let _ = reply_tx.send(result.map_err(GemError::Hsms));
                });
                false
            }

            GemCommand::SendReply { msg } => {
                if let Err(e) = self.hsms_communicator.send_reply(msg).await {
                    tracing::error!("GEM: Failed to send reply: {}", e);
                }
                false
            }

            GemCommand::Shutdown { reply_tx } => {
                tracing::info!("GEM Manager: shutdown requested");
                let _ = self.hsms_communicator.shutdown().await;
                let _ = reply_tx.send(Ok(()));
                true
            }
        }
    }
}
