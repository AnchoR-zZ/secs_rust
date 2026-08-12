//! HSMS-SS public values and internal component boundaries.

pub mod api;
mod codec;
pub mod config;
pub mod error;
mod generation;
pub mod lifecycle;
mod model;
mod profile;
mod protocol;
mod supervisor;
mod wire;

pub use api::{
    ConnectionCloseReason, ControlIntent, DataEventToken, EndpointEvent, EndpointEventEnvelope,
    InboundPrimary, InboundToken, PeerRejectDisposition, PeerRejectNotice, PrimaryMessage,
    ProtocolNotice, ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent, ReplyToken,
    SecondaryMessage, SendReceipt,
};
pub use config::{ConnectionMode, EndpointConfig, EndpointLimits, HsmsTimeouts};
pub use error::{ConfigError, IdentifierError, OperationError, ProtocolError, TimeoutKind};
pub use lifecycle::{
    EndpointPhase, EndpointStateSnapshot, GenerationSlotSnapshot, RunningIntent, SessionState,
};
pub use model::ids::{ConnectionGeneration, Function, SessionId, Stream};
pub use protocol::header::RejectReason;
