//! Defines immutable application message values and inbound reply capabilities.
//!
//! Applications provide only stream, function, and optional SECS-II body.
//! Session IDs, W-bit policy, System Bytes, and inbound classification remain
//! owned by the endpoint and Core.

#![allow(dead_code)]

use std::{fmt, sync::Arc};

use crate::{
    hsms::model::ids::{
        ConnectionGeneration, Function, ReplyCapabilityId, ReplyCapabilityIncarnation, Stream,
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

/// Single-use capability for responding to an inbound W=1 Primary.
#[must_use = "reply, abort, or explicitly abandon this inbound reply capability"]
pub struct ReplyToken {
    /// Unforgeable ledger-instance brand shared only with its owning issuer.
    brand: Arc<ReplyTokenBrand>,
    /// Opaque identity used to consume this reply authority exactly once.
    capability_id: ReplyCapabilityId,
    /// TCP incarnation on which the inbound primary was received.
    generation: ConnectionGeneration,
    /// Private exact-reservation identity that closes capability-ID ABA.
    incarnation: ReplyCapabilityIncarnation,
    /// Private admission hint; the Core ledger remains authoritative.
    normal_secondary_available: bool,
}

/// Private allocation whose pointer identity brands one reply ledger instance.
struct ReplyTokenBrand {
    /// Prevents construction outside this neutral contract module.
    private: (),
}

/// Move-only mint retained privately by exactly one reply ledger instance.
///
/// Other crate modules may create independent issuers, but their brands cannot
/// validate against an existing ledger. The issuer intentionally implements
/// neither [`Clone`] nor [`Copy`].
pub(crate) struct ReplyTokenIssuer {
    /// Unique allocation shared by tokens minted for this issuer.
    brand: Arc<ReplyTokenBrand>,
}

impl ReplyTokenIssuer {
    /// Creates a fresh issuer whose brand cannot equal any live issuer brand.
    pub(crate) fn new() -> Self {
        Self {
            brand: Arc::new(ReplyTokenBrand { private: () }),
        }
    }

    /// Mints one move-only token carrying this issuer's unforgeable brand.
    pub(crate) fn issue(
        &self,
        capability_id: ReplyCapabilityId,
        generation: ConnectionGeneration,
        incarnation: ReplyCapabilityIncarnation,
        normal_secondary_available: bool,
    ) -> ReplyToken {
        ReplyToken::from_issuer(
            Arc::clone(&self.brand),
            capability_id,
            generation,
            incarnation,
            normal_secondary_available,
        )
    }

    /// Validates the token's generation route and exact issuer without consuming it.
    ///
    /// Generation is checked before issuer identity so an obsolete token from a
    /// previous connection can be classified as stale even though each
    /// generation owns a fresh issuer. Success returns a crate-private numeric
    /// observation used only for exact live-entry validation.
    pub(crate) fn validate_route(
        &self,
        token: &ReplyToken,
        expected_generation: ConnectionGeneration,
    ) -> Result<ValidatedReplyTokenRoute, ReplyTokenRouteError> {
        if token.generation != expected_generation {
            return Err(ReplyTokenRouteError::WrongGeneration {
                expected: expected_generation,
                actual: token.generation,
            });
        }
        if !Arc::ptr_eq(&self.brand, &token.brand) {
            return Err(ReplyTokenRouteError::ForeignIssuer);
        }
        Ok(ValidatedReplyTokenRoute {
            capability_id: token.capability_id,
            generation: token.generation,
            incarnation: token.incarnation,
        })
    }
}

/// Failure routing a borrowed reply token to one generation-scoped issuer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReplyTokenRouteError {
    /// The token belongs to an obsolete or otherwise different connection generation.
    WrongGeneration {
        /// Generation expected by the receiving publication aggregate.
        expected: ConnectionGeneration,
        /// Generation carried by the borrowed application token.
        actual: ConnectionGeneration,
    },
    /// The token was minted by another reply-ledger instance in this generation.
    ForeignIssuer,
}

/// Numeric token identity released only after borrowed route validation succeeds.
#[must_use = "validated token identity must be matched against the live reply entry"]
pub(crate) struct ValidatedReplyTokenRoute {
    /// Opaque capability ID proven to carry the owning ledger's issuer brand.
    capability_id: ReplyCapabilityId,
    /// TCP generation proven equal to the receiving aggregate's generation.
    generation: ConnectionGeneration,
    /// Exact reservation incarnation observed without consuming the token.
    incarnation: ReplyCapabilityIncarnation,
}

impl ValidatedReplyTokenRoute {
    /// Consumes the observation and returns its exact ledger lookup identity.
    pub(crate) fn into_parts(
        self,
    ) -> (
        ConnectionGeneration,
        ReplyCapabilityId,
        ReplyCapabilityIncarnation,
    ) {
        (self.generation, self.capability_id, self.incarnation)
    }
}

impl ReplyToken {
    /// Constructs a token from an issuer-owned brand inside this module only.
    fn from_issuer(
        brand: Arc<ReplyTokenBrand>,
        capability_id: ReplyCapabilityId,
        generation: ConnectionGeneration,
        incarnation: ReplyCapabilityIncarnation,
        normal_secondary_available: bool,
    ) -> Self {
        Self {
            brand,
            capability_id,
            generation,
            incarnation,
            normal_secondary_available,
        }
    }

    /// Creates a deterministic foreign-brand token for crate-internal unit tests.
    #[cfg(test)]
    pub(crate) fn new(
        capability_id: ReplyCapabilityId,
        generation: ConnectionGeneration,
        normal_secondary_available: bool,
    ) -> Self {
        ReplyTokenIssuer::new().issue(
            capability_id,
            generation,
            ReplyCapabilityIncarnation::new(1),
            normal_secondary_available,
        )
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
    /// capability identity, incarnation, and private reply-contract hints hidden.
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
        assert!(!debug.contains("incarnation"));
        assert!(!debug.contains("123456"));
    }

    /// Guards the production token API against consuming validation or
    /// reintroducing raw numeric-identity accessors.
    #[test]
    fn reply_token_production_surface_uses_borrowed_route_validation() {
        let source = include_str!("message.rs");
        let forbidden_surfaces = [
            concat!("pub(crate) fn ", "from_issuer"),
            concat!("pub(crate) const fn ", "capability_id(&self)"),
            concat!("pub(crate) const fn ", "generation(&self)"),
            concat!("pub(crate) const fn ", "incarnation(&self)"),
            concat!("pub(crate) fn ", "into_claim(self)"),
        ];

        for forbidden in forbidden_surfaces {
            assert!(
                !source.contains(forbidden),
                "production ReplyToken surface exposed forbidden API: {forbidden}"
            );
        }
        assert!(source.contains("pub(crate) fn validate_route("));
        assert!(source.contains("Arc::ptr_eq"));
    }
}
