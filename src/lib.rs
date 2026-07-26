//! A greenfield HSMS-SS and SECS-II protocol library.
//!
//! The crate currently provides strict SECS-II binary encoding/decoding plus
//! frozen HSMS value types and component boundaries. HSMS protocol state
//! machines, transport integration, and asynchronous runtime behavior belong
//! to later implementation waves.

pub mod hsms;
pub mod secs2;
pub mod sml;

pub use hsms::{
    ConfigError, ConnectionCloseReason, ConnectionGeneration, ConnectionMode, ControlIntent,
    DataEventToken, EndpointConfig, EndpointEvent, EndpointEventEnvelope, EndpointLimits,
    EndpointPhase, EndpointStateSnapshot, Function, GenerationSlotSnapshot, HsmsTimeouts,
    IdentifierError, InboundPrimary, InboundToken, OperationError, PeerRejectDisposition,
    PeerRejectNotice, PrimaryMessage, ProtocolError, ProtocolNotice, RejectReason,
    ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent, ReplyToken, RunningIntent,
    SecondaryMessage, SendReceipt, SessionId, SessionState, Stream, TimeoutKind,
};
pub use secs2::{
    AsciiString, DecodeLimits, LocalizedEncodingCode, LocalizedString, SecsItem, SecsItemError,
};
