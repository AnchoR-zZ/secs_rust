//! Neutral value contracts shared by the public API, Core, and generation runtime.
//!
//! This private module owns data-only boundaries and depends only on stable
//! HSMS leaf modules. Public application values are re-exported through
//! [`crate::hsms::api`]; runtime contracts remain crate-private.

#![allow(dead_code)]

mod command;
mod completion;
mod core_effect;
mod core_input;
mod endpoint_event;
mod message;
mod orchestration;
mod write;

pub use command::ControlIntent;
pub use completion::SendReceipt;
pub use endpoint_event::{
    ConnectionCloseReason, EndpointEvent, EndpointEventEnvelope, PeerRejectDisposition,
    PeerRejectNotice, ProtocolNotice,
};
pub use message::{
    DataEventToken, InboundPrimary, InboundToken, PrimaryMessage, ReplyToken, SecondaryMessage,
};
#[allow(unused_imports)]
pub(crate) use message::{ReplyTokenClaim, ReplyTokenIssuer, ValidatedReplyTokenClaim};

#[allow(unused_imports)]
pub(crate) use command::CoreCommand;
#[allow(unused_imports)]
pub(crate) use completion::{CoreCommandCompletion, CoreCompletionValue};
#[allow(unused_imports)]
pub(crate) use core_effect::CoreEffect;
#[allow(unused_imports)]
pub(crate) use core_input::{ApplicationDeliveryResult, CoreEvent, CoreInput, ShutdownKind};
#[allow(unused_imports)]
pub(crate) use orchestration::{
    DeliveryPurpose, OperationOwner, OutboundCorrelationState, OutboundHeaderIdentity,
    OutboundRole, RejectCorrelationEligibility, RejectReference, RejectSelector,
};
#[allow(unused_imports)]
pub(crate) use write::{DataGateState, PeerResponseCommit, ScheduleFailure, WriteClass};
