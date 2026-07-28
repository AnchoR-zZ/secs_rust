//! Owns bounded Reply capabilities inside `PublicationResources`.
//!
//! The ledger stores only immutable reply contracts and their live publication
//! state. It does not own application payloads, delivery identities, commands,
//! writes, or runtime effects. A read-only use plan lets `PublicationResources`
//! coordinate downstream preflight before the exact capability is consumed.
//! Global reset and close use a request-bound prepare/validate/commit receipt
//! protocol so Pending and Available authority are cleared together before the
//! matching Delivery batch can commit.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use crate::hsms::{
    contracts::{ReplyToken, ReplyTokenIssuer, ReplyTokenRouteError},
    model::ids::{ConnectionGeneration, ReplyCapabilityId, ReplyCapabilityIncarnation},
};

use super::super::{
    authority::{
        DeliveryClearRequestIdentity, PublicationAggregateIdentity,
        PublicationClearScope as ReplyClearScope, PublicationMutationAuthority,
    },
    contracts::{
        ReplyCapabilityMode, ReplyContract, ReplyUseCommitError, ReplyUseKind, ReplyUseTerminal,
        ReplyUseUnavailable,
    },
};

/// Failure constructing a logically bounded reply-capability ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum ReplyLedgerConfigError {
    /// A zero bound could never admit an inbound W=1 Primary.
    ZeroCapacity,
}

/// Live publication state retained for one reply capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum ReplyCapabilityState {
    /// The token is inside a publication attempt and cannot yet be consumed.
    PendingPublication,
    /// Publication completed successfully and the application may use the token.
    Available,
}

/// One live, Core-authoritative reply capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplyCapabilityEntry {
    /// Never-reused identity of this exact reservation.
    incarnation: ReplyCapabilityIncarnation,
    /// Immutable response-header authority compiled from the inbound Primary.
    contract: ReplyContract,
    /// Whether the application has successfully received the corresponding token.
    state: ReplyCapabilityState,
}

/// Private allocation whose pointer identity brands one reply-ledger instance.
#[derive(Debug)]
struct ReplyLedgerBrand {
    /// Private field preventing structural construction outside this module.
    private: (),
}

/// Opaque identity of one intended Reply ledger inside a publication aggregate.
///
/// Delivery receives this identity explicitly at construction, preventing a
/// same-aggregate second Reply ledger from becoming owner through first use.
#[derive(Debug)]
pub(in crate::hsms::core::resources::publication) struct ReplyLedgerIdentity {
    /// Exact publication aggregate that owns this Reply ledger.
    aggregate: PublicationAggregateIdentity,
    /// Private pointer brand distinguishing this exact Reply ledger instance.
    brand: Arc<ReplyLedgerBrand>,
    /// TCP generation exclusively owned by the identified Reply ledger.
    generation: ConnectionGeneration,
}

impl ReplyLedgerIdentity {
    /// Duplicates this non-authorizing identity for another owning resource.
    pub(in crate::hsms::core::resources::publication) fn duplicate(&self) -> Self {
        Self {
            aggregate: self.aggregate.duplicate(),
            brand: Arc::clone(&self.brand),
            generation: self.generation,
        }
    }

    /// Returns the TCP generation owned by this Reply ledger.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.generation
    }

    /// Returns whether this Reply ledger belongs to `aggregate`.
    pub(in crate::hsms::core::resources::publication) fn matches_aggregate(
        &self,
        aggregate: &PublicationAggregateIdentity,
    ) -> bool {
        self.aggregate.exact_eq(aggregate)
    }

    /// Returns whether `other` names this exact Reply ledger instance.
    pub(in crate::hsms::core::resources::publication) fn exact_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.brand, &other.brand)
            && self.aggregate.exact_eq(&other.aggregate)
            && self.generation == other.generation
    }

    /// Returns whether `ticket` was issued by this exact Reply ledger.
    pub(in crate::hsms::core::resources::publication) fn matches_ticket(
        &self,
        ticket: &ReplyPublicationTicket,
    ) -> bool {
        Arc::ptr_eq(&self.brand, &ticket.brand)
    }
}

/// Move-only Delivery request consumed by the intended Reply ledger.
#[derive(Debug)]
#[must_use = "bind this request to the matching Reply clear preparation"]
pub(in crate::hsms::core::resources::publication) struct ReplyClearRequest {
    /// Exact per-preparation nonce retained later by the clear receipt.
    clear_request: DeliveryClearRequestIdentity,
    /// Exact Reply ledger that Delivery requires to perform the clear.
    reply_ledger: ReplyLedgerIdentity,
}

impl ReplyClearRequest {
    /// Binds one nonce half to the exact intended Reply ledger.
    pub(in crate::hsms::core::resources::publication) fn new(
        clear_request: DeliveryClearRequestIdentity,
        reply_ledger: &ReplyLedgerIdentity,
    ) -> Self {
        Self {
            clear_request,
            reply_ledger: reply_ledger.duplicate(),
        }
    }

    /// Returns whether this request belongs to `aggregate`.
    fn matches_aggregate(&self, aggregate: &PublicationAggregateIdentity) -> bool {
        self.clear_request.matches_aggregate(aggregate)
    }

    /// Returns the TCP generation bound to this request.
    const fn generation(&self) -> ConnectionGeneration {
        self.clear_request.generation()
    }

    /// Returns the semantic scope bound to this request.
    const fn scope(&self) -> ReplyClearScope {
        self.clear_request.scope()
    }

    /// Returns whether this request targets `reply_ledger` exactly.
    fn matches_reply_ledger(&self, reply_ledger: &ReplyLedgerIdentity) -> bool {
        self.reply_ledger.exact_eq(reply_ledger)
    }

    /// Consumes the request into the nonce identity retained by the receipt.
    fn into_identity(self) -> DeliveryClearRequestIdentity {
        self.clear_request
    }
}

/// Failure reserving a new pending-publication capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum ReplyReserveError {
    /// The TCP generation ended and this ledger is permanently closed.
    Closing,
    /// The contract belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation embedded in the supplied reply contract.
        actual: ConnectionGeneration,
    },
    /// The capability identity is already live in this ledger.
    DuplicateId {
        /// Active identity that cannot be registered again.
        capability_id: ReplyCapabilityId,
    },
    /// Pending and available capabilities already occupy the configured bound.
    CapacityExhausted {
        /// Maximum number of simultaneously live capabilities.
        capacity: usize,
    },
    /// Every representable reservation incarnation has already been issued.
    IncarnationExhausted,
}

/// Exact reservation ticket retained across application publication.
///
/// The ticket is intentionally opaque and move-only. CoreResources may borrow
/// it for duplicate publication completion, or turn the current live entry
/// into an exact revocation plan when publication fails.
#[derive(Debug)]
#[must_use = "a reply publication ticket must be published, revoked, or cleared by reset"]
pub(in crate::hsms::core::resources::publication) struct ReplyPublicationTicket {
    /// Private brand of the exact reply ledger that issued this ticket.
    brand: Arc<ReplyLedgerBrand>,
    /// Exact pending reservation captured at insertion.
    snapshot: ReplyEntrySnapshot,
}

impl ReplyPublicationTicket {
    /// Returns the generation that owns this pending reservation.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.snapshot.generation
    }

    /// Returns the public capability identity assigned to this reservation.
    pub(in crate::hsms::core::resources::publication) const fn capability_id(
        &self,
    ) -> ReplyCapabilityId {
        self.snapshot.capability_id
    }

    /// Captures an opaque complete identity for later exact observation.
    ///
    /// The returned value includes ledger pointer identity plus every immutable
    /// reservation field. It is not a mutation authority and has no raw
    /// constructor outside this module.
    pub(in crate::hsms::core::resources::publication) fn identity(
        &self,
    ) -> ReplyPublicationTicketIdentity {
        ReplyPublicationTicketIdentity {
            brand: Arc::clone(&self.brand),
            snapshot: self.snapshot,
        }
    }
}

impl PartialEq for ReplyPublicationTicket {
    /// Compares two tickets by ledger instance and complete reservation snapshot.
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.brand, &other.brand) && self.snapshot == other.snapshot
    }
}

impl Eq for ReplyPublicationTicket {}

/// Opaque complete observation of one exact reply-publication ticket.
///
/// Delivery snapshots retain this value to detect same-shaped tickets issued
/// by different reply-ledger instances. The value is intentionally non-`Copy`
/// and exposes no raw fields or constructor.
#[derive(Debug)]
pub(in crate::hsms::core::resources::publication) struct ReplyPublicationTicketIdentity {
    /// Private brand of the reply ledger that issued the observed ticket.
    brand: Arc<ReplyLedgerBrand>,
    /// Complete generation, ID, incarnation, and contract observation.
    snapshot: ReplyEntrySnapshot,
}

impl ReplyPublicationTicketIdentity {
    /// Returns whether `ticket` is the exact ticket represented by this identity.
    ///
    /// Pointer identity distinguishes ledger instances even when every
    /// generation, ID, incarnation, and contract field has the same value.
    pub(in crate::hsms::core::resources::publication) fn matches(
        &self,
        ticket: &ReplyPublicationTicket,
    ) -> bool {
        Arc::ptr_eq(&self.brand, &ticket.brand) && self.snapshot == ticket.snapshot
    }
}

/// Exact artifacts created by one successful pending reservation.
///
/// CoreResources retains the publication ticket with its delivery state and
/// transfers the unique opaque token to the application only after publication
/// succeeds. Both artifacts carry the same private incarnation.
#[derive(Debug)]
#[must_use = "split the reservation into its publication ticket and application token"]
pub(in crate::hsms::core::resources::publication) struct ReplyReservation {
    /// Exact ticket used to publish or explicitly revoke this reservation.
    publication_ticket: ReplyPublicationTicket,
    /// Unique opaque authority eventually transferred to the application.
    reply_token: ReplyToken,
}

impl ReplyReservation {
    /// Separates this reservation into its exact publication and application artifacts.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (ReplyPublicationTicket, ReplyToken) {
        (self.publication_ticket, self.reply_token)
    }
}

/// Result of completing the application-publication half of a capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "publication decisions determine whether reply authority became available"]
pub(in crate::hsms::core::resources::publication) enum ReplyPublicationDecision {
    /// The ticket was issued by a different reply-ledger instance.
    ForeignLedger,
    /// The exact pending capability became available.
    MadeAvailable,
    /// The exact capability was already available, so no state changed.
    AlreadyAvailable,
    /// No live capability owns the supplied identity.
    UnknownOrTerminal,
    /// The ID now names a later reservation, so no state changed.
    IncarnationChanged {
        /// Reused identity whose current incarnation differs from the ticket.
        capability_id: ReplyCapabilityId,
    },
    /// The ID and incarnation matched but the immutable contract did not.
    ContractChanged {
        /// Identity whose contract failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The completion belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation supplied with the publication completion.
        actual: ConnectionGeneration,
    },
}

/// Reason an exact explicit-revocation plan could not be prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum ReplyRevocationUnavailable {
    /// The ticket was issued by a different reply-ledger instance.
    ForeignLedger,
    /// The ticket belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation captured by the stale ticket.
        actual: ConnectionGeneration,
    },
    /// The reservation was already consumed, revoked, reset, or never existed.
    UnknownOrTerminal,
    /// The ID now names a later reservation.
    IncarnationChanged {
        /// Reused identity whose incarnation failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The ID and incarnation matched but the immutable contract did not.
    ContractChanged {
        /// Identity whose contract failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
}

/// Exact, move-only plan for revoking one live capability.
#[derive(Debug)]
#[must_use = "an explicit revocation plan must be committed or deliberately discarded"]
pub(in crate::hsms::core::resources::publication) struct ReplyRevocationPlan {
    /// Private brand of the exact reply ledger that prepared this plan.
    brand: Arc<ReplyLedgerBrand>,
    /// Exact live entry and state observed during revocation preparation.
    snapshot: ReplyRevocationSnapshot,
}

/// Successful explicit revocation of one exact live capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a revocation terminal releases capacity and reply authority"]
pub(in crate::hsms::core::resources::publication) struct ReplyRevocationTerminal {
    /// Authoritative contract released by the revocation.
    contract: ReplyContract,
    /// Live state from which the capability was revoked.
    previous_state: ReplyCapabilityState,
}

impl ReplyRevocationTerminal {
    /// Returns the contract whose authority was removed.
    pub(in crate::hsms::core::resources::publication) const fn contract(self) -> ReplyContract {
        self.contract
    }

    /// Returns the publication state observed immediately before removal.
    pub(in crate::hsms::core::resources::publication) const fn previous_state(
        self,
    ) -> ReplyCapabilityState {
        self.previous_state
    }
}

/// Failure committing a revocation plan whose exact entry has changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum ReplyRevocationCommitError {
    /// The plan was prepared by a different reply-ledger instance.
    ForeignLedger,
    /// The plan belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation captured by the stale plan.
        actual: ConnectionGeneration,
    },
    /// The reservation was already consumed, revoked, reset, or never existed.
    UnknownOrTerminal {
        /// Identity that no longer names a live entry.
        capability_id: ReplyCapabilityId,
    },
    /// The ID now names a later reservation.
    IncarnationChanged {
        /// Reused identity whose incarnation failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The ID and incarnation matched but the immutable contract did not.
    ContractChanged {
        /// Identity whose contract failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
    /// The exact reservation changed publication state after preparation.
    StateChanged {
        /// Identity whose state failed exact revalidation.
        capability_id: ReplyCapabilityId,
    },
}

/// Exact immutable identity shared by every cross-call reply plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplyEntrySnapshot {
    /// TCP generation whose ledger produced the plan.
    generation: ConnectionGeneration,
    /// Exact capability intended for the terminal transition.
    capability_id: ReplyCapabilityId,
    /// Never-reused identity of the reservation observed by the plan.
    incarnation: ReplyCapabilityIncarnation,
    /// Authoritative contract observed while the capability was available.
    contract: ReplyContract,
}

impl ReplyEntrySnapshot {
    /// Captures the exact reservation that every later mutation must revalidate.
    const fn new(
        generation: ConnectionGeneration,
        capability_id: ReplyCapabilityId,
        incarnation: ReplyCapabilityIncarnation,
        contract: ReplyContract,
    ) -> Self {
        Self {
            generation,
            capability_id,
            incarnation,
            contract,
        }
    }
}

/// Exact live entry and publication state captured by a revocation plan.
#[derive(Debug, PartialEq, Eq)]
struct ReplyRevocationSnapshot {
    /// Immutable identity of the reservation to revoke.
    entry: ReplyEntrySnapshot,
    /// Publication state that must remain unchanged until commit.
    state: ReplyCapabilityState,
}

/// Authorized, move-only plan for a normal F+1 Secondary.
///
/// Only a contract with a real F+1 function can construct this type. It is the
/// sole plan variant that exposes a normal-response contract to downstream
/// Operation and Write preflight.
#[derive(Debug)]
#[must_use = "an authorized normal reply plan must be committed or explicitly discarded"]
pub(in crate::hsms::core::resources::publication) struct AuthorizedNormalReplyPlan {
    /// Private brand of the exact reply ledger that prepared this plan.
    brand: Arc<ReplyLedgerBrand>,
    /// Exact available capability captured for later commit.
    snapshot: ReplyEntrySnapshot,
}

impl AuthorizedNormalReplyPlan {
    /// Returns the generation whose live capability produced this plan.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.snapshot.generation
    }

    /// Returns the exact capability that must still be available at commit.
    pub(in crate::hsms::core::resources::publication) const fn capability_id(
        &self,
    ) -> ReplyCapabilityId {
        self.snapshot.capability_id
    }

    /// Returns the Core-authoritative contract proven to support normal F+1.
    pub(in crate::hsms::core::resources::publication) const fn contract(&self) -> ReplyContract {
        self.snapshot.contract
    }
}

/// Authorized, move-only plan for a header-only SxF0 abort.
#[derive(Debug)]
#[must_use = "an authorized abort-reply plan must be committed or explicitly discarded"]
pub(in crate::hsms::core::resources::publication) struct AuthorizedAbortReplyPlan {
    /// Private brand of the exact reply ledger that prepared this plan.
    brand: Arc<ReplyLedgerBrand>,
    /// Exact available capability captured for later commit.
    snapshot: ReplyEntrySnapshot,
}

impl AuthorizedAbortReplyPlan {
    /// Returns the generation whose live capability produced this plan.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.snapshot.generation
    }

    /// Returns the exact capability that must still be available at commit.
    pub(in crate::hsms::core::resources::publication) const fn capability_id(
        &self,
    ) -> ReplyCapabilityId {
        self.snapshot.capability_id
    }

    /// Returns the Core-authoritative contract used to build header-only SxF0.
    pub(in crate::hsms::core::resources::publication) const fn contract(&self) -> ReplyContract {
        self.snapshot.contract
    }
}

/// Authorized, move-only plan for local reply-capability abandonment.
#[derive(Debug)]
#[must_use = "an authorized abandon-reply plan must be committed or explicitly discarded"]
pub(in crate::hsms::core::resources::publication) struct AuthorizedAbandonReplyPlan {
    /// Private brand of the exact reply ledger that prepared this plan.
    brand: Arc<ReplyLedgerBrand>,
    /// Exact available capability captured for later commit.
    snapshot: ReplyEntrySnapshot,
}

impl AuthorizedAbandonReplyPlan {
    /// Returns the generation whose live capability produced this plan.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.snapshot.generation
    }

    /// Returns the exact capability that must still be available at commit.
    pub(in crate::hsms::core::resources::publication) const fn capability_id(
        &self,
    ) -> ReplyCapabilityId {
        self.snapshot.capability_id
    }
}

/// Exhaustive, move-only authorized use plan for an available capability.
///
/// Callers must match the semantic variant before performing downstream
/// resource preflight. Only `Normal` and `Abort` need a protocol write, while
/// `Abandon` is a local terminal action.
#[derive(Debug)]
#[must_use = "match the reply-use plan and commit its exact semantic variant"]
pub(in crate::hsms::core::resources::publication) enum ReplyUsePlan {
    /// Normal F+1 response authority proven available by the stored contract.
    Normal(
        /// Opaque authorized normal-response plan.
        AuthorizedNormalReplyPlan,
    ),
    /// Header-only SxF0 authority valid for normal and abort-only contracts.
    Abort(
        /// Opaque authorized abort-response plan.
        AuthorizedAbortReplyPlan,
    ),
    /// Local capability release requiring no protocol write.
    Abandon(
        /// Opaque authorized abandonment plan.
        AuthorizedAbandonReplyPlan,
    ),
}

impl ReplyUsePlan {
    /// Borrows the exact reply-ledger brand captured by this plan.
    fn brand(&self) -> &Arc<ReplyLedgerBrand> {
        match self {
            Self::Normal(plan) => &plan.brand,
            Self::Abort(plan) => &plan.brand,
            Self::Abandon(plan) => &plan.brand,
        }
    }

    /// Returns the immutable live-entry snapshot captured by this plan.
    const fn snapshot(&self) -> ReplyEntrySnapshot {
        match self {
            Self::Normal(plan) => plan.snapshot,
            Self::Abort(plan) => plan.snapshot,
            Self::Abandon(plan) => plan.snapshot,
        }
    }

    /// Returns the semantic terminal action authorized by this plan.
    const fn use_kind(&self) -> ReplyUseKind {
        match self {
            Self::Normal(_) => ReplyUseKind::Normal,
            Self::Abort(_) => ReplyUseKind::Abort,
            Self::Abandon(_) => ReplyUseKind::Abandon,
        }
    }

    /// Returns the generation whose capability produced this plan.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.snapshot().generation
    }

    /// Returns the exact capability identity captured by this plan.
    pub(in crate::hsms::core::resources::publication) const fn capability_id(
        &self,
    ) -> ReplyCapabilityId {
        self.snapshot().capability_id
    }

    /// Returns the authoritative reply contract captured by this plan.
    pub(in crate::hsms::core::resources::publication) const fn contract(&self) -> ReplyContract {
        self.snapshot().contract
    }

    /// Returns the normal, abort, or abandon action captured by this plan.
    pub(in crate::hsms::core::resources::publication) const fn kind(&self) -> ReplyUseKind {
        self.use_kind()
    }
}

/// Ownership-preserving failure from committing one prepared reply use.
#[derive(Debug)]
#[must_use = "recover the unchanged plan and token or deliberately abandon them"]
pub(in crate::hsms::core::resources::publication) struct ReplyUseCommitFailure {
    /// Structured reason no reply capability was consumed.
    reason: ReplyUseCommitError,
    /// Exact move-only plan returned unchanged after failed revalidation.
    plan: ReplyUsePlan,
    /// Original application token returned unchanged after failed revalidation.
    token: ReplyToken,
}

impl ReplyUseCommitFailure {
    /// Returns the copyable reason no reply capability was consumed.
    pub(in crate::hsms::core::resources::publication) const fn reason(
        &self,
    ) -> ReplyUseCommitError {
        self.reason
    }

    /// Consumes the failure into its reason, unchanged plan, and original token.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (ReplyUseCommitError, ReplyUsePlan, ReplyToken) {
        (self.reason, self.plan, self.token)
    }
}

/// Counts live capabilities removed by a session or generation reset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[must_use = "reset summaries report how much live reply authority was revoked"]
pub(in crate::hsms::core::resources::publication) struct ReplyResetSummary {
    /// Capabilities removed before their token publication completed.
    pending_publication: usize,
    /// Capabilities removed after their tokens reached the application.
    available: usize,
}

impl ReplyResetSummary {
    /// Returns how many pending-publication capabilities were revoked.
    pub(in crate::hsms::core::resources::publication) const fn pending_publication(self) -> usize {
        self.pending_publication
    }

    /// Returns how many application-available capabilities were revoked.
    pub(in crate::hsms::core::resources::publication) const fn available(self) -> usize {
        self.available
    }

    /// Returns the total number of live capabilities removed by the reset.
    pub(in crate::hsms::core::resources::publication) const fn total(self) -> usize {
        self.pending_publication + self.available
    }
}

/// Failure preparing a global Reply clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum ReplyClearPrepareError {
    /// The supplied authority belongs to a different publication aggregate.
    ForeignAggregate,
    /// The clear request was issued by a different publication aggregate.
    ForeignRequestAggregate,
    /// The clear request targets another Reply ledger in this aggregate.
    ForeignReplyLedger,
    /// The clear request belongs to a different TCP generation.
    WrongRequestGeneration {
        /// Generation owned by this Reply ledger.
        expected: ConnectionGeneration,
        /// Generation bound into the Delivery request.
        actual: ConnectionGeneration,
    },
    /// The clear request carries the wrong reset-versus-close scope.
    WrongRequestScope {
        /// Scope required by the invoked Reply preparation method.
        expected: ReplyClearScope,
        /// Scope bound into the Delivery request.
        actual: ReplyClearScope,
    },
    /// Selected-session reset cannot begin after generation close.
    Closing,
}

/// Ownership-preserving failure to bind a Delivery clear request to Reply.
#[derive(Debug)]
#[must_use = "recover or deliberately discard the unchanged clear request"]
pub(in crate::hsms::core::resources::publication) struct ReplyClearPrepareFailure {
    /// Structured reason Reply preparation did not begin.
    reason: ReplyClearPrepareError,
    /// Exact move-only request returned without mutation.
    request: ReplyClearRequest,
}

impl ReplyClearPrepareFailure {
    /// Returns the copyable preparation failure reason.
    pub(in crate::hsms::core::resources::publication) const fn reason(
        &self,
    ) -> ReplyClearPrepareError {
        self.reason
    }

    /// Consumes the failure into its reason and unchanged request.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (ReplyClearPrepareError, ReplyClearRequest) {
        (self.reason, self.request)
    }
}

/// Failure cross-validating Delivery's pending W=1 publication tickets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum ReplyClearValidationError {
    /// A ticket was issued by a different Reply ledger.
    ForeignLedger,
    /// A ticket belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by the prepared Reply ledger.
        expected: ConnectionGeneration,
        /// Generation captured by the supplied ticket.
        actual: ConnectionGeneration,
    },
    /// A supplied ticket no longer names a live Reply entry.
    UnknownOrTerminal {
        /// Missing capability identity.
        capability_id: ReplyCapabilityId,
    },
    /// A supplied ticket names a later or earlier reservation incarnation.
    IncarnationChanged {
        /// Capability whose incarnation did not match.
        capability_id: ReplyCapabilityId,
    },
    /// A supplied ticket's immutable contract did not match Reply authority.
    ContractChanged {
        /// Capability whose contract did not match.
        capability_id: ReplyCapabilityId,
    },
    /// Delivery supplied a ticket for an already-published capability.
    NotPendingPublication {
        /// Capability whose publication is already available.
        capability_id: ReplyCapabilityId,
    },
    /// Delivery supplied the same pending ticket more than once.
    DuplicateDeliveryTicket {
        /// Duplicated capability identity.
        capability_id: ReplyCapabilityId,
    },
    /// One pending Reply entry has no corresponding Delivery publication.
    MissingDeliveryTicket {
        /// Pending capability omitted by Delivery preparation.
        capability_id: ReplyCapabilityId,
    },
}

/// Failure revalidating a typed global Reply-clear commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum ReplyClearCommitError {
    /// The supplied authority belongs to a different publication aggregate.
    ForeignAggregate,
    /// The commit was prepared by another Reply ledger in the same aggregate.
    ForeignLedger,
    /// The commit belongs to a different TCP generation.
    WrongGeneration {
        /// Generation owned by this Reply ledger.
        expected: ConnectionGeneration,
        /// Generation captured by the stale commit.
        actual: ConnectionGeneration,
    },
    /// Open-versus-closing state changed after preparation.
    ClosingStateChanged {
        /// Closing state captured by the preparation.
        expected: bool,
        /// Closing state observed during commit.
        actual: bool,
    },
    /// The complete live capability set changed after preparation.
    EntrySetChanged,
    /// A capability ID now names another reservation incarnation.
    IncarnationChanged {
        /// Capability whose incarnation changed.
        capability_id: ReplyCapabilityId,
    },
    /// A capability's immutable contract changed after preparation.
    ContractChanged {
        /// Capability whose contract changed.
        capability_id: ReplyCapabilityId,
    },
    /// A capability's publication state changed after preparation.
    StateChanged {
        /// Capability whose state changed.
        capability_id: ReplyCapabilityId,
    },
    /// A closed Reply ledger unexpectedly retained live capabilities.
    ClosingWithLiveEntries {
        /// Number of impossible live entries behind the close fence.
        live: usize,
    },
}

/// Exact complete state captured for one global Reply clear.
#[derive(Debug)]
struct ReplyClearCommitState {
    /// Private brand of the exact Reply ledger that prepared the clear.
    ledger_brand: Arc<ReplyLedgerBrand>,
    /// Publication aggregate that owns the prepared Reply ledger.
    aggregate: PublicationAggregateIdentity,
    /// TCP generation captured by preparation.
    generation: ConnectionGeneration,
    /// Exact Delivery clear request consumed by Reply preparation.
    clear_request: DeliveryClearRequestIdentity,
    /// Open-versus-closing state captured by preparation.
    expected_closing: bool,
    /// Complete live entry snapshots in ascending capability-ID order.
    snapshots: Vec<ReplyRevocationSnapshot>,
    /// Pending and available counts computed before commit.
    summary: ReplyResetSummary,
}

/// Read-only preparation for clearing Reply on Selected-session reset.
#[derive(Debug)]
#[must_use = "cross-validate pending Delivery tickets before committing Reply clear"]
pub(in crate::hsms::core::resources::publication) struct ReplySelectedResetPreparation {
    /// Complete immutable Reply state captured for validation and commit.
    state: ReplyClearCommitState,
}

/// Read-only preparation for clearing Reply on TCP-generation end.
#[derive(Debug)]
#[must_use = "cross-validate pending Delivery tickets before committing Reply clear"]
pub(in crate::hsms::core::resources::publication) struct ReplyGenerationEndPreparation {
    /// Complete immutable Reply state captured for validation and commit.
    state: ReplyClearCommitState,
}

/// Move-only Selected-reset commit produced only after ticket cross-validation.
#[derive(Debug)]
#[must_use = "commit the validated global Reply clear or deliberately discard it"]
pub(in crate::hsms::core::resources::publication) struct ReplySelectedResetCommit {
    /// Complete exact state that commit must revalidate.
    state: ReplyClearCommitState,
}

/// Move-only generation-end commit produced only after ticket cross-validation.
#[derive(Debug)]
#[must_use = "commit the validated global Reply clear or deliberately discard it"]
pub(in crate::hsms::core::resources::publication) struct ReplyGenerationEndCommit {
    /// Complete exact state that commit must revalidate.
    state: ReplyClearCommitState,
}

/// Ownership-preserving failure from clear cross-validation.
#[derive(Debug)]
#[must_use = "recover or deliberately discard the unchanged clear preparation"]
pub(in crate::hsms::core::resources::publication) struct ReplyClearValidationFailure<P> {
    /// Structured reason ticket cross-validation failed.
    reason: ReplyClearValidationError,
    /// Exact move-only preparation returned without mutation.
    preparation: P,
}

impl<P> ReplyClearValidationFailure<P> {
    /// Returns the copyable validation failure reason.
    pub(in crate::hsms::core::resources::publication) const fn reason(
        &self,
    ) -> ReplyClearValidationError {
        self.reason
    }

    /// Consumes the failure into its reason and unchanged preparation.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (ReplyClearValidationError, P) {
        (self.reason, self.preparation)
    }
}

/// Ownership-preserving failure from global Reply-clear commit.
#[derive(Debug)]
#[must_use = "recover or deliberately discard the unchanged clear commit"]
pub(in crate::hsms::core::resources::publication) struct ReplyClearCommitFailure<C> {
    /// Structured reason commit revalidation failed.
    reason: ReplyClearCommitError,
    /// Exact move-only commit returned without mutation.
    commit: C,
}

impl<C> ReplyClearCommitFailure<C> {
    /// Returns the copyable commit failure reason.
    pub(in crate::hsms::core::resources::publication) const fn reason(
        &self,
    ) -> ReplyClearCommitError {
        self.reason
    }

    /// Consumes the failure into its reason and unchanged commit.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (ReplyClearCommitError, C) {
        (self.reason, self.commit)
    }
}

/// Opaque proof that one exact Reply ledger completed a global clear.
///
/// The runtime scope remains explicit so Delivery can reject a generation-end
/// receipt used for Selected reset, or the reverse, without mutation.
#[derive(Debug)]
#[must_use = "the clear receipt must authorize the matching Delivery batch commit"]
pub(in crate::hsms::core::resources::publication) struct ReplyClearReceipt {
    /// Selected-reset or generation-end semantic scope.
    scope: ReplyClearScope,
    /// Publication aggregate whose Reply ledger completed the clear.
    aggregate: PublicationAggregateIdentity,
    /// Exact Reply-ledger instance that completed the clear.
    ledger_brand: Arc<ReplyLedgerBrand>,
    /// TCP generation whose complete Reply authority was cleared.
    generation: ConnectionGeneration,
    /// Exact Delivery preparation request authorized by this clear.
    clear_request: DeliveryClearRequestIdentity,
    /// Whether generation-end commit raised the permanent close fence.
    began_close: bool,
    /// Pending and available capability counts removed by the clear.
    summary: ReplyResetSummary,
}

impl ReplyClearReceipt {
    /// Returns the semantic scope proven by this receipt.
    pub(in crate::hsms::core::resources::publication) const fn scope(&self) -> ReplyClearScope {
        self.scope
    }

    /// Returns the TCP generation whose Reply authority was cleared.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.generation
    }

    /// Returns whether this generation-end commit raised the close fence.
    pub(in crate::hsms::core::resources::publication) const fn began_close(&self) -> bool {
        self.began_close
    }

    /// Returns counts of pending and available authority removed by the clear.
    pub(in crate::hsms::core::resources::publication) const fn summary(&self) -> ReplyResetSummary {
        self.summary
    }

    /// Returns whether this receipt belongs to `aggregate`.
    pub(in crate::hsms::core::resources::publication) fn matches_aggregate(
        &self,
        aggregate: &PublicationAggregateIdentity,
    ) -> bool {
        self.aggregate.exact_eq(aggregate)
    }

    /// Returns whether this receipt cleared the exact intended Reply ledger.
    pub(in crate::hsms::core::resources::publication) fn covers_reply_ledger(
        &self,
        identity: &ReplyLedgerIdentity,
    ) -> bool {
        Arc::ptr_eq(&self.ledger_brand, &identity.brand)
    }

    /// Returns whether this receipt answers `request` exactly once by identity.
    pub(in crate::hsms::core::resources::publication) fn answers_request(
        &self,
        request: &DeliveryClearRequestIdentity,
    ) -> bool {
        self.clear_request.exact_eq(request)
    }
}

impl ReplySelectedResetPreparation {
    /// Cross-validates the complete set of pending Delivery W=1 tickets.
    ///
    /// Success consumes the preparation into a typed commit. Failure returns
    /// the exact preparation unchanged and performs no mutation.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn validate_pending_tickets<'a, I>(
        self,
        tickets: I,
    ) -> Result<ReplySelectedResetCommit, ReplyClearValidationFailure<ReplySelectedResetPreparation>>
    where
        I: IntoIterator<Item = &'a ReplyPublicationTicket>,
    {
        match ReplyCapabilityLedger::validate_clear_tickets(&self.state, tickets) {
            Ok(()) => Ok(ReplySelectedResetCommit { state: self.state }),
            Err(reason) => Err(ReplyClearValidationFailure {
                reason,
                preparation: self,
            }),
        }
    }
}

impl ReplyGenerationEndPreparation {
    /// Cross-validates the complete set of pending Delivery W=1 tickets.
    ///
    /// Success consumes the preparation into a typed commit. Failure returns
    /// the exact preparation unchanged and performs no mutation.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn validate_pending_tickets<'a, I>(
        self,
        tickets: I,
    ) -> Result<ReplyGenerationEndCommit, ReplyClearValidationFailure<ReplyGenerationEndPreparation>>
    where
        I: IntoIterator<Item = &'a ReplyPublicationTicket>,
    {
        match ReplyCapabilityLedger::validate_clear_tickets(&self.state, tickets) {
            Ok(()) => Ok(ReplyGenerationEndCommit { state: self.state }),
            Err(reason) => Err(ReplyClearValidationFailure {
                reason,
                preparation: self,
            }),
        }
    }
}

/// Bounded owner of all live reply authority for one TCP generation.
pub(in crate::hsms::core::resources::publication) struct ReplyCapabilityLedger {
    /// TCP generation whose inbound W=1 Primaries may create entries.
    generation: ConnectionGeneration,
    /// Logical maximum across pending-publication and available entries.
    capacity: usize,
    /// Exact publication aggregate that owns this Reply ledger.
    aggregate: PublicationAggregateIdentity,
    /// Unforgeable pointer brand distinguishing this exact ledger instance.
    brand: Arc<ReplyLedgerBrand>,
    /// Unforgeable mint whose brand authenticates application reply tokens.
    token_issuer: ReplyTokenIssuer,
    /// Permanent fence set when the owning TCP generation ends.
    closing: bool,
    /// Incarnation to issue next, or `None` after the complete domain is spent.
    next_incarnation: Option<ReplyCapabilityIncarnation>,
    /// Lazily allocated live entries keyed by their externally assigned IDs.
    entries: BTreeMap<ReplyCapabilityId, ReplyCapabilityEntry>,
}

impl ReplyCapabilityLedger {
    /// Creates an empty generation-scoped ledger with logical `capacity`.
    ///
    /// The backing tree allocates lazily, so even `usize::MAX` does not request
    /// a proportional allocation. Zero is rejected because it cannot support
    /// reliable publication of any inbound W=1 Primary.
    pub(in crate::hsms::core::resources::publication) fn new(
        generation: ConnectionGeneration,
        capacity: usize,
        authority: &PublicationMutationAuthority,
    ) -> Result<Self, ReplyLedgerConfigError> {
        if capacity == 0 {
            return Err(ReplyLedgerConfigError::ZeroCapacity);
        }
        Ok(Self {
            generation,
            capacity,
            aggregate: authority.identity(),
            brand: Arc::new(ReplyLedgerBrand { private: () }),
            token_issuer: ReplyTokenIssuer::new(),
            closing: false,
            next_incarnation: Some(ReplyCapabilityIncarnation::new(1)),
            entries: BTreeMap::new(),
        })
    }

    /// Returns the TCP generation exclusively owned by this ledger.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.generation
    }

    /// Captures the exact intended Reply-ledger identity for Delivery binding.
    pub(in crate::hsms::core::resources::publication) fn identity(&self) -> ReplyLedgerIdentity {
        ReplyLedgerIdentity {
            aggregate: self.aggregate.duplicate(),
            brand: Arc::clone(&self.brand),
            generation: self.generation,
        }
    }

    /// Returns the logical maximum number of simultaneously live capabilities.
    pub(in crate::hsms::core::resources::publication) const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the combined number of pending-publication and available entries.
    pub(in crate::hsms::core::resources::publication) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether the ledger currently owns no live capability.
    pub(in crate::hsms::core::resources::publication) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns whether the generation-end closing fence has been raised.
    pub(in crate::hsms::core::resources::publication) const fn is_closing(&self) -> bool {
        self.closing
    }

    /// Returns whether another pending capability can be reserved.
    pub(in crate::hsms::core::resources::publication) fn has_capacity(&self) -> bool {
        !self.closing && self.next_incarnation.is_some() && self.entries.len() < self.capacity
    }

    /// Checks a borrowed token's connection generation and exact issuer route.
    ///
    /// No live-entry lookup or mutation occurs. A caller can therefore map an
    /// obsolete token to a stale-generation command error before transferring
    /// ownership into reply-use preparation.
    pub(in crate::hsms::core::resources::publication) fn precheck_token_route(
        &self,
        token: &ReplyToken,
    ) -> Result<(), ReplyTokenRouteError> {
        self.token_issuer
            .validate_route(token, self.generation)
            .map(|_route| ())
    }

    /// Registers `capability_id` as pending publication under `contract`.
    ///
    /// The returned ticket names this exact reservation rather than only its
    /// externally assigned ID. Wrong generation, duplicate identity, exhausted
    /// capacity, and exhausted incarnation space leave the ledger unchanged.
    pub(in crate::hsms::core::resources::publication) fn reserve_pending(
        &mut self,
        capability_id: ReplyCapabilityId,
        contract: ReplyContract,
    ) -> Result<ReplyReservation, ReplyReserveError> {
        if self.closing {
            return Err(ReplyReserveError::Closing);
        }
        if contract.generation() != self.generation {
            return Err(ReplyReserveError::WrongGeneration {
                expected: self.generation,
                actual: contract.generation(),
            });
        }

        if self.entries.contains_key(&capability_id) {
            return Err(ReplyReserveError::DuplicateId { capability_id });
        }
        if self.entries.len() >= self.capacity {
            return Err(ReplyReserveError::CapacityExhausted {
                capacity: self.capacity,
            });
        }
        let Some(incarnation) = self.next_incarnation else {
            return Err(ReplyReserveError::IncarnationExhausted);
        };

        let snapshot =
            ReplyEntrySnapshot::new(self.generation, capability_id, incarnation, contract);
        self.entries.insert(
            capability_id,
            ReplyCapabilityEntry {
                incarnation,
                contract,
                state: ReplyCapabilityState::PendingPublication,
            },
        );
        self.next_incarnation = incarnation
            .get()
            .checked_add(1)
            .map(ReplyCapabilityIncarnation::new);
        Ok(ReplyReservation {
            publication_ticket: ReplyPublicationTicket {
                brand: Arc::clone(&self.brand),
                snapshot,
            },
            reply_token: self.token_issuer.issue(
                capability_id,
                self.generation,
                incarnation,
                contract.supports_normal_secondary(),
            ),
        })
    }

    /// Preflights publication of one exact pending capability without mutation.
    ///
    /// The result is authoritative only while the caller retains exclusive
    /// access to the containing publication aggregate and invokes no callbacks.
    /// This lets the aggregate validate both Reply and Delivery before the
    /// first cross-ledger mutation.
    pub(in crate::hsms::core::resources::publication) fn preflight_publication(
        &self,
        ticket: &ReplyPublicationTicket,
    ) -> ReplyPublicationDecision {
        if !Arc::ptr_eq(&self.brand, &ticket.brand) {
            return ReplyPublicationDecision::ForeignLedger;
        }
        let snapshot = ticket.snapshot;
        if snapshot.generation != self.generation {
            return ReplyPublicationDecision::WrongGeneration {
                expected: self.generation,
                actual: snapshot.generation,
            };
        }
        let Some(entry) = self.entries.get(&snapshot.capability_id) else {
            return ReplyPublicationDecision::UnknownOrTerminal;
        };
        if entry.incarnation != snapshot.incarnation {
            return ReplyPublicationDecision::IncarnationChanged {
                capability_id: snapshot.capability_id,
            };
        }
        if entry.contract != snapshot.contract {
            return ReplyPublicationDecision::ContractChanged {
                capability_id: snapshot.capability_id,
            };
        }
        match entry.state {
            ReplyCapabilityState::PendingPublication => ReplyPublicationDecision::MadeAvailable,
            ReplyCapabilityState::Available => ReplyPublicationDecision::AlreadyAvailable,
        }
    }

    /// Makes the exact pending capability available after successful publication.
    ///
    /// The borrowed reservation ticket lets duplicate completion remain
    /// idempotent while preventing a delayed completion from publishing a later
    /// reservation that reused the same external ID and immutable contract.
    pub(in crate::hsms::core::resources::publication) fn mark_available(
        &mut self,
        ticket: &ReplyPublicationTicket,
    ) -> ReplyPublicationDecision {
        let decision = self.preflight_publication(ticket);
        if decision != ReplyPublicationDecision::MadeAvailable {
            return decision;
        }
        let capability_id = ticket.snapshot.capability_id;
        let entry = self
            .entries
            .get_mut(&capability_id)
            .expect("preflight proved the exact pending reply entry exists");
        entry.state = ReplyCapabilityState::Available;
        ReplyPublicationDecision::MadeAvailable
    }

    /// Prepares a move-only revocation plan from an exact reservation ticket.
    ///
    /// Preparation captures the current publication state as well as the
    /// reservation incarnation. Any stale ticket leaves the ledger unchanged.
    pub(in crate::hsms::core::resources::publication) fn prepare_revocation(
        &self,
        ticket: &ReplyPublicationTicket,
    ) -> Result<ReplyRevocationPlan, ReplyRevocationUnavailable> {
        if !Arc::ptr_eq(&self.brand, &ticket.brand) {
            return Err(ReplyRevocationUnavailable::ForeignLedger);
        }
        let snapshot = ticket.snapshot;
        if snapshot.generation != self.generation {
            return Err(ReplyRevocationUnavailable::WrongGeneration {
                expected: self.generation,
                actual: snapshot.generation,
            });
        }
        let Some(entry) = self.entries.get(&snapshot.capability_id) else {
            return Err(ReplyRevocationUnavailable::UnknownOrTerminal);
        };
        if entry.incarnation != snapshot.incarnation {
            return Err(ReplyRevocationUnavailable::IncarnationChanged {
                capability_id: snapshot.capability_id,
            });
        }
        if entry.contract != snapshot.contract {
            return Err(ReplyRevocationUnavailable::ContractChanged {
                capability_id: snapshot.capability_id,
            });
        }
        Ok(ReplyRevocationPlan {
            brand: Arc::clone(&self.brand),
            snapshot: ReplyRevocationSnapshot {
                entry: snapshot,
                state: entry.state,
            },
        })
    }

    /// Commits one exact explicit-revocation plan and releases its capacity.
    ///
    /// Generation, ID, incarnation, contract, and publication state must all
    /// still match. Every failure leaves all current entries unchanged.
    pub(in crate::hsms::core::resources::publication) fn commit_revocation(
        &mut self,
        plan: ReplyRevocationPlan,
    ) -> Result<ReplyRevocationTerminal, ReplyRevocationCommitError> {
        if !Arc::ptr_eq(&self.brand, &plan.brand) {
            return Err(ReplyRevocationCommitError::ForeignLedger);
        }
        let snapshot = plan.snapshot;
        if snapshot.entry.generation != self.generation {
            return Err(ReplyRevocationCommitError::WrongGeneration {
                expected: self.generation,
                actual: snapshot.entry.generation,
            });
        }
        let Some(entry) = self.entries.get(&snapshot.entry.capability_id) else {
            return Err(ReplyRevocationCommitError::UnknownOrTerminal {
                capability_id: snapshot.entry.capability_id,
            });
        };
        if entry.incarnation != snapshot.entry.incarnation {
            return Err(ReplyRevocationCommitError::IncarnationChanged {
                capability_id: snapshot.entry.capability_id,
            });
        }
        if entry.contract != snapshot.entry.contract {
            return Err(ReplyRevocationCommitError::ContractChanged {
                capability_id: snapshot.entry.capability_id,
            });
        }
        if entry.state != snapshot.state {
            return Err(ReplyRevocationCommitError::StateChanged {
                capability_id: snapshot.entry.capability_id,
            });
        }

        let Some(entry) = self.entries.remove(&snapshot.entry.capability_id) else {
            return Err(ReplyRevocationCommitError::UnknownOrTerminal {
                capability_id: snapshot.entry.capability_id,
            });
        };
        Ok(ReplyRevocationTerminal {
            contract: entry.contract,
            previous_state: entry.state,
        })
    }

    /// Preflights an exact available capability use without consuming its token.
    ///
    /// This operation trusts only the stored [`ReplyContract`] and never reads
    /// the non-authoritative public token hint. The caller retains ownership of
    /// `token` on every outcome, allowing downstream resource admission to
    /// return it intact or explicitly abandon the capability.
    pub(in crate::hsms::core::resources::publication) fn prepare_use(
        &self,
        token: &ReplyToken,
        use_kind: ReplyUseKind,
    ) -> Result<ReplyUsePlan, ReplyUseUnavailable> {
        let validated = self
            .token_issuer
            .validate_route(token, self.generation)
            .map_err(|error| match error {
                ReplyTokenRouteError::ForeignIssuer => ReplyUseUnavailable::ForeignIssuer,
                ReplyTokenRouteError::WrongGeneration { expected, actual } => {
                    ReplyUseUnavailable::WrongGeneration { expected, actual }
                }
            })?;
        let (generation, capability_id, incarnation) = validated.into_parts();
        let Some(entry) = self.entries.get(&capability_id).copied() else {
            return Err(ReplyUseUnavailable::UnknownOrTerminal);
        };
        if entry.incarnation != incarnation {
            return Err(ReplyUseUnavailable::IncarnationChanged { capability_id });
        }
        if entry.state == ReplyCapabilityState::PendingPublication {
            return Err(ReplyUseUnavailable::PendingPublication);
        }

        let snapshot =
            ReplyEntrySnapshot::new(generation, capability_id, incarnation, entry.contract);
        let plan = match (use_kind, entry.contract.mode()) {
            (ReplyUseKind::Normal, ReplyCapabilityMode::NormalSecondary { .. }) => {
                ReplyUsePlan::Normal(AuthorizedNormalReplyPlan {
                    brand: Arc::clone(&self.brand),
                    snapshot,
                })
            }
            (ReplyUseKind::Normal, ReplyCapabilityMode::AbortOnly) => {
                return Err(ReplyUseUnavailable::NormalSecondaryUnavailable);
            }
            (ReplyUseKind::Abort, _) => ReplyUsePlan::Abort(AuthorizedAbortReplyPlan {
                brand: Arc::clone(&self.brand),
                snapshot,
            }),
            (ReplyUseKind::Abandon, _) => ReplyUsePlan::Abandon(AuthorizedAbandonReplyPlan {
                brand: Arc::clone(&self.brand),
                snapshot,
            }),
        };
        Ok(plan)
    }

    /// Commits one prepared use and its original token after exact revalidation.
    ///
    /// Success removes exactly one available entry. Generation, ID,
    /// incarnation, contract, publication state, and authorized use kind are
    /// all revalidated. Every failure leaves all entries unchanged and returns
    /// both move-only inputs so downstream admission can retry or abandon.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn commit_use(
        &mut self,
        plan: ReplyUsePlan,
        token: ReplyToken,
    ) -> Result<ReplyUseTerminal, ReplyUseCommitFailure> {
        let snapshot = plan.snapshot();
        let use_kind = plan.use_kind();
        let failure = |reason, plan, token| ReplyUseCommitFailure {
            reason,
            plan,
            token,
        };
        let token_route = match self.token_issuer.validate_route(&token, self.generation) {
            Ok(route) => route,
            Err(ReplyTokenRouteError::ForeignIssuer) => {
                return Err(failure(ReplyUseCommitError::ForeignIssuer, plan, token));
            }
            Err(ReplyTokenRouteError::WrongGeneration { expected, actual }) => {
                return Err(failure(
                    ReplyUseCommitError::TokenWrongGeneration { expected, actual },
                    plan,
                    token,
                ));
            }
        };
        let (token_generation, token_capability_id, token_incarnation) = token_route.into_parts();
        if token_generation != snapshot.generation
            || token_capability_id != snapshot.capability_id
            || token_incarnation != snapshot.incarnation
        {
            return Err(failure(ReplyUseCommitError::TokenPlanMismatch, plan, token));
        }
        if !Arc::ptr_eq(&self.brand, plan.brand()) {
            return Err(failure(ReplyUseCommitError::ForeignResources, plan, token));
        }
        if snapshot.generation != self.generation {
            return Err(failure(
                ReplyUseCommitError::WrongGeneration {
                    expected: self.generation,
                    actual: snapshot.generation,
                },
                plan,
                token,
            ));
        }
        let Some(entry) = self.entries.get(&snapshot.capability_id) else {
            return Err(failure(
                ReplyUseCommitError::UnknownOrTerminal {
                    capability_id: snapshot.capability_id,
                },
                plan,
                token,
            ));
        };
        if entry.incarnation != snapshot.incarnation {
            return Err(failure(
                ReplyUseCommitError::IncarnationChanged {
                    capability_id: snapshot.capability_id,
                },
                plan,
                token,
            ));
        }
        if entry.contract != snapshot.contract {
            return Err(failure(
                ReplyUseCommitError::ContractChanged {
                    capability_id: snapshot.capability_id,
                },
                plan,
                token,
            ));
        }
        if entry.state == ReplyCapabilityState::PendingPublication {
            return Err(failure(
                ReplyUseCommitError::PendingPublication {
                    capability_id: snapshot.capability_id,
                },
                plan,
                token,
            ));
        }
        if !Self::use_kind_matches_contract(entry.contract, use_kind) {
            return Err(failure(
                ReplyUseCommitError::PlanChanged {
                    capability_id: snapshot.capability_id,
                },
                plan,
                token,
            ));
        }

        let Some(entry) = self.entries.remove(&snapshot.capability_id) else {
            return Err(failure(
                ReplyUseCommitError::UnknownOrTerminal {
                    capability_id: snapshot.capability_id,
                },
                plan,
                token,
            ));
        };
        Ok(ReplyUseTerminal::Consumed {
            contract: entry.contract,
            use_kind,
        })
    }

    /// Prepares a complete Selected-session Reply reset without mutation.
    ///
    /// The caller must cross-validate every pending Delivery W=1 ticket before
    /// this preparation can become a commit. Available capabilities are also
    /// captured and will be cleared even though Delivery no longer owns them.
    pub(in crate::hsms::core::resources::publication) fn prepare_selected_session_reset(
        &self,
        authority: &PublicationMutationAuthority,
        request: ReplyClearRequest,
    ) -> Result<ReplySelectedResetPreparation, ReplyClearPrepareFailure> {
        if !self.aggregate.matches_authority(authority) {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::ForeignAggregate,
                request,
            });
        }
        if !request.matches_aggregate(&self.aggregate) {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::ForeignRequestAggregate,
                request,
            });
        }
        if !request.matches_reply_ledger(&self.identity()) {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::ForeignReplyLedger,
                request,
            });
        }
        if request.generation() != self.generation {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::WrongRequestGeneration {
                    expected: self.generation,
                    actual: request.generation(),
                },
                request,
            });
        }
        if request.scope() != ReplyClearScope::SelectedSessionReset {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::WrongRequestScope {
                    expected: ReplyClearScope::SelectedSessionReset,
                    actual: request.scope(),
                },
                request,
            });
        }
        if self.closing {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::Closing,
                request,
            });
        }
        Ok(ReplySelectedResetPreparation {
            state: self.capture_clear_state(request.into_identity()),
        })
    }

    /// Prepares a complete TCP-generation Reply clear without mutation.
    ///
    /// Preparation remains valid for an already-closed empty ledger so close
    /// can produce an idempotent scoped receipt.
    pub(in crate::hsms::core::resources::publication) fn prepare_generation_end(
        &self,
        authority: &PublicationMutationAuthority,
        request: ReplyClearRequest,
    ) -> Result<ReplyGenerationEndPreparation, ReplyClearPrepareFailure> {
        if !self.aggregate.matches_authority(authority) {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::ForeignAggregate,
                request,
            });
        }
        if !request.matches_aggregate(&self.aggregate) {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::ForeignRequestAggregate,
                request,
            });
        }
        if !request.matches_reply_ledger(&self.identity()) {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::ForeignReplyLedger,
                request,
            });
        }
        if request.generation() != self.generation {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::WrongRequestGeneration {
                    expected: self.generation,
                    actual: request.generation(),
                },
                request,
            });
        }
        if request.scope() != ReplyClearScope::GenerationEnd {
            return Err(ReplyClearPrepareFailure {
                reason: ReplyClearPrepareError::WrongRequestScope {
                    expected: ReplyClearScope::GenerationEnd,
                    actual: request.scope(),
                },
                request,
            });
        }
        Ok(ReplyGenerationEndPreparation {
            state: self.capture_clear_state(request.into_identity()),
        })
    }

    /// Commits a validated Selected-session Reply reset and returns its receipt.
    ///
    /// All validation completes before `entries.clear()`. On failure the exact
    /// move-only commit is returned and the Reply ledger remains unchanged.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn commit_selected_session_reset(
        &mut self,
        authority: &mut PublicationMutationAuthority,
        commit: ReplySelectedResetCommit,
    ) -> Result<ReplyClearReceipt, ReplyClearCommitFailure<ReplySelectedResetCommit>> {
        if let Err(reason) = self.validate_clear_commit(authority, &commit.state) {
            return Err(ReplyClearCommitFailure { reason, commit });
        }
        let state = commit.state;
        self.entries.clear();
        Ok(ReplyClearReceipt {
            scope: ReplyClearScope::SelectedSessionReset,
            aggregate: state.aggregate,
            ledger_brand: state.ledger_brand,
            generation: state.generation,
            clear_request: state.clear_request,
            began_close: false,
            summary: state.summary,
        })
    }

    /// Commits a validated generation-end Reply clear and returns its receipt.
    ///
    /// The first commit raises the permanent close fence. An already-closed
    /// empty ledger produces an idempotent receipt with `began_close=false`.
    /// Every structured failure returns the exact commit without mutation.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn commit_generation_end(
        &mut self,
        authority: &mut PublicationMutationAuthority,
        commit: ReplyGenerationEndCommit,
    ) -> Result<ReplyClearReceipt, ReplyClearCommitFailure<ReplyGenerationEndCommit>> {
        if let Err(reason) = self.validate_clear_commit(authority, &commit.state) {
            return Err(ReplyClearCommitFailure { reason, commit });
        }
        if commit.state.expected_closing && !self.entries.is_empty() {
            return Err(ReplyClearCommitFailure {
                reason: ReplyClearCommitError::ClosingWithLiveEntries {
                    live: self.entries.len(),
                },
                commit,
            });
        }

        let state = commit.state;
        let began_close = !self.closing;
        self.closing = true;
        self.entries.clear();
        Ok(ReplyClearReceipt {
            scope: ReplyClearScope::GenerationEnd,
            aggregate: state.aggregate,
            ledger_brand: state.ledger_brand,
            generation: state.generation,
            clear_request: state.clear_request,
            began_close,
            summary: state.summary,
        })
    }

    /// Revalidates that the exact opaque use variant agrees with Core authority.
    const fn use_kind_matches_contract(contract: ReplyContract, use_kind: ReplyUseKind) -> bool {
        match use_kind {
            ReplyUseKind::Normal => {
                matches!(contract.mode(), ReplyCapabilityMode::NormalSecondary { .. })
            }
            ReplyUseKind::Abort | ReplyUseKind::Abandon => true,
        }
    }

    /// Captures the complete live Reply set for later clear validation.
    fn capture_clear_state(
        &self,
        clear_request: DeliveryClearRequestIdentity,
    ) -> ReplyClearCommitState {
        let mut summary = ReplyResetSummary::default();
        let mut snapshots = Vec::with_capacity(self.entries.len());
        for (capability_id, entry) in &self.entries {
            match entry.state {
                ReplyCapabilityState::PendingPublication => {
                    summary.pending_publication += 1;
                }
                ReplyCapabilityState::Available => {
                    summary.available += 1;
                }
            }
            snapshots.push(ReplyRevocationSnapshot {
                entry: ReplyEntrySnapshot::new(
                    self.generation,
                    *capability_id,
                    entry.incarnation,
                    entry.contract,
                ),
                state: entry.state,
            });
        }
        ReplyClearCommitState {
            ledger_brand: Arc::clone(&self.brand),
            aggregate: self.aggregate.duplicate(),
            generation: self.generation,
            clear_request,
            expected_closing: self.closing,
            snapshots,
            summary,
        }
    }

    /// Validates that Delivery exposes every and only pending Reply ticket.
    fn validate_clear_tickets<'a, I>(
        state: &ReplyClearCommitState,
        tickets: I,
    ) -> Result<(), ReplyClearValidationError>
    where
        I: IntoIterator<Item = &'a ReplyPublicationTicket>,
    {
        let mut seen = BTreeSet::new();
        for ticket in tickets {
            if !Arc::ptr_eq(&state.ledger_brand, &ticket.brand) {
                return Err(ReplyClearValidationError::ForeignLedger);
            }
            if ticket.snapshot.generation != state.generation {
                return Err(ReplyClearValidationError::WrongGeneration {
                    expected: state.generation,
                    actual: ticket.snapshot.generation,
                });
            }
            let search = state
                .snapshots
                .binary_search_by_key(&ticket.snapshot.capability_id, |snapshot| {
                    snapshot.entry.capability_id
                });
            let Ok(index) = search else {
                return Err(ReplyClearValidationError::UnknownOrTerminal {
                    capability_id: ticket.snapshot.capability_id,
                });
            };
            let snapshot = &state.snapshots[index];
            if snapshot.entry.incarnation != ticket.snapshot.incarnation {
                return Err(ReplyClearValidationError::IncarnationChanged {
                    capability_id: ticket.snapshot.capability_id,
                });
            }
            if snapshot.entry.contract != ticket.snapshot.contract {
                return Err(ReplyClearValidationError::ContractChanged {
                    capability_id: ticket.snapshot.capability_id,
                });
            }
            if snapshot.state != ReplyCapabilityState::PendingPublication {
                return Err(ReplyClearValidationError::NotPendingPublication {
                    capability_id: ticket.snapshot.capability_id,
                });
            }
            if !seen.insert(ticket.snapshot.capability_id) {
                return Err(ReplyClearValidationError::DuplicateDeliveryTicket {
                    capability_id: ticket.snapshot.capability_id,
                });
            }
        }
        if let Some(missing) = state.snapshots.iter().find(|snapshot| {
            snapshot.state == ReplyCapabilityState::PendingPublication
                && !seen.contains(&snapshot.entry.capability_id)
        }) {
            return Err(ReplyClearValidationError::MissingDeliveryTicket {
                capability_id: missing.entry.capability_id,
            });
        }
        Ok(())
    }

    /// Revalidates a complete global-clear snapshot before any mutation.
    fn validate_clear_commit(
        &self,
        authority: &PublicationMutationAuthority,
        state: &ReplyClearCommitState,
    ) -> Result<(), ReplyClearCommitError> {
        if !self.aggregate.matches_authority(authority)
            || !self.aggregate.exact_eq(&state.aggregate)
        {
            return Err(ReplyClearCommitError::ForeignAggregate);
        }
        if !Arc::ptr_eq(&self.brand, &state.ledger_brand) {
            return Err(ReplyClearCommitError::ForeignLedger);
        }
        if state.generation != self.generation {
            return Err(ReplyClearCommitError::WrongGeneration {
                expected: self.generation,
                actual: state.generation,
            });
        }
        if state.expected_closing != self.closing {
            return Err(ReplyClearCommitError::ClosingStateChanged {
                expected: state.expected_closing,
                actual: self.closing,
            });
        }
        if state.snapshots.len() != self.entries.len() {
            return Err(ReplyClearCommitError::EntrySetChanged);
        }
        for (snapshot, (capability_id, entry)) in state.snapshots.iter().zip(&self.entries) {
            if snapshot.entry.capability_id != *capability_id {
                return Err(ReplyClearCommitError::EntrySetChanged);
            }
            if snapshot.entry.incarnation != entry.incarnation {
                return Err(ReplyClearCommitError::IncarnationChanged {
                    capability_id: *capability_id,
                });
            }
            if snapshot.entry.contract != entry.contract {
                return Err(ReplyClearCommitError::ContractChanged {
                    capability_id: *capability_id,
                });
            }
            if snapshot.state != entry.state {
                return Err(ReplyClearCommitError::StateChanged {
                    capability_id: *capability_id,
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        contracts::{ReplyToken, ReplyTokenIssuer},
        model::ids::{ConnectionGeneration, ReplyCapabilityId, SystemBytes},
        Function, SessionId, Stream,
    };

    use super::{
        DeliveryClearRequestIdentity, PublicationMutationAuthority, ReplyCapabilityIncarnation,
        ReplyCapabilityLedger, ReplyCapabilityMode, ReplyCapabilityState, ReplyClearRequest,
        ReplyClearScope, ReplyContract, ReplyLedgerConfigError, ReplyPublicationDecision,
        ReplyPublicationTicket, ReplyReserveError, ReplyRevocationCommitError,
        ReplyRevocationUnavailable, ReplyUseCommitError, ReplyUseKind, ReplyUsePlan,
        ReplyUseTerminal, ReplyUseUnavailable,
    };

    /// Creates a deterministic reply contract for the supplied generation and function.
    fn contract(generation: u64, function: u8) -> ReplyContract {
        ReplyContract::from_primary_parts(
            ConnectionGeneration::new(generation),
            SessionId::new(7).expect("valid Data Session ID"),
            Stream::new(3).expect("valid stream"),
            Function::new(function),
            true,
            SystemBytes::new(0x0102_0304),
        )
        .expect("odd W=1 Primary")
    }

    /// Creates a non-zero-capacity ledger owned by generation seven.
    fn ledger(capacity: usize) -> ReplyCapabilityLedger {
        let authority = PublicationMutationAuthority::for_test();
        ReplyCapabilityLedger::new(ConnectionGeneration::new(7), capacity, &authority)
            .expect("non-zero logical capacity")
    }

    /// Issues one Delivery-shaped request targeting `ledger` and `scope`.
    fn clear_request(
        ledger: &ReplyCapabilityLedger,
        authority: &PublicationMutationAuthority,
        scope: ReplyClearScope,
    ) -> ReplyClearRequest {
        let aggregate = authority.identity();
        let (_delivery_identity, reply_nonce) =
            DeliveryClearRequestIdentity::issue(&aggregate, ledger.generation(), scope);
        ReplyClearRequest::new(reply_nonce, &ledger.identity())
    }

    /// Splits one pending reservation into its exact ticket and opaque token.
    fn reserve_parts(
        ledger: &mut ReplyCapabilityLedger,
        id: u64,
        function: u8,
    ) -> (ReplyPublicationTicket, ReplyToken) {
        let capability_id = ReplyCapabilityId::new(id);
        ledger
            .reserve_pending(capability_id, contract(7, function))
            .expect("capacity available")
            .into_parts()
    }

    /// Reserves and publishes one capability, returning all exact artifacts.
    fn available(
        ledger: &mut ReplyCapabilityLedger,
        id: u64,
        function: u8,
    ) -> (ReplyCapabilityId, ReplyPublicationTicket, ReplyToken) {
        let capability_id = ReplyCapabilityId::new(id);
        let (ticket, token) = reserve_parts(ledger, id, function);
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        (capability_id, ticket, token)
    }

    /// Exhaustively matches one authorized semantic plan, verifies its
    /// read-only fields, and returns the same move-only variant for commit.
    fn verify_authorized_plan(
        plan: ReplyUsePlan,
        expected_kind: ReplyUseKind,
        expected_id: ReplyCapabilityId,
        expected_contract: ReplyContract,
    ) -> ReplyUsePlan {
        match (plan, expected_kind) {
            (ReplyUsePlan::Normal(plan), ReplyUseKind::Normal) => {
                assert_eq!(plan.generation(), ConnectionGeneration::new(7));
                assert_eq!(plan.capability_id(), expected_id);
                assert_eq!(plan.contract(), expected_contract);
                assert!(matches!(
                    plan.contract().mode(),
                    ReplyCapabilityMode::NormalSecondary { .. }
                ));
                ReplyUsePlan::Normal(plan)
            }
            (ReplyUsePlan::Abort(plan), ReplyUseKind::Abort) => {
                assert_eq!(plan.generation(), ConnectionGeneration::new(7));
                assert_eq!(plan.capability_id(), expected_id);
                assert_eq!(plan.contract(), expected_contract);
                ReplyUsePlan::Abort(plan)
            }
            (ReplyUsePlan::Abandon(plan), ReplyUseKind::Abandon) => {
                assert_eq!(plan.generation(), ConnectionGeneration::new(7));
                assert_eq!(plan.capability_id(), expected_id);
                ReplyUsePlan::Abandon(plan)
            }
            (unexpected, expected_kind) => {
                panic!("unexpected plan {unexpected:?} for {expected_kind:?}")
            }
        }
    }

    /// Commits a prepared test plan with its retained token and projects only
    /// the copyable error reason for concise equality assertions.
    fn commit_use(
        ledger: &mut ReplyCapabilityLedger,
        plan: ReplyUsePlan,
        token: ReplyToken,
    ) -> Result<ReplyUseTerminal, ReplyUseCommitError> {
        ledger
            .commit_use(plan, token)
            .map_err(|failure| failure.reason())
    }

    /// Confirms zero capacity is rejected while an extreme logical capacity
    /// does not eagerly allocate proportional memory.
    #[test]
    fn construction_is_structured_and_lazily_bounded() {
        assert!(matches!(
            ReplyCapabilityLedger::new(
                ConnectionGeneration::new(7),
                0,
                &PublicationMutationAuthority::for_test(),
            ),
            Err(ReplyLedgerConfigError::ZeroCapacity)
        ));

        let mut ledger = ledger(usize::MAX);
        assert_eq!(ledger.capacity(), usize::MAX);
        assert!(ledger.is_empty());
        let _reservation = ledger
            .reserve_pending(ReplyCapabilityId::new(1), contract(7, 1))
            .expect("one lazy tree node fits");
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms pending and available entries share one logical capacity and
    /// every failed reservation leaves existing state intact.
    #[test]
    fn capacity_counts_both_live_states_and_reservation_is_atomic() {
        let mut ledger = ledger(2);
        let first = ReplyCapabilityId::new(1);
        let second = ReplyCapabilityId::new(2);
        let _first_reservation = ledger
            .reserve_pending(first, contract(7, 1))
            .expect("first entry");
        let (second_ticket, _second_token) = ledger
            .reserve_pending(second, contract(7, 3))
            .expect("second entry")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&second_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert!(!ledger.has_capacity());

        assert!(matches!(
            ledger.reserve_pending(ReplyCapabilityId::new(3), contract(7, 5)),
            Err(ReplyReserveError::CapacityExhausted { capacity: 2 })
        ));
        assert!(matches!(
            ledger.reserve_pending(first, contract(7, 1)),
            Err(ReplyReserveError::DuplicateId { capability_id }) if capability_id == first
        ));
        assert!(matches!(
            ledger.reserve_pending(ReplyCapabilityId::new(4), contract(8, 1)),
            Err(ReplyReserveError::WrongGeneration {
                expected,
                actual,
            }) if expected == ConnectionGeneration::new(7)
                && actual == ConnectionGeneration::new(8)
        ));
        assert_eq!(ledger.len(), 2);
    }

    /// Confirms publication permits only the exact Pending-to-Available
    /// transition and treats repeats and unknown IDs without mutation.
    #[test]
    fn publication_transition_is_exact_and_idempotent() {
        let mut ledger = ledger(2);
        let capability_id = ReplyCapabilityId::new(1);
        let (ticket, _token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("pending capability")
            .into_parts();
        let foreign_authority = PublicationMutationAuthority::for_test();
        let mut foreign =
            ReplyCapabilityLedger::new(ConnectionGeneration::new(8), 1, &foreign_authority)
                .expect("foreign ledger");
        let (foreign_ticket, _foreign_token) = foreign
            .reserve_pending(capability_id, contract(8, 1))
            .expect("foreign pending capability")
            .into_parts();

        assert_eq!(
            ledger.mark_available(&foreign_ticket),
            ReplyPublicationDecision::ForeignLedger
        );
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::AlreadyAvailable
        );
        let revoke = ledger
            .prepare_revocation(&ticket)
            .expect("available capability can be revoked");
        let _terminal = ledger
            .commit_revocation(revoke)
            .expect("exact revocation succeeds");
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::UnknownOrTerminal
        );
        assert!(ledger.is_empty());
    }

    /// Confirms same-shaped tickets and revocation plans remain bound to the
    /// exact reply-ledger instance that issued them.
    #[test]
    fn same_shaped_foreign_ticket_and_revocation_plan_are_non_mutating() {
        let mut first = ledger(1);
        let mut second = ledger(1);
        let capability_id = ReplyCapabilityId::new(1);
        let (first_ticket, _first_token) = reserve_parts(&mut first, 1, 1);
        let (second_ticket, _second_token) = reserve_parts(&mut second, 1, 1);

        let first_identity = first_ticket.identity();
        assert!(!first_identity.matches(&second_ticket));
        assert_eq!(first_ticket.generation(), second_ticket.generation());
        assert_eq!(first_ticket.capability_id(), second_ticket.capability_id());

        assert_eq!(
            second.mark_available(&first_ticket),
            ReplyPublicationDecision::ForeignLedger
        );
        assert!(matches!(
            second.prepare_revocation(&first_ticket),
            Err(ReplyRevocationUnavailable::ForeignLedger)
        ));
        assert_eq!(second.len(), 1);

        let foreign_plan = first
            .prepare_revocation(&first_ticket)
            .expect("first ledger owns the exact pending ticket");
        assert_eq!(
            second.commit_revocation(foreign_plan),
            Err(ReplyRevocationCommitError::ForeignLedger)
        );
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);

        assert_eq!(
            first.mark_available(&first_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert_eq!(
            second.mark_available(&second_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        for (owner, ticket) in [(&mut first, &first_ticket), (&mut second, &second_ticket)] {
            let plan = owner
                .prepare_revocation(ticket)
                .expect("owning ledger accepts its exact ticket");
            let terminal = owner
                .commit_revocation(plan)
                .expect("owning ledger accepts its exact plan");
            assert_eq!(terminal.contract(), contract(7, 1));
        }
        assert_eq!(capability_id, ReplyCapabilityId::new(1));
        assert!(first.is_empty());
        assert!(second.is_empty());
    }

    /// Confirms every cross-call use-plan variant, including the F255 abort and
    /// abandon paths, rejects an otherwise identical foreign ledger.
    #[test]
    fn same_shaped_foreign_use_plans_are_non_mutating() {
        for (function, use_kind) in [
            (1, ReplyUseKind::Normal),
            (255, ReplyUseKind::Abort),
            (255, ReplyUseKind::Abandon),
        ] {
            let mut first = ledger(1);
            let mut second = ledger(1);
            let (capability_id, first_ticket, first_token) = available(&mut first, 1, function);
            let (_same_id, _second_ticket, second_token) = available(&mut second, 1, function);

            let foreign_authorization = first
                .prepare_use(&first_token, use_kind)
                .expect("first ledger authorizes its own token");
            let foreign_plan = verify_authorized_plan(
                foreign_authorization,
                use_kind,
                capability_id,
                contract(7, function),
            );
            let foreign_failure = second
                .commit_use(foreign_plan, second_token)
                .expect_err("same-shaped foreign plan remains ledger-branded");
            assert_eq!(
                foreign_failure.reason(),
                ReplyUseCommitError::ForeignResources
            );
            let (_reason, _foreign_plan, second_token) = foreign_failure.into_parts();
            assert_eq!(first.len(), 1);
            assert_eq!(second.len(), 1);

            let own_authorization = second
                .prepare_use(&second_token, use_kind)
                .expect("foreign rejection leaves the second entry usable");
            let own_plan = verify_authorized_plan(
                own_authorization,
                use_kind,
                capability_id,
                contract(7, function),
            );
            assert!(matches!(
                commit_use(&mut second, own_plan, second_token),
                Ok(ReplyUseTerminal::Consumed { .. })
            ));

            let cleanup = first
                .prepare_revocation(&first_ticket)
                .expect("foreign commit leaves the first entry live");
            let _terminal = first
                .commit_revocation(cleanup)
                .expect("first ledger still accepts its own revocation");
            assert!(first.is_empty());
            assert!(second.is_empty());
        }
    }

    /// Confirms pending publication cannot be planned or accidentally consumed.
    #[test]
    fn pending_capability_cannot_be_used() {
        let mut ledger = ledger(1);
        let capability_id = ReplyCapabilityId::new(1);
        let (_ticket, token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("pending capability")
            .into_parts();

        assert!(matches!(
            ledger.prepare_use(&token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::PendingPublication)
        ));
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms normal, abort, and abandon plans consume available authority
    /// once and return the exact stored contract and selected use.
    #[test]
    fn authorized_use_kinds_consume_exactly_once() {
        for (index, use_kind) in [
            ReplyUseKind::Normal,
            ReplyUseKind::Abort,
            ReplyUseKind::Abandon,
        ]
        .into_iter()
        .enumerate()
        {
            let mut ledger = ledger(1);
            let (capability_id, _ticket, token) = available(&mut ledger, index as u64 + 1, 1);
            let plan = ledger
                .prepare_use(&token, use_kind)
                .expect("available capability");
            let plan = verify_authorized_plan(plan, use_kind, capability_id, contract(7, 1));

            assert_eq!(
                commit_use(&mut ledger, plan, token),
                Ok(ReplyUseTerminal::Consumed {
                    contract: contract(7, 1),
                    use_kind,
                })
            );
            assert!(ledger.is_empty());
            assert!(ledger.has_capacity());
        }
    }

    /// Confirms F255 normal preparation is non-mutating and returns the same
    /// token ownership path for a later explicit abort.
    #[test]
    fn function_255_normal_rejection_preserves_abort_authority() {
        let mut ledger = ledger(1);
        let (capability_id, _ticket, token) = available(&mut ledger, 1, 255);
        assert!(matches!(
            ledger.prepare_use(&token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::NormalSecondaryUnavailable)
        ));
        assert_eq!(ledger.len(), 1);

        let abort = ledger
            .prepare_use(&token, ReplyUseKind::Abort)
            .expect("normal rejection leaves the original token usable");
        let abort =
            verify_authorized_plan(abort, ReplyUseKind::Abort, capability_id, contract(7, 255));
        assert!(matches!(
            commit_use(&mut ledger, abort, token),
            Ok(ReplyUseTerminal::Consumed { .. })
        ));
        assert!(ledger.is_empty());
    }

    /// Confirms F255 still authorizes both header-only abort and local abandon.
    #[test]
    fn function_255_allows_abort_and_abandon_once() {
        for use_kind in [ReplyUseKind::Abort, ReplyUseKind::Abandon] {
            let mut ledger = ledger(1);
            let (capability_id, _ticket, token) = available(&mut ledger, 1, 255);
            let plan = ledger
                .prepare_use(&token, use_kind)
                .expect("abort-only capability supports this use");
            let plan = verify_authorized_plan(plan, use_kind, capability_id, contract(7, 255));
            assert_eq!(
                commit_use(&mut ledger, plan, token),
                Ok(ReplyUseTerminal::Consumed {
                    contract: contract(7, 255),
                    use_kind,
                })
            );
            assert!(ledger.is_empty());
        }
    }

    /// Confirms a wrong-generation token and unknown identity cannot produce a
    /// plan or consume a numerically matching live capability.
    #[test]
    fn wrong_generation_and_unknown_use_are_non_mutating() {
        let mut ledger = ledger(1);
        let (capability_id, ticket, _current_token) = available(&mut ledger, 1, 1);
        let wrong_generation_token = ledger.token_issuer.issue(
            capability_id,
            ConnectionGeneration::new(8),
            ticket.snapshot.incarnation,
            true,
        );
        let unknown_token = ledger.token_issuer.issue(
            ReplyCapabilityId::new(99),
            ConnectionGeneration::new(7),
            ticket.snapshot.incarnation,
            true,
        );

        assert!(matches!(
            ledger.prepare_use(&wrong_generation_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::WrongGeneration {
                expected,
                actual,
            })
                if expected == ConnectionGeneration::new(7)
                    && actual == ConnectionGeneration::new(8)
        ));
        assert!(matches!(
            ledger.prepare_use(&unknown_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::UnknownOrTerminal)
        ));
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms a foreign issuer cannot authorize a token even when every
    /// visible and private numeric identity field equals the live reservation.
    #[test]
    fn foreign_issuer_token_with_exact_numeric_identity_is_non_mutating() {
        let mut ledger = ledger(1);
        let (capability_id, ticket, current_token) = available(&mut ledger, 1, 1);
        let foreign_issuer = ReplyTokenIssuer::new();
        let foreign_token = foreign_issuer.issue(
            capability_id,
            ConnectionGeneration::new(7),
            ticket.snapshot.incarnation,
            true,
        );

        assert!(matches!(
            ledger.prepare_use(&foreign_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::ForeignIssuer)
        ));
        assert_eq!(ledger.len(), 1);

        let current = ledger
            .prepare_use(&current_token, ReplyUseKind::Normal)
            .expect("owning issuer token remains usable");
        let current =
            verify_authorized_plan(current, ReplyUseKind::Normal, capability_id, contract(7, 1));
        assert!(matches!(
            commit_use(&mut ledger, current, current_token),
            Ok(ReplyUseTerminal::Consumed { .. })
        ));
    }

    /// Confirms an exact use plan becomes stale after explicit revocation.
    #[test]
    fn use_plan_becomes_stale_after_explicit_revocation() {
        let mut ledger = ledger(1);
        let (capability_id, ticket, token) = available(&mut ledger, 1, 1);
        let authorization = ledger
            .prepare_use(&token, ReplyUseKind::Normal)
            .expect("exact use plan");
        let stale_plan = verify_authorized_plan(
            authorization,
            ReplyUseKind::Normal,
            capability_id,
            contract(7, 1),
        );
        let revocation = ledger
            .prepare_revocation(&ticket)
            .expect("exact revocation plan");

        let _terminal = ledger
            .commit_revocation(revocation)
            .expect("revocation wins");
        assert_eq!(
            commit_use(&mut ledger, stale_plan, token),
            Err(ReplyUseCommitError::UnknownOrTerminal { capability_id })
        );
        assert!(ledger.is_empty());
    }

    /// Confirms revocation removes either live state, restores capacity, and
    /// rejects wrong-generation or duplicate requests without side effects.
    #[test]
    fn revocation_is_exact_and_terminal() {
        let mut ledger = ledger(2);
        let pending = ReplyCapabilityId::new(1);
        let published = ReplyCapabilityId::new(2);
        let (pending_ticket, _pending_token) = ledger
            .reserve_pending(pending, contract(7, 1))
            .expect("pending entry")
            .into_parts();
        let (published_ticket, _published_token) = ledger
            .reserve_pending(published, contract(7, 3))
            .expect("published entry")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&published_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        let foreign_authority = PublicationMutationAuthority::for_test();
        let mut foreign =
            ReplyCapabilityLedger::new(ConnectionGeneration::new(8), 1, &foreign_authority)
                .expect("foreign ledger");
        let (foreign_ticket, _foreign_token) = foreign
            .reserve_pending(pending, contract(8, 1))
            .expect("foreign pending entry")
            .into_parts();

        assert!(matches!(
            ledger.prepare_revocation(&foreign_ticket),
            Err(ReplyRevocationUnavailable::ForeignLedger)
        ));
        let pending_plan = ledger
            .prepare_revocation(&pending_ticket)
            .expect("pending revocation plan");
        let pending_terminal = ledger
            .commit_revocation(pending_plan)
            .expect("pending revocation commits");
        assert_eq!(pending_terminal.contract(), contract(7, 1));
        assert_eq!(
            pending_terminal.previous_state(),
            ReplyCapabilityState::PendingPublication
        );
        let published_plan = ledger
            .prepare_revocation(&published_ticket)
            .expect("available revocation plan");
        let published_terminal = ledger
            .commit_revocation(published_plan)
            .expect("available revocation commits");
        assert_eq!(published_terminal.contract(), contract(7, 3));
        assert_eq!(
            published_terminal.previous_state(),
            ReplyCapabilityState::Available
        );
        assert!(matches!(
            ledger.prepare_revocation(&published_ticket),
            Err(ReplyRevocationUnavailable::UnknownOrTerminal)
        ));
        assert!(ledger.is_empty());
        assert!(ledger.has_capacity());
    }

    /// Confirms clear cross-validation requires every pending Reply entry and
    /// returns the exact preparation so a complete retry can proceed.
    #[test]
    fn clear_ticket_validation_is_complete_and_retryable() {
        let mut authority = PublicationMutationAuthority::for_test();
        let mut ledger = ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 2, &authority)
            .expect("Reply ledger");
        let (first_ticket, _first_token) = reserve_parts(&mut ledger, 1, 1);
        let (second_ticket, _second_token) = reserve_parts(&mut ledger, 2, 3);
        let request = clear_request(&ledger, &authority, ReplyClearScope::SelectedSessionReset);
        let preparation = ledger
            .prepare_selected_session_reset(&authority, request)
            .expect("exact clear request");
        let failure = preparation
            .validate_pending_tickets([&first_ticket])
            .expect_err("second pending ticket is mandatory");
        assert_eq!(
            failure.reason(),
            super::ReplyClearValidationError::MissingDeliveryTicket {
                capability_id: ReplyCapabilityId::new(2),
            }
        );
        let (_reason, preparation) = failure.into_parts();
        let commit = preparation
            .validate_pending_tickets([&first_ticket, &second_ticket])
            .expect("complete ticket set retries exactly");
        let mut foreign_authority = PublicationMutationAuthority::for_test();
        let failure = ledger
            .commit_selected_session_reset(&mut foreign_authority, commit)
            .expect_err("foreign aggregate authority cannot commit Reply clear");
        assert_eq!(
            failure.reason(),
            super::ReplyClearCommitError::ForeignAggregate
        );
        let (_reason, commit) = failure.into_parts();
        let receipt = ledger
            .commit_selected_session_reset(&mut authority, commit)
            .expect("unchanged complete snapshot commits");
        assert_eq!(receipt.summary().pending_publication(), 2);
        assert_eq!(receipt.summary().available(), 0);
        assert!(ledger.is_empty());
    }

    /// Confirms a clear request cannot cross either publication-aggregate or
    /// exact Reply-ledger identity, and every failure returns the request.
    #[test]
    fn clear_request_rejects_foreign_aggregate_and_foreign_reply_ledger() {
        let first_authority = PublicationMutationAuthority::for_test();
        let foreign_authority = PublicationMutationAuthority::for_test();
        let first = ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 1, &first_authority)
            .expect("first Reply ledger");
        let mut same_aggregate_foreign =
            ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 1, &first_authority)
                .expect("second Reply ledger in the same aggregate");
        let mut foreign_aggregate =
            ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 1, &foreign_authority)
                .expect("Reply ledger in another aggregate");
        let _same_aggregate_reservation = same_aggregate_foreign
            .reserve_pending(ReplyCapabilityId::new(1), contract(7, 1))
            .expect("same-aggregate foreign entry");
        let _foreign_aggregate_reservation = foreign_aggregate
            .reserve_pending(ReplyCapabilityId::new(1), contract(7, 1))
            .expect("foreign-aggregate entry");

        let request = clear_request(
            &first,
            &first_authority,
            ReplyClearScope::SelectedSessionReset,
        );
        let failure = first
            .prepare_selected_session_reset(&foreign_authority, request)
            .expect_err("foreign mutation authority cannot prepare owner Reply");
        assert_eq!(
            failure.reason(),
            super::ReplyClearPrepareError::ForeignAggregate
        );
        let (_reason, request) = failure.into_parts();

        let failure = foreign_aggregate
            .prepare_selected_session_reset(&foreign_authority, request)
            .expect_err("request carries the first publication aggregate");
        assert_eq!(
            failure.reason(),
            super::ReplyClearPrepareError::ForeignRequestAggregate
        );
        let (_reason, request) = failure.into_parts();

        let failure = same_aggregate_foreign
            .prepare_selected_session_reset(&first_authority, request)
            .expect_err("request targets the first exact Reply ledger");
        assert_eq!(
            failure.reason(),
            super::ReplyClearPrepareError::ForeignReplyLedger
        );
        let (_reason, request) = failure.into_parts();

        assert!(first
            .prepare_selected_session_reset(&first_authority, request)
            .is_ok());
        assert!(first.is_empty());
        assert_eq!(same_aggregate_foreign.len(), 1);
        assert_eq!(foreign_aggregate.len(), 1);
    }

    /// Confirms reset and generation-end requests are scope-bound before any
    /// Reply clear can be prepared or committed.
    #[test]
    fn clear_request_rejects_wrong_scope_without_mutation() {
        let authority = PublicationMutationAuthority::for_test();
        let mut ledger = ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 1, &authority)
            .expect("Reply ledger");
        let _reservation = ledger
            .reserve_pending(ReplyCapabilityId::new(1), contract(7, 1))
            .expect("pending entry remains untouched");
        let request = clear_request(&ledger, &authority, ReplyClearScope::SelectedSessionReset);

        let failure = ledger
            .prepare_generation_end(&authority, request)
            .expect_err("Selected-reset request cannot prepare generation close");
        assert_eq!(
            failure.reason(),
            super::ReplyClearPrepareError::WrongRequestScope {
                expected: ReplyClearScope::GenerationEnd,
                actual: ReplyClearScope::SelectedSessionReset,
            }
        );
        let (_reason, request) = failure.into_parts();
        assert!(ledger
            .prepare_selected_session_reset(&authority, request)
            .is_ok());
        assert_eq!(ledger.len(), 1);
        assert!(!ledger.is_closing());
    }

    /// Confirms a replayed clear commit cannot remove a same-shaped later
    /// incarnation and returns its exact move-only commit without mutation.
    #[test]
    fn clear_commit_replay_is_exact_and_non_mutating() {
        let mut authority = PublicationMutationAuthority::for_test();
        let mut ledger = ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 1, &authority)
            .expect("Reply ledger");
        let capability_id = ReplyCapabilityId::new(1);
        let (original_ticket, _original_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("original pending reservation")
            .into_parts();

        let first_request =
            clear_request(&ledger, &authority, ReplyClearScope::SelectedSessionReset);
        let first_preparation = ledger
            .prepare_selected_session_reset(&authority, first_request)
            .expect("first clear preparation");
        let first_commit = first_preparation
            .validate_pending_tickets([&original_ticket])
            .expect("first ticket cross-validation");
        let stale_request =
            clear_request(&ledger, &authority, ReplyClearScope::SelectedSessionReset);
        let stale_preparation = ledger
            .prepare_selected_session_reset(&authority, stale_request)
            .expect("same-snapshot clear preparation");
        let stale_commit = stale_preparation
            .validate_pending_tickets([&original_ticket])
            .expect("same pending ticket cross-validation");

        let _receipt = ledger
            .commit_selected_session_reset(&mut authority, first_commit)
            .expect("first clear wins");
        let (current_ticket, _current_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("same-shaped later incarnation")
            .into_parts();
        let failure = ledger
            .commit_selected_session_reset(&mut authority, stale_commit)
            .expect_err("stale clear cannot consume a later incarnation");
        assert_eq!(
            failure.reason(),
            super::ReplyClearCommitError::IncarnationChanged { capability_id }
        );
        let (_reason, _stale_commit) = failure.into_parts();
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            ledger.mark_available(&current_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
    }

    /// Confirms generation end invalidates plans, reports both live states,
    /// raises a permanent closing fence, and rejects every later reservation.
    #[test]
    fn generation_end_reports_states_and_permanently_closes_ledger() {
        let mut authority = PublicationMutationAuthority::for_test();
        let mut ledger = ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 2, &authority)
            .expect("non-zero logical capacity");
        let pending = ReplyCapabilityId::new(1);
        let (pending_ticket, _pending_token) = ledger
            .reserve_pending(pending, contract(7, 1))
            .expect("pending entry")
            .into_parts();
        let (published, _published_ticket, published_token) = available(&mut ledger, 2, 3);
        let stale_authorization = ledger
            .prepare_use(&published_token, ReplyUseKind::Abort)
            .expect("available entry");
        let stale_plan = verify_authorized_plan(
            stale_authorization,
            ReplyUseKind::Abort,
            published,
            contract(7, 3),
        );

        let request = clear_request(&ledger, &authority, ReplyClearScope::GenerationEnd);
        let preparation = ledger
            .prepare_generation_end(&authority, request)
            .expect("exact generation-end request");
        let commit = preparation
            .validate_pending_tickets([&pending_ticket])
            .expect("one pending Delivery ticket");
        let receipt = ledger
            .commit_generation_end(&mut authority, commit)
            .expect("unchanged Reply snapshot commits");
        assert!(receipt.began_close());
        let summary = receipt.summary();
        assert_eq!(summary.pending_publication(), 1);
        assert_eq!(summary.available(), 1);
        assert_eq!(summary.total(), 2);
        assert!(ledger.is_empty());
        assert!(ledger.is_closing());
        assert!(!ledger.has_capacity());
        assert_eq!(
            commit_use(&mut ledger, stale_plan, published_token),
            Err(ReplyUseCommitError::UnknownOrTerminal {
                capability_id: published
            })
        );

        assert!(matches!(
            ledger.reserve_pending(ReplyCapabilityId::new(3), contract(7, 5)),
            Err(ReplyReserveError::Closing)
        ));
        assert!(ledger.is_empty());
    }

    /// Confirms stale application tokens, publication tickets, use plans, and
    /// revocation plans cannot mutate same-ID, same-contract reconstructions.
    #[test]
    fn incarnation_closes_same_id_same_contract_aba_for_every_cross_call_artifact() {
        let mut ledger = ledger(1);
        let capability_id = ReplyCapabilityId::new(41);
        let (original_ticket, stale_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("original reservation")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&original_ticket),
            ReplyPublicationDecision::MadeAvailable
        );

        let stale_revocation = ledger
            .prepare_revocation(&original_ticket)
            .expect("original stale revocation plan");
        let removal = ledger
            .prepare_revocation(&original_ticket)
            .expect("original removal plan");
        let _terminal = ledger
            .commit_revocation(removal)
            .expect("remove original reservation");

        let (second_ticket, second_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("first same-ID reconstruction")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&original_ticket),
            ReplyPublicationDecision::IncarnationChanged { capability_id }
        );
        assert!(matches!(
            ledger.prepare_use(&stale_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::IncarnationChanged { capability_id })
                if capability_id == ReplyCapabilityId::new(41)
        ));
        assert_eq!(
            ledger.mark_available(&second_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert_eq!(
            ledger.commit_revocation(stale_revocation),
            Err(ReplyRevocationCommitError::IncarnationChanged { capability_id })
        );

        let second_authorization = ledger
            .prepare_use(&second_token, ReplyUseKind::Normal)
            .expect("second reservation authorization");
        let stale_use = verify_authorized_plan(
            second_authorization,
            ReplyUseKind::Normal,
            capability_id,
            contract(7, 1),
        );
        let second_removal = ledger
            .prepare_revocation(&second_ticket)
            .expect("second reservation removal plan");
        let _terminal = ledger
            .commit_revocation(second_removal)
            .expect("remove second reservation");

        let (current_ticket, current_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("second same-ID reconstruction")
            .into_parts();
        assert_eq!(
            ledger.mark_available(&current_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        assert_eq!(
            commit_use(&mut ledger, stale_use, second_token),
            Err(ReplyUseCommitError::IncarnationChanged { capability_id })
        );
        assert_eq!(ledger.len(), 1);

        let current_authorization = ledger
            .prepare_use(&current_token, ReplyUseKind::Normal)
            .expect("current reservation remains available");
        let current_use = verify_authorized_plan(
            current_authorization,
            ReplyUseKind::Normal,
            capability_id,
            contract(7, 1),
        );
        assert!(matches!(
            commit_use(&mut ledger, current_use, current_token),
            Ok(ReplyUseTerminal::Consumed { .. })
        ));
        assert!(ledger.is_empty());
    }

    /// Confirms a revocation plan also binds the publication state observed at
    /// preparation and cannot remove an entry after that state changes.
    #[test]
    fn revocation_plan_revalidates_publication_state() {
        let mut ledger = ledger(1);
        let capability_id = ReplyCapabilityId::new(51);
        let (ticket, _token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("pending reservation")
            .into_parts();
        let stale_pending_plan = ledger
            .prepare_revocation(&ticket)
            .expect("pending revocation plan");
        assert_eq!(
            ledger.mark_available(&ticket),
            ReplyPublicationDecision::MadeAvailable
        );

        assert_eq!(
            ledger.commit_revocation(stale_pending_plan),
            Err(ReplyRevocationCommitError::StateChanged { capability_id })
        );
        let current_plan = ledger
            .prepare_revocation(&ticket)
            .expect("current available revocation plan");
        let terminal = ledger
            .commit_revocation(current_plan)
            .expect("current state commits");
        assert_eq!(terminal.previous_state(), ReplyCapabilityState::Available);
        assert!(ledger.is_empty());
    }

    /// Confirms the final representable incarnation is issued once and the
    /// following reservation fails structurally without wrapping to an old ID.
    #[test]
    fn incarnation_exhaustion_is_structured_and_never_wraps() {
        let mut ledger = ledger(1);
        ledger.next_incarnation = Some(ReplyCapabilityIncarnation::new(u64::MAX));
        let (final_ticket, _final_token) = ledger
            .reserve_pending(ReplyCapabilityId::new(61), contract(7, 1))
            .expect("final representable incarnation is usable")
            .into_parts();
        assert_eq!(final_ticket.snapshot.incarnation.get(), u64::MAX);
        assert!(ledger.next_incarnation.is_none());
        let removal = ledger
            .prepare_revocation(&final_ticket)
            .expect("final reservation revocation");
        let _terminal = ledger
            .commit_revocation(removal)
            .expect("remove final reservation");

        assert!(!ledger.has_capacity());
        assert!(matches!(
            ledger.reserve_pending(ReplyCapabilityId::new(62), contract(7, 1)),
            Err(ReplyReserveError::IncarnationExhausted)
        ));
        assert!(ledger.is_empty());
        assert!(!ledger.is_closing());
        assert!(ledger.next_incarnation.is_none());
    }

    /// Confirms a Selected-session reset clears authority without closing the
    /// ledger, and a reused ID receives a new exact token incarnation.
    #[test]
    fn selected_session_reset_revokes_authority_but_allows_new_incarnation() {
        let mut authority = PublicationMutationAuthority::for_test();
        let mut ledger = ReplyCapabilityLedger::new(ConnectionGeneration::new(7), 2, &authority)
            .expect("non-zero logical capacity");
        let (old_ticket, stale_token) = ledger
            .reserve_pending(ReplyCapabilityId::new(1), contract(7, 1))
            .expect("pending entry")
            .into_parts();
        let (_second_id, _second_ticket, _second_token) = available(&mut ledger, 2, 255);

        let request = clear_request(&ledger, &authority, ReplyClearScope::SelectedSessionReset);
        let preparation = ledger
            .prepare_selected_session_reset(&authority, request)
            .expect("exact Selected-reset request");
        let commit = preparation
            .validate_pending_tickets([&old_ticket])
            .expect("one pending Delivery ticket");
        let receipt = ledger
            .commit_selected_session_reset(&mut authority, commit)
            .expect("unchanged Reply snapshot commits");
        let summary = receipt.summary();
        assert_eq!(summary.pending_publication(), 1);
        assert_eq!(summary.available(), 1);
        assert_eq!(ledger.generation(), ConnectionGeneration::new(7));
        assert!(ledger.is_empty());
        assert!(!ledger.is_closing());
        assert!(ledger.has_capacity());

        let capability_id = ReplyCapabilityId::new(1);
        let (current_ticket, current_token) = ledger
            .reserve_pending(capability_id, contract(7, 1))
            .expect("selected reset keeps ledger reusable")
            .into_parts();
        assert!(matches!(
            ledger.prepare_use(&stale_token, ReplyUseKind::Normal),
            Err(ReplyUseUnavailable::IncarnationChanged { capability_id })
                if capability_id == ReplyCapabilityId::new(1)
        ));
        assert_eq!(
            ledger.mark_available(&current_ticket),
            ReplyPublicationDecision::MadeAvailable
        );
        let current = ledger
            .prepare_use(&current_token, ReplyUseKind::Normal)
            .expect("new incarnation is usable");
        let current =
            verify_authorized_plan(current, ReplyUseKind::Normal, capability_id, contract(7, 1));
        assert!(matches!(
            commit_use(&mut ledger, current, current_token),
            Ok(ReplyUseTerminal::Consumed { .. })
        ));
    }
}
