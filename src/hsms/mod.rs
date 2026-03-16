use crate::hsms::message::HsmsMessage;
use serde::Serialize;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::oneshot;

pub mod communicator;
pub mod config;
pub(crate) mod manager;
pub mod message;
pub(crate) mod session;
pub(crate) mod stream_util;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
pub enum ConnectionState {
    NotConnected,
    NotSelected,
    Selected,
}

#[derive(Debug, Error)]
pub enum HsmsError {
    #[error("HSMS io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("HSMS channel closed: {op}")]
    ChannelClosed { op: &'static str },
    #[error("HSMS timeout {kind} after {duration:?}")]
    Timeout { kind: &'static str, duration: Duration },
    #[error("HSMS invalid state: expected {expected:?}, got {actual:?}")]
    InvalidState {
        expected: ConnectionState,
        actual: ConnectionState,
    },
    #[error("HSMS protocol error: {message}")]
    Protocol { message: String },
    #[error("HSMS reply dropped: {message}")]
    ReplyDropped { message: String },
}

#[derive(Debug)]
pub enum HsmsCommand {
    SendMessage {
        msg: HsmsMessage,
    },

    SendMessageNeedReply {
        msg: HsmsMessage,
        reply_tx: oneshot::Sender<Result<HsmsMessage, HsmsError>>,
    },

    SendReply {
        msg: HsmsMessage,
    },

    Select {
        reply_tx: oneshot::Sender<Result<HsmsMessage, HsmsError>>,
    },
    NotSelect {
        reply_tx: oneshot::Sender<Result<HsmsMessage, HsmsError>>,
    },
    NotConnect {
        reply_tx: oneshot::Sender<Result<HsmsMessage, HsmsError>>,
    },
    Shutdown {
        reply_tx: oneshot::Sender<Result<(), HsmsError>>,
    },
}
