//! Public application messages and single-use inbound reply capabilities.
//!
//! Applications provide SECS stream, function, and optional Message Text.
//! Session IDs, W-bit policy, System Bytes, and capability allocation remain
//! owned by the future protocol core.

// Internal constructors become production-reachable with the future endpoint runtime.
#![allow(dead_code)]

use std::{fmt, sync::Arc};

use crate::{
    hsms::model::ids::{ConnectionGeneration, Function, ReplyCapabilityId, Stream},
    secs2::SecsItem,
};

/// Application-owned primary message content.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimaryMessage {
    /// Seven-bit SECS stream number supplied by the application or peer.
    stream: Stream,
    /// SECS primary function number.
    function: Function,
    /// Decoded Message Text, or `None` when no text is present.
    body: Option<SecsItem>,
}

impl PrimaryMessage {
    /// Creates primary content from its stream, function, and optional body.
    #[must_use]
    pub const fn new(stream: Stream, function: Function, body: Option<SecsItem>) -> Self {
        Self {
            stream,
            function,
            body,
        }
    }

    /// Returns the primary message's stream.
    #[must_use]
    pub const fn stream(&self) -> Stream {
        self.stream
    }

    /// Returns the primary message's function.
    #[must_use]
    pub const fn function(&self) -> Function {
        self.function
    }

    /// Borrows the decoded body, returning `None` for absent Message Text.
    #[must_use]
    pub const fn body(&self) -> Option<&SecsItem> {
        self.body.as_ref()
    }

    /// Consumes the message and returns its optional decoded body.
    #[must_use]
    pub fn into_body(self) -> Option<SecsItem> {
        self.body
    }
}

/// A validated secondary returned by a pending request.
#[derive(Clone, Debug, PartialEq)]
pub struct SecondaryMessage {
    /// Stream validated by the pending transaction matcher.
    stream: Stream,
    /// Secondary function validated by the matcher.
    function: Function,
    /// Decoded Message Text, or `None` when absent.
    body: Option<SecsItem>,
}

impl SecondaryMessage {
    /// Creates a Secondary after the protocol core validates its transaction.
    pub(crate) const fn new(stream: Stream, function: Function, body: Option<SecsItem>) -> Self {
        Self {
            stream,
            function,
            body,
        }
    }

    /// Returns the matched stream number.
    #[must_use]
    pub const fn stream(&self) -> Stream {
        self.stream
    }

    /// Returns the matched function number.
    #[must_use]
    pub const fn function(&self) -> Function {
        self.function
    }

    /// Borrows the decoded body, returning `None` when absent.
    #[must_use]
    pub const fn body(&self) -> Option<&SecsItem> {
        self.body.as_ref()
    }

    /// Consumes the Secondary and returns its optional body.
    #[must_use]
    pub fn into_body(self) -> Option<SecsItem> {
        self.body
    }
}

/// Single-use authority for responding to one inbound W=1 Primary.
#[must_use = "reply, abort, or explicitly abandon this inbound reply capability"]
pub struct ReplyToken {
    /// Private owner identity because tokens cross the application boundary.
    owner: Arc<()>,
    /// Monotonic identity used to consume this capability exactly once.
    capability_id: ReplyCapabilityId,
    /// TCP incarnation on which the Primary arrived.
    generation: ConnectionGeneration,
    /// Admission hint indicating whether a normal F+1 reply is representable.
    normal_secondary_available: bool,
}

impl ReplyToken {
    /// Creates a token for one core-owned reply capability.
    pub(crate) fn from_core(
        owner: Arc<()>,
        capability_id: ReplyCapabilityId,
        generation: ConnectionGeneration,
        normal_secondary_available: bool,
    ) -> Self {
        Self {
            owner,
            capability_id,
            generation,
            normal_secondary_available,
        }
    }

    /// Creates an isolated token for API admission tests.
    #[cfg(test)]
    pub(crate) fn for_test(
        capability_id: ReplyCapabilityId,
        generation: ConnectionGeneration,
        normal_secondary_available: bool,
    ) -> Self {
        Self::from_core(
            Arc::new(()),
            capability_id,
            generation,
            normal_secondary_available,
        )
    }

    /// Returns the pre-admission normal-Secondary capability hint.
    pub(crate) const fn normal_secondary_available(&self) -> bool {
        self.normal_secondary_available
    }

    /// Consumes the token into the minimal fields validated by the protocol core.
    pub(crate) fn into_claim(self) -> (Arc<()>, ReplyCapabilityId, ConnectionGeneration, bool) {
        (
            self.owner,
            self.capability_id,
            self.generation,
            self.normal_secondary_available,
        )
    }
}

impl fmt::Debug for ReplyToken {
    /// Formats only the generation and keeps capability identity opaque.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplyToken")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

/// Opaque marker for an inbound W=0 Primary with no reply authority.
pub struct DataEventToken {
    /// Private field preventing application construction.
    private: (),
}

impl DataEventToken {
    /// Creates a marker after the Core classifies an inbound W=0 Primary.
    pub(crate) const fn new() -> Self {
        Self { private: () }
    }
}

impl fmt::Debug for DataEventToken {
    /// Formats the marker without exposing its representation.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DataEventToken")
            .finish_non_exhaustive()
    }
}

/// Exactly one token kind accompanying an inbound Primary.
#[derive(Debug)]
pub enum InboundToken {
    /// Single-use authority for an inbound W=1 Primary.
    Reply(ReplyToken),
    /// Opaque W=0 marker that carries no reply authority.
    Data(DataEventToken),
}

/// An inbound Primary classified by the protocol core.
#[derive(Debug)]
pub struct InboundPrimary {
    /// Decoded application message content.
    message: PrimaryMessage,
    /// Reply capability or W=0 marker matching the inbound W-bit.
    token: InboundToken,
}

impl InboundPrimary {
    /// Combines classified Primary content with its exclusive token.
    pub(crate) const fn new(message: PrimaryMessage, token: InboundToken) -> Self {
        Self { message, token }
    }

    /// Borrows the classified Primary content.
    #[must_use]
    pub const fn message(&self) -> &PrimaryMessage {
        &self.message
    }

    /// Borrows the reply capability or W=0 marker.
    #[must_use]
    pub const fn token(&self) -> &InboundToken {
        &self.token
    }

    /// Consumes the event into its message and exclusive token.
    #[must_use]
    pub fn into_parts(self) -> (PrimaryMessage, InboundToken) {
        (self.message, self.token)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::hsms::model::ids::{ConnectionGeneration, ReplyCapabilityId};

    use super::{DataEventToken, ReplyToken};

    /// Confirms opaque inbound tokens do not reveal correlation identities.
    #[test]
    fn token_debug_output_is_opaque() {
        let data = DataEventToken::new();
        let reply = ReplyToken::from_core(
            Arc::new(()),
            ReplyCapabilityId::new(123_456),
            ConnectionGeneration::new(7),
            false,
        );

        assert_eq!(format!("{data:?}"), "DataEventToken { .. }");
        let debug = format!("{reply:?}");
        assert!(debug.contains("generation"));
        assert!(!debug.contains("123456"));
        assert!(!debug.contains("normal_secondary_available"));
    }
}
