//! HSMS-SS public values and internal component boundaries.

pub mod api;
#[cfg(any(feature = "runtime-tokio", test))]
mod codec;
pub mod config;
#[cfg(any(feature = "runtime-tokio", test))]
mod core;
pub mod error;
#[cfg(any(feature = "runtime-tokio", test))]
mod generation;
pub mod lifecycle;
mod model;
#[cfg(any(feature = "runtime-tokio", test))]
mod profile;
mod protocol;
#[cfg(feature = "runtime-tokio")]
mod scheduling;
#[cfg(feature = "runtime-tokio")]
mod supervisor;
#[cfg(any(feature = "runtime-tokio", test))]
mod wire;

#[cfg(feature = "runtime-tokio")]
pub mod endpoint;
#[cfg(feature = "runtime-tokio")]
pub use endpoint::{DiagnosticEvent, DiagnosticReceiver, DiagnosticRecord};
#[cfg(feature = "runtime-tokio")]
pub use endpoint::{
    EndpointError, HsmsEndpoint, HsmsHandle, HsmsRuntime, MessageError, StartReceipt,
};
#[cfg(feature = "runtime-tokio")]
pub use endpoint::{HsmsReceiver, PrimaryReceiver, ProtocolErrorReceiver, ReplyError};

pub use api::{
    ConnectionCloseReason, ControlIntent, DataEventToken, InboundPrimary, InboundToken,
    MessageContext, PeerRejectDisposition, PeerRejectNotice, PrimaryMessage, ProtocolNotice,
    ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent, ReplyToken, SecondaryMessage,
    SendReceipt,
};
pub use api::{
    HeaderViolationKind, InboundProtocolError, InboundViolationKind, PayloadViolationKind,
};
pub use config::{ConnectionMode, EndpointConfig, EndpointLimits, HsmsTimeouts, RuntimePolicy};
pub use error::{ConfigError, IdentifierError, OperationError, ProtocolError, TimeoutKind};
pub use lifecycle::{
    ConnectionExitReport, EndpointPhase, EndpointStateSnapshot, GenerationSlotSnapshot,
    RunningIntent, SessionState,
};
pub use model::ids::{ConnectionGeneration, Function, SessionId, Stream};
pub use protocol::header::RejectReason;
