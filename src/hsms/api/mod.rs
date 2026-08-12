//! Public application-owned HSMS values and endpoint event contracts.
//!
//! Runtime-internal Core and writer protocols deliberately do not live here.
//! The eventual endpoint handle will consume these values without exposing
//! protocol headers, System Bytes, or internal resource ownership.

mod command;
mod completion;
mod event;
mod message;
mod reply;

pub use command::ControlIntent;
pub use completion::SendReceipt;
pub use event::{
    ConnectionCloseReason, EndpointEvent, EndpointEventEnvelope, PeerRejectDisposition,
    PeerRejectNotice, ProtocolNotice,
};
pub use message::{
    DataEventToken, InboundPrimary, InboundToken, PrimaryMessage, ReplyToken, SecondaryMessage,
};
pub use reply::{ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent};
