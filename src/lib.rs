//! A greenfield HSMS-SS and SECS-II protocol library.
//!
//! SECS-II and SML codecs can be used independently of Tokio. The optional Tokio
//! runtime exposes endpoint lifecycle, HSMS control, Send/Request, bounded inbound
//! delivery and single-use replies, backed by a deterministic protocol core and
//! supervised TCP generations. Release validation and full diagnostics are ongoing.

pub mod hsms;
pub mod secs2;
pub mod sml;
pub use hsms::RuntimePolicy;
#[cfg(feature = "runtime-tokio")]
pub use hsms::{DiagnosticEvent, DiagnosticReceiver, DiagnosticRecord};
#[cfg(feature = "runtime-tokio")]
pub use hsms::{EndpointError, HsmsEndpoint, HsmsHandle, HsmsRuntime, MessageError, StartReceipt};
pub use hsms::{
    HeaderViolationKind, InboundProtocolError, InboundViolationKind, PayloadViolationKind,
};
#[cfg(feature = "runtime-tokio")]
pub use hsms::{HsmsReceiver, PrimaryReceiver, ProtocolErrorReceiver, ReplyError};

pub use hsms::{
    ConfigError, ConnectionCloseReason, ConnectionExitReport, ConnectionGeneration, ConnectionMode,
    ControlIntent, DataEventToken, EndpointConfig, EndpointLimits, EndpointPhase,
    EndpointStateSnapshot, Function, GenerationSlotSnapshot, HsmsTimeouts, IdentifierError,
    InboundPrimary, InboundToken, MessageContext, OperationError, PeerRejectDisposition,
    PeerRejectNotice, PrimaryMessage, ProtocolError, ProtocolNotice, RejectReason,
    ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent, ReplyToken, RunningIntent,
    SecondaryMessage, SendReceipt, SessionId, SessionState, Stream, TimeoutKind,
};
pub use secs2::{
    AsciiString, DecodeLimits, LocalizedEncodingCode, LocalizedString, SecsItem, SecsItemError,
    StreamError,
};
