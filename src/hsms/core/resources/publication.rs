//! Coordinates application Delivery and Reply capability state as one subaggregate.
//!
//! This module is the only production owner of the publication mutation
//! authority and both generation-scoped ledgers. Its methods expose complete
//! use cases and keep every cross-ledger preparation, request, receipt, and
//! rollback step inside one synchronous, callback-free borrow.

mod authority;
mod contracts;
mod delivery;
mod reply;

use crate::hsms::{
    contracts::{ApplicationDeliveryResult, DeliveryPurpose, ReplyToken, ReplyTokenRouteError},
    model::ids::{ConnectionGeneration, DeliveryId, ReplyCapabilityId},
};

pub(crate) use self::contracts::{
    NormalSecondaryUnavailable, PublicationAdmissionError, PublicationCloseError,
    PublicationFinishError, PublicationInvariantViolation, PublicationResetError,
    PublicationResourceKind, PublicationResourcesBuildError, ReplyCapabilityMode, ReplyContract,
    ReplyContractError, ReplyUseCommitError, ReplyUseKind, ReplyUseTerminal, ReplyUseUnavailable,
};

use self::{
    authority::PublicationMutationAuthority,
    delivery::{
        ApplicationDeliveryLedger, DeliveryBinding, DeliveryCloseSummary, DeliveryCommitError,
        DeliveryDisposition, DeliveryLedgerConfigError, DeliveryPrepareError,
        DeliveryRegisterError, DeliveryResetSummary,
    },
    reply::{
        ReplyCapabilityLedger, ReplyClearCommitError, ReplyClearPrepareError,
        ReplyClearValidationError, ReplyLedgerConfigError, ReplyPublicationDecision,
        ReplyReserveError, ReplyResetSummary, ReplyRevocationPlan, ReplyRevocationUnavailable,
        ReplyUsePlan,
    },
};

/// Public-safe terminal description of one finished application publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "the terminal determines application completion and diagnostics"]
pub(crate) struct PublicationDeliveryTerminal {
    /// Connection generation that owned the completed delivery.
    generation: ConnectionGeneration,
    /// Exact delivery identity removed by the completion.
    delivery_id: DeliveryId,
    /// Semantic application-publication purpose stripped of private tickets.
    purpose: DeliveryPurpose,
    /// Delivered, full, or closed outcome reported by the runtime.
    result: ApplicationDeliveryResult,
}

impl PublicationDeliveryTerminal {
    /// Returns the connection generation that owned this terminal delivery.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the exact identity of the terminal delivery.
    pub(crate) const fn delivery_id(&self) -> DeliveryId {
        self.delivery_id
    }

    /// Returns the semantic purpose of the terminal delivery.
    pub(crate) const fn purpose(&self) -> DeliveryPurpose {
        self.purpose
    }

    /// Returns the runtime delivery result retained by the terminal.
    pub(crate) const fn result(&self) -> ApplicationDeliveryResult {
        self.result
    }
}

/// Ticket-free description of one delivery removed by reset or close.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[must_use = "a drained publication must drive completion and diagnostics"]
pub(crate) struct PublicationDisposition {
    /// Exact identity of the drained reliable delivery.
    delivery_id: DeliveryId,
    /// Semantic publication purpose with private Reply authority erased.
    purpose: DeliveryPurpose,
}

impl PublicationDisposition {
    /// Returns the exact identity of the drained reliable delivery.
    pub(crate) const fn delivery_id(&self) -> DeliveryId {
        self.delivery_id
    }

    /// Returns the semantic purpose of the drained delivery.
    pub(crate) const fn purpose(&self) -> DeliveryPurpose {
        self.purpose
    }
}

/// Complete result of one Selected-session publication reset.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "reset dispositions and revoked capability counts must be routed"]
pub(crate) struct PublicationResetSummary {
    /// Data deliveries removed in ascending delivery-ID order.
    deliveries: Vec<PublicationDisposition>,
    /// Reply capabilities cleared before successful application publication.
    pending_reply_capabilities: usize,
    /// Reply capabilities revoked after successful application publication.
    available_reply_capabilities: usize,
}

impl PublicationResetSummary {
    /// Borrows ticket-free Data dispositions in ascending delivery-ID order.
    pub(crate) fn deliveries(&self) -> &[PublicationDisposition] {
        &self.deliveries
    }

    /// Returns the number of pending-publication Reply capabilities revoked.
    pub(crate) const fn pending_reply_capabilities(&self) -> usize {
        self.pending_reply_capabilities
    }

    /// Returns the number of application-available Reply capabilities revoked.
    pub(crate) const fn available_reply_capabilities(&self) -> usize {
        self.available_reply_capabilities
    }
}

/// Complete idempotent result of permanently closing publication resources.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "close dispositions and revoked capability counts must be routed"]
pub(crate) struct PublicationCloseSummary {
    /// Whether this call raised both generation-close fences.
    began_close: bool,
    /// All deliveries removed in ascending delivery-ID order.
    deliveries: Vec<PublicationDisposition>,
    /// Reply capabilities cleared before successful application publication.
    pending_reply_capabilities: usize,
    /// Reply capabilities revoked after successful application publication.
    available_reply_capabilities: usize,
}

impl PublicationCloseSummary {
    /// Returns whether this call performed the open-to-closing transition.
    pub(crate) const fn began_close(&self) -> bool {
        self.began_close
    }

    /// Borrows ticket-free dispositions in ascending delivery-ID order.
    pub(crate) fn deliveries(&self) -> &[PublicationDisposition] {
        &self.deliveries
    }

    /// Returns the number of pending-publication Reply capabilities revoked.
    pub(crate) const fn pending_reply_capabilities(&self) -> usize {
        self.pending_reply_capabilities
    }

    /// Returns the number of application-available Reply capabilities revoked.
    pub(crate) const fn available_reply_capabilities(&self) -> usize {
        self.available_reply_capabilities
    }
}

/// Move-only reply-use preparation that retains its original application token.
///
/// Downstream Operation and Write preflight may inspect this value. If that
/// preflight fails, [`Self::into_token`] returns the exact token unchanged;
/// callers may retry later or prepare an explicit `Abandon` terminal.
#[derive(Debug)]
#[must_use = "commit this reply use or recover its original token"]
pub(crate) struct PreparedReplyUse {
    /// Exact read-only Reply-ledger plan prepared from the retained token.
    plan: ReplyUsePlan,
    /// Original application token retained until successful terminal commit.
    token: ReplyToken,
}

impl PreparedReplyUse {
    /// Returns the generation whose live capability produced this preparation.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.plan.generation()
    }

    /// Returns the exact capability identity captured by this preparation.
    pub(crate) const fn capability_id(&self) -> ReplyCapabilityId {
        self.plan.capability_id()
    }

    /// Returns the authoritative immutable reply contract for downstream preflight.
    pub(crate) const fn contract(&self) -> ReplyContract {
        self.plan.contract()
    }

    /// Returns the normal, abort, or abandon action selected by the application.
    pub(crate) const fn kind(&self) -> ReplyUseKind {
        self.plan.kind()
    }

    /// Cancels downstream preflight and returns the original token unchanged.
    pub(crate) fn into_token(self) -> ReplyToken {
        self.token
    }
}

/// Ownership-preserving failure preparing one application-selected reply use.
#[derive(Debug)]
#[must_use = "recover the original token or explicitly abandon the capability"]
pub(crate) struct PublicationReplyUsePrepareFailure {
    /// Structured reason no exact Reply plan was prepared.
    reason: ReplyUseUnavailable,
    /// Original application token returned without consumption.
    token: ReplyToken,
}

impl PublicationReplyUsePrepareFailure {
    /// Returns the copyable reason no exact Reply plan was prepared.
    pub(crate) const fn reason(&self) -> ReplyUseUnavailable {
        self.reason
    }

    /// Consumes the failure into its reason and unchanged application token.
    pub(crate) fn into_parts(self) -> (ReplyUseUnavailable, ReplyToken) {
        (self.reason, self.token)
    }
}

/// Ownership-preserving failure committing one prepared application reply use.
#[derive(Debug)]
#[must_use = "recover the unchanged preparation or explicitly abandon its token"]
pub(crate) struct PublicationReplyUseCommitFailure {
    /// Structured reason no live Reply capability was consumed.
    reason: ReplyUseCommitError,
    /// Exact plan and original token returned together without mutation.
    preparation: PreparedReplyUse,
}

impl PublicationReplyUseCommitFailure {
    /// Returns the copyable reason no live Reply capability was consumed.
    pub(crate) const fn reason(&self) -> ReplyUseCommitError {
        self.reason
    }

    /// Consumes the failure into its reason and unchanged preparation.
    pub(crate) fn into_parts(self) -> (ReplyUseCommitError, PreparedReplyUse) {
        (self.reason, self.preparation)
    }
}

/// Deferred Reply transition paired with one exact Delivery finish preflight.
enum FinishReplyAction {
    /// The delivery carries no Reply authority.
    None,
    /// Successful publication must make the retained W=1 capability available.
    Publish,
    /// Failed publication must commit this exact prepared revocation.
    Revoke(
        /// Move-only Reply revocation plan prepared before Delivery mutation.
        ReplyRevocationPlan,
    ),
}

/// Sole production owner of reliable application publication state.
pub(crate) struct PublicationResources {
    /// TCP connection generation exclusively owned by this subaggregate.
    generation: ConnectionGeneration,
    /// Exact branded authority required by both child ledgers.
    authority: PublicationMutationAuthority,
    /// Bounded owner of pending and application-available Reply capabilities.
    replies: ReplyCapabilityLedger,
    /// Bounded owner of reliable application-delivery correlations.
    deliveries: ApplicationDeliveryLedger,
    /// Test-only notice inserted between W=1 Reply reserve and Delivery commit.
    #[cfg(test)]
    intervening_notice_for_test: Option<DeliveryId>,
}

impl PublicationResources {
    /// Builds empty Reply and Delivery owners bound to one generation and aggregate.
    ///
    /// `reply_capacity` bounds all live W=1 capabilities. `delivery_capacity`
    /// bounds all reliable application publications, including W=0 Data and
    /// protocol notices. Child construction remains lazy for large capacities.
    pub(crate) fn new(
        generation: ConnectionGeneration,
        reply_capacity: usize,
        delivery_capacity: usize,
    ) -> Result<Self, PublicationResourcesBuildError> {
        let authority = PublicationMutationAuthority::new();
        let replies = ReplyCapabilityLedger::new(generation, reply_capacity, &authority)
            .map_err(Self::reply_build_error)?;
        let reply_identity = replies.identity();
        let deliveries = ApplicationDeliveryLedger::new(
            generation,
            delivery_capacity,
            &authority,
            &reply_identity,
        )
        .map_err(Self::delivery_build_error)?;
        Ok(Self {
            generation,
            authority,
            replies,
            deliveries,
            #[cfg(test)]
            intervening_notice_for_test: None,
        })
    }

    /// Returns the TCP connection generation owned by this subaggregate.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the number of live pending application deliveries.
    pub(crate) fn delivery_len(&self) -> usize {
        self.deliveries.len()
    }

    /// Returns the number of pending or application-available Reply capabilities.
    pub(crate) fn reply_len(&self) -> usize {
        self.replies.len()
    }

    /// Admits one inbound W=0 Primary as a reliable application publication.
    ///
    /// `delivery_id` must be generation-unique and monotonically increasing.
    /// Success retains no Reply authority.
    pub(crate) fn admit_inbound_w0(
        &mut self,
        delivery_id: DeliveryId,
    ) -> Result<(), PublicationAdmissionError> {
        self.admit_delivery(delivery_id, DeliveryBinding::InboundPrimaryW0)
    }

    /// Atomically reserves and admits one inbound W=1 Primary publication.
    ///
    /// Delivery preflight occurs before Reply reservation. The reservation and
    /// Delivery commit then run without callbacks. Defensive Delivery failure
    /// revokes the exact fresh reservation before returning, so no orphan Reply
    /// authority can survive any structured error.
    pub(crate) fn admit_inbound_w1(
        &mut self,
        delivery_id: DeliveryId,
        capability_id: ReplyCapabilityId,
        contract: ReplyContract,
    ) -> Result<ReplyToken, PublicationAdmissionError> {
        let preparation = self
            .deliveries
            .prepare_registration(&self.authority, self.generation, delivery_id)
            .map_err(Self::delivery_admission_error)?;
        let reservation = self
            .replies
            .reserve_pending(capability_id, contract)
            .map_err(Self::reply_admission_error)?;
        #[cfg(test)]
        if let Some(notice_id) = self.intervening_notice_for_test.take() {
            self.admit_delivery(notice_id, DeliveryBinding::ProtocolNotice)
                .expect("test failpoint must insert one valid intervening notice");
        }
        let (ticket, token) = reservation.into_parts();
        let binding = DeliveryBinding::InboundPrimaryW1 { ticket };
        if let Err(rejection) =
            self.deliveries
                .commit_registration(&mut self.authority, preparation, binding)
        {
            let reason = rejection.reason();
            let (_reason, _preparation, binding) = rejection.into_parts();
            let ticket = match binding {
                DeliveryBinding::InboundPrimaryW1 { ticket } => ticket,
                DeliveryBinding::InboundPrimaryW0 | DeliveryBinding::ProtocolNotice => {
                    unreachable!("W=1 admission returned a non-W=1 binding")
                }
            };
            let rollback = self
                .replies
                .prepare_revocation(&ticket)
                .expect("fresh callback-free Reply reservation remains revocable");
            let _terminal = self
                .replies
                .commit_revocation(rollback)
                .expect("fresh callback-free Reply rollback remains exact");
            return Err(Self::delivery_admission_error(reason));
        }
        Ok(token)
    }

    /// Admits one non-data protocol notice as a reliable application publication.
    ///
    /// Protocol notices survive Selected-session reset but are drained by
    /// permanent generation close.
    pub(crate) fn admit_protocol_notice(
        &mut self,
        delivery_id: DeliveryId,
    ) -> Result<(), PublicationAdmissionError> {
        self.admit_delivery(delivery_id, DeliveryBinding::ProtocolNotice)
    }

    /// Completes one exact reliable application publication across both ledgers.
    ///
    /// W=1 `Delivered` makes Reply authority available. W=1 `Full` or `Closed`
    /// revokes it. Both child ledgers are fully preflighted before Delivery is
    /// removed, and the remaining Reply transition is infallible while this
    /// callback-free exclusive aggregate borrow is held.
    pub(crate) fn finish_delivery(
        &mut self,
        generation: ConnectionGeneration,
        delivery_id: DeliveryId,
        result: ApplicationDeliveryResult,
    ) -> Result<PublicationDeliveryTerminal, PublicationFinishError> {
        let preparation = self
            .deliveries
            .prepare_finish(generation, delivery_id, result)
            .map_err(Self::delivery_finish_prepare_error)?;
        let reply_action = match preparation.reply_ticket() {
            None => FinishReplyAction::None,
            Some(ticket) if result == ApplicationDeliveryResult::Delivered => {
                let decision = self.replies.preflight_publication(ticket);
                if decision != ReplyPublicationDecision::MadeAvailable {
                    return Err(Self::reply_publication_finish_error(
                        decision,
                        ticket.capability_id(),
                    ));
                }
                FinishReplyAction::Publish
            }
            Some(ticket) => {
                let plan = self.replies.prepare_revocation(ticket).map_err(|error| {
                    Self::reply_revocation_finish_error(error, ticket.capability_id())
                })?;
                FinishReplyAction::Revoke(plan)
            }
        };
        let commit = preparation.into_commit();
        let terminal = self
            .deliveries
            .commit_finish(&mut self.authority, commit)
            .map_err(|failure| Self::delivery_finish_commit_error(failure.reason()))?;
        let (generation, delivery_id, binding, result) = terminal.into_parts();
        let purpose = binding.purpose();
        match (reply_action, binding) {
            (FinishReplyAction::None, DeliveryBinding::InboundPrimaryW0)
            | (FinishReplyAction::None, DeliveryBinding::ProtocolNotice) => {}
            (FinishReplyAction::Publish, DeliveryBinding::InboundPrimaryW1 { ticket }) => {
                let decision = self.replies.mark_available(&ticket);
                assert_eq!(
                    decision,
                    ReplyPublicationDecision::MadeAvailable,
                    "callback-free Reply publication must match its successful preflight"
                );
            }
            (FinishReplyAction::Revoke(plan), DeliveryBinding::InboundPrimaryW1 { .. }) => {
                let _terminal = self
                    .replies
                    .commit_revocation(plan)
                    .expect("callback-free Reply revocation must match its successful preflight");
            }
            _ => unreachable!("Delivery binding changed after exact callback-free preflight"),
        }
        Ok(PublicationDeliveryTerminal {
            generation,
            delivery_id,
            purpose,
            result,
        })
    }

    /// Clears all Selected-session Data publications and every live Reply capability.
    ///
    /// The complete pending-ticket sets are cross-validated before mutation.
    /// Reply clear and Delivery drain then commit back-to-back without exposing
    /// their request, preparation, commit, or receipt values.
    pub(crate) fn reset_selected(
        &mut self,
    ) -> Result<PublicationResetSummary, PublicationResetError> {
        let (delivery_preparation, request) = self.deliveries.prepare_selected_session_reset();
        let reply_preparation = self
            .replies
            .prepare_selected_session_reset(&self.authority, request)
            .map_err(|failure| Self::reset_prepare_error(failure.reason()))?;
        let reply_commit = reply_preparation
            .validate_pending_tickets(delivery_preparation.reply_tickets())
            .map_err(|failure| Self::reset_validation_error(failure.reason()))?;
        let receipt = self
            .replies
            .commit_selected_session_reset(&mut self.authority, reply_commit)
            .map_err(|failure| Self::reset_commit_error(failure.reason()))?;
        let reply_summary = receipt.summary();
        let delivery_commit = delivery_preparation
            .authorize_reply_clear(receipt)
            .expect("same-call reset receipt must authorize its exact Delivery preparation");
        let delivery_summary = self
            .deliveries
            .commit_selected_session_reset(&mut self.authority, delivery_commit)
            .expect("callback-free Delivery reset must match its successful preflight");
        Ok(Self::reset_summary(delivery_summary, reply_summary))
    }

    /// Permanently closes both publication ledgers and drains every delivery.
    ///
    /// Repeated calls are idempotent. The complete pending-ticket sets are
    /// cross-validated before mutation, and no intermediate close authority
    /// leaves this synchronous aggregate method.
    pub(crate) fn close_generation(
        &mut self,
    ) -> Result<PublicationCloseSummary, PublicationCloseError> {
        let (delivery_preparation, request) = self.deliveries.prepare_close();
        let reply_preparation = self
            .replies
            .prepare_generation_end(&self.authority, request)
            .map_err(|failure| Self::close_prepare_error(failure.reason()))?;
        let reply_commit = reply_preparation
            .validate_pending_tickets(delivery_preparation.reply_tickets())
            .map_err(|failure| Self::close_validation_error(failure.reason()))?;
        let receipt = self
            .replies
            .commit_generation_end(&mut self.authority, reply_commit)
            .map_err(|failure| Self::close_commit_error(failure.reason()))?;
        let reply_summary = receipt.summary();
        let reply_began_close = receipt.began_close();
        let delivery_commit = delivery_preparation
            .authorize_reply_clear(receipt)
            .expect("same-call close receipt must authorize its exact Delivery preparation");
        let delivery_summary = self
            .deliveries
            .commit_close(&mut self.authority, delivery_commit)
            .expect("callback-free Delivery close must match its successful preflight");
        assert_eq!(
            delivery_summary.began_close(),
            reply_began_close,
            "Reply and Delivery generation-close fences must advance together"
        );
        Ok(Self::close_summary(delivery_summary, reply_summary))
    }

    /// Checks token generation and issuer routing without transferring ownership.
    ///
    /// The generation-first error ordering lets `HsmsCore` map an old
    /// generation token to `StaleConnectionGeneration` before reply-use
    /// preparation or any downstream resource preflight.
    pub(crate) fn precheck_reply_token_route(
        &self,
        token: &ReplyToken,
    ) -> Result<(), ReplyTokenRouteError> {
        self.replies.precheck_token_route(token)
    }

    /// Prepares one application-selected Reply use while retaining its token.
    ///
    /// Every failure returns the original token. Success bundles the token with
    /// its exact plan so downstream Operation and Write admission cannot lose
    /// or substitute application reply authority.
    pub(crate) fn prepare_reply_use(
        &self,
        token: ReplyToken,
        use_kind: ReplyUseKind,
    ) -> Result<PreparedReplyUse, PublicationReplyUsePrepareFailure> {
        match self.replies.prepare_use(&token, use_kind) {
            Ok(plan) => Ok(PreparedReplyUse { plan, token }),
            Err(reason) => Err(PublicationReplyUsePrepareFailure { reason, token }),
        }
    }

    /// Commits one exact reply-use plan together with its retained original token.
    ///
    /// Success consumes the capability exactly once. Every failure returns the
    /// unchanged plan-token bundle, so a caller can recover the token or later
    /// prepare an explicit `Abandon` terminal.
    #[allow(clippy::result_large_err)]
    pub(crate) fn commit_reply_use(
        &mut self,
        preparation: PreparedReplyUse,
    ) -> Result<ReplyUseTerminal, PublicationReplyUseCommitFailure> {
        let PreparedReplyUse { plan, token } = preparation;
        match self.replies.commit_use(plan, token) {
            Ok(terminal) => Ok(terminal),
            Err(failure) => {
                let (reason, plan, token) = failure.into_parts();
                Err(PublicationReplyUseCommitFailure {
                    reason,
                    preparation: PreparedReplyUse { plan, token },
                })
            }
        }
    }

    /// Admits one non-W=1 Delivery binding after exact read-only preflight.
    ///
    /// No other child ledger mutates, so every structured commit failure leaves
    /// the complete publication aggregate unchanged.
    fn admit_delivery(
        &mut self,
        delivery_id: DeliveryId,
        binding: DeliveryBinding,
    ) -> Result<(), PublicationAdmissionError> {
        let preparation = self
            .deliveries
            .prepare_registration(&self.authority, self.generation, delivery_id)
            .map_err(Self::delivery_admission_error)?;
        self.deliveries
            .commit_registration(&mut self.authority, preparation, binding)
            .map_err(|failure| Self::delivery_admission_error(failure.reason()))
    }

    /// Maps private Reply construction failures into facade-owned diagnostics.
    fn reply_build_error(error: ReplyLedgerConfigError) -> PublicationResourcesBuildError {
        match error {
            ReplyLedgerConfigError::ZeroCapacity => PublicationResourcesBuildError::ZeroCapacity {
                resource: PublicationResourceKind::ReplyCapability,
            },
        }
    }

    /// Maps private Delivery construction failures without exposing owner identities.
    fn delivery_build_error(error: DeliveryLedgerConfigError) -> PublicationResourcesBuildError {
        match error {
            DeliveryLedgerConfigError::ZeroCapacity => {
                PublicationResourcesBuildError::ZeroCapacity {
                    resource: PublicationResourceKind::ApplicationDelivery,
                }
            }
            DeliveryLedgerConfigError::ForeignReplyAggregate => {
                PublicationResourcesBuildError::InvariantViolation {
                    violation: PublicationInvariantViolation::OwnershipMismatch,
                }
            }
            DeliveryLedgerConfigError::ReplyGenerationMismatch { expected, actual } => {
                PublicationResourcesBuildError::InvariantViolation {
                    violation: PublicationInvariantViolation::GenerationMismatch {
                        expected,
                        actual,
                    },
                }
            }
        }
    }

    /// Maps private Delivery admission failures into publication-level causes.
    fn delivery_admission_error(error: DeliveryRegisterError) -> PublicationAdmissionError {
        match error {
            DeliveryRegisterError::Closing => PublicationAdmissionError::Closing,
            DeliveryRegisterError::WrongGeneration { expected, actual }
            | DeliveryRegisterError::ReplyTicketWrongGeneration { expected, actual } => {
                PublicationAdmissionError::WrongGeneration { expected, actual }
            }
            DeliveryRegisterError::DuplicateId { delivery_id } => {
                PublicationAdmissionError::DuplicateDelivery { delivery_id }
            }
            DeliveryRegisterError::NonMonotonicOrReusedId {
                highest_registered_id,
                attempted_id,
            } => PublicationAdmissionError::NonMonotonicOrReusedDelivery {
                highest_registered_id,
                attempted_id,
            },
            DeliveryRegisterError::CapacityExhausted { capacity } => {
                PublicationAdmissionError::CapacityExhausted {
                    resource: PublicationResourceKind::ApplicationDelivery,
                    capacity,
                }
            }
            DeliveryRegisterError::IncarnationExhausted => {
                PublicationAdmissionError::IdentityExhausted {
                    resource: PublicationResourceKind::ApplicationDelivery,
                }
            }
            DeliveryRegisterError::AdmissionStateChanged { delivery_id } => {
                PublicationAdmissionError::AdmissionStateChanged { delivery_id }
            }
            DeliveryRegisterError::ForeignAggregate
            | DeliveryRegisterError::ForeignLedger
            | DeliveryRegisterError::ReplyTicketForeignLedger => {
                PublicationAdmissionError::InvariantViolation {
                    violation: PublicationInvariantViolation::OwnershipMismatch,
                }
            }
        }
    }

    /// Maps private Reply reservation failures into publication-level causes.
    fn reply_admission_error(error: ReplyReserveError) -> PublicationAdmissionError {
        match error {
            ReplyReserveError::Closing => PublicationAdmissionError::Closing,
            ReplyReserveError::WrongGeneration { expected, actual } => {
                PublicationAdmissionError::WrongGeneration { expected, actual }
            }
            ReplyReserveError::DuplicateId { capability_id } => {
                PublicationAdmissionError::DuplicateReplyCapability { capability_id }
            }
            ReplyReserveError::CapacityExhausted { capacity } => {
                PublicationAdmissionError::CapacityExhausted {
                    resource: PublicationResourceKind::ReplyCapability,
                    capacity,
                }
            }
            ReplyReserveError::IncarnationExhausted => {
                PublicationAdmissionError::IdentityExhausted {
                    resource: PublicationResourceKind::ReplyCapability,
                }
            }
        }
    }

    /// Maps read-only Delivery completion failures into facade semantics.
    fn delivery_finish_prepare_error(error: DeliveryPrepareError) -> PublicationFinishError {
        match error {
            DeliveryPrepareError::WrongGeneration { expected, actual } => {
                PublicationFinishError::WrongGeneration { expected, actual }
            }
            DeliveryPrepareError::UnknownOrTerminal { delivery_id } => {
                PublicationFinishError::UnknownOrTerminalDelivery { delivery_id }
            }
        }
    }

    /// Maps a failed Reply-publication decision for `capability_id`.
    fn reply_publication_finish_error(
        decision: ReplyPublicationDecision,
        capability_id: ReplyCapabilityId,
    ) -> PublicationFinishError {
        match decision {
            ReplyPublicationDecision::WrongGeneration { expected, actual } => {
                PublicationFinishError::WrongGeneration { expected, actual }
            }
            ReplyPublicationDecision::ForeignLedger => PublicationFinishError::InvariantViolation {
                violation: PublicationInvariantViolation::OwnershipMismatch,
            },
            ReplyPublicationDecision::IncarnationChanged { capability_id }
            | ReplyPublicationDecision::ContractChanged { capability_id } => {
                PublicationFinishError::ReplyAuthorityChanged { capability_id }
            }
            ReplyPublicationDecision::MadeAvailable
            | ReplyPublicationDecision::AlreadyAvailable
            | ReplyPublicationDecision::UnknownOrTerminal => {
                PublicationFinishError::ReplyAuthorityChanged { capability_id }
            }
        }
    }

    /// Maps failed Reply revocation for the Delivery-owned capability.
    fn reply_revocation_finish_error(
        error: ReplyRevocationUnavailable,
        capability_id: ReplyCapabilityId,
    ) -> PublicationFinishError {
        match error {
            ReplyRevocationUnavailable::WrongGeneration { expected, actual } => {
                PublicationFinishError::WrongGeneration { expected, actual }
            }
            ReplyRevocationUnavailable::ForeignLedger => {
                PublicationFinishError::InvariantViolation {
                    violation: PublicationInvariantViolation::OwnershipMismatch,
                }
            }
            ReplyRevocationUnavailable::IncarnationChanged { capability_id }
            | ReplyRevocationUnavailable::ContractChanged { capability_id } => {
                PublicationFinishError::ReplyAuthorityChanged { capability_id }
            }
            ReplyRevocationUnavailable::UnknownOrTerminal => {
                PublicationFinishError::ReplyAuthorityChanged { capability_id }
            }
        }
    }

    /// Maps exact Delivery commit revalidation failures into facade semantics.
    fn delivery_finish_commit_error(error: DeliveryCommitError) -> PublicationFinishError {
        match error {
            DeliveryCommitError::WrongGeneration { expected, actual } => {
                PublicationFinishError::WrongGeneration { expected, actual }
            }
            DeliveryCommitError::UnknownOrTerminal { delivery_id } => {
                PublicationFinishError::UnknownOrTerminalDelivery { delivery_id }
            }
            DeliveryCommitError::IncarnationChanged { delivery_id }
            | DeliveryCommitError::BindingChanged { delivery_id } => {
                PublicationFinishError::InvariantViolation {
                    violation: PublicationInvariantViolation::DeliveryChanged { delivery_id },
                }
            }
            DeliveryCommitError::EntrySetChanged => PublicationFinishError::InvariantViolation {
                violation: PublicationInvariantViolation::DeliverySetChanged,
            },
            DeliveryCommitError::ClosingStateChanged { expected, actual } => {
                PublicationFinishError::InvariantViolation {
                    violation: PublicationInvariantViolation::LifecycleChanged {
                        expected_closing: expected,
                        actual_closing: actual,
                    },
                }
            }
            DeliveryCommitError::ClosingWithPendingEntries { pending } => {
                PublicationFinishError::InvariantViolation {
                    violation: PublicationInvariantViolation::ClosedWithRetainedResources {
                        resource: PublicationResourceKind::ApplicationDelivery,
                        count: pending,
                    },
                }
            }
            DeliveryCommitError::ForeignAggregate | DeliveryCommitError::ForeignLedger => {
                PublicationFinishError::InvariantViolation {
                    violation: PublicationInvariantViolation::OwnershipMismatch,
                }
            }
        }
    }

    /// Maps the first Reply half of Selected-session reset into facade semantics.
    fn reset_prepare_error(error: ReplyClearPrepareError) -> PublicationResetError {
        match error {
            ReplyClearPrepareError::Closing => PublicationResetError::Closing,
            other => PublicationResetError::InvariantViolation {
                violation: Self::clear_prepare_invariant(other),
            },
        }
    }

    /// Maps reset ticket-set validation into a public-safe invariant.
    fn reset_validation_error(error: ReplyClearValidationError) -> PublicationResetError {
        PublicationResetError::InvariantViolation {
            violation: Self::clear_validation_invariant(error),
        }
    }

    /// Maps reset mutation revalidation into a public-safe invariant.
    fn reset_commit_error(error: ReplyClearCommitError) -> PublicationResetError {
        PublicationResetError::InvariantViolation {
            violation: Self::clear_commit_invariant(error),
        }
    }

    /// Maps the first Reply half of generation close into facade semantics.
    fn close_prepare_error(error: ReplyClearPrepareError) -> PublicationCloseError {
        let violation = match error {
            ReplyClearPrepareError::Closing => PublicationInvariantViolation::LifecycleChanged {
                expected_closing: false,
                actual_closing: true,
            },
            other => Self::clear_prepare_invariant(other),
        };
        PublicationCloseError::InvariantViolation { violation }
    }

    /// Maps close ticket-set validation into a public-safe invariant.
    fn close_validation_error(error: ReplyClearValidationError) -> PublicationCloseError {
        PublicationCloseError::InvariantViolation {
            violation: Self::clear_validation_invariant(error),
        }
    }

    /// Maps close mutation revalidation into a public-safe invariant.
    fn close_commit_error(error: ReplyClearCommitError) -> PublicationCloseError {
        PublicationCloseError::InvariantViolation {
            violation: Self::clear_commit_invariant(error),
        }
    }

    /// Erases private clear-request mechanics while preserving their diagnosis.
    fn clear_prepare_invariant(error: ReplyClearPrepareError) -> PublicationInvariantViolation {
        match error {
            ReplyClearPrepareError::ForeignAggregate
            | ReplyClearPrepareError::ForeignRequestAggregate
            | ReplyClearPrepareError::ForeignReplyLedger => {
                PublicationInvariantViolation::OwnershipMismatch
            }
            ReplyClearPrepareError::WrongRequestGeneration { expected, actual } => {
                PublicationInvariantViolation::GenerationMismatch { expected, actual }
            }
            ReplyClearPrepareError::WrongRequestScope { .. } => {
                PublicationInvariantViolation::LifecycleScopeMismatch
            }
            ReplyClearPrepareError::Closing => PublicationInvariantViolation::LifecycleChanged {
                expected_closing: false,
                actual_closing: true,
            },
        }
    }

    /// Erases private ticket-validation mechanics while preserving identity facts.
    fn clear_validation_invariant(
        error: ReplyClearValidationError,
    ) -> PublicationInvariantViolation {
        match error {
            ReplyClearValidationError::ForeignLedger => {
                PublicationInvariantViolation::OwnershipMismatch
            }
            ReplyClearValidationError::WrongGeneration { expected, actual } => {
                PublicationInvariantViolation::GenerationMismatch { expected, actual }
            }
            ReplyClearValidationError::UnknownOrTerminal { capability_id }
            | ReplyClearValidationError::IncarnationChanged { capability_id }
            | ReplyClearValidationError::ContractChanged { capability_id }
            | ReplyClearValidationError::NotPendingPublication { capability_id }
            | ReplyClearValidationError::DuplicateDeliveryTicket { capability_id }
            | ReplyClearValidationError::MissingDeliveryTicket { capability_id } => {
                PublicationInvariantViolation::ReplyCapabilitySetChanged {
                    capability_id: Some(capability_id),
                }
            }
        }
    }

    /// Erases private clear-commit mechanics while preserving changed state.
    fn clear_commit_invariant(error: ReplyClearCommitError) -> PublicationInvariantViolation {
        match error {
            ReplyClearCommitError::ForeignAggregate | ReplyClearCommitError::ForeignLedger => {
                PublicationInvariantViolation::OwnershipMismatch
            }
            ReplyClearCommitError::WrongGeneration { expected, actual } => {
                PublicationInvariantViolation::GenerationMismatch { expected, actual }
            }
            ReplyClearCommitError::ClosingStateChanged { expected, actual } => {
                PublicationInvariantViolation::LifecycleChanged {
                    expected_closing: expected,
                    actual_closing: actual,
                }
            }
            ReplyClearCommitError::EntrySetChanged => {
                PublicationInvariantViolation::ReplyCapabilitySetChanged {
                    capability_id: None,
                }
            }
            ReplyClearCommitError::IncarnationChanged { capability_id }
            | ReplyClearCommitError::ContractChanged { capability_id }
            | ReplyClearCommitError::StateChanged { capability_id } => {
                PublicationInvariantViolation::ReplyCapabilityChanged { capability_id }
            }
            ReplyClearCommitError::ClosingWithLiveEntries { live } => {
                PublicationInvariantViolation::ClosedWithRetainedResources {
                    resource: PublicationResourceKind::ReplyCapability,
                    count: live,
                }
            }
        }
    }

    /// Converts private Delivery dispositions into ticket-free aggregate results.
    fn dispositions(summary: Vec<DeliveryDisposition>) -> Vec<PublicationDisposition> {
        summary
            .into_iter()
            .map(|disposition| {
                let purpose = disposition.purpose();
                let (delivery_id, _binding) = disposition.into_parts();
                PublicationDisposition {
                    delivery_id,
                    purpose,
                }
            })
            .collect()
    }

    /// Combines final child summaries after a successful Selected-session reset.
    fn reset_summary(
        deliveries: DeliveryResetSummary,
        replies: ReplyResetSummary,
    ) -> PublicationResetSummary {
        PublicationResetSummary {
            deliveries: Self::dispositions(deliveries.into_deliveries()),
            pending_reply_capabilities: replies.pending_publication(),
            available_reply_capabilities: replies.available(),
        }
    }

    /// Combines final child summaries after a successful generation close.
    fn close_summary(
        deliveries: DeliveryCloseSummary,
        replies: ReplyResetSummary,
    ) -> PublicationCloseSummary {
        PublicationCloseSummary {
            began_close: deliveries.began_close(),
            deliveries: Self::dispositions(deliveries.into_deliveries()),
            pending_reply_capabilities: replies.pending_publication(),
            available_reply_capabilities: replies.available(),
        }
    }

    /// Arms one test-only Delivery mutation between W=1 reserve and commit.
    ///
    /// The next W=1 admission inserts `notice_id` after Reply reservation,
    /// forcing defensive Delivery revalidation to fail and exercise exact
    /// Reply rollback. Production builds contain neither this field nor method.
    #[cfg(test)]
    fn intervene_next_w1_admission_with_notice(&mut self, notice_id: DeliveryId) {
        self.intervening_notice_for_test = Some(notice_id);
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        contracts::{ApplicationDeliveryResult, DeliveryPurpose, ReplyTokenRouteError},
        model::ids::{ConnectionGeneration, DeliveryId, ReplyCapabilityId, SystemBytes},
        Function, SessionId, Stream,
    };

    use super::{
        PublicationAdmissionError, PublicationResourceKind, PublicationResources,
        PublicationResourcesBuildError, ReplyContract, ReplyUseCommitError, ReplyUseKind,
        ReplyUseTerminal, ReplyUseUnavailable,
    };

    /// Generation shared by same-route aggregate tests.
    const GENERATION: ConnectionGeneration = ConnectionGeneration::new(7);

    /// Creates a deterministic W=1 reply contract for the requested generation.
    fn contract(generation: ConnectionGeneration, function: u8) -> ReplyContract {
        ReplyContract::from_primary_parts(
            generation,
            SessionId::new(3).expect("valid Data Session ID"),
            Stream::new(5).expect("valid stream"),
            Function::new(function),
            true,
            SystemBytes::new(0x0102_0304),
        )
        .expect("odd W=1 Primary")
    }

    /// Creates a publication aggregate with enough room for focused scenarios.
    fn resources(generation: ConnectionGeneration) -> PublicationResources {
        PublicationResources::new(generation, 8, 8).expect("positive publication capacities")
    }

    /// Confirms a W=1 publication moves through Pending, Delivered, Available,
    /// and one exact normal-reply terminal without leaking child artifacts.
    #[test]
    fn w1_delivered_then_reply_use_completes_the_full_aggregate_lifecycle() {
        let mut resources = resources(GENERATION);
        let capability_id = ReplyCapabilityId::new(11);
        let reply_contract = contract(GENERATION, 1);
        let token = resources
            .admit_inbound_w1(DeliveryId::new(1), capability_id, reply_contract)
            .expect("W=1 admission reserves both resources");
        assert_eq!(resources.delivery_len(), 1);
        assert_eq!(resources.reply_len(), 1);

        let terminal = resources
            .finish_delivery(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("Delivered publication publishes Reply authority");
        assert_eq!(terminal.generation(), GENERATION);
        assert_eq!(terminal.delivery_id(), DeliveryId::new(1));
        assert_eq!(
            terminal.purpose(),
            DeliveryPurpose::InboundReplyCapability(capability_id)
        );
        assert_eq!(terminal.result(), ApplicationDeliveryResult::Delivered);
        assert_eq!(resources.delivery_len(), 0);
        assert_eq!(resources.reply_len(), 1);
        assert_eq!(resources.precheck_reply_token_route(&token), Ok(()));

        let preparation = resources
            .prepare_reply_use(token, ReplyUseKind::Normal)
            .expect("available reply token prepares normal F+1");
        assert_eq!(preparation.generation(), GENERATION);
        assert_eq!(preparation.capability_id(), capability_id);
        assert_eq!(preparation.contract(), reply_contract);
        assert_eq!(preparation.kind(), ReplyUseKind::Normal);
        assert_eq!(
            resources
                .commit_reply_use(preparation)
                .expect("exact prepared reply use commits"),
            ReplyUseTerminal::Consumed {
                contract: reply_contract,
                use_kind: ReplyUseKind::Normal,
            }
        );
        assert_eq!(resources.reply_len(), 0);
    }

    /// Confirms downstream preflight may return a prepared Reply token, after
    /// which the same authority can be prepared again and consumed exactly once.
    #[test]
    fn downstream_failure_recovers_token_for_reprepare_and_commit() {
        let mut resources = resources(GENERATION);
        let capability_id = ReplyCapabilityId::new(12);
        let reply_contract = contract(GENERATION, 1);
        let token = resources
            .admit_inbound_w1(DeliveryId::new(1), capability_id, reply_contract)
            .expect("W=1 admission");
        let _terminal = resources
            .finish_delivery(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("Delivered publication makes the Reply token available");

        let first_preparation = resources
            .prepare_reply_use(token, ReplyUseKind::Normal)
            .expect("first downstream preflight obtains Reply authority");
        assert_eq!(first_preparation.capability_id(), capability_id);
        assert_eq!(first_preparation.contract(), reply_contract);
        let recovered_token = first_preparation.into_token();
        assert_eq!(
            resources.reply_len(),
            1,
            "discarding a preparation must not consume Reply authority"
        );

        let second_preparation = resources
            .prepare_reply_use(recovered_token, ReplyUseKind::Normal)
            .expect("the recovered token can be prepared again");
        assert_eq!(
            resources
                .commit_reply_use(second_preparation)
                .expect("the re-prepared Reply use commits"),
            ReplyUseTerminal::Consumed {
                contract: reply_contract,
                use_kind: ReplyUseKind::Normal,
            }
        );
        assert_eq!(resources.reply_len(), 0);
    }

    /// Confirms a token rejected during pending application publication is
    /// returned unchanged and becomes usable after that Delivery is reported.
    #[test]
    fn pending_publication_failure_returns_token_for_later_delivered_use() {
        let mut resources = resources(GENERATION);
        let capability_id = ReplyCapabilityId::new(13);
        let reply_contract = contract(GENERATION, 3);
        let token = resources
            .admit_inbound_w1(DeliveryId::new(1), capability_id, reply_contract)
            .expect("W=1 admission retains pending Reply authority");

        let failure = resources
            .prepare_reply_use(token, ReplyUseKind::Normal)
            .expect_err("pending application publication cannot authorize a Reply");
        assert_eq!(failure.reason(), ReplyUseUnavailable::PendingPublication);
        let (reason, recovered_token) = failure.into_parts();
        assert_eq!(reason, ReplyUseUnavailable::PendingPublication);
        assert_eq!(resources.reply_len(), 1);

        let _terminal = resources
            .finish_delivery(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("Delivered publication makes the same token available");
        let preparation = resources
            .prepare_reply_use(recovered_token, ReplyUseKind::Normal)
            .expect("the recovered token is usable after Delivered");
        assert_eq!(preparation.capability_id(), capability_id);
        assert_eq!(
            resources
                .commit_reply_use(preparation)
                .expect("the recovered token commits exactly once"),
            ReplyUseTerminal::Consumed {
                contract: reply_contract,
                use_kind: ReplyUseKind::Normal,
            }
        );
        assert_eq!(resources.reply_len(), 0);
    }

    /// Confirms a defensive Delivery commit failure after Reply reservation
    /// revokes the fresh reservation and leaves no orphan capability.
    #[test]
    fn w1_admission_defensive_commit_failure_rolls_back_reply_exactly() {
        let mut resources = resources(GENERATION);
        resources.intervene_next_w1_admission_with_notice(DeliveryId::new(2));

        assert!(matches!(
            resources.admit_inbound_w1(
                DeliveryId::new(1),
                ReplyCapabilityId::new(1),
                contract(GENERATION, 1),
            ),
            Err(PublicationAdmissionError::AdmissionStateChanged { delivery_id })
                if delivery_id == DeliveryId::new(1)
        ));
        assert_eq!(resources.reply_len(), 0);
        assert_eq!(resources.delivery_len(), 1);

        let close = resources
            .close_generation()
            .expect("only the intervening notice remains");
        assert_eq!(close.deliveries().len(), 1);
        assert_eq!(close.deliveries()[0].delivery_id(), DeliveryId::new(2));
        assert_eq!(
            close.deliveries()[0].purpose(),
            DeliveryPurpose::ProtocolNotice
        );
        assert_eq!(close.pending_reply_capabilities(), 0);
        assert_eq!(close.available_reply_capabilities(), 0);
    }

    /// Confirms runtime Full and Closed outcomes revoke pending Reply authority
    /// while returning ticket-free Delivery terminals.
    #[test]
    fn full_and_closed_w1_deliveries_revoke_reply_authority() {
        for result in [
            ApplicationDeliveryResult::Full,
            ApplicationDeliveryResult::Closed,
        ] {
            let mut resources = resources(GENERATION);
            let token = resources
                .admit_inbound_w1(
                    DeliveryId::new(1),
                    ReplyCapabilityId::new(1),
                    contract(GENERATION, 1),
                )
                .expect("W=1 admission");
            let terminal = resources
                .finish_delivery(GENERATION, DeliveryId::new(1), result)
                .expect("failed publication revokes Reply before returning");
            assert_eq!(terminal.result(), result);
            assert_eq!(resources.delivery_len(), 0);
            assert_eq!(resources.reply_len(), 0);

            let failure = resources
                .prepare_reply_use(token, ReplyUseKind::Abort)
                .expect_err("revoked token cannot prepare a reply");
            assert_eq!(failure.reason(), ReplyUseUnavailable::UnknownOrTerminal);
            let (_reason, _token) = failure.into_parts();
        }
    }

    /// Confirms Selected reset removes W=0 Data but retains protocol notices,
    /// while generation close later drains that retained notice.
    #[test]
    fn selected_reset_and_close_apply_distinct_w0_notice_scopes() {
        let mut resources = resources(GENERATION);
        resources
            .admit_inbound_w0(DeliveryId::new(1))
            .expect("W=0 delivery admission");
        resources
            .admit_protocol_notice(DeliveryId::new(2))
            .expect("protocol notice admission");

        let reset = resources
            .reset_selected()
            .expect("Selected reset clears only Data delivery");
        assert_eq!(reset.deliveries().len(), 1);
        assert_eq!(reset.deliveries()[0].delivery_id(), DeliveryId::new(1));
        assert_eq!(
            reset.deliveries()[0].purpose(),
            DeliveryPurpose::InboundPrimary
        );
        assert_eq!(reset.pending_reply_capabilities(), 0);
        assert_eq!(reset.available_reply_capabilities(), 0);
        assert_eq!(resources.delivery_len(), 1);

        let close = resources
            .close_generation()
            .expect("generation close drains retained notice");
        assert!(close.began_close());
        assert_eq!(close.deliveries().len(), 1);
        assert_eq!(close.deliveries()[0].delivery_id(), DeliveryId::new(2));
        assert_eq!(
            close.deliveries()[0].purpose(),
            DeliveryPurpose::ProtocolNotice
        );
        assert_eq!(resources.delivery_len(), 0);
        assert_eq!(resources.reply_len(), 0);

        let repeated = resources
            .close_generation()
            .expect("generation close is idempotent");
        assert!(!repeated.began_close());
        assert!(repeated.deliveries().is_empty());
    }

    /// Confirms each facade reset captures fresh state: an old completed clear
    /// cannot be retained across calls to authorize later Reply or Delivery state.
    #[test]
    fn reset_intermediates_cannot_escape_or_authorize_later_reply_state() {
        let mut resources = resources(GENERATION);
        let first_token = resources
            .admit_inbound_w1(
                DeliveryId::new(1),
                ReplyCapabilityId::new(1),
                contract(GENERATION, 1),
            )
            .expect("first W=1 admission");
        let _terminal = resources
            .finish_delivery(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("first capability becomes available");

        let first_reset = resources
            .reset_selected()
            .expect("first reset clears available authority");
        assert!(first_reset.deliveries().is_empty());
        assert_eq!(first_reset.pending_reply_capabilities(), 0);
        assert_eq!(first_reset.available_reply_capabilities(), 1);
        let stale = resources
            .prepare_reply_use(first_token, ReplyUseKind::Abort)
            .expect_err("completed reset invalidates the old token");
        assert_eq!(stale.reason(), ReplyUseUnavailable::UnknownOrTerminal);
        let (_reason, _stale_token) = stale.into_parts();

        let _second_token = resources
            .admit_inbound_w1(
                DeliveryId::new(2),
                ReplyCapabilityId::new(2),
                contract(GENERATION, 3),
            )
            .expect("new state is admitted only after the old reset completed");
        let second_reset = resources
            .reset_selected()
            .expect("second reset captures a fresh pending set");
        assert_eq!(second_reset.deliveries().len(), 1);
        assert_eq!(
            second_reset.deliveries()[0].delivery_id(),
            DeliveryId::new(2)
        );
        assert_eq!(second_reset.pending_reply_capabilities(), 1);
        assert_eq!(second_reset.available_reply_capabilities(), 0);
        assert_eq!(resources.delivery_len(), 0);
        assert_eq!(resources.reply_len(), 0);
    }

    /// Confirms foreign prepare and commit failures preserve the original live
    /// token until its owning aggregate consumes it successfully.
    #[test]
    fn foreign_live_token_failures_preserve_ownership_for_the_owner() {
        let mut owner = resources(GENERATION);
        let mut foreign = resources(GENERATION);
        let reply_contract = contract(GENERATION, 1);
        let token = owner
            .admit_inbound_w1(
                DeliveryId::new(1),
                ReplyCapabilityId::new(1),
                reply_contract,
            )
            .expect("owner admission");
        let _terminal = owner
            .finish_delivery(
                GENERATION,
                DeliveryId::new(1),
                ApplicationDeliveryResult::Delivered,
            )
            .expect("owner publishes token");

        let prepare_failure = foreign
            .prepare_reply_use(token, ReplyUseKind::Normal)
            .expect_err("same-generation foreign issuer is rejected");
        assert_eq!(prepare_failure.reason(), ReplyUseUnavailable::ForeignIssuer);
        let (_reason, token) = prepare_failure.into_parts();

        let preparation = owner
            .prepare_reply_use(token, ReplyUseKind::Normal)
            .expect("owner prepares the recovered token");
        let commit_failure = foreign
            .commit_reply_use(preparation)
            .expect_err("foreign aggregate cannot commit the owner's plan-token pair");
        assert_eq!(commit_failure.reason(), ReplyUseCommitError::ForeignIssuer);
        let (_reason, preparation) = commit_failure.into_parts();
        assert_eq!(
            owner
                .commit_reply_use(preparation)
                .expect("owning aggregate commits the recovered preparation"),
            ReplyUseTerminal::Consumed {
                contract: reply_contract,
                use_kind: ReplyUseKind::Normal,
            }
        );
        assert_eq!(owner.reply_len(), 0);
        assert_eq!(foreign.reply_len(), 0);
    }

    /// Confirms generation-first borrowed routing can implement a stable
    /// stale-connection command error without consuming an old token.
    #[test]
    fn stale_generation_route_is_reported_before_foreign_issuer() {
        let mut old = resources(ConnectionGeneration::new(7));
        let current = resources(ConnectionGeneration::new(8));
        let token = old
            .admit_inbound_w1(
                DeliveryId::new(1),
                ReplyCapabilityId::new(1),
                contract(ConnectionGeneration::new(7), 1),
            )
            .expect("old generation W=1 admission");

        assert_eq!(
            current.precheck_reply_token_route(&token),
            Err(ReplyTokenRouteError::WrongGeneration {
                expected: ConnectionGeneration::new(8),
                actual: ConnectionGeneration::new(7),
            })
        );
        assert_eq!(old.precheck_reply_token_route(&token), Ok(()));
    }

    /// Confirms both zero child capacities are reported through the aggregate
    /// constructor without partially exposing either child owner.
    #[test]
    fn aggregate_construction_reports_each_child_capacity_error() {
        assert!(matches!(
            PublicationResources::new(GENERATION, 0, 1),
            Err(PublicationResourcesBuildError::ZeroCapacity {
                resource: PublicationResourceKind::ReplyCapability,
            })
        ));
        assert!(matches!(
            PublicationResources::new(GENERATION, 1, 0),
            Err(PublicationResourcesBuildError::ZeroCapacity {
                resource: PublicationResourceKind::ApplicationDelivery,
            })
        ));
    }
}
