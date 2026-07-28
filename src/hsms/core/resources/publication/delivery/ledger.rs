//! Owns exact, bounded Delivery correlation inside `PublicationResources`.
//!
//! This generation-scoped Sans-I/O ledger retains each move-only delivery
//! binding until application publication reaches an exact terminal transition.
//! W=1 bindings own the corresponding reply-publication ticket, allowing the
//! parent `PublicationResources` aggregate to mutate Reply state before
//! committing the Delivery transition. Every mutation is aggregate-gated.
//! Reset and close additionally require a one-shot, request-bound global Reply
//! clear receipt, and allocate their disposition buffers before either ledger
//! begins mutation.

use std::{collections::BTreeMap, sync::Arc};

use crate::hsms::{
    contracts::{ApplicationDeliveryResult, DeliveryPurpose},
    model::ids::{ConnectionGeneration, DeliveryId},
};

use super::super::{
    authority::{
        DeliveryClearRequestIdentity, PublicationAggregateIdentity, PublicationMutationAuthority,
    },
    reply::{
        ReplyClearReceipt, ReplyClearRequest, ReplyClearScope, ReplyLedgerIdentity,
        ReplyPublicationTicket, ReplyPublicationTicketIdentity,
    },
};

/// Failure constructing a logically bounded application-delivery ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum DeliveryLedgerConfigError {
    /// A zero bound could never admit a reliable publication attempt.
    ZeroCapacity,
    /// The intended Reply ledger belongs to another publication aggregate.
    ForeignReplyAggregate,
    /// The intended Reply ledger belongs to another TCP generation.
    ReplyGenerationMismatch {
        /// Generation assigned to this Delivery ledger.
        expected: ConnectionGeneration,
        /// Generation owned by the intended Reply ledger.
        actual: ConnectionGeneration,
    },
}

/// Exact resource binding retained for one application publication.
///
/// The value is move-only because a W=1 variant owns the sole exact
/// [`ReplyPublicationTicket`] needed to publish or revoke reply authority.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a delivery binding may own an exact reply-publication ticket"]
pub(in crate::hsms::core::resources::publication) enum DeliveryBinding {
    /// Inbound W=0 Primary carrying no reply authority.
    InboundPrimaryW0,
    /// Inbound W=1 Primary whose exact reply reservation remains pending.
    InboundPrimaryW1 {
        /// Ticket borrowed for Reply preflight before Delivery commit.
        ticket: ReplyPublicationTicket,
    },
    /// Non-data protocol diagnostic that survives Selected-session reset.
    ProtocolNotice,
}

impl DeliveryBinding {
    /// Projects this owned binding into its copyable diagnostic purpose.
    ///
    /// The returned value never replaces the exact W=1 ticket retained here.
    pub(in crate::hsms::core::resources::publication) fn purpose(&self) -> DeliveryPurpose {
        match self {
            Self::InboundPrimaryW0 => DeliveryPurpose::InboundPrimary,
            Self::InboundPrimaryW1 { ticket } => {
                DeliveryPurpose::InboundReplyCapability(ticket.capability_id())
            }
            Self::ProtocolNotice => DeliveryPurpose::ProtocolNotice,
        }
    }

    /// Borrows the exact reply-publication ticket for a W=1 binding.
    ///
    /// Returns `None` for W=0 Data and protocol-notice publications.
    pub(in crate::hsms::core::resources::publication) const fn reply_ticket(
        &self,
    ) -> Option<&ReplyPublicationTicket> {
        match self {
            Self::InboundPrimaryW1 { ticket } => Some(ticket),
            Self::InboundPrimaryW0 | Self::ProtocolNotice => None,
        }
    }

    /// Returns whether this binding belongs to Selected-session Data.
    const fn is_selected_session_data(&self) -> bool {
        matches!(self, Self::InboundPrimaryW0 | Self::InboundPrimaryW1 { .. })
    }

    /// Captures the immutable binding fields required for exact commit validation.
    fn identity(&self) -> DeliveryBindingIdentity {
        match self {
            Self::InboundPrimaryW0 => DeliveryBindingIdentity::InboundPrimaryW0,
            Self::InboundPrimaryW1 { ticket } => DeliveryBindingIdentity::InboundPrimaryW1 {
                ticket: ticket.identity(),
            },
            Self::ProtocolNotice => DeliveryBindingIdentity::ProtocolNotice,
        }
    }
}

/// Copyable identity used only to revalidate one retained move-only binding.
#[derive(Debug)]
enum DeliveryBindingIdentity {
    /// Exact shape of an inbound W=0 publication.
    InboundPrimaryW0,
    /// Publicly observable identity of an exact W=1 publication ticket.
    InboundPrimaryW1 {
        /// Opaque full identity of the retained reply-publication ticket.
        ticket: ReplyPublicationTicketIdentity,
    },
    /// Exact shape of a protocol-notice publication.
    ProtocolNotice,
}

impl DeliveryBindingIdentity {
    /// Returns whether this identity belongs to Selected-session Data.
    const fn is_selected_session_data(&self) -> bool {
        matches!(self, Self::InboundPrimaryW0 | Self::InboundPrimaryW1 { .. })
    }

    /// Returns whether `binding` exactly matches this captured observation.
    ///
    /// W=1 matching includes reply-ledger pointer identity plus the complete
    /// generation, capability ID, incarnation, and contract snapshot.
    fn matches(&self, binding: &DeliveryBinding) -> bool {
        match (self, binding) {
            (Self::InboundPrimaryW0, DeliveryBinding::InboundPrimaryW0)
            | (Self::ProtocolNotice, DeliveryBinding::ProtocolNotice) => true,
            (
                Self::InboundPrimaryW1 { ticket: identity },
                DeliveryBinding::InboundPrimaryW1 { ticket },
            ) => identity.matches(ticket),
            _ => false,
        }
    }
}

/// Never-reused identity of one exact entry in this ledger instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeliveryIncarnation(
    /// Monotonic non-zero value that permanently stops after `u64::MAX`.
    u64,
);

impl DeliveryIncarnation {
    /// Wraps the next internally allocated incarnation value.
    const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Returns the numeric incarnation for checked advancement.
    const fn get(self) -> u64 {
        self.0
    }
}

/// One pending application publication and its exact cross-ledger binding.
#[derive(Debug, PartialEq, Eq)]
struct DeliveryEntry {
    /// Never-reused incarnation captured by every prepare token.
    incarnation: DeliveryIncarnation,
    /// Move-only W=0, W=1, or protocol-notice resource binding.
    binding: DeliveryBinding,
}

/// Private allocation whose pointer identity brands one ledger instance.
#[derive(Debug)]
struct DeliveryLedgerBrand {
    /// Private field preventing structural construction outside this module.
    private: (),
}

/// Stable reason an attempted registration did not mutate the ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum DeliveryRegisterError {
    /// The authority belongs to a different publication aggregate.
    ForeignAggregate,
    /// The preparation was created by a different Delivery ledger.
    ForeignLedger,
    /// Generation close permanently fenced all later registrations.
    Closing,
    /// The registration envelope belongs to a different TCP generation.
    WrongGeneration {
        /// Generation exclusively owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation supplied by the attempted registration.
        actual: ConnectionGeneration,
    },
    /// A W=1 ticket belongs to a different TCP generation.
    ReplyTicketWrongGeneration {
        /// Generation exclusively owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation embedded in the supplied reply ticket.
        actual: ConnectionGeneration,
    },
    /// A W=1 ticket was issued by another Reply ledger in this aggregate.
    ReplyTicketForeignLedger,
    /// The supplied identity already names a pending delivery.
    DuplicateId {
        /// Live identity that cannot be registered twice.
        delivery_id: DeliveryId,
    },
    /// The supplied identity is lower than or equal to a previously accepted ID.
    NonMonotonicOrReusedId {
        /// Greatest identity ever accepted by this ledger.
        highest_registered_id: DeliveryId,
        /// Stale, reordered, or reused identity that was rejected.
        attempted_id: DeliveryId,
    },
    /// Pending deliveries already occupy the configured logical bound.
    CapacityExhausted {
        /// Maximum number of simultaneously pending deliveries.
        capacity: usize,
    },
    /// Every representable delivery-entry incarnation has already been issued.
    IncarnationExhausted,
    /// Delivery admission state changed after read-only preparation.
    AdmissionStateChanged {
        /// Identity whose frozen admission conditions no longer hold.
        delivery_id: DeliveryId,
    },
}

/// Read-only admission proof for one future Delivery registration.
///
/// W=1 integration prepares this value before reserving Reply authority. The
/// parent aggregate then reserves the reply capability and immediately commits
/// this unchanged plan, avoiding a normal-path compensating transaction.
#[derive(Debug)]
#[must_use = "registration preparation must be committed or deliberately discarded"]
pub(in crate::hsms::core::resources::publication) struct DeliveryRegistrationPreparation {
    /// Private brand of the exact Delivery ledger that prepared admission.
    brand: Arc<DeliveryLedgerBrand>,
    /// Publication aggregate observed during admission preparation.
    aggregate: PublicationAggregateIdentity,
    /// TCP generation frozen by admission preparation.
    generation: ConnectionGeneration,
    /// Delivery identity frozen by admission preparation.
    delivery_id: DeliveryId,
    /// Open-versus-closing state observed before Reply reservation.
    expected_closing: bool,
    /// Greatest accepted ID observed before Reply reservation.
    expected_highest_registered_id: Option<DeliveryId>,
    /// Pending-entry count observed before Reply reservation.
    expected_len: usize,
    /// Logical admission capacity observed before Reply reservation.
    expected_capacity: usize,
    /// Exact fresh incarnation reserved logically by this plan.
    incarnation: DeliveryIncarnation,
}

impl DeliveryRegistrationPreparation {
    /// Returns the generation frozen for this registration.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the Delivery identity frozen for this registration.
    pub(in crate::hsms::core::resources::publication) const fn delivery_id(&self) -> DeliveryId {
        self.delivery_id
    }
}

/// Move-only registration failure that returns the exact rejected binding.
#[derive(Debug)]
#[must_use = "a rejected binding may contain a reply ticket that must be revoked or retried"]
pub(in crate::hsms::core::resources::publication) struct DeliveryRegisterRejection {
    /// Stable reason no pending delivery was inserted.
    reason: DeliveryRegisterError,
    /// Exact frozen admission plan returned without mutation.
    preparation: DeliveryRegistrationPreparation,
    /// Exact caller-owned binding returned without alteration.
    binding: DeliveryBinding,
}

impl DeliveryRegisterRejection {
    /// Creates a rejection from its reason, unchanged plan, and binding.
    const fn new(
        reason: DeliveryRegisterError,
        preparation: DeliveryRegistrationPreparation,
        binding: DeliveryBinding,
    ) -> Self {
        Self {
            reason,
            preparation,
            binding,
        }
    }

    /// Returns the copyable reason registration was rejected.
    pub(in crate::hsms::core::resources::publication) const fn reason(
        &self,
    ) -> DeliveryRegisterError {
        self.reason
    }

    /// Borrows the exact binding returned to the caller.
    pub(in crate::hsms::core::resources::publication) const fn binding(&self) -> &DeliveryBinding {
        &self.binding
    }

    /// Consumes the rejection into its reason, unchanged plan, and binding.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (
        DeliveryRegisterError,
        DeliveryRegistrationPreparation,
        DeliveryBinding,
    ) {
        (self.reason, self.preparation, self.binding)
    }
}

/// Failure preparing completion of one exact pending delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum DeliveryPrepareError {
    /// The completion belongs to a different TCP generation.
    WrongGeneration {
        /// Generation exclusively owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation supplied by the stale completion.
        actual: ConnectionGeneration,
    },
    /// The delivery is unknown, already finished, reset, or drained by close.
    UnknownOrTerminal {
        /// Identity that no longer names a pending delivery.
        delivery_id: DeliveryId,
    },
}

/// Failure revalidating an opaque single-delivery or batch commit token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum DeliveryCommitError {
    /// The authority belongs to a different publication aggregate.
    ForeignAggregate,
    /// The token was prepared by a different delivery-ledger instance.
    ForeignLedger,
    /// The token belongs to a different TCP generation.
    WrongGeneration {
        /// Generation exclusively owned by this ledger.
        expected: ConnectionGeneration,
        /// Generation captured by the commit token.
        actual: ConnectionGeneration,
    },
    /// The exact delivery disappeared before commit.
    UnknownOrTerminal {
        /// Identity that no longer names a pending delivery.
        delivery_id: DeliveryId,
    },
    /// The identity now names an entry with a different incarnation.
    IncarnationChanged {
        /// Delivery whose exact entry failed revalidation.
        delivery_id: DeliveryId,
    },
    /// The entry's retained W=0, W=1, or notice binding changed.
    BindingChanged {
        /// Delivery whose binding failed exact semantic revalidation.
        delivery_id: DeliveryId,
    },
    /// A batch plan no longer describes the complete current entry set.
    EntrySetChanged,
    /// Open-versus-closing state changed after batch preparation.
    ClosingStateChanged {
        /// Closing state captured during preparation.
        expected: bool,
        /// Closing state observed during commit.
        actual: bool,
    },
    /// A previously closed ledger unexpectedly retained pending entries.
    ClosingWithPendingEntries {
        /// Number of impossible pending entries retained behind the close fence.
        pending: usize,
    },
}

/// Ownership-preserving failure from a Delivery commit.
#[derive(Debug)]
#[must_use = "recover or deliberately discard the unchanged Delivery commit"]
pub(in crate::hsms::core::resources::publication) struct DeliveryCommitFailure<C> {
    /// Structured reason exact commit revalidation failed.
    reason: DeliveryCommitError,
    /// Exact move-only commit returned without mutation.
    commit: C,
}

impl<C> DeliveryCommitFailure<C> {
    /// Returns the copyable commit failure reason.
    pub(in crate::hsms::core::resources::publication) const fn reason(
        &self,
    ) -> DeliveryCommitError {
        self.reason
    }

    /// Consumes the failure into its reason and unchanged commit.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (DeliveryCommitError, C) {
        (self.reason, self.commit)
    }
}

/// Failure authorizing a prepared Delivery drain with a Reply clear receipt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::hsms::core::resources::publication) enum DeliveryClearAuthorizationError {
    /// The receipt belongs to a different publication aggregate.
    ForeignAggregate,
    /// The receipt belongs to a different TCP generation.
    WrongGeneration {
        /// Generation captured by Delivery preparation.
        expected: ConnectionGeneration,
        /// Generation proven by the Reply clear receipt.
        actual: ConnectionGeneration,
    },
    /// The receipt proves the wrong reset-versus-close semantic scope.
    WrongScope {
        /// Scope required by the prepared Delivery drain.
        expected: ReplyClearScope,
        /// Scope proven by the supplied Reply receipt.
        actual: ReplyClearScope,
    },
    /// The receipt was produced by another Reply ledger in this aggregate.
    ForeignReplyLedger,
    /// The receipt answers another same-shaped Delivery preparation.
    RequestMismatch,
}

/// Ownership-preserving Reply-clear authorization failure.
#[derive(Debug)]
#[must_use = "recover or deliberately discard the unchanged Delivery preparation"]
pub(in crate::hsms::core::resources::publication) struct DeliveryClearAuthorizationFailure<P> {
    /// Structured reason the Reply receipt could not authorize Delivery.
    reason: DeliveryClearAuthorizationError,
    /// Exact move-only Delivery preparation returned without mutation.
    preparation: P,
    /// Exact move-only Reply receipt returned without mutation.
    receipt: ReplyClearReceipt,
}

impl<P> DeliveryClearAuthorizationFailure<P> {
    /// Returns the copyable authorization failure reason.
    pub(in crate::hsms::core::resources::publication) const fn reason(
        &self,
    ) -> DeliveryClearAuthorizationError {
        self.reason
    }

    /// Consumes the failure into its reason, preparation, and Reply receipt.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (DeliveryClearAuthorizationError, P, ReplyClearReceipt) {
        (self.reason, self.preparation, self.receipt)
    }
}

/// Exact immutable fields captured before a later Delivery mutation.
#[derive(Debug)]
struct DeliveryEntrySnapshot {
    /// Generation that owned the entry during preparation.
    generation: ConnectionGeneration,
    /// Exact delivery identity observed during preparation.
    delivery_id: DeliveryId,
    /// Never-reused entry incarnation observed during preparation.
    incarnation: DeliveryIncarnation,
    /// Semantic identity of the retained move-only binding.
    binding: DeliveryBindingIdentity,
}

/// Read-only preparation for one application-delivery completion.
///
/// The borrowed binding exposes its exact W=1 ticket for Reply preflight while
/// preventing Delivery mutation until this value is consumed into a commit token.
#[must_use = "finish preparation must be inspected and converted into one commit token"]
pub(in crate::hsms::core::resources::publication) struct DeliveryFinishPreparation<'a> {
    /// Private ledger-instance brand copied into the opaque commit token.
    brand: Arc<DeliveryLedgerBrand>,
    /// Exact entry fields that commit must revalidate.
    snapshot: DeliveryEntrySnapshot,
    /// Runtime-neutral publication outcome to retain in the terminal.
    result: ApplicationDeliveryResult,
    /// Borrowed move-only binding that remains owned by the ledger.
    binding: &'a DeliveryBinding,
}

impl DeliveryFinishPreparation<'_> {
    /// Returns the exact delivery identity being prepared.
    pub(in crate::hsms::core::resources::publication) const fn delivery_id(&self) -> DeliveryId {
        self.snapshot.delivery_id
    }

    /// Returns the copyable purpose projected from the retained binding.
    pub(in crate::hsms::core::resources::publication) fn purpose(&self) -> DeliveryPurpose {
        self.binding.purpose()
    }

    /// Returns the runtime-neutral result that will become terminal.
    pub(in crate::hsms::core::resources::publication) const fn result(
        &self,
    ) -> ApplicationDeliveryResult {
        self.result
    }

    /// Borrows the exact W=1 reply ticket for Reply preflight.
    ///
    /// Returns `None` for W=0 and protocol-notice publications.
    pub(in crate::hsms::core::resources::publication) const fn reply_ticket(
        &self,
    ) -> Option<&ReplyPublicationTicket> {
        self.binding.reply_ticket()
    }

    /// Consumes all read-only borrows into one opaque exact commit token.
    pub(in crate::hsms::core::resources::publication) fn into_commit(self) -> DeliveryFinishCommit {
        DeliveryFinishCommit {
            brand: self.brand,
            snapshot: self.snapshot,
            result: self.result,
        }
    }
}

/// Opaque move-only token authorizing one exact prepared finish.
#[derive(Debug)]
#[must_use = "a prepared finish token must be committed or deliberately discarded"]
pub(in crate::hsms::core::resources::publication) struct DeliveryFinishCommit {
    /// Private brand of the ledger that prepared this token.
    brand: Arc<DeliveryLedgerBrand>,
    /// Exact entry fields that commit must revalidate.
    snapshot: DeliveryEntrySnapshot,
    /// Runtime-neutral publication result captured during preparation.
    result: ApplicationDeliveryResult,
}

/// Move-only successful completion of one exact application delivery.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a delivery terminal must drive its binding-specific aggregate transition"]
pub(in crate::hsms::core::resources::publication) struct DeliveryTerminal {
    /// TCP generation whose exact entry reached its terminal result.
    generation: ConnectionGeneration,
    /// Unique identity removed by this completion.
    delivery_id: DeliveryId,
    /// Exact W=0, W=1, or notice binding removed by commit.
    binding: DeliveryBinding,
    /// Delivered, full, or closed runtime-neutral outcome.
    result: ApplicationDeliveryResult,
}

impl DeliveryTerminal {
    /// Returns the copyable purpose projected from the owned binding.
    pub(in crate::hsms::core::resources::publication) fn purpose(&self) -> DeliveryPurpose {
        self.binding.purpose()
    }

    /// Consumes the terminal into generation, identity, binding, and result.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (
        ConnectionGeneration,
        DeliveryId,
        DeliveryBinding,
        ApplicationDeliveryResult,
    ) {
        (self.generation, self.delivery_id, self.binding, self.result)
    }
}

/// Read-only view of one delivery targeted by a prepared batch drain.
#[derive(Debug)]
pub(in crate::hsms::core::resources::publication) struct PreparedDeliveryView<'a> {
    /// Exact delivery identity exposed in ascending order.
    delivery_id: DeliveryId,
    /// Borrowed binding that remains owned by the ledger until commit.
    binding: &'a DeliveryBinding,
}

impl PreparedDeliveryView<'_> {
    /// Returns the exact delivery identity represented by this view.
    pub(in crate::hsms::core::resources::publication) const fn delivery_id(&self) -> DeliveryId {
        self.delivery_id
    }

    /// Returns the copyable purpose projected from the retained binding.
    pub(in crate::hsms::core::resources::publication) fn purpose(&self) -> DeliveryPurpose {
        self.binding.purpose()
    }

    /// Borrows the exact W=1 reply ticket for Reply preflight.
    ///
    /// Returns `None` for W=0 and protocol-notice publications.
    pub(in crate::hsms::core::resources::publication) const fn reply_ticket(
        &self,
    ) -> Option<&ReplyPublicationTicket> {
        self.binding.reply_ticket()
    }
}

/// Shared opaque fields captured by a Selected-reset or generation-close plan.
#[derive(Debug)]
struct DeliveryDrainCommitState {
    /// Private brand of the ledger that prepared this batch.
    brand: Arc<DeliveryLedgerBrand>,
    /// Publication aggregate that owns the prepared Delivery ledger.
    aggregate: PublicationAggregateIdentity,
    /// Generation that owned every captured entry.
    generation: ConnectionGeneration,
    /// Exact intended Reply ledger frozen at Delivery construction.
    reply_ledger_identity: ReplyLedgerIdentity,
    /// Per-preparation nonce that the matching Reply receipt must answer.
    clear_request: DeliveryClearRequestIdentity,
    /// Open-versus-closing state observed during preparation.
    expected_closing: bool,
    /// Complete entry-set snapshot in ascending `DeliveryId` order.
    snapshots: Vec<DeliveryEntrySnapshot>,
    /// Empty output buffer allocated before either cross-ledger commit.
    dispositions: Vec<DeliveryDisposition>,
}

/// Read-only batch preparation for a Selected-session reset.
#[must_use = "reset preparation must preflight Reply state before Delivery commit"]
#[derive(Debug)]
pub(in crate::hsms::core::resources::publication) struct DeliveryResetPreparation<'a> {
    /// Opaque exact state consumed into the reset commit token.
    state: DeliveryDrainCommitState,
    /// Data deliveries targeted by reset in ascending identity order.
    deliveries: Vec<PreparedDeliveryView<'a>>,
}

impl<'a> DeliveryResetPreparation<'a> {
    /// Returns whether no Selected-session Data delivery is targeted.
    pub(in crate::hsms::core::resources::publication) fn is_empty(&self) -> bool {
        self.deliveries.is_empty()
    }

    /// Returns the number of Data deliveries targeted by this reset.
    pub(in crate::hsms::core::resources::publication) fn len(&self) -> usize {
        self.deliveries.len()
    }

    /// Borrows all targeted deliveries in ascending `DeliveryId` order.
    pub(in crate::hsms::core::resources::publication) fn deliveries(
        &self,
    ) -> &[PreparedDeliveryView<'_>] {
        &self.deliveries
    }

    /// Iterates over every pending W=1 ticket for global Reply cross-validation.
    pub(in crate::hsms::core::resources::publication) fn reply_tickets(
        &self,
    ) -> impl Iterator<Item = &ReplyPublicationTicket> + '_ {
        self.deliveries
            .iter()
            .filter_map(|delivery| delivery.reply_ticket())
    }

    /// Authorizes this reset with one completed global Reply clear.
    ///
    /// Only a same-aggregate, same-generation Selected-reset receipt from the
    /// permanently associated Reply ledger can produce a commit token.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn authorize_reply_clear(
        self,
        receipt: ReplyClearReceipt,
    ) -> Result<DeliveryResetCommit, DeliveryClearAuthorizationFailure<DeliveryResetPreparation<'a>>>
    {
        match ApplicationDeliveryLedger::validate_clear_receipt(
            &self.state,
            &receipt,
            ReplyClearScope::SelectedSessionReset,
        ) {
            Ok(()) => Ok(DeliveryResetCommit { state: self.state }),
            Err(reason) => Err(DeliveryClearAuthorizationFailure {
                reason,
                preparation: self,
                receipt,
            }),
        }
    }
}

/// Opaque move-only token authorizing one exact Selected-session reset.
#[derive(Debug)]
#[must_use = "a prepared reset token must be committed or deliberately discarded"]
pub(in crate::hsms::core::resources::publication) struct DeliveryResetCommit {
    /// Complete ledger snapshot captured by reset preparation.
    state: DeliveryDrainCommitState,
}

/// Read-only batch preparation for permanent generation close.
#[must_use = "close preparation must preflight Reply state before Delivery commit"]
#[derive(Debug)]
pub(in crate::hsms::core::resources::publication) struct DeliveryClosePreparation<'a> {
    /// Opaque exact state consumed into the close commit token.
    state: DeliveryDrainCommitState,
    /// All pending deliveries in ascending identity order.
    deliveries: Vec<PreparedDeliveryView<'a>>,
}

impl<'a> DeliveryClosePreparation<'a> {
    /// Returns whether this close preparation targets no pending delivery.
    pub(in crate::hsms::core::resources::publication) fn is_empty(&self) -> bool {
        self.deliveries.is_empty()
    }

    /// Returns the number of pending deliveries targeted by this close.
    pub(in crate::hsms::core::resources::publication) fn len(&self) -> usize {
        self.deliveries.len()
    }

    /// Borrows all targeted deliveries in ascending `DeliveryId` order.
    pub(in crate::hsms::core::resources::publication) fn deliveries(
        &self,
    ) -> &[PreparedDeliveryView<'_>] {
        &self.deliveries
    }

    /// Iterates over every pending W=1 ticket for global Reply cross-validation.
    pub(in crate::hsms::core::resources::publication) fn reply_tickets(
        &self,
    ) -> impl Iterator<Item = &ReplyPublicationTicket> + '_ {
        self.deliveries
            .iter()
            .filter_map(|delivery| delivery.reply_ticket())
    }

    /// Authorizes this close with one completed global Reply clear.
    ///
    /// Only a same-aggregate, same-generation generation-end receipt from the
    /// permanently associated Reply ledger can produce a commit token.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn authorize_reply_clear(
        self,
        receipt: ReplyClearReceipt,
    ) -> Result<DeliveryCloseCommit, DeliveryClearAuthorizationFailure<DeliveryClosePreparation<'a>>>
    {
        match ApplicationDeliveryLedger::validate_clear_receipt(
            &self.state,
            &receipt,
            ReplyClearScope::GenerationEnd,
        ) {
            Ok(()) => Ok(DeliveryCloseCommit { state: self.state }),
            Err(reason) => Err(DeliveryClearAuthorizationFailure {
                reason,
                preparation: self,
                receipt,
            }),
        }
    }
}

/// Opaque move-only token authorizing one exact generation close.
#[derive(Debug)]
#[must_use = "a prepared close token must be committed or deliberately discarded"]
pub(in crate::hsms::core::resources::publication) struct DeliveryCloseCommit {
    /// Complete ledger snapshot captured by close preparation.
    state: DeliveryDrainCommitState,
}

/// Move-only pending delivery removed by reset or generation close.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "a drained delivery binding must complete aggregate cleanup"]
pub(in crate::hsms::core::resources::publication) struct DeliveryDisposition {
    /// Identity removed from the pending-delivery map.
    delivery_id: DeliveryId,
    /// Exact W=0, W=1, or notice binding removed by the drain.
    binding: DeliveryBinding,
}

impl DeliveryDisposition {
    /// Returns the copyable purpose projected from the owned binding.
    pub(in crate::hsms::core::resources::publication) fn purpose(&self) -> DeliveryPurpose {
        self.binding.purpose()
    }

    /// Consumes the disposition into its exact identity and binding.
    pub(in crate::hsms::core::resources::publication) fn into_parts(
        self,
    ) -> (DeliveryId, DeliveryBinding) {
        (self.delivery_id, self.binding)
    }
}

/// Data-delivery dispositions committed when a Selected session ends.
#[derive(Debug, Default, PartialEq, Eq)]
#[must_use = "session-reset dispositions must complete aggregate cleanup"]
pub(in crate::hsms::core::resources::publication) struct DeliveryResetSummary {
    /// Removed Data bindings in ascending `DeliveryId` order.
    deliveries: Vec<DeliveryDisposition>,
}

impl DeliveryResetSummary {
    /// Returns whether the reset removed no Selected-session Data delivery.
    pub(in crate::hsms::core::resources::publication) fn is_empty(&self) -> bool {
        self.deliveries.is_empty()
    }

    /// Returns the number of Data deliveries removed by this reset.
    pub(in crate::hsms::core::resources::publication) fn len(&self) -> usize {
        self.deliveries.len()
    }

    /// Borrows committed dispositions in ascending `DeliveryId` order.
    pub(in crate::hsms::core::resources::publication) fn deliveries(
        &self,
    ) -> &[DeliveryDisposition] {
        &self.deliveries
    }

    /// Consumes the summary into ascending-identity reset dispositions.
    pub(in crate::hsms::core::resources::publication) fn into_deliveries(
        self,
    ) -> Vec<DeliveryDisposition> {
        self.deliveries
    }
}

/// Idempotent result of committing permanent delivery-ledger close.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "close dispositions must complete aggregate cleanup"]
pub(in crate::hsms::core::resources::publication) struct DeliveryCloseSummary {
    /// Whether this commit raised the permanent generation-close fence.
    began_close: bool,
    /// All removed bindings in ascending `DeliveryId` order.
    deliveries: Vec<DeliveryDisposition>,
}

impl DeliveryCloseSummary {
    /// Returns whether this commit performed the open-to-closing transition.
    pub(in crate::hsms::core::resources::publication) const fn began_close(&self) -> bool {
        self.began_close
    }

    /// Returns whether this close commit removed no pending delivery.
    pub(in crate::hsms::core::resources::publication) fn is_empty(&self) -> bool {
        self.deliveries.is_empty()
    }

    /// Returns the number of pending deliveries removed by this commit.
    pub(in crate::hsms::core::resources::publication) fn len(&self) -> usize {
        self.deliveries.len()
    }

    /// Borrows committed dispositions in ascending `DeliveryId` order.
    pub(in crate::hsms::core::resources::publication) fn deliveries(
        &self,
    ) -> &[DeliveryDisposition] {
        &self.deliveries
    }

    /// Consumes the summary into ascending-identity close dispositions.
    pub(in crate::hsms::core::resources::publication) fn into_deliveries(
        self,
    ) -> Vec<DeliveryDisposition> {
        self.deliveries
    }
}

/// Bounded owner of pending application-delivery bindings for one generation.
pub(in crate::hsms::core::resources::publication) struct ApplicationDeliveryLedger {
    /// TCP generation whose publications may be registered and completed.
    generation: ConnectionGeneration,
    /// Logical maximum number of simultaneously pending delivery attempts.
    capacity: usize,
    /// Exact publication aggregate that owns every Delivery mutation.
    aggregate: PublicationAggregateIdentity,
    /// Unforgeable pointer brand distinguishing this exact ledger instance.
    brand: Arc<DeliveryLedgerBrand>,
    /// Permanent fence raised when the owning TCP generation starts closing.
    closing: bool,
    /// Greatest Delivery ID ever successfully registered, including terminals.
    highest_registered_id: Option<DeliveryId>,
    /// Incarnation issued on the next successful registration, if representable.
    next_incarnation: Option<DeliveryIncarnation>,
    /// Exact intended Reply ledger frozen at Delivery construction.
    reply_ledger_identity: ReplyLedgerIdentity,
    /// Lazily allocated unique pending entries ordered by `DeliveryId`.
    entries: BTreeMap<DeliveryId, DeliveryEntry>,
}

impl ApplicationDeliveryLedger {
    /// Creates an empty generation-scoped ledger with logical `capacity`.
    ///
    /// The B-tree allocates lazily, so even `usize::MAX` does not request
    /// proportional storage. Zero is rejected because reliable publication
    /// would otherwise be impossible.
    pub(in crate::hsms::core::resources::publication) fn new(
        generation: ConnectionGeneration,
        capacity: usize,
        authority: &PublicationMutationAuthority,
        reply_ledger_identity: &ReplyLedgerIdentity,
    ) -> Result<Self, DeliveryLedgerConfigError> {
        if capacity == 0 {
            return Err(DeliveryLedgerConfigError::ZeroCapacity);
        }
        let aggregate = authority.identity();
        if !reply_ledger_identity.matches_aggregate(&aggregate) {
            return Err(DeliveryLedgerConfigError::ForeignReplyAggregate);
        }
        if reply_ledger_identity.generation() != generation {
            return Err(DeliveryLedgerConfigError::ReplyGenerationMismatch {
                expected: generation,
                actual: reply_ledger_identity.generation(),
            });
        }
        Ok(Self {
            generation,
            capacity,
            aggregate,
            brand: Arc::new(DeliveryLedgerBrand { private: () }),
            closing: false,
            highest_registered_id: None,
            next_incarnation: Some(DeliveryIncarnation::new(1)),
            reply_ledger_identity: reply_ledger_identity.duplicate(),
            entries: BTreeMap::new(),
        })
    }

    /// Returns the TCP generation exclusively owned by this ledger.
    pub(in crate::hsms::core::resources::publication) const fn generation(
        &self,
    ) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the configured logical maximum for pending deliveries.
    pub(in crate::hsms::core::resources::publication) const fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the number of currently pending application deliveries.
    pub(in crate::hsms::core::resources::publication) fn len(&self) -> usize {
        self.entries.len()
    }

    /// Returns whether no application delivery is currently pending.
    pub(in crate::hsms::core::resources::publication) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns whether generation close permanently fenced registration.
    pub(in crate::hsms::core::resources::publication) const fn is_closing(&self) -> bool {
        self.closing
    }

    /// Returns whether a higher ID and fresh incarnation can occupy capacity.
    pub(in crate::hsms::core::resources::publication) fn has_capacity(&self) -> bool {
        let id_domain_remains = self
            .highest_registered_id
            .is_none_or(|delivery_id| delivery_id.get() < u64::MAX);
        !self.closing
            && id_domain_remains
            && self.next_incarnation.is_some()
            && self.entries.len() < self.capacity
    }

    /// Freezes admission for one future Delivery registration without mutation.
    ///
    /// W=1 integration calls this before reserving Reply authority. Success
    /// captures generation, ID ordering, capacity, close fence, and the next
    /// incarnation. A failed preparation never burns an ID or incarnation.
    ///
    /// Rust cannot retain a borrow of this child ledger while mutating its
    /// sibling Reply ledger. `CoreResources` must therefore expose W=1
    /// admission only as one private, callback-free use case:
    /// Delivery prepare → Reply reserve → Delivery commit.
    pub(in crate::hsms::core::resources::publication) fn prepare_registration(
        &self,
        authority: &PublicationMutationAuthority,
        generation: ConnectionGeneration,
        delivery_id: DeliveryId,
    ) -> Result<DeliveryRegistrationPreparation, DeliveryRegisterError> {
        if !self.aggregate.matches_authority(authority) {
            return Err(DeliveryRegisterError::ForeignAggregate);
        }
        if self.closing {
            return Err(DeliveryRegisterError::Closing);
        }
        if generation != self.generation {
            return Err(DeliveryRegisterError::WrongGeneration {
                expected: self.generation,
                actual: generation,
            });
        }
        if self.entries.contains_key(&delivery_id) {
            return Err(DeliveryRegisterError::DuplicateId { delivery_id });
        }
        if let Some(highest_registered_id) = self.highest_registered_id {
            if delivery_id <= highest_registered_id {
                return Err(DeliveryRegisterError::NonMonotonicOrReusedId {
                    highest_registered_id,
                    attempted_id: delivery_id,
                });
            }
        }
        if self.entries.len() >= self.capacity {
            return Err(DeliveryRegisterError::CapacityExhausted {
                capacity: self.capacity,
            });
        }
        let Some(incarnation) = self.next_incarnation else {
            return Err(DeliveryRegisterError::IncarnationExhausted);
        };
        Ok(DeliveryRegistrationPreparation {
            brand: Arc::clone(&self.brand),
            aggregate: self.aggregate.duplicate(),
            generation,
            delivery_id,
            expected_closing: self.closing,
            expected_highest_registered_id: self.highest_registered_id,
            expected_len: self.entries.len(),
            expected_capacity: self.capacity,
            incarnation,
        })
    }

    /// Commits one frozen registration with its final move-only binding.
    ///
    /// W=1 callers reserve Reply authority only after preparation and perform
    /// this commit immediately without callbacks or other ledger mutation.
    /// Foreign or stale plans return `binding` intact and mutate no state.
    /// `AdmissionStateChanged` is a defensive invariant signal; the private
    /// aggregate use case must not treat it as a normal compensation branch.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn commit_registration(
        &mut self,
        authority: &mut PublicationMutationAuthority,
        preparation: DeliveryRegistrationPreparation,
        binding: DeliveryBinding,
    ) -> Result<(), DeliveryRegisterRejection> {
        if !self.aggregate.matches_authority(authority)
            || !self.aggregate.exact_eq(&preparation.aggregate)
        {
            return Err(DeliveryRegisterRejection::new(
                DeliveryRegisterError::ForeignAggregate,
                preparation,
                binding,
            ));
        }
        if !Arc::ptr_eq(&self.brand, &preparation.brand) {
            return Err(DeliveryRegisterRejection::new(
                DeliveryRegisterError::ForeignLedger,
                preparation,
                binding,
            ));
        }
        if preparation.generation != self.generation {
            return Err(DeliveryRegisterRejection::new(
                DeliveryRegisterError::WrongGeneration {
                    expected: self.generation,
                    actual: preparation.generation,
                },
                preparation,
                binding,
            ));
        }
        if self.closing {
            return Err(DeliveryRegisterRejection::new(
                DeliveryRegisterError::Closing,
                preparation,
                binding,
            ));
        }
        if self.closing != preparation.expected_closing
            || self.highest_registered_id != preparation.expected_highest_registered_id
            || self.entries.len() != preparation.expected_len
            || self.capacity != preparation.expected_capacity
            || self.next_incarnation != Some(preparation.incarnation)
        {
            return Err(DeliveryRegisterRejection::new(
                DeliveryRegisterError::AdmissionStateChanged {
                    delivery_id: preparation.delivery_id,
                },
                preparation,
                binding,
            ));
        }

        if let Some(ticket) = binding.reply_ticket() {
            if ticket.generation() != self.generation {
                return Err(DeliveryRegisterRejection::new(
                    DeliveryRegisterError::ReplyTicketWrongGeneration {
                        expected: self.generation,
                        actual: ticket.generation(),
                    },
                    preparation,
                    binding,
                ));
            }
            if !self.reply_ledger_identity.matches_ticket(ticket) {
                return Err(DeliveryRegisterRejection::new(
                    DeliveryRegisterError::ReplyTicketForeignLedger,
                    preparation,
                    binding,
                ));
            }
        }

        let previous = self.entries.insert(
            preparation.delivery_id,
            DeliveryEntry {
                incarnation: preparation.incarnation,
                binding,
            },
        );
        debug_assert!(
            previous.is_none(),
            "unchanged prepared admission cannot contain the new identity"
        );
        self.highest_registered_id = Some(preparation.delivery_id);
        self.next_incarnation = preparation
            .incarnation
            .get()
            .checked_add(1)
            .map(DeliveryIncarnation::new);
        Ok(())
    }

    /// Prepares one exact completion without changing pending Delivery state.
    ///
    /// `generation`, `delivery_id`, and `result` identify the runtime
    /// completion. Success borrows the exact retained W=1 ticket, if present,
    /// so Reply state can be updated before Delivery commit.
    pub(in crate::hsms::core::resources::publication) fn prepare_finish(
        &self,
        generation: ConnectionGeneration,
        delivery_id: DeliveryId,
        result: ApplicationDeliveryResult,
    ) -> Result<DeliveryFinishPreparation<'_>, DeliveryPrepareError> {
        if generation != self.generation {
            return Err(DeliveryPrepareError::WrongGeneration {
                expected: self.generation,
                actual: generation,
            });
        }
        let Some(entry) = self.entries.get(&delivery_id) else {
            return Err(DeliveryPrepareError::UnknownOrTerminal { delivery_id });
        };
        Ok(DeliveryFinishPreparation {
            brand: Arc::clone(&self.brand),
            snapshot: DeliveryEntrySnapshot {
                generation,
                delivery_id,
                incarnation: entry.incarnation,
                binding: entry.binding.identity(),
            },
            result,
            binding: &entry.binding,
        })
    }

    /// Commits one exact prepared completion after full entry revalidation.
    ///
    /// `authority` proves exact aggregate ownership. Foreign, stale, replayed, or
    /// changed tokens return an error before any entry is removed.
    pub(in crate::hsms::core::resources::publication) fn commit_finish(
        &mut self,
        authority: &mut PublicationMutationAuthority,
        commit: DeliveryFinishCommit,
    ) -> Result<DeliveryTerminal, DeliveryCommitFailure<DeliveryFinishCommit>> {
        if !self.aggregate.matches_authority(authority) {
            return Err(DeliveryCommitFailure {
                reason: DeliveryCommitError::ForeignAggregate,
                commit,
            });
        }
        if let Err(reason) = self.validate_brand_and_snapshot(&commit.brand, &commit.snapshot) {
            return Err(DeliveryCommitFailure { reason, commit });
        }
        // The same exclusive borrow performs validation and removal without an
        // intervening callback, so disappearance here is an internal bug only.
        let entry = self
            .entries
            .remove(&commit.snapshot.delivery_id)
            .expect("exact entry was fully validated before removal");
        Ok(DeliveryTerminal {
            generation: self.generation,
            delivery_id: commit.snapshot.delivery_id,
            binding: entry.binding,
            result: commit.result,
        })
    }

    /// Prepares a Selected-session reset without mutating the ledger.
    ///
    /// The returned views contain only W=0 and W=1 Data deliveries in ascending
    /// identity order, while the opaque token snapshots every entry so commit
    /// can validate the complete map before removing any Data binding. The
    /// returned Reply request is unique to this preparation.
    pub(in crate::hsms::core::resources::publication) fn prepare_selected_session_reset(
        &self,
    ) -> (DeliveryResetPreparation<'_>, ReplyClearRequest) {
        let target_count = self
            .entries
            .values()
            .filter(|entry| entry.binding.is_selected_session_data())
            .count();
        let (clear_request, reply_nonce) = DeliveryClearRequestIdentity::issue(
            &self.aggregate,
            self.generation,
            ReplyClearScope::SelectedSessionReset,
        );
        let reply_request = ReplyClearRequest::new(reply_nonce, &self.reply_ledger_identity);
        let state = self.capture_drain_state(target_count, clear_request);
        let deliveries = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.binding.is_selected_session_data())
            .map(|(delivery_id, entry)| PreparedDeliveryView {
                delivery_id: *delivery_id,
                binding: &entry.binding,
            })
            .collect();
        (
            DeliveryResetPreparation { state, deliveries },
            reply_request,
        )
    }

    /// Commits one exact Selected-session reset after validating all snapshots.
    ///
    /// `authority` proves exact aggregate ownership. Validation and output allocation
    /// finish before the first removal, so every structured failure leaves the
    /// complete current map unchanged. Protocol notices remain pending.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn commit_selected_session_reset(
        &mut self,
        authority: &mut PublicationMutationAuthority,
        commit: DeliveryResetCommit,
    ) -> Result<DeliveryResetSummary, DeliveryCommitFailure<DeliveryResetCommit>> {
        if !self.aggregate.matches_authority(authority) {
            return Err(DeliveryCommitFailure {
                reason: DeliveryCommitError::ForeignAggregate,
                commit,
            });
        }
        if let Err(reason) = self.validate_drain_state(&commit.state) {
            return Err(DeliveryCommitFailure { reason, commit });
        }
        let state = commit.state;
        let target_count = state
            .snapshots
            .iter()
            .filter(|snapshot| snapshot.binding.is_selected_session_data())
            .count();
        debug_assert!(state.dispositions.capacity() >= target_count);
        let mut deliveries = state.dispositions;
        for snapshot in state
            .snapshots
            .into_iter()
            .filter(|snapshot| snapshot.binding.is_selected_session_data())
        {
            // Full-map validation and this loop share one exclusive borrow; no
            // external mutation can invalidate a previously checked key.
            let entry = self
                .entries
                .remove(&snapshot.delivery_id)
                .expect("complete batch was validated before the first removal");
            deliveries.push(DeliveryDisposition {
                delivery_id: snapshot.delivery_id,
                binding: entry.binding,
            });
        }
        Ok(DeliveryResetSummary { deliveries })
    }

    /// Prepares permanent generation close without changing ledger state.
    ///
    /// Every pending binding is exposed read-only in ascending identity order
    /// so Reply cleanup can complete before the close token is committed. The
    /// returned Reply request is unique to this preparation.
    pub(in crate::hsms::core::resources::publication) fn prepare_close(
        &self,
    ) -> (DeliveryClosePreparation<'_>, ReplyClearRequest) {
        let (clear_request, reply_nonce) = DeliveryClearRequestIdentity::issue(
            &self.aggregate,
            self.generation,
            ReplyClearScope::GenerationEnd,
        );
        let reply_request = ReplyClearRequest::new(reply_nonce, &self.reply_ledger_identity);
        let state = self.capture_drain_state(self.entries.len(), clear_request);
        let deliveries = self
            .entries
            .iter()
            .map(|(delivery_id, entry)| PreparedDeliveryView {
                delivery_id: *delivery_id,
                binding: &entry.binding,
            })
            .collect();
        (
            DeliveryClosePreparation { state, deliveries },
            reply_request,
        )
    }

    /// Permanently fences registration and commits one exact full drain.
    ///
    /// `authority` proves exact aggregate ownership. The first validated open-state
    /// token drains every binding in ascending identity order. A token prepared
    /// after close commits an idempotent empty result with `began_close=false`.
    #[allow(clippy::result_large_err)]
    pub(in crate::hsms::core::resources::publication) fn commit_close(
        &mut self,
        authority: &mut PublicationMutationAuthority,
        commit: DeliveryCloseCommit,
    ) -> Result<DeliveryCloseSummary, DeliveryCommitFailure<DeliveryCloseCommit>> {
        if !self.aggregate.matches_authority(authority) {
            return Err(DeliveryCommitFailure {
                reason: DeliveryCommitError::ForeignAggregate,
                commit,
            });
        }
        if let Err(reason) = self.validate_drain_state(&commit.state) {
            return Err(DeliveryCommitFailure { reason, commit });
        }
        if commit.state.expected_closing {
            if !self.entries.is_empty() {
                return Err(DeliveryCommitFailure {
                    reason: DeliveryCommitError::ClosingWithPendingEntries {
                        pending: self.entries.len(),
                    },
                    commit,
                });
            }
            return Ok(DeliveryCloseSummary {
                began_close: false,
                deliveries: Vec::new(),
            });
        }

        let state = commit.state;
        debug_assert!(state.dispositions.capacity() >= self.entries.len());
        let mut deliveries = state.dispositions;
        self.closing = true;
        for (delivery_id, entry) in std::mem::take(&mut self.entries) {
            deliveries.push(DeliveryDisposition {
                delivery_id,
                binding: entry.binding,
            });
        }
        Ok(DeliveryCloseSummary {
            began_close: true,
            deliveries,
        })
    }

    /// Captures every current entry for later all-or-nothing batch validation.
    fn capture_drain_state(
        &self,
        disposition_capacity: usize,
        clear_request: DeliveryClearRequestIdentity,
    ) -> DeliveryDrainCommitState {
        let snapshots = self
            .entries
            .iter()
            .map(|(delivery_id, entry)| DeliveryEntrySnapshot {
                generation: self.generation,
                delivery_id: *delivery_id,
                incarnation: entry.incarnation,
                binding: entry.binding.identity(),
            })
            .collect();
        DeliveryDrainCommitState {
            brand: Arc::clone(&self.brand),
            aggregate: self.aggregate.duplicate(),
            generation: self.generation,
            reply_ledger_identity: self.reply_ledger_identity.duplicate(),
            clear_request,
            expected_closing: self.closing,
            snapshots,
            dispositions: Vec::with_capacity(disposition_capacity),
        }
    }

    /// Validates one scoped Reply receipt before creating a Delivery commit.
    fn validate_clear_receipt(
        state: &DeliveryDrainCommitState,
        receipt: &ReplyClearReceipt,
        expected_scope: ReplyClearScope,
    ) -> Result<(), DeliveryClearAuthorizationError> {
        if !receipt.matches_aggregate(&state.aggregate) {
            return Err(DeliveryClearAuthorizationError::ForeignAggregate);
        }
        if receipt.generation() != state.generation {
            return Err(DeliveryClearAuthorizationError::WrongGeneration {
                expected: state.generation,
                actual: receipt.generation(),
            });
        }
        if receipt.scope() != expected_scope {
            return Err(DeliveryClearAuthorizationError::WrongScope {
                expected: expected_scope,
                actual: receipt.scope(),
            });
        }
        if !receipt.covers_reply_ledger(&state.reply_ledger_identity) {
            return Err(DeliveryClearAuthorizationError::ForeignReplyLedger);
        }
        if !receipt.answers_request(&state.clear_request) {
            return Err(DeliveryClearAuthorizationError::RequestMismatch);
        }
        Ok(())
    }

    /// Validates one branded exact entry snapshot without mutating the ledger.
    fn validate_brand_and_snapshot(
        &self,
        brand: &Arc<DeliveryLedgerBrand>,
        snapshot: &DeliveryEntrySnapshot,
    ) -> Result<(), DeliveryCommitError> {
        if !Arc::ptr_eq(&self.brand, brand) {
            return Err(DeliveryCommitError::ForeignLedger);
        }
        if snapshot.generation != self.generation {
            return Err(DeliveryCommitError::WrongGeneration {
                expected: self.generation,
                actual: snapshot.generation,
            });
        }
        let Some(entry) = self.entries.get(&snapshot.delivery_id) else {
            return Err(DeliveryCommitError::UnknownOrTerminal {
                delivery_id: snapshot.delivery_id,
            });
        };
        if entry.incarnation != snapshot.incarnation {
            return Err(DeliveryCommitError::IncarnationChanged {
                delivery_id: snapshot.delivery_id,
            });
        }
        if !snapshot.binding.matches(&entry.binding) {
            return Err(DeliveryCommitError::BindingChanged {
                delivery_id: snapshot.delivery_id,
            });
        }
        Ok(())
    }

    /// Validates one complete branded batch snapshot before any drain mutation.
    fn validate_drain_state(
        &self,
        state: &DeliveryDrainCommitState,
    ) -> Result<(), DeliveryCommitError> {
        if !self.aggregate.exact_eq(&state.aggregate) {
            return Err(DeliveryCommitError::ForeignAggregate);
        }
        if !Arc::ptr_eq(&self.brand, &state.brand) {
            return Err(DeliveryCommitError::ForeignLedger);
        }
        if state.generation != self.generation {
            return Err(DeliveryCommitError::WrongGeneration {
                expected: self.generation,
                actual: state.generation,
            });
        }
        if state.expected_closing != self.closing {
            return Err(DeliveryCommitError::ClosingStateChanged {
                expected: state.expected_closing,
                actual: self.closing,
            });
        }
        if state.snapshots.len() != self.entries.len() {
            return Err(DeliveryCommitError::EntrySetChanged);
        }
        for (snapshot, (delivery_id, entry)) in state.snapshots.iter().zip(&self.entries) {
            if snapshot.delivery_id != *delivery_id {
                return Err(DeliveryCommitError::EntrySetChanged);
            }
            if snapshot.incarnation != entry.incarnation {
                return Err(DeliveryCommitError::IncarnationChanged {
                    delivery_id: *delivery_id,
                });
            }
            if !snapshot.binding.matches(&entry.binding) {
                return Err(DeliveryCommitError::BindingChanged {
                    delivery_id: *delivery_id,
                });
            }
        }
        Ok(())
    }

    /// Injects the next incarnation for boundary-focused unit tests.
    ///
    /// `next` is the value issued by the next successful registration; `None`
    /// represents permanent exhaustion.
    #[cfg(test)]
    fn set_next_incarnation_for_test(&mut self, next: Option<u64>) {
        debug_assert!(next != Some(0));
        self.next_incarnation = next.map(DeliveryIncarnation::new);
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        contracts::{ApplicationDeliveryResult, DeliveryPurpose},
        model::ids::{
            ConnectionGeneration, DeliveryId, Function, ReplyCapabilityId, SessionId, Stream,
            SystemBytes,
        },
    };

    use super::super::super::{
        contracts::ReplyContract,
        reply::{ReplyCapabilityLedger, ReplyPublicationDecision},
    };
    use super::{
        ApplicationDeliveryLedger, DeliveryBinding, DeliveryCommitError, DeliveryLedgerConfigError,
        DeliveryPrepareError, DeliveryRegisterError, PublicationMutationAuthority,
        ReplyClearReceipt, ReplyClearRequest, ReplyPublicationTicket,
    };

    /// Deterministic generation owned by every ordinary delivery test.
    const GENERATION: ConnectionGeneration = ConnectionGeneration::new(7);

    /// Creates a non-zero-capacity ledger owned by generation seven.
    fn ledger(
        authority: &PublicationMutationAuthority,
        capacity: usize,
    ) -> ApplicationDeliveryLedger {
        let replies = reply_ledger(authority, GENERATION);
        ledger_for_reply(authority, &replies, capacity)
    }

    /// Creates a Delivery ledger bound to one explicit intended Reply ledger.
    fn ledger_for_reply(
        authority: &PublicationMutationAuthority,
        replies: &ReplyCapabilityLedger,
        capacity: usize,
    ) -> ApplicationDeliveryLedger {
        ApplicationDeliveryLedger::new(GENERATION, capacity, authority, &replies.identity())
            .expect("non-zero logical capacity")
    }

    /// Creates aggregate-only mutation authority for one focused unit test.
    fn authority() -> PublicationMutationAuthority {
        PublicationMutationAuthority::for_test()
    }

    /// Creates a reply ledger owned by `generation` with ample lazy capacity.
    fn reply_ledger(
        authority: &PublicationMutationAuthority,
        generation: ConnectionGeneration,
    ) -> ReplyCapabilityLedger {
        ReplyCapabilityLedger::new(generation, 16, authority).expect("non-zero reply capacity")
    }

    /// Compiles one deterministic reply contract for `generation`.
    fn reply_contract(generation: ConnectionGeneration) -> ReplyContract {
        ReplyContract::from_primary_parts(
            generation,
            SessionId::new(3).expect("ordinary Data Session ID"),
            Stream::new(1).expect("seven-bit stream"),
            Function::new(1),
            true,
            SystemBytes::new(0x0102_0304),
        )
        .expect("valid W=1 Primary")
    }

    /// Reserves one exact pending reply ticket and discards only its public token.
    fn reserve_ticket(
        replies: &mut ReplyCapabilityLedger,
        generation: ConnectionGeneration,
        capability_id: u64,
    ) -> ReplyPublicationTicket {
        let (ticket, token) = replies
            .reserve_pending(
                ReplyCapabilityId::new(capability_id),
                reply_contract(generation),
            )
            .expect("pending reply reservation")
            .into_parts();
        drop(token);
        ticket
    }

    /// Registers one binding in generation seven and asserts success.
    fn register(
        ledger: &mut ApplicationDeliveryLedger,
        authority: &mut PublicationMutationAuthority,
        delivery_id: u64,
        binding: DeliveryBinding,
    ) {
        let preparation = ledger
            .prepare_registration(authority, GENERATION, DeliveryId::new(delivery_id))
            .expect("test delivery admission");
        ledger
            .commit_registration(authority, preparation, binding)
            .expect("test delivery registration");
    }

    /// Registers W=1 in the required callback-free cross-ledger order.
    fn register_w1(
        ledger: &mut ApplicationDeliveryLedger,
        replies: &mut ReplyCapabilityLedger,
        authority: &mut PublicationMutationAuthority,
        delivery_id: u64,
        capability_id: u64,
    ) {
        let preparation = ledger
            .prepare_registration(authority, GENERATION, DeliveryId::new(delivery_id))
            .expect("W=1 Delivery admission precedes Reply reservation");
        let ticket = reserve_ticket(replies, GENERATION, capability_id);
        ledger
            .commit_registration(
                authority,
                preparation,
                DeliveryBinding::InboundPrimaryW1 { ticket },
            )
            .expect("no callback or Delivery mutation occurred during Reply reservation");
    }

    /// Finishes one exact delivery and returns its move-only terminal.
    fn finish(
        ledger: &mut ApplicationDeliveryLedger,
        authority: &mut PublicationMutationAuthority,
        delivery_id: u64,
        result: ApplicationDeliveryResult,
    ) -> super::DeliveryTerminal {
        let commit = ledger
            .prepare_finish(GENERATION, DeliveryId::new(delivery_id), result)
            .expect("exact finish preparation")
            .into_commit();
        ledger
            .commit_finish(authority, commit)
            .expect("exact finish commit")
    }

    /// Converts committed dispositions into ordered identity-purpose pairs.
    fn disposition_purposes(
        dispositions: Vec<super::DeliveryDisposition>,
    ) -> Vec<(DeliveryId, DeliveryPurpose)> {
        dispositions
            .into_iter()
            .map(|disposition| {
                let (delivery_id, binding) = disposition.into_parts();
                (delivery_id, binding.purpose())
            })
            .collect()
    }

    /// Executes the callback-free Reply half of one Selected reset and returns
    /// the exact Delivery commit authorized by its consumed receipt.
    fn authorize_selected_reset<'a>(
        replies: &mut ReplyCapabilityLedger,
        authority: &mut PublicationMutationAuthority,
        preparation: super::DeliveryResetPreparation<'a>,
        request: ReplyClearRequest,
    ) -> super::DeliveryResetCommit {
        let receipt = selected_clear_receipt(replies, authority, &preparation, request);
        preparation
            .authorize_reply_clear(receipt)
            .expect("receipt answers this exact Delivery reset")
    }

    /// Commits the complete Reply half of one Selected reset while retaining
    /// the borrowed Delivery preparation for later receipt authorization.
    fn selected_clear_receipt(
        replies: &mut ReplyCapabilityLedger,
        authority: &mut PublicationMutationAuthority,
        preparation: &super::DeliveryResetPreparation<'_>,
        request: ReplyClearRequest,
    ) -> ReplyClearReceipt {
        let reply_preparation = replies
            .prepare_selected_session_reset(authority, request)
            .expect("request targets the intended Reply ledger");
        let reply_commit = reply_preparation
            .validate_pending_tickets(preparation.reply_tickets())
            .expect("Delivery exposes every pending Reply ticket");
        replies
            .commit_selected_session_reset(authority, reply_commit)
            .expect("complete Reply snapshot remains unchanged")
    }

    /// Executes the callback-free Reply half of one generation close and
    /// returns the exact Delivery commit authorized by its consumed receipt.
    fn authorize_generation_close<'a>(
        replies: &mut ReplyCapabilityLedger,
        authority: &mut PublicationMutationAuthority,
        preparation: super::DeliveryClosePreparation<'a>,
        request: ReplyClearRequest,
    ) -> super::DeliveryCloseCommit {
        let receipt = generation_clear_receipt(replies, authority, &preparation, request);
        preparation
            .authorize_reply_clear(receipt)
            .expect("receipt answers this exact Delivery close")
    }

    /// Commits the complete Reply half of generation close while retaining the
    /// borrowed Delivery preparation for later receipt authorization.
    fn generation_clear_receipt(
        replies: &mut ReplyCapabilityLedger,
        authority: &mut PublicationMutationAuthority,
        preparation: &super::DeliveryClosePreparation<'_>,
        request: ReplyClearRequest,
    ) -> ReplyClearReceipt {
        let reply_preparation = replies
            .prepare_generation_end(authority, request)
            .expect("request targets the intended Reply ledger");
        let reply_commit = reply_preparation
            .validate_pending_tickets(preparation.reply_tickets())
            .expect("Delivery exposes every pending Reply ticket");
        replies
            .commit_generation_end(authority, reply_commit)
            .expect("complete Reply snapshot remains unchanged")
    }

    /// Confirms zero capacity is structured while extreme logical capacity is lazy.
    #[test]
    fn construction_is_structured_and_lazily_bounded() {
        let mut owner_authority = authority();
        let replies = reply_ledger(&owner_authority, GENERATION);
        assert!(matches!(
            ApplicationDeliveryLedger::new(GENERATION, 0, &owner_authority, &replies.identity(),),
            Err(DeliveryLedgerConfigError::ZeroCapacity)
        ));
        let foreign_authority = authority();
        let foreign_replies = reply_ledger(&foreign_authority, GENERATION);
        assert!(matches!(
            ApplicationDeliveryLedger::new(
                GENERATION,
                1,
                &owner_authority,
                &foreign_replies.identity(),
            ),
            Err(DeliveryLedgerConfigError::ForeignReplyAggregate)
        ));
        let wrong_generation_replies = reply_ledger(&owner_authority, ConnectionGeneration::new(8));
        assert!(matches!(
            ApplicationDeliveryLedger::new(
                GENERATION,
                1,
                &owner_authority,
                &wrong_generation_replies.identity(),
            ),
            Err(DeliveryLedgerConfigError::ReplyGenerationMismatch {
                expected,
                actual,
            }) if expected == GENERATION && actual == ConnectionGeneration::new(8)
        ));

        let mut ledger = ledger(&owner_authority, usize::MAX);
        assert_eq!(ledger.generation(), GENERATION);
        assert_eq!(ledger.capacity(), usize::MAX);
        assert!(ledger.is_empty());
        assert!(ledger.has_capacity());
        register(
            &mut ledger,
            &mut owner_authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms every binding projects the frozen public diagnostic purpose.
    #[test]
    fn binding_projects_w0_w1_and_protocol_notice_purposes() {
        let authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let w1 = DeliveryBinding::InboundPrimaryW1 {
            ticket: reserve_ticket(&mut replies, GENERATION, 22),
        };
        assert_eq!(
            DeliveryBinding::InboundPrimaryW0.purpose(),
            DeliveryPurpose::InboundPrimary
        );
        assert_eq!(
            w1.purpose(),
            DeliveryPurpose::InboundReplyCapability(ReplyCapabilityId::new(22))
        );
        assert_eq!(
            DeliveryBinding::ProtocolNotice.purpose(),
            DeliveryPurpose::ProtocolNotice
        );
    }

    /// Confirms capacity is rejected before Reply reservation, while a stale
    /// commit still returns an already-created W=1 ticket without mutation.
    #[test]
    fn failed_registration_returns_move_only_w1_binding_intact() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 2);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        let preparation = ledger
            .prepare_registration(&authority, GENERATION, DeliveryId::new(2))
            .expect("second slot is initially admissible");
        let ticket = reserve_ticket(&mut replies, GENERATION, 31);
        register(
            &mut ledger,
            &mut authority,
            3,
            DeliveryBinding::InboundPrimaryW0,
        );
        let rejection = ledger
            .commit_registration(
                &mut authority,
                preparation,
                DeliveryBinding::InboundPrimaryW1 { ticket },
            )
            .expect_err("another registration invalidated frozen admission");
        assert_eq!(
            rejection.reason(),
            DeliveryRegisterError::AdmissionStateChanged {
                delivery_id: DeliveryId::new(2),
            }
        );
        assert_eq!(
            rejection.binding().purpose(),
            DeliveryPurpose::InboundReplyCapability(ReplyCapabilityId::new(31))
        );
        let (_, _preparation, binding) = rejection.into_parts();
        assert_eq!(
            replies.mark_available(
                binding
                    .reply_ticket()
                    .expect("rejection returned the exact W=1 ticket")
            ),
            ReplyPublicationDecision::MadeAvailable
        );
        assert_eq!(ledger.len(), 2);
        assert!(matches!(
            ledger.prepare_registration(&authority, GENERATION, DeliveryId::new(4)),
            Err(DeliveryRegisterError::CapacityExhausted { capacity: 2 })
        ));
    }

    /// Confirms duplicate, stale-order, envelope-generation, and ticket-generation
    /// failures are structured and leave registration state unchanged.
    #[test]
    fn registration_rejects_invalid_identity_and_generation_without_mutation() {
        let mut authority = authority();
        let mut ledger = ledger(&authority, 4);
        register(
            &mut ledger,
            &mut authority,
            2,
            DeliveryBinding::InboundPrimaryW0,
        );

        let duplicate = ledger
            .prepare_registration(&authority, GENERATION, DeliveryId::new(2))
            .expect_err("live duplicate");
        assert_eq!(
            duplicate,
            DeliveryRegisterError::DuplicateId {
                delivery_id: DeliveryId::new(2)
            }
        );
        let reordered = ledger
            .prepare_registration(&authority, GENERATION, DeliveryId::new(1))
            .expect_err("lower ID violates monotonic registration");
        assert_eq!(
            reordered,
            DeliveryRegisterError::NonMonotonicOrReusedId {
                highest_registered_id: DeliveryId::new(2),
                attempted_id: DeliveryId::new(1),
            }
        );

        let wrong_generation = ledger
            .prepare_registration(&authority, ConnectionGeneration::new(8), DeliveryId::new(3))
            .expect_err("stale generation");
        assert_eq!(
            wrong_generation,
            DeliveryRegisterError::WrongGeneration {
                expected: GENERATION,
                actual: ConnectionGeneration::new(8),
            }
        );

        let mut foreign_replies = reply_ledger(&authority, ConnectionGeneration::new(8));
        let preparation = ledger
            .prepare_registration(&authority, GENERATION, DeliveryId::new(3))
            .expect("Delivery envelope itself is admissible");
        let foreign_ticket = ledger
            .commit_registration(
                &mut authority,
                preparation,
                DeliveryBinding::InboundPrimaryW1 {
                    ticket: reserve_ticket(&mut foreign_replies, ConnectionGeneration::new(8), 9),
                },
            )
            .expect_err("reply ticket belongs to another generation");
        assert_eq!(
            foreign_ticket.reason(),
            DeliveryRegisterError::ReplyTicketWrongGeneration {
                expected: GENERATION,
                actual: ConnectionGeneration::new(8),
            }
        );
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms finish preparation retains Delivery state while exposing the
    /// exact ticket needed to publish Reply authority before commit.
    #[test]
    fn finish_prepares_reply_first_then_commits_full_binding() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 2);
        register_w1(&mut ledger, &mut replies, &mut authority, 1, 41);

        let prepared = ledger
            .prepare_finish(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("exact pending delivery");
        assert_eq!(prepared.delivery_id(), DeliveryId::new(1));
        assert_eq!(
            prepared.purpose(),
            DeliveryPurpose::InboundReplyCapability(ReplyCapabilityId::new(41))
        );
        assert_eq!(prepared.result(), ApplicationDeliveryResult::Delivered);
        assert_eq!(ledger.len(), 1);
        assert_eq!(
            replies.mark_available(
                prepared
                    .reply_ticket()
                    .expect("W=1 preparation borrows its exact ticket")
            ),
            ReplyPublicationDecision::MadeAvailable
        );

        let terminal = ledger
            .commit_finish(&mut authority, prepared.into_commit())
            .expect("reply mutation cannot invalidate Delivery state");
        assert_eq!(
            terminal.purpose(),
            DeliveryPurpose::InboundReplyCapability(ReplyCapabilityId::new(41))
        );
        let (generation, delivery_id, binding, result) = terminal.into_parts();
        assert_eq!(generation, GENERATION);
        assert_eq!(delivery_id, DeliveryId::new(1));
        assert_eq!(result, ApplicationDeliveryResult::Delivered);
        assert_eq!(
            binding.purpose(),
            DeliveryPurpose::InboundReplyCapability(ReplyCapabilityId::new(41))
        );
        assert!(ledger.is_empty());
    }

    /// Confirms Delivered, Full, and Closed each reach one exact terminal.
    #[test]
    fn finish_preserves_all_application_delivery_results() {
        let mut authority = authority();
        let mut ledger = ledger(&authority, 3);
        for (id, binding) in [
            (1, DeliveryBinding::InboundPrimaryW0),
            (2, DeliveryBinding::ProtocolNotice),
            (3, DeliveryBinding::InboundPrimaryW0),
        ] {
            register(&mut ledger, &mut authority, id, binding);
        }
        for (id, expected_purpose, result) in [
            (
                1,
                DeliveryPurpose::InboundPrimary,
                ApplicationDeliveryResult::Delivered,
            ),
            (
                2,
                DeliveryPurpose::ProtocolNotice,
                ApplicationDeliveryResult::Full,
            ),
            (
                3,
                DeliveryPurpose::InboundPrimary,
                ApplicationDeliveryResult::Closed,
            ),
        ] {
            let terminal = finish(&mut ledger, &mut authority, id, result);
            assert_eq!(terminal.purpose(), expected_purpose);
            assert_eq!(terminal.into_parts().3, result);
        }
        assert!(ledger.is_empty());
    }

    /// Confirms foreign and replayed commit tokens cannot remove live entries.
    #[test]
    fn foreign_and_stale_finish_tokens_are_non_mutating() {
        let mut authority = authority();
        let mut first = ledger(&authority, 2);
        let mut second = ledger(&authority, 2);
        register(
            &mut first,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        register(
            &mut second,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );

        let foreign = first
            .prepare_finish(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("first ledger entry")
            .into_commit();
        assert_eq!(
            second
                .commit_finish(&mut authority, foreign)
                .expect_err("foreign Delivery ledger rejects the commit")
                .reason(),
            DeliveryCommitError::ForeignLedger
        );
        assert_eq!(second.len(), 1);

        let winner = first
            .prepare_finish(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("first exact plan")
            .into_commit();
        let stale = first
            .prepare_finish(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Closed,
            )
            .expect("second read-only plan")
            .into_commit();
        let _terminal = first
            .commit_finish(&mut authority, winner)
            .expect("first commit wins");
        assert_eq!(
            first
                .commit_finish(&mut authority, stale)
                .expect_err("replayed finish commit is stale")
                .reason(),
            DeliveryCommitError::UnknownOrTerminal {
                delivery_id: DeliveryId::new(1),
            }
        );
        assert!(first.is_empty());
    }

    /// Confirms incarnation and binding changes are detected before removal.
    #[test]
    fn exact_finish_revalidates_incarnation_and_binding() {
        let mut authority = authority();
        let mut ledger = ledger(&authority, 2);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        let incarnation_token = ledger
            .prepare_finish(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("exact plan")
            .into_commit();
        ledger
            .entries
            .get_mut(&DeliveryId::new(1))
            .expect("test entry")
            .incarnation = super::DeliveryIncarnation::new(99);
        assert_eq!(
            ledger
                .commit_finish(&mut authority, incarnation_token)
                .expect_err("changed incarnation rejects exact commit")
                .reason(),
            DeliveryCommitError::IncarnationChanged {
                delivery_id: DeliveryId::new(1)
            }
        );
        assert_eq!(ledger.len(), 1);

        let binding_token = ledger
            .prepare_finish(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("new exact plan")
            .into_commit();
        ledger
            .entries
            .get_mut(&DeliveryId::new(1))
            .expect("test entry")
            .binding = DeliveryBinding::ProtocolNotice;
        assert_eq!(
            ledger
                .commit_finish(&mut authority, binding_token)
                .expect_err("changed binding rejects exact commit")
                .reason(),
            DeliveryCommitError::BindingChanged {
                delivery_id: DeliveryId::new(1)
            }
        );
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms a same-shaped ticket from another Reply ledger cannot replace
    /// the exact W=1 binding captured by Delivery preparation.
    #[test]
    fn exact_finish_rejects_same_shaped_foreign_reply_ticket() {
        let mut authority = authority();
        let mut first_replies = reply_ledger(&authority, GENERATION);
        let mut second_replies = reply_ledger(&authority, GENERATION);
        let mut deliveries = ledger_for_reply(&authority, &first_replies, 1);
        let admission = deliveries
            .prepare_registration(&authority, GENERATION, DeliveryId::new(1))
            .expect("Delivery admission precedes both test reservations");
        let first_ticket = reserve_ticket(&mut first_replies, GENERATION, 1);
        let second_ticket = reserve_ticket(&mut second_replies, GENERATION, 1);
        assert!(!first_ticket.identity().matches(&second_ticket));

        deliveries
            .commit_registration(
                &mut authority,
                admission,
                DeliveryBinding::InboundPrimaryW1 {
                    ticket: first_ticket,
                },
            )
            .expect("intended Reply ticket commits");
        let prepared = deliveries
            .prepare_finish(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("exact first-ledger ticket")
            .into_commit();
        deliveries
            .entries
            .get_mut(&DeliveryId::new(1))
            .expect("test entry")
            .binding = DeliveryBinding::InboundPrimaryW1 {
            ticket: second_ticket,
        };

        assert_eq!(
            deliveries
                .commit_finish(&mut authority, prepared)
                .expect_err("same-shaped foreign ticket changes exact binding")
                .reason(),
            DeliveryCommitError::BindingChanged {
                delivery_id: DeliveryId::new(1),
            }
        );
        assert_eq!(deliveries.len(), 1);
    }

    /// Confirms an ID remains fenced after its terminal entry is removed.
    #[test]
    fn finished_delivery_id_cannot_be_reconstructed() {
        let mut authority = authority();
        let mut ledger = ledger(&authority, 2);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        let _terminal = finish(
            &mut ledger,
            &mut authority,
            1,
            ApplicationDeliveryResult::Delivered,
        );
        let rejection = ledger
            .prepare_registration(&authority, GENERATION, DeliveryId::new(1))
            .expect_err("terminal identity is permanently fenced");
        assert_eq!(
            rejection,
            DeliveryRegisterError::NonMonotonicOrReusedId {
                highest_registered_id: DeliveryId::new(1),
                attempted_id: DeliveryId::new(1),
            }
        );
        assert!(ledger.is_empty());
    }

    /// Confirms reset exposes W=1 tickets, commits Reply cleanup first, drains
    /// Data in stable order, and preserves protocol notices.
    #[test]
    fn selected_reset_is_two_phase_sorted_and_notice_preserving() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 4);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        register(
            &mut ledger,
            &mut authority,
            2,
            DeliveryBinding::ProtocolNotice,
        );
        register_w1(&mut ledger, &mut replies, &mut authority, 3, 51);
        register(
            &mut ledger,
            &mut authority,
            4,
            DeliveryBinding::ProtocolNotice,
        );

        let (prepared, request) = ledger.prepare_selected_session_reset();
        assert_eq!(prepared.len(), 2);
        assert!(!prepared.is_empty());
        let disposition_buffer = prepared.state.dispositions.as_ptr();
        assert!(prepared.state.dispositions.capacity() >= 2);
        assert_eq!(
            prepared
                .deliveries()
                .iter()
                .map(|delivery| delivery.delivery_id())
                .collect::<Vec<_>>(),
            vec![DeliveryId::new(1), DeliveryId::new(3)]
        );
        assert_eq!(prepared.reply_tickets().count(), 1);
        let commit = authorize_selected_reset(&mut replies, &mut authority, prepared, request);
        let summary = ledger
            .commit_selected_session_reset(&mut authority, commit)
            .expect("Reply mutation does not change Delivery snapshots");
        assert_eq!(summary.len(), 2);
        let dispositions = summary.into_deliveries();
        assert_eq!(dispositions.as_ptr(), disposition_buffer);
        assert_eq!(
            disposition_purposes(dispositions),
            vec![
                (DeliveryId::new(1), DeliveryPurpose::InboundPrimary),
                (
                    DeliveryId::new(3),
                    DeliveryPurpose::InboundReplyCapability(ReplyCapabilityId::new(51)),
                ),
            ]
        );
        assert_eq!(ledger.len(), 2);
        let (second, request) = ledger.prepare_selected_session_reset();
        assert!(second.is_empty());
        let commit = authorize_selected_reset(&mut replies, &mut authority, second, request);
        assert!(ledger
            .commit_selected_session_reset(&mut authority, commit)
            .expect("idempotent reset")
            .is_empty());
    }

    /// Confirms a changed batch fails before removing any still-targeted entry.
    #[test]
    fn batch_reset_revalidates_complete_map_before_any_drain() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 3);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        register(
            &mut ledger,
            &mut authority,
            2,
            DeliveryBinding::InboundPrimaryW0,
        );
        let (preparation, request) = ledger.prepare_selected_session_reset();
        let stale_reset =
            authorize_selected_reset(&mut replies, &mut authority, preparation, request);
        let _terminal = finish(
            &mut ledger,
            &mut authority,
            1,
            ApplicationDeliveryResult::Closed,
        );
        assert_eq!(
            ledger
                .commit_selected_session_reset(&mut authority, stale_reset)
                .expect_err("Delivery mutation invalidates the prepared batch")
                .reason(),
            DeliveryCommitError::EntrySetChanged
        );
        assert_eq!(ledger.len(), 1);
        assert!(ledger
            .prepare_finish(
                GENERATION,
                DeliveryId::new(2),
                ApplicationDeliveryResult::Delivered
            )
            .is_ok());
    }

    /// Confirms a foreign batch token and a token prepared before another
    /// terminal transition cannot close or drain the current ledger.
    #[test]
    fn foreign_and_stale_batch_tokens_are_non_mutating() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut first = ledger_for_reply(&authority, &replies, 2);
        let mut second = ledger_for_reply(&authority, &replies, 2);
        register(
            &mut first,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        register(
            &mut second,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );

        let (preparation, request) = first.prepare_close();
        let foreign =
            authorize_generation_close(&mut replies, &mut authority, preparation, request);
        assert_eq!(
            second
                .commit_close(&mut authority, foreign)
                .expect_err("another Delivery ledger rejects the close commit")
                .reason(),
            DeliveryCommitError::ForeignLedger
        );
        assert_eq!(second.len(), 1);
        assert!(!second.is_closing());

        let (preparation, request) = first.prepare_close();
        let stale = authorize_generation_close(&mut replies, &mut authority, preparation, request);
        let (preparation, request) = first.prepare_close();
        let winner = authorize_generation_close(&mut replies, &mut authority, preparation, request);
        let closed = first
            .commit_close(&mut authority, winner)
            .expect("first exact close token wins");
        assert!(closed.began_close());
        assert_eq!(
            first
                .commit_close(&mut authority, stale)
                .expect_err("close replay observes the raised fence")
                .reason(),
            DeliveryCommitError::ClosingStateChanged {
                expected: false,
                actual: true,
            }
        );
        assert!(first.is_closing());
        assert!(first.is_empty());
    }

    /// Confirms an impossible non-empty map behind the permanent close fence is
    /// reported explicitly and never silently discarded or claimed as drained.
    #[test]
    fn closed_ledger_with_pending_entries_reports_invariant_violation() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 1);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::ProtocolNotice,
        );
        ledger.closing = true;

        let (prepared, request) = ledger.prepare_close();
        assert_eq!(prepared.len(), 1);
        let commit = authorize_generation_close(&mut replies, &mut authority, prepared, request);
        assert_eq!(
            ledger
                .commit_close(&mut authority, commit)
                .expect_err("closed Delivery cannot retain pending entries")
                .reason(),
            DeliveryCommitError::ClosingWithPendingEntries { pending: 1 }
        );
        assert!(ledger.is_closing());
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms close preflights W=1 cleanup, drains all bindings once in stable
    /// order, permanently fences registration, and remains idempotent.
    #[test]
    fn generation_close_is_two_phase_permanent_and_idempotent() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 3);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::ProtocolNotice,
        );
        register(
            &mut ledger,
            &mut authority,
            2,
            DeliveryBinding::InboundPrimaryW0,
        );
        register_w1(&mut ledger, &mut replies, &mut authority, 3, 61);

        let (prepared, request) = ledger.prepare_close();
        assert_eq!(prepared.len(), 3);
        assert!(!prepared.is_empty());
        let disposition_buffer = prepared.state.dispositions.as_ptr();
        assert!(prepared.state.dispositions.capacity() >= 3);
        let commit = authorize_generation_close(&mut replies, &mut authority, prepared, request);
        let close = ledger
            .commit_close(&mut authority, commit)
            .expect("exact close commit");
        assert!(close.began_close());
        assert_eq!(close.len(), 3);
        let dispositions = close.into_deliveries();
        assert_eq!(dispositions.as_ptr(), disposition_buffer);
        assert_eq!(
            disposition_purposes(dispositions),
            vec![
                (DeliveryId::new(1), DeliveryPurpose::ProtocolNotice),
                (DeliveryId::new(2), DeliveryPurpose::InboundPrimary),
                (
                    DeliveryId::new(3),
                    DeliveryPurpose::InboundReplyCapability(ReplyCapabilityId::new(61)),
                ),
            ]
        );
        assert!(ledger.is_closing());
        assert!(ledger.is_empty());
        assert!(!ledger.has_capacity());

        let (duplicate, request) = ledger.prepare_close();
        assert!(duplicate.is_empty());
        let commit = authorize_generation_close(&mut replies, &mut authority, duplicate, request);
        let duplicate = ledger
            .commit_close(&mut authority, commit)
            .expect("idempotent close commit");
        assert!(!duplicate.began_close());
        assert!(duplicate.is_empty());

        let rejection = ledger
            .prepare_registration(&authority, GENERATION, DeliveryId::new(4))
            .expect_err("close permanently fenced registration");
        assert_eq!(rejection, DeliveryRegisterError::Closing);
    }

    /// Confirms incarnation exhaustion never wraps and disables admission.
    #[test]
    fn final_incarnation_is_issued_once_then_exhausted() {
        let mut authority = authority();
        let mut ledger = ledger(&authority, 2);
        ledger.set_next_incarnation_for_test(Some(u64::MAX));
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        assert!(!ledger.has_capacity());
        let _terminal = finish(
            &mut ledger,
            &mut authority,
            1,
            ApplicationDeliveryResult::Delivered,
        );
        let rejection = ledger
            .prepare_registration(&authority, GENERATION, DeliveryId::new(2))
            .expect_err("incarnation sequence is permanently exhausted");
        assert_eq!(rejection, DeliveryRegisterError::IncarnationExhausted);
    }

    /// Confirms maximum Delivery ID is terminal for allocation and never wraps.
    #[test]
    fn maximum_delivery_id_is_accepted_once_then_fences_the_domain() {
        let mut authority = authority();
        let mut ledger = ledger(&authority, 1);
        register(
            &mut ledger,
            &mut authority,
            u64::MAX,
            DeliveryBinding::InboundPrimaryW0,
        );
        assert!(!ledger.has_capacity());
        let _terminal = finish(
            &mut ledger,
            &mut authority,
            u64::MAX,
            ApplicationDeliveryResult::Delivered,
        );
        assert_eq!(
            ledger
                .prepare_registration(&authority, GENERATION, DeliveryId::new(u64::MAX))
                .expect_err("maximum identity cannot be reused"),
            DeliveryRegisterError::NonMonotonicOrReusedId {
                highest_registered_id: DeliveryId::new(u64::MAX),
                attempted_id: DeliveryId::new(u64::MAX),
            }
        );
        assert!(!ledger.has_capacity());
    }

    /// Confirms Selected reset removes live entries without reopening old IDs.
    #[test]
    fn selected_reset_preserves_the_historical_delivery_id_fence() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 1);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        let (preparation, request) = ledger.prepare_selected_session_reset();
        let commit = authorize_selected_reset(&mut replies, &mut authority, preparation, request);
        let summary = ledger
            .commit_selected_session_reset(&mut authority, commit)
            .expect("exact reset commits");
        assert_eq!(summary.len(), 1);
        assert_eq!(
            ledger
                .prepare_registration(&authority, GENERATION, DeliveryId::new(1))
                .expect_err("reset does not reopen a historical identity"),
            DeliveryRegisterError::NonMonotonicOrReusedId {
                highest_registered_id: DeliveryId::new(1),
                attempted_id: DeliveryId::new(1),
            }
        );
    }

    /// Confirms every failed preparation or commit leaves registration counters
    /// unchanged, including a same-shaped ticket from another Reply ledger.
    #[test]
    fn failed_admission_never_advances_id_or_incarnation_counters() {
        let mut authority = authority();
        let intended_replies = reply_ledger(&authority, GENERATION);
        let mut foreign_replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &intended_replies, 1);
        assert_eq!(ledger.highest_registered_id, None);
        assert_eq!(
            ledger.next_incarnation,
            Some(super::DeliveryIncarnation::new(1))
        );

        assert!(matches!(
            ledger.prepare_registration(
                &authority,
                ConnectionGeneration::new(8),
                DeliveryId::new(1),
            ),
            Err(DeliveryRegisterError::WrongGeneration { .. })
        ));
        let preparation = ledger
            .prepare_registration(&authority, GENERATION, DeliveryId::new(1))
            .expect("first identity remains admissible");
        let foreign_ticket = reserve_ticket(&mut foreign_replies, GENERATION, 1);
        let rejection = ledger
            .commit_registration(
                &mut authority,
                preparation,
                DeliveryBinding::InboundPrimaryW1 {
                    ticket: foreign_ticket,
                },
            )
            .expect_err("same-aggregate foreign Reply ledger is rejected");
        assert_eq!(
            rejection.reason(),
            DeliveryRegisterError::ReplyTicketForeignLedger
        );
        let (_reason, _preparation, _binding) = rejection.into_parts();
        assert_eq!(ledger.highest_registered_id, None);
        assert_eq!(
            ledger.next_incarnation,
            Some(super::DeliveryIncarnation::new(1))
        );
        assert!(ledger.is_empty());

        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::InboundPrimaryW0,
        );
        assert_eq!(ledger.highest_registered_id, Some(DeliveryId::new(1)));
        assert_eq!(
            ledger.next_incarnation,
            Some(super::DeliveryIncarnation::new(2))
        );
        assert!(matches!(
            ledger.prepare_registration(&authority, GENERATION, DeliveryId::new(2)),
            Err(DeliveryRegisterError::CapacityExhausted { capacity: 1 })
        ));
        assert_eq!(
            ledger.next_incarnation,
            Some(super::DeliveryIncarnation::new(2))
        );
        assert!(intended_replies.is_empty());
    }

    /// Confirms aggregate-branded mutation authority is non-interchangeable and
    /// a foreign-authority rejection returns both plan and binding for retry.
    #[test]
    fn foreign_publication_authority_is_non_mutating_and_retryable() {
        let mut owner = authority();
        let mut foreign = authority();
        let mut ledger = ledger(&owner, 1);
        let preparation = ledger
            .prepare_registration(&owner, GENERATION, DeliveryId::new(1))
            .expect("owner prepares admission");
        let rejection = ledger
            .commit_registration(&mut foreign, preparation, DeliveryBinding::InboundPrimaryW0)
            .expect_err("foreign aggregate authority cannot commit");
        assert_eq!(rejection.reason(), DeliveryRegisterError::ForeignAggregate);
        assert!(ledger.is_empty());
        let (_reason, preparation, binding) = rejection.into_parts();
        ledger
            .commit_registration(&mut owner, preparation, binding)
            .expect("returned proof retries under its owner");
        assert_eq!(ledger.len(), 1);
    }

    /// Confirms two same-shaped Delivery preparations cannot exchange Reply
    /// receipts, while each ownership-preserving failure can be retried exactly.
    #[test]
    fn same_shaped_clear_preparations_reject_cross_swapped_receipts() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 1);
        let (first, first_request) = ledger.prepare_selected_session_reset();
        let (second, second_request) = ledger.prepare_selected_session_reset();
        let first_receipt =
            selected_clear_receipt(&mut replies, &mut authority, &first, first_request);
        let second_receipt =
            selected_clear_receipt(&mut replies, &mut authority, &second, second_request);

        let first_failure = first
            .authorize_reply_clear(second_receipt)
            .expect_err("second receipt carries another request nonce");
        assert_eq!(
            first_failure.reason(),
            super::DeliveryClearAuthorizationError::RequestMismatch
        );
        let (_reason, first, second_receipt) = first_failure.into_parts();

        let second_failure = second
            .authorize_reply_clear(first_receipt)
            .expect_err("first receipt carries another request nonce");
        assert_eq!(
            second_failure.reason(),
            super::DeliveryClearAuthorizationError::RequestMismatch
        );
        let (_reason, second, first_receipt) = second_failure.into_parts();

        let first_commit = first
            .authorize_reply_clear(first_receipt)
            .expect("returned first preparation accepts its own receipt");
        let second_commit = second
            .authorize_reply_clear(second_receipt)
            .expect("returned second preparation accepts its own receipt");
        assert!(ledger
            .commit_selected_session_reset(&mut authority, first_commit)
            .expect("first empty reset commits")
            .is_empty());
        assert!(ledger
            .commit_selected_session_reset(&mut authority, second_commit)
            .expect("second same-snapshot empty reset remains valid")
            .is_empty());
    }

    /// Confirms an unused old empty-clear receipt cannot authorize a later
    /// reset after new available Reply authority appears.
    #[test]
    fn old_empty_clear_receipt_cannot_authorize_later_available_reply_state() {
        let mut authority = authority();
        let mut replies = reply_ledger(&authority, GENERATION);
        let mut ledger = ledger_for_reply(&authority, &replies, 1);
        let (old_preparation, old_request) = ledger.prepare_selected_session_reset();
        let old_receipt =
            selected_clear_receipt(&mut replies, &mut authority, &old_preparation, old_request);
        drop(old_preparation);

        register_w1(&mut ledger, &mut replies, &mut authority, 1, 1);
        let finish_preparation = ledger
            .prepare_finish(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("pending W=1 publication");
        assert_eq!(
            replies.mark_available(
                finish_preparation
                    .reply_ticket()
                    .expect("W=1 Delivery owns the pending ticket"),
            ),
            ReplyPublicationDecision::MadeAvailable
        );
        let _terminal = ledger
            .commit_finish(&mut authority, finish_preparation.into_commit())
            .expect("Delivery publication completes");
        assert_eq!(replies.len(), 1);
        assert!(ledger.is_empty());

        let (current_preparation, current_request) = ledger.prepare_selected_session_reset();
        let failure = current_preparation
            .authorize_reply_clear(old_receipt)
            .expect_err("old empty receipt carries an earlier request nonce");
        assert_eq!(
            failure.reason(),
            super::DeliveryClearAuthorizationError::RequestMismatch
        );
        let (_reason, current_preparation, old_receipt) = failure.into_parts();
        assert_eq!(old_receipt.summary().total(), 0);
        assert!(current_preparation.is_empty());
        drop(current_preparation);
        drop(current_request);
        assert_eq!(replies.len(), 1);
    }

    /// Confirms stale-generation and unknown finish preparation are non-mutating.
    #[test]
    fn stale_or_unknown_finish_preparation_is_non_mutating() {
        let mut authority = authority();
        let mut ledger = ledger(&authority, 2);
        register(
            &mut ledger,
            &mut authority,
            1,
            DeliveryBinding::ProtocolNotice,
        );
        assert!(matches!(
            ledger.prepare_finish(
                ConnectionGeneration::new(8),
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            ),
            Err(DeliveryPrepareError::WrongGeneration {
                expected,
                actual,
            }) if expected == GENERATION && actual == ConnectionGeneration::new(8)
        ));
        assert!(matches!(
            ledger.prepare_finish(
                GENERATION,
                DeliveryId::new(99),
                ApplicationDeliveryResult::Delivered,
            ),
            Err(DeliveryPrepareError::UnknownOrTerminal {
                delivery_id,
            }) if delivery_id == DeliveryId::new(99)
        ));
        assert_eq!(ledger.len(), 1);
    }
}
