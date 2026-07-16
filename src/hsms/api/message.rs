//! Application-facing SECS message values and inbound reply capabilities.
//!
//! Applications provide only stream, function, and optional SECS-II body.
//! Session IDs, W-bit policy, System Bytes, and inbound classification remain
//! owned by the endpoint and Core.

#![allow(dead_code)]

use crate::{
    hsms::{
        model::ids::{EventSequence, SystemBytes},
        ConnectionGeneration, Function, SessionId, Stream,
    },
    secs2::SecsItem,
};

/// Application-owned primary message content.
///
/// The API operation determines the W-bit: `send` writes W=0 and `request`
/// writes W=1. The application never supplies System Bytes or an HSMS header.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimaryMessage {
    /// Seven-bit SECS stream number supplied by the application or peer.
    stream: Stream,
    /// SECS primary function number.
    function: Function,
    /// Decoded Message Text, or `None` when the HSMS Data message has no text.
    body: Option<SecsItem>,
}

impl PrimaryMessage {
    /// Creates application-owned primary content from `stream`, `function`,
    /// and optional decoded `body`.
    ///
    /// The eventual `send` or `request` operation supplies the W-bit and the
    /// protocol layer supplies Session ID and System Bytes.
    #[must_use]
    pub const fn new(stream: Stream, function: Function, body: Option<SecsItem>) -> Self {
        Self {
            stream,
            function,
            body,
        }
    }

    #[must_use]
    /// Returns the primary message's seven-bit stream number.
    pub const fn stream(&self) -> Stream {
        self.stream
    }

    #[must_use]
    /// Returns the primary message's function number.
    pub const fn function(&self) -> Function {
        self.function
    }

    #[must_use]
    /// Borrows the decoded SECS-II body, returning `None` for absent text.
    pub const fn body(&self) -> Option<&SecsItem> {
        self.body.as_ref()
    }

    #[must_use]
    /// Consumes the message and returns its optional decoded body.
    pub fn into_body(self) -> Option<SecsItem> {
        self.body
    }
}

/// A validated secondary returned by a pending request.
#[derive(Clone, Debug, PartialEq)]
pub struct SecondaryMessage {
    /// Stream number validated by the pending transaction's response matcher.
    stream: Stream,
    /// Secondary function number validated by the response matcher.
    function: Function,
    /// Decoded Message Text, or `None` when the Secondary has no text.
    body: Option<SecsItem>,
}

impl SecondaryMessage {
    /// Creates a validated Secondary from its matched `stream`, `function`,
    /// and optional decoded `body`; only the Core may call this constructor.
    pub(crate) const fn new(stream: Stream, function: Function, body: Option<SecsItem>) -> Self {
        Self {
            stream,
            function,
            body,
        }
    }

    #[must_use]
    /// Returns the matched Secondary stream number.
    pub const fn stream(&self) -> Stream {
        self.stream
    }

    #[must_use]
    /// Returns the matched Secondary function number.
    pub const fn function(&self) -> Function {
        self.function
    }

    #[must_use]
    /// Borrows the decoded SECS-II body, returning `None` for absent text.
    pub const fn body(&self) -> Option<&SecsItem> {
        self.body.as_ref()
    }

    #[must_use]
    /// Consumes the Secondary and returns its optional decoded body.
    pub fn into_body(self) -> Option<SecsItem> {
        self.body
    }
}

/// Single-use capability for replying to an inbound W=1 primary.
#[derive(Debug)]
pub struct ReplyToken {
    /// TCP incarnation on which the inbound primary was received.
    pub(crate) generation: ConnectionGeneration,
    /// Session ID that the generated Secondary must preserve.
    pub(crate) session_id: SessionId,
    /// Stream number that the generated Secondary must preserve.
    pub(crate) stream: Stream,
    /// Function number the generated Secondary must use.
    pub(crate) reply_function: Function,
    /// System Bytes that correlate the Secondary to its inbound Primary.
    pub(crate) system_bytes: SystemBytes,
}

/// Delivery identity for an inbound W=0 primary.
#[derive(Debug)]
pub struct DataEventToken {
    /// TCP incarnation that produced the unreplyable W=0 event.
    pub(crate) generation: ConnectionGeneration,
    /// Endpoint publication identity assigned to the event.
    pub(crate) sequence: EventSequence,
}

/// Exactly one token kind accompanies an inbound primary.
#[derive(Debug)]
pub enum InboundToken {
    /// Single-use authority to send the Secondary for an inbound W=1 Primary.
    Reply(ReplyToken),
    /// Delivery identity for an inbound W=0 Primary that expects no reply.
    Data(DataEventToken),
}

/// An inbound primary classified by the Core.
#[derive(Debug)]
pub struct InboundPrimary {
    /// Primary content decoded and classified by the Core.
    message: PrimaryMessage,
    /// Exactly one capability matching the inbound W-bit.
    token: InboundToken,
}

impl InboundPrimary {
    /// Combines classified primary `message` content with its exclusive
    /// inbound `token`; only the Core may construct this value.
    pub(crate) const fn new(message: PrimaryMessage, token: InboundToken) -> Self {
        Self { message, token }
    }

    #[must_use]
    /// Borrows the classified primary message content.
    pub const fn message(&self) -> &PrimaryMessage {
        &self.message
    }

    #[must_use]
    /// Borrows the reply capability or W=0 delivery identity.
    pub const fn token(&self) -> &InboundToken {
        &self.token
    }

    #[must_use]
    /// Consumes the event and returns its message and exclusive token.
    pub fn into_parts(self) -> (PrimaryMessage, InboundToken) {
        (self.message, self.token)
    }
}
