//! GEM Session — GEM 协议消息处理器
//!
//! 在 HSMS Selected 状态下处理 GEM 协议逻辑的组件。
//! 根据 GemRole (Equipment/Host) 自动处理 S1Fx 消息并驱动状态机。

use crate::gem::config::GemRole;
use crate::gem::gem_state::{GemControl, StateEvent, StateMachineConfig};
use crate::gem::message;
use crate::hsms::communicator::HsmsCommunicator;
use crate::hsms::message::HsmsMessage;

/// 消息处理结果
pub enum MessageHandleResult {
    /// GEM 层已处理该消息（如 S1F1 自动回复等）
    Handled,
    /// GEM 层不处理，需透传给上层应用
    Unhandled(HsmsMessage),
}

/// GEM Session — 消息处理器 + 状态机驱动器
pub struct GemSession {
    /// Equipment 或 Host
    #[allow(dead_code)]
    role: GemRole,
    /// GEM 控制状态机
    pub gem_control: GemControl,
    /// HSMS 通信器，用于发送回复消息
    communicator: HsmsCommunicator,
    /// S1F13/F14 通信握手是否完成
    pub communication_established: bool,
    /// 设备型号名
    mdln: String,
    /// 软件版本
    softrev: String,
}

impl GemSession {
    pub fn new(
        role: GemRole,
        state_machine_config: StateMachineConfig,
        communicator: HsmsCommunicator,
        mdln: String,
        softrev: String,
    ) -> Self {
        Self {
            role,
            gem_control: GemControl::new(state_machine_config),
            communicator,
            communication_established: false,
            mdln,
            softrev,
        }
    }

    /// 处理入站 HSMS 数据消息
    ///
    /// 根据 stream/function 分发处理，GEM 层自动处理的消息返回 Handled，
    /// 其余返回 Unhandled 由上层应用自行处理。
    pub async fn handle_inbound_message(&mut self, msg: HsmsMessage) -> MessageHandleResult {
        let stream = msg.header.stream;
        let function = msg.header.function;

        match (stream, function) {
            // S1F0 — Abort Transaction
            (1, 0) => {
                tracing::info!("GEM: Received S1F0 Abort Transaction");
                self.gem_control.handle_event(StateEvent::ReceivedS1F0Event);
                MessageHandleResult::Handled
            }

            // S1F1 — Are You There (Primary)
            (1, 1) => self.handle_s1f1(msg).await,

            // S1F2 — On Line Data (Reply to S1F1)
            (1, 2) => {
                tracing::info!("GEM: Received S1F2 On Line Data");
                self.gem_control.handle_event(StateEvent::ReceivedS1F2Event);
                MessageHandleResult::Handled
            }

            // S1F13 — Establish Communication Request (Primary)
            (1, 13) => self.handle_s1f13(msg).await,

            // S1F14 — Establish Communication Acknowledge (Reply to S1F13)
            (1, 14) => {
                tracing::info!("GEM: Received S1F14 Establish Communication Ack");
                self.communication_established = true;
                MessageHandleResult::Handled
            }

            // S1F15 — Request OFF-LINE (Primary)
            (1, 15) => self.handle_s1f15(msg).await,

            // S1F16 — OFF-LINE Acknowledge (Reply to S1F15)
            // 通常由 send_message_with_reply 接收，不会走到这里
            // 但如果确实收到了，标记为 Handled
            (1, 16) => {
                tracing::info!("GEM: Received S1F16 OFF-LINE Ack");
                MessageHandleResult::Handled
            }

            // S1F17 — Request ON-LINE (Primary)
            (1, 17) => self.handle_s1f17(msg).await,

            // S1F18 — ON-LINE Acknowledge (Reply to S1F17)
            (1, 18) => {
                tracing::info!("GEM: Received S1F18 ON-LINE Ack");
                MessageHandleResult::Handled
            }

            // 其他消息 → 透传给上层
            _ => MessageHandleResult::Unhandled(msg),
        }
    }

    /// 处理 S1F1 Are You There
    async fn handle_s1f1(&mut self, msg: HsmsMessage) -> MessageHandleResult {
        tracing::info!("GEM: Received S1F1 Are You There");

        // 只有在需要回复时才发送 S1F2 (检查 W-bit)
        if msg.header.w_bit {
            let reply = message::build_s1f2_reply(&msg, &self.mdln, &self.softrev);
            if let Err(e) = self.communicator.send_reply(reply).await {
                tracing::error!("GEM: Failed to send S1F2 reply: {}", e);
            }
        }

        MessageHandleResult::Handled
    }

    /// 处理 S1F13 Establish Communication Request
    async fn handle_s1f13(&mut self, msg: HsmsMessage) -> MessageHandleResult {
        tracing::info!("GEM: Received S1F13 Establish Communication Request");

        // 回复 S1F14 (COMMACK = 0, accepted)
        if msg.header.w_bit {
            let reply = message::build_s1f14_reply(&msg, 0, &self.mdln, &self.softrev);
            if let Err(e) = self.communicator.send_reply(reply).await {
                tracing::error!("GEM: Failed to send S1F14 reply: {}", e);
            }
        }

        self.communication_established = true;
        MessageHandleResult::Handled
    }

    /// 处理 S1F15 Request OFF-LINE
    async fn handle_s1f15(&mut self, msg: HsmsMessage) -> MessageHandleResult {
        tracing::info!("GEM: Received S1F15 Request OFF-LINE");

        // 回复 S1F16 (OFLACK = 0, accepted)
        if msg.header.w_bit {
            let reply = message::build_s1f16_reply(&msg, 0);
            if let Err(e) = self.communicator.send_reply(reply).await {
                tracing::error!("GEM: Failed to send S1F16 reply: {}", e);
            }
        }

        self.gem_control.handle_event(StateEvent::ReceivedS1F15Event);
        MessageHandleResult::Handled
    }

    /// 处理 S1F17 Request ON-LINE
    async fn handle_s1f17(&mut self, msg: HsmsMessage) -> MessageHandleResult {
        tracing::info!("GEM: Received S1F17 Request ON-LINE");

        // 回复 S1F18 (ONLACK = 0, accepted)
        if msg.header.w_bit {
            let reply = message::build_s1f18_reply(&msg, 0);
            if let Err(e) = self.communicator.send_reply(reply).await {
                tracing::error!("GEM: Failed to send S1F18 reply: {}", e);
            }
        }

        self.gem_control.handle_event(StateEvent::ReceivedS1F17Event);
        MessageHandleResult::Handled
    }

    /// Equipment 模式：主动发起 S1F13 通信建立
    pub async fn initiate_communication(&mut self) -> bool {
        let session_id = *self.communicator.state_rx().borrow();
        let _ = session_id; // 使用 hsms_config 中的 session_id
        
        let s1f13 = message::build_s1f13(
            0, // session_id 会在 HSMS 层自动处理
            &self.mdln,
            &self.softrev,
        );

        tracing::info!("GEM: Sending S1F13 Establish Communication Request");

        match self.communicator.send_message_with_reply(s1f13).await {
            Ok(reply) => {
                if reply.header.stream == 1 && reply.header.function == 14 {
                    tracing::info!("GEM: S1F14 received, communication established");
                    self.communication_established = true;
                    true
                } else {
                    tracing::warn!(
                        "GEM: Unexpected reply to S1F13: S{}F{}",
                        reply.header.stream,
                        reply.header.function
                    );
                    false
                }
            }
            Err(e) => {
                tracing::error!("GEM: S1F13 failed: {}", e);
                false
            }
        }
    }

    /// Equipment 模式：在 AttemptOnLine 状态发送 S1F1 并等待 S1F2
    pub async fn attempt_online(&mut self) -> bool {
        let s1f1 = message::build_s1f1(0);

        tracing::info!("GEM: Sending S1F1 Are You There (AttemptOnLine)");

        match self.communicator.send_message_with_reply(s1f1).await {
            Ok(reply) => {
                if reply.header.stream == 1 && reply.header.function == 2 {
                    tracing::info!("GEM: S1F2 received, transitioning to ON-LINE");
                    self.gem_control.handle_event(StateEvent::ReceivedS1F2Event);
                    true
                } else {
                    tracing::warn!("GEM: Unexpected reply to S1F1: S{}F{}", reply.header.stream, reply.header.function);
                    self.gem_control.handle_event(StateEvent::ReceivedS1F0Event);
                    false
                }
            }
            Err(e) => {
                tracing::error!("GEM: S1F1 reply timeout or error: {}", e);
                self.gem_control.handle_event(StateEvent::ReceivedS1F1ReplyTimeoutEvent);
                false
            }
        }
    }

    /// 重置通信状态（TCP 断开时调用）
    pub fn reset(&mut self) {
        self.communication_established = false;
    }
}
