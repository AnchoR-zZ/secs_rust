//! Public application facade for neutral HSMS boundary contracts.
//!
//! `HsmsHandle` and `HsmsRuntime` are implemented in Wave 2A after admission
//! and lifecycle semantics are available.

mod reply;

pub use crate::hsms::contracts::{
    ConnectionCloseReason, EndpointEvent, EndpointEventEnvelope, PeerRejectDisposition,
    PeerRejectNotice, ProtocolNotice, SendReceipt,
};
pub use crate::hsms::contracts::{
    ControlIntent, DataEventToken, InboundPrimary, InboundToken, PrimaryMessage, ReplyToken,
    SecondaryMessage,
};
pub use reply::{ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent};
