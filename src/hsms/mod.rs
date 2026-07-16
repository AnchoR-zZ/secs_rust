//! HSMS-SS public values and internal component boundaries.

mod admission;
pub mod api;
pub mod config;
mod core;
pub mod error;
mod generation;
pub mod lifecycle;
mod model;
mod profile;
mod supervisor;
mod wire;

pub use api::{
    ConnectionCloseReason, ControlIntent, DataEventToken, EndpointEvent, EndpointEventEnvelope,
    InboundPrimary, InboundToken, PrimaryMessage, ProtocolNotice, ReplyToken, SecondaryMessage,
    SendReceipt,
};
pub use config::{ConnectionMode, EndpointConfig, EndpointLimits, HsmsTimeouts};
pub use error::{ConfigError, IdentifierError, OperationError, ProtocolError, TimeoutKind};
pub use lifecycle::{
    EndpointPhase, EndpointStateSnapshot, GenerationSlotSnapshot, RunningIntent, SessionState,
};
pub use model::ids::{ConnectionGeneration, Function, SessionId, Stream};
