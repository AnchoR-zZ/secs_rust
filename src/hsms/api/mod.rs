//! Public application-owned HSMS values and endpoint event contracts.
//!
//! Runtime-internal Core and writer protocols deliberately do not live here.
//! Endpoint handles consume these values without exposing
//! protocol headers, System Bytes, or internal resource ownership.

mod command;
mod completion;
mod event;
mod message;
mod reply;
mod violation;
pub use violation::{
    HeaderViolationKind, InboundProtocolError, InboundViolationKind, PayloadViolationKind,
};

pub use command::ControlIntent;
pub use completion::SendReceipt;
pub use event::{ConnectionCloseReason, PeerRejectDisposition, PeerRejectNotice, ProtocolNotice};
pub use message::{
    DataEventToken, InboundPrimary, InboundToken, MessageContext, PrimaryMessage, ReplyToken,
    SecondaryMessage,
};
pub use reply::{ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent};
