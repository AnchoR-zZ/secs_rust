//! Defines immutable application message values and inbound reply capabilities.
//!
//! Applications provide only stream, function, and optional SECS-II body.
//! Session IDs, W-bit policy, System Bytes, and inbound classification remain
//! owned by the endpoint and Core.

#![allow(dead_code)]

use std::fmt;

use crate::{
    hsms::model::ids::{ConnectionGeneration, Function, ReplyCapabilityId, Stream},
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

/// Single-use capability for responding to an inbound W=1 Primary.
#[must_use = "reply, abort, or explicitly abandon this inbound reply capability"]
pub struct ReplyToken {
    /// Opaque identity used to consume this reply authority exactly once.
    capability_id: ReplyCapabilityId,
    /// TCP incarnation on which the inbound primary was received.
    generation: ConnectionGeneration,
    /// Private admission hint; the Core ledger remains authoritative.
    normal_secondary_available: bool,
}

impl ReplyToken {
    /// Creates the single-use reply authority identified by `capability_id`.
    ///
    /// `normal_secondary_available` lets admission return an abort-only token
    /// before ownership transfer. The Core ledger validates the mode again;
    /// header correlation remains exclusively in that ledger.
    pub(crate) const fn new(
        capability_id: ReplyCapabilityId,
        generation: ConnectionGeneration,
        normal_secondary_available: bool,
    ) -> Self {
        Self {
            capability_id,
            generation,
            normal_secondary_available,
        }
    }

    /// Returns the identity used to consume this authority exactly once.
    pub(crate) const fn capability_id(&self) -> ReplyCapabilityId {
        self.capability_id
    }

    /// Returns the TCP generation on which the primary arrived.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the private pre-Core admission hint for a normal Secondary.
    ///
    /// This hint is deliberately crate-private: applications receive opaque
    /// authority and choose reply, abort, or abandon without observing Core's
    /// authoritative reply-contract classification.
    pub(crate) const fn normal_secondary_available(&self) -> bool {
        self.normal_secondary_available
    }
}

impl fmt::Debug for ReplyToken {
    /// Formats the opaque reply capability without exposing raw transaction
    /// header fields or System Bytes through the public API.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplyToken")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Opaque marker for an inbound W=0 primary that carries no reply authority.
///
/// The surrounding [`crate::hsms::EndpointEventEnvelope`] carries publication
/// sequence and generation metadata. This marker is constructed only inside
/// the crate so applications cannot fabricate Core-classified inbound data.
pub struct DataEventToken {
    /// Private zero-sized field that prevents application construction.
    private: (),
}

impl DataEventToken {
    /// Creates an opaque marker for a Core-classified inbound W=0 primary.
    ///
    /// The returned marker contains no generation or publication identity;
    /// those values belong to the endpoint event envelope.
    pub(crate) const fn new() -> Self {
        Self { private: () }
    }
}

impl fmt::Debug for DataEventToken {
    /// Formats the marker without exposing its private representation.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DataEventToken")
            .finish_non_exhaustive()
    }
}

/// Exactly one token kind accompanies an inbound primary.
#[derive(Debug)]
pub enum InboundToken {
    /// Single-use authority to send the Secondary for an inbound W=1 Primary.
    Reply(ReplyToken),
    /// Opaque marker for an inbound W=0 Primary that expects no reply.
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
    /// Borrows the reply capability or opaque W=0 marker.
    pub const fn token(&self) -> &InboundToken {
        &self.token
    }

    #[must_use]
    /// Consumes the event and returns its message and exclusive token.
    pub fn into_parts(self) -> (PrimaryMessage, InboundToken) {
        (self.message, self.token)
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::model::ids::{ConnectionGeneration, ReplyCapabilityId};

    use super::{DataEventToken, ReplyToken};

    /// Verifies that the public marker's debug form exposes no private state.
    #[test]
    fn data_event_token_debug_is_opaque() {
        let token = DataEventToken::new();

        assert_eq!(format!("{token:?}"), "DataEventToken { .. }");
    }

    /// Confirms public token diagnostics expose only generation while keeping
    /// capability identity and private reply-contract hints hidden.
    #[test]
    fn reply_token_debug_hides_capability_identity() {
        let token = ReplyToken::new(
            ReplyCapabilityId::new(123_456),
            ConnectionGeneration::new(7),
            false,
        );
        let debug = format!("{token:?}");

        assert!(!token.normal_secondary_available());
        assert!(debug.contains("generation"));
        assert!(!debug.contains("normal_secondary_available"));
        assert!(!debug.contains("123456"));
    }
}
