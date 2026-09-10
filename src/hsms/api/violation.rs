//! Reliable application context for malformed inbound protocol frames.
//! Reports preserve exact received headers and structured decoder causes so an
//! application can diagnose or construct its own S9 response without owning wire IDs.

use super::MessageContext;
pub use crate::hsms::protocol::violation::{HeaderViolationKind, PayloadViolationKind};
use crate::secs2::codec::DecodeError;

/// Stable category for a complete but invalid inbound HSMS frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboundViolationKind {
    /// Invalid or unsupported HSMS header fields, in validator priority order.
    Header(HeaderViolationKind),
    /// Invalid SECS-II Message Text or a configured decoder resource bound.
    Payload(PayloadViolationKind),
}

/// Owned reliable error report associated with one complete received frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboundProtocolError {
    /// Exact received header and originating connection generation.
    context: MessageContext,
    /// Stable classification used for protocol and application decisions.
    kind: InboundViolationKind,
    /// Detailed SECS-II decoding error; absent for pure header errors.
    source: Option<DecodeError>,
}

impl InboundProtocolError {
    /// Combines captured context, validator classification and optional codec cause.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) const fn new(
        context: MessageContext,
        kind: InboundViolationKind,
        source: Option<DecodeError>,
    ) -> Self {
        Self {
            context,
            kind,
            source,
        }
    }

    /// Borrows the received header and originating generation.
    pub const fn context(&self) -> &MessageContext {
        &self.context
    }

    /// Returns the stable error classification without losing decoder detail.
    pub const fn kind(&self) -> InboundViolationKind {
        self.kind
    }

    /// Borrows the detailed SECS-II error, when Message Text decoding failed.
    pub const fn decode_error(&self) -> Option<&DecodeError> {
        self.source.as_ref()
    }
}
