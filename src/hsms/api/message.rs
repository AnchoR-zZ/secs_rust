//! Public application messages and single-use inbound reply capabilities.
//!
//! Applications provide SECS stream, function, and optional Message Text.
//! Session IDs, W-bit policy, System Bytes, and capability allocation remain
//! owned by the protocol core and its single-owner Driver.

#[cfg(any(feature = "runtime-tokio", test))]
use crate::hsms::model::ids::ReplyCapabilityId;
use std::fmt;
#[cfg(any(feature = "runtime-tokio", test))]
use std::sync::Arc;

use crate::{
    hsms::model::ids::{ConnectionGeneration, Function, Stream},
    secs2::SecsItem,
};

pub use crate::secs2::PrimaryMessage;

/// Immutable protocol-header context for diagnostics and application S9 content.
/// It confers no authority to choose headers for outbound protocol operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageContext {
    /// TCP incarnation on which this header was received or its request committed.
    generation: ConnectionGeneration,
    /// Ten header bytes in wire order, excluding the four-byte length prefix.
    header: [u8; 10],
}

impl MessageContext {
    /// Preserves captured header bytes, including structurally invalid fields.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) const fn from_header(generation: ConnectionGeneration, header: [u8; 10]) -> Self {
        Self { generation, header }
    }
    /// Preserves the unique wire representation of a validated Data header.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) fn from_data(
        generation: ConnectionGeneration,
        header: crate::hsms::protocol::header::DataHeader,
    ) -> Self {
        let session = header.session_id().get().to_be_bytes();
        let system = header.system_bytes().get().to_be_bytes();
        Self {
            generation,
            header: [
                session[0],
                session[1],
                header.stream().get() | if header.reply_expected() { 0x80 } else { 0 },
                header.function().get(),
                0,
                0,
                system[0],
                system[1],
                system[2],
                system[3],
            ],
        }
    }

    /// Returns the captured ten-byte header for diagnostic/MHEAD construction.
    pub const fn header(&self) -> &[u8; 10] {
        &self.header
    }

    /// Returns the originating connection generation for correlation.
    pub const fn generation(self) -> ConnectionGeneration {
        self.generation
    }
}

/// A validated secondary returned by a pending request.
#[derive(Clone, Debug, PartialEq)]
pub struct SecondaryMessage {
    /// Exact matched response header and originating connection generation.
    context: MessageContext,
    /// Stream validated by the pending transaction matcher.
    stream: Stream,
    /// Secondary function validated by the matcher.
    function: Function,
    /// Decoded Message Text, or `None` when absent.
    body: Option<SecsItem>,
}

impl SecondaryMessage {
    /// Creates a Secondary from fields validated by the protocol core's
    /// response matcher and transferred through the Driver completion path.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) const fn new(
        stream: Stream,
        function: Function,
        body: Option<SecsItem>,
        context: MessageContext,
    ) -> Self {
        Self {
            context,
            stream,
            function,
            body,
        }
    }

    /// Borrows the immutable header context of this matched response.
    pub const fn context(&self) -> &MessageContext {
        &self.context
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
    #[cfg(any(feature = "runtime-tokio", test))]
    owner: Arc<()>,
    /// Monotonic identity used to consume this capability exactly once.
    #[cfg(any(feature = "runtime-tokio", test))]
    capability_id: ReplyCapabilityId,
    /// TCP incarnation on which the Primary arrived.
    generation: ConnectionGeneration,
    /// Admission hint indicating whether a normal F+1 reply is representable.
    #[cfg(any(feature = "runtime-tokio", test))]
    normal_secondary_available: bool,
}

impl ReplyToken {
    /// Verifies immutable routing identity without consuming the capability.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) fn belongs_to(&self, owner: &Arc<()>, generation: ConnectionGeneration) -> bool {
        Arc::ptr_eq(&self.owner, owner) && self.generation == generation
    }
    /// Creates a token for one core-owned reply capability.
    #[cfg(any(feature = "runtime-tokio", test))]
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
    #[cfg(any(feature = "runtime-tokio", test))]
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
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) const fn normal_secondary_available(&self) -> bool {
        self.normal_secondary_available
    }

    /// Consumes the token into fields required for protocol admission.
    #[cfg(any(feature = "runtime-tokio", test))]
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
    _private: (),
}

impl DataEventToken {
    /// Creates a marker after the Core classifies an inbound W=0 Primary.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) const fn new() -> Self {
        Self { _private: () }
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
    /// Immutable header and generation associated with the received content.
    context: MessageContext,
}

impl InboundPrimary {
    /// Combines classified Primary content with its exclusive token.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) const fn new(
        message: PrimaryMessage,
        token: InboundToken,
        context: MessageContext,
    ) -> Self {
        Self {
            message,
            token,
            context,
        }
    }

    /// Borrows immutable received-header and connection context.
    pub const fn context(&self) -> &MessageContext {
        &self.context
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

    use crate::{
        hsms::model::ids::{ConnectionGeneration, Function, ReplyCapabilityId, Stream},
        secs2::SecsItem,
    };

    use super::{DataEventToken, PrimaryMessage, ReplyToken, SecondaryMessage};

    /// Creates the fixed stream used by API message ownership tests.
    fn stream() -> Stream {
        Stream::new(7).expect("fixture stream is valid")
    }

    /// Confirms Driver translation can consume a Primary without cloning and
    /// retains absent Message Text as an explicit `None` value.
    #[test]
    fn primary_into_parts_transfers_owned_fields_without_body() {
        let message = PrimaryMessage::new(stream(), Function::new(1), None);

        let (actual_stream, function, body) = message.into_parts();

        assert_eq!(actual_stream, stream());
        assert_eq!(function, Function::new(1));
        assert_eq!(body, None);
    }

    /// Confirms consuming Primary translation and crate-private Secondary
    /// construction preserve typed-empty Message Text rather than merging it
    /// with absent text.
    #[test]
    fn data_message_parts_preserve_typed_empty_body() {
        let body = Some(SecsItem::List(Vec::new()));
        let primary = PrimaryMessage::new(stream(), Function::new(3), body.clone());

        let (actual_stream, function, transferred_body) = primary.into_parts();
        assert_eq!(actual_stream, stream());
        assert_eq!(function, Function::new(3));
        assert_eq!(transferred_body, body);

        let secondary = SecondaryMessage::new(
            stream(),
            Function::new(4),
            transferred_body,
            super::MessageContext::from_header(
                ConnectionGeneration::new(3),
                [0, 7, 7, 4, 0, 0, 0, 0, 0, 1],
            ),
        );
        assert_eq!(secondary.stream(), stream());
        assert_eq!(secondary.function(), Function::new(4));
        assert_eq!(secondary.into_body(), body);
    }

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
