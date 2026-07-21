//! A greenfield HSMS-SS and SECS-II protocol library.
//!
//! Wave 0 freezes value types and component boundaries. Protocol, transport,
//! and asynchronous runtime behavior are implemented by later waves.

pub mod hsms;
pub mod secs2;
pub mod sml;

pub use hsms::{
    ConfigError, ConnectionCloseReason, ConnectionGeneration, ConnectionMode, ControlIntent,
    DataEventToken, EndpointConfig, EndpointEvent, EndpointEventEnvelope, EndpointLimits,
    EndpointPhase, EndpointStateSnapshot, Function, GenerationSlotSnapshot, HsmsTimeouts,
    IdentifierError, InboundPrimary, InboundToken, OperationError, PrimaryMessage, ProtocolError,
    ProtocolNotice, ReplyToken, RunningIntent, SecondaryMessage, SendReceipt, SessionId,
    SessionState, Stream, TimeoutKind,
};
pub use secs2::{
    AsciiString, DecodeLimits, LocalizedEncodingCode, LocalizedString, SecsItem, SecsItemError,
};
