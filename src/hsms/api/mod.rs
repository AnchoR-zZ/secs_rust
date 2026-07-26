//! Application-facing messages and endpoint boundary values.
//!
//! `HsmsHandle` and `HsmsRuntime` are implemented in Wave 2A after admission
//! and lifecycle semantics are available.

pub(crate) mod command;
pub(crate) mod completion;
mod event;
mod message;

pub use command::ControlIntent;
pub use completion::SendReceipt;
pub use event::{ConnectionCloseReason, EndpointEvent, EndpointEventEnvelope, ProtocolNotice};
pub use message::{
    DataEventToken, InboundPrimary, InboundToken, PrimaryMessage, ReplyToken, SecondaryMessage,
};

pub(crate) use command::ProtocolCommand;
/// Crate-only completion seam shared by Core and its runtime result router.
#[allow(unused_imports)]
pub(crate) use completion::{CommandCompletion, CompletionValue};
