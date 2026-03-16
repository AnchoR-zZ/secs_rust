pub mod gem_state;
pub mod config;
pub mod message;
pub mod communicator;
pub(crate) mod manager;
pub(crate) mod session;

pub use gem_state::DeviceState;

use crate::hsms::message::HsmsMessage;
use crate::hsms::HsmsError;
use thiserror::Error;
use tokio::sync::oneshot;

// ============================================================================
// GemError — GEM 层错误类型
// ============================================================================

#[derive(Debug, Error)]
pub enum GemError {
    #[error("GEM HSMS error: {0}")]
    Hsms(#[from] HsmsError),

    #[error("GEM invalid state: {message}")]
    InvalidState { message: String },

    #[error("GEM communication not established (S1F13/F14 handshake incomplete)")]
    CommunicationNotEstablished,

    #[error("GEM channel closed: {op}")]
    ChannelClosed { op: &'static str },

    #[error("GEM timeout: {kind}")]
    Timeout { kind: &'static str },

    #[error("GEM reply dropped: {message}")]
    ReplyDropped { message: String },
}

// ============================================================================
// GemCommand — Communicator → Manager 的命令通道消息
// ============================================================================

#[derive(Debug)]
pub enum GemCommand {
    /// 操作员请求上线（触发状态机 #3 事件）
    OperatorOnline {
        reply_tx: oneshot::Sender<Result<(), GemError>>,
    },

    /// 操作员请求下线（触发状态机 #6/#12 事件）
    OperatorOffline {
        reply_tx: oneshot::Sender<Result<(), GemError>>,
    },

    /// 切换为 Local 模式
    SetLocal {
        reply_tx: oneshot::Sender<Result<(), GemError>>,
    },

    /// 切换为 Remote 模式
    SetRemote {
        reply_tx: oneshot::Sender<Result<(), GemError>>,
    },

    /// 透传 SECS-II 数据消息（fire-and-forget）
    SendMessage { msg: HsmsMessage },

    /// 透传 SECS-II 数据消息并等待回复
    SendMessageWithReply {
        msg: HsmsMessage,
        reply_tx: oneshot::Sender<Result<HsmsMessage, GemError>>,
    },

    /// 透传回复消息
    SendReply { msg: HsmsMessage },

    /// 关闭 GEM 层
    Shutdown {
        reply_tx: oneshot::Sender<Result<(), GemError>>,
    },
}