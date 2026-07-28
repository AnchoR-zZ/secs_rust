//! Linear write and peer-response contracts shared by Core and generation runtime.
//!
//! This module binds every scheduled message to one write occurrence and binds
//! every deferred peer response to one independently branded control occurrence.
//! The child lifecycle module advances those authorities through scheduling,
//! fencing, proceed or abort, and terminal completion without reconstructing
//! mutation permission from copyable identity snapshots.
//!
//! Phase A guarantees exact occurrence matching only within one issuer instance.
//! It does not prove that a connection generation has a single authoritative
//! ControlFsm or issuer. Phase B must seal construction and ownership inside the
//! generation's `HsmsCore`/`CoreResources` aggregate and bind its generation brand.

mod lifecycle;
mod normalization;

use std::sync::Arc;

use crate::hsms::{
    model::ids::{ConnectionGeneration, OperationId, WriteId},
    protocol::{header::ControlMessage, message::ProtocolMessage},
};

use super::orchestration::{
    OutboundHeaderIdentity, OutboundMessageShapeError, OutboundOperationKind, OutboundRole,
};

#[allow(unused_imports)]
pub(crate) use lifecycle::{
    AbandonedPeerResponse, AbortWriteReceipt, AbortingAuthority, BeginWriteFailure,
    BeginWriteFence, BeginWriteObservation, CommittedPeerHookAbort, CommittedPeerResponseFence,
    ForeignPeerResponseResolution, InvalidWriteAuthority, MustCloseGeneration, NoHookFence,
    PeerHookAbort, PeerHookAbortError, PeerHookAbortRejection, PeerResponseFence,
    PeerResponseFenceContinuation, PeerResponseResolutionAbort, PeerResponseResolutionError,
    ProceedWriteReceipt, ProceededAuthority, QueuedAuthority, SchedulingAuthority,
    TerminalWriteTransition, WriteBindError, WriteBindFailure, WriteReceiptIssuer,
    WriteRegistration, WriteSpec, WriteSpecError, WriteSpecFailure, WriteTerminalOutcome,
    WriteTerminalReceipt,
};
#[allow(unused_imports)]
pub(crate) use normalization::{normalize_runtime_result, WritePhase};

/// Scheduling class derived from the exact protocol message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteClass {
    /// Control and protocol-safety traffic that bypasses the Data gate.
    Critical,
    /// Ordinary Data traffic governed by the Data gate.
    Data,
}

impl WriteClass {
    /// Derives writer admission policy from the complete protocol-message variant.
    ///
    /// `message` is the exact value later owned by [`PreparedWrite`]. Control
    /// messages always use the critical lane and Data messages always use the
    /// bounded Data lane.
    const fn from_message(message: &ProtocolMessage) -> Self {
        match message {
            ProtocolMessage::Control(_) => Self::Critical,
            ProtocolMessage::Data(_) => Self::Data,
        }
    }
}

/// Whether the generation writer currently accepts ordinary Data traffic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataGateState {
    /// New Data writes may be scheduled.
    Open,
    /// New Data writes are rejected while critical traffic may continue.
    Closed,
}

/// Reason the runtime could not reserve a writer queue position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScheduleFailure {
    /// The bounded writer queue has no remaining capacity.
    CapacityExhausted,
    /// The Data gate rejected an ordinary Data write.
    DataGateClosed,
    /// The generation scheduler is no longer accepting work.
    SchedulerStopped,
}

/// Deferred semantic transition encoded by one peer-response occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerResponseCommitKind {
    /// The response carries no selection-state transition.
    None,
    /// A successful peer `Select.rsp` commits selection.
    SelectAccepted,
    /// A successful peer `Deselect.rsp` commits deselection.
    DeselectAccepted,
}

/// Private allocation whose address brands one peer-response issuer instance.
#[derive(Debug)]
struct PeerResponseCommitBrand {
    /// Zero-sized field preventing construction outside this module.
    private: (),
}

/// One exact peer-response issuance.
///
/// Its allocation identity distinguishes repeated same-header responses from
/// each other. The exact response and semantic transition therefore cannot be
/// rebound merely by reproducing their copyable field values.
#[derive(Debug)]
struct PeerResponseOccurrence {
    /// Connection generation that owns the response.
    generation: ConnectionGeneration,
    /// Private identity of the issuer that created this occurrence.
    issuer_brand: Arc<PeerResponseCommitBrand>,
    /// Exact typed control response that must be scheduled.
    response: ControlMessage,
    /// Deferred transition permitted by this exact response.
    kind: PeerResponseCommitKind,
}

/// Error returned when an issuer receives an inconsistent response request.
#[must_use = "an invalid peer-response issuance must be handled"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerResponseIssueError {
    /// A no-transition issuance was requested for a response that changes state
    /// or is not a supported response to a peer request.
    NoTransitionResponse {
        /// Exact inconsistent response supplied to the issuer.
        response: ControlMessage,
    },
    /// Select acceptance was requested for a non-successful or non-Select response.
    SelectAcceptance {
        /// Exact inconsistent response supplied to the issuer.
        response: ControlMessage,
    },
    /// Deselect acceptance was requested for a non-successful or non-Deselect response.
    DeselectAcceptance {
        /// Exact inconsistent response supplied to the issuer.
        response: ControlMessage,
    },
}

/// Independently branded peer-response authority issuer.
///
/// The issuer is intended to be owned by one ControlFsm instance. Phase A does
/// not prevent another crate module from constructing a separate issuer for the
/// same generation; aggregate ownership is deliberately deferred to Phase B.
#[must_use = "a peer-response issuer owns one exact issuer-instance brand"]
#[derive(Debug)]
pub(crate) struct PeerResponseCommitIssuer {
    /// Exact connection generation stamped onto this issuer's occurrences.
    generation: ConnectionGeneration,
    /// Private issuer identity copied into each distinct occurrence.
    brand: Arc<PeerResponseCommitBrand>,
}

impl PeerResponseCommitIssuer {
    /// Creates a fresh issuer brand for one intended ControlFsm instance.
    ///
    /// Phase A exposes this constructor only through the narrow
    /// `contracts::peer_response` façade but does not prove that `generation`
    /// has only one issuer. Phase B must seal construction inside the
    /// generation's `HsmsCore`/`CoreResources` owner.
    pub(crate) fn new(generation: ConnectionGeneration) -> Self {
        Self::allocate(generation)
    }

    /// Creates an issuer for isolated write-contract and FSM unit tests.
    ///
    /// Production builds do not contain this convenience constructor.
    #[cfg(test)]
    pub(crate) fn new_for_test(generation: ConnectionGeneration) -> Self {
        Self::allocate(generation)
    }

    /// Allocates the unique issuer brand shared by production and test setup.
    fn allocate(generation: ConnectionGeneration) -> Self {
        Self {
            generation,
            brand: Arc::new(PeerResponseCommitBrand { private: () }),
        }
    }

    /// Issues a no-transition bundle for one exact non-success or Linktest response.
    ///
    /// `response` must be a non-successful Select/Deselect response or a
    /// Linktest response. The return value inseparably owns that message and its
    /// one-shot commit authority.
    ///
    /// # Errors
    ///
    /// Returns [`PeerResponseIssueError::NoTransitionResponse`] if the
    /// exact response would require a selection transition or is not a supported
    /// response to a peer request.
    pub(crate) fn issue_none(
        &self,
        response: ControlMessage,
    ) -> Result<PendingPeerResponseWrite, PeerResponseIssueError> {
        let valid = match response {
            ControlMessage::SelectResponse { status, .. } => !status.is_success(),
            ControlMessage::DeselectResponse { status, .. } => !status.is_success(),
            ControlMessage::LinktestResponse { .. } => true,
            _ => false,
        };
        if !valid {
            return Err(PeerResponseIssueError::NoTransitionResponse { response });
        }
        Ok(self.issue(response, PeerResponseCommitKind::None))
    }

    /// Issues a Select-acceptance bundle for one exact successful Select response.
    ///
    /// # Errors
    ///
    /// Returns [`PeerResponseIssueError::SelectAcceptance`] if
    /// `response` is not a successful `Select.rsp`.
    pub(crate) fn issue_select_accepted(
        &self,
        response: ControlMessage,
    ) -> Result<PendingPeerResponseWrite, PeerResponseIssueError> {
        if !matches!(
            response,
            ControlMessage::SelectResponse { status, .. } if status.is_success()
        ) {
            return Err(PeerResponseIssueError::SelectAcceptance { response });
        }
        Ok(self.issue(response, PeerResponseCommitKind::SelectAccepted))
    }

    /// Issues a Deselect-acceptance bundle for one exact successful Deselect response.
    ///
    /// # Errors
    ///
    /// Returns [`PeerResponseIssueError::DeselectAcceptance`] if
    /// `response` is not a successful `Deselect.rsp`.
    pub(crate) fn issue_deselect_accepted(
        &self,
        response: ControlMessage,
    ) -> Result<PendingPeerResponseWrite, PeerResponseIssueError> {
        if !matches!(
            response,
            ControlMessage::DeselectResponse { status, .. } if status.is_success()
        ) {
            return Err(PeerResponseIssueError::DeselectAcceptance { response });
        }
        Ok(self.issue(response, PeerResponseCommitKind::DeselectAccepted))
    }

    /// Returns whether `commit` belongs to this exact issuer instance.
    ///
    /// The check compares generation and allocation identity. It performs no
    /// mutation and is suitable for the prepare phase of a cross-ledger commit.
    pub(crate) fn owns(&self, commit: &PeerResponseCommit) -> bool {
        self.generation == commit.generation()
            && Arc::ptr_eq(&self.brand, &commit.occurrence.issuer_brand)
    }

    /// Consumes validated authority into a success receipt.
    ///
    /// # Errors
    ///
    /// A foreign authority is returned intact through
    /// [`ForeignPeerResponseCommit`] so a failed prepare/commit boundary cannot
    /// lose its one-shot input.
    pub(crate) fn commit(
        &self,
        commit: PeerResponseCommit,
    ) -> Result<PeerResponseCommitReceipt, ForeignPeerResponseCommit> {
        if !self.owns(&commit) {
            return Err(ForeignPeerResponseCommit { commit });
        }
        Ok(PeerResponseCommitReceipt {
            occurrence: commit.occurrence,
        })
    }

    /// Creates one independently branded occurrence after semantic validation.
    fn issue(
        &self,
        response: ControlMessage,
        kind: PeerResponseCommitKind,
    ) -> PendingPeerResponseWrite {
        let occurrence = Arc::new(PeerResponseOccurrence {
            generation: self.generation,
            issuer_brand: Arc::clone(&self.brand),
            response,
            kind,
        });
        PendingPeerResponseWrite {
            commit: PeerResponseCommit { occurrence },
        }
    }
}

/// Inseparable response message and deferred peer-response commit authority.
///
/// The value is intentionally move-only. Callers may inspect its exact response
/// but only [`WriteSpec::peer_response`] may turn it into a schedulable write.
#[must_use = "a pending peer response must be scheduled or closed with its generation"]
#[derive(Debug)]
pub(crate) struct PendingPeerResponseWrite {
    /// Commit authority whose occurrence also owns the exact response message.
    commit: PeerResponseCommit,
}

impl PendingPeerResponseWrite {
    /// Returns the generation that issued this peer response.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.commit.generation()
    }

    /// Returns the exact typed response owned by this occurrence for binding.
    ///
    /// This observer is private to the sealed write-contract module. Production
    /// callers must not copy the response out and reconstruct a no-hook write.
    fn response(&self) -> ControlMessage {
        self.commit.response()
    }

    /// Returns the exact typed response for focused in-crate test assertions.
    ///
    /// The observer is absent from production builds so it cannot bypass the
    /// peer-response bundle in normal Core code.
    #[cfg(test)]
    pub(crate) fn response_for_test(&self) -> ControlMessage {
        self.response()
    }

    /// Consumes the bundle into its commit for focused ControlFsm tests.
    ///
    /// Production builds omit this escape hatch; normal code must cross the
    /// WriteLedger BeginWrite fence before ControlFsm receives the authority.
    #[cfg(test)]
    pub(crate) fn into_commit_for_test(self) -> PeerResponseCommit {
        self.into_commit()
    }

    /// Returns whether the response carries no deferred state transition.
    pub(crate) fn is_none(&self) -> bool {
        self.commit.is_none()
    }

    /// Returns whether the response commits successful peer selection.
    pub(crate) fn is_select_accepted(&self) -> bool {
        self.commit.is_select_accepted()
    }

    /// Returns whether the response commits successful peer deselection.
    pub(crate) fn is_deselect_accepted(&self) -> bool {
        self.commit.is_deselect_accepted()
    }

    /// Consumes the bundle into its exact one-shot commit authority.
    ///
    /// The response remains retained inside the occurrence and can therefore
    /// still be checked when the authority is committed.
    fn into_commit(self) -> PeerResponseCommit {
        self.commit
    }
}

/// One-shot authority committed only after the exact response reaches BeginWrite.
#[must_use = "peer-response authority must be retained through its write fence"]
#[derive(Debug)]
pub(crate) struct PeerResponseCommit {
    /// Unique issuance containing generation, exact response, issuer, and kind.
    occurrence: Arc<PeerResponseOccurrence>,
}

impl PeerResponseCommit {
    /// Returns the exact connection generation bound to this authority.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact typed response retained by this authority.
    ///
    /// This remains private to the sealed write-contract module so a commit
    /// cannot be used as a raw-message escape hatch.
    fn response(&self) -> ControlMessage {
        self.occurrence.response
    }

    /// Returns whether this response carries no deferred transition.
    pub(crate) fn is_none(&self) -> bool {
        matches!(self.occurrence.kind, PeerResponseCommitKind::None)
    }

    /// Returns whether this response commits peer Select acceptance.
    pub(crate) fn is_select_accepted(&self) -> bool {
        matches!(self.occurrence.kind, PeerResponseCommitKind::SelectAccepted)
    }

    /// Returns whether this response commits peer Deselect acceptance.
    pub(crate) fn is_deselect_accepted(&self) -> bool {
        matches!(
            self.occurrence.kind,
            PeerResponseCommitKind::DeselectAccepted
        )
    }

    /// Captures an opaque exact-occurrence expectation for the write fence.
    fn expectation(&self) -> PeerResponseExpectation {
        PeerResponseExpectation {
            occurrence: Arc::clone(&self.occurrence),
        }
    }
}

/// Move-only proof that one issuer committed one exact response occurrence.
///
/// The receipt alone does not prove that an authoritative ControlFsm mutated.
/// That stronger fact exists only when the receipt is returned inside
/// `core::control::CommittedPeerResponse`; Phase B will seal that path behind
/// the generation aggregate.
#[must_use = "a committed peer-response receipt must resolve its write fence"]
#[derive(Debug)]
pub(crate) struct PeerResponseCommitReceipt {
    /// Exact committed peer-response occurrence.
    occurrence: Arc<PeerResponseOccurrence>,
}

impl PeerResponseCommitReceipt {
    /// Returns the exact generation that committed the response.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns whether the committed response carried no transition.
    pub(crate) fn is_none(&self) -> bool {
        matches!(self.occurrence.kind, PeerResponseCommitKind::None)
    }

    /// Returns whether peer Select acceptance was committed.
    pub(crate) fn is_select_accepted(&self) -> bool {
        matches!(self.occurrence.kind, PeerResponseCommitKind::SelectAccepted)
    }

    /// Returns whether peer Deselect acceptance was committed.
    pub(crate) fn is_deselect_accepted(&self) -> bool {
        matches!(
            self.occurrence.kind,
            PeerResponseCommitKind::DeselectAccepted
        )
    }

    /// Returns whether this receipt proves `expectation`'s exact occurrence.
    fn matches(&self, expectation: &PeerResponseExpectation) -> bool {
        Arc::ptr_eq(&self.occurrence, &expectation.occurrence)
    }
}

/// Move-only rejection returning foreign peer-response authority intact.
#[must_use = "foreign peer-response authority must be recovered"]
#[derive(Debug)]
pub(crate) struct ForeignPeerResponseCommit {
    /// Exact authority rejected by the ControlFsm issuer.
    commit: PeerResponseCommit,
}

impl ForeignPeerResponseCommit {
    /// Recovers the exact foreign authority.
    pub(crate) fn into_commit(self) -> PeerResponseCommit {
        self.commit
    }
}

/// Exact peer-response occurrence expected by one write fence.
#[derive(Debug)]
struct PeerResponseExpectation {
    /// Unique issuance that a successful control receipt must share.
    occurrence: Arc<PeerResponseOccurrence>,
}

/// Private allocation whose address brands one generation-local WriteLedger.
#[derive(Debug)]
struct WriteIssuerBrand {
    /// Zero-sized field preventing construction outside the write contracts.
    private: (),
}

/// Private identity shared by all linear authorities for one write issuance.
#[derive(Debug)]
struct WriteOccurrence {
    /// Private identity of the WriteReceiptIssuer that bound this occurrence.
    issuer_brand: Arc<WriteIssuerBrand>,
    /// Connection generation whose writer owns the occurrence.
    generation: ConnectionGeneration,
    /// Core-assigned identity of this write attempt.
    write_id: WriteId,
    /// Operation whose lifetime owns the write.
    operation_id: OperationId,
    /// Scheduling class derived from the exact message.
    class: WriteClass,
    /// Complete immutable HSMS header identity derived from the message.
    identity: OutboundHeaderIdentity,
    /// Exact semantic outbound kind derived from the message.
    kind: OutboundOperationKind,
}

/// Owned write descriptor passed exactly once to the generation runtime.
///
/// Construction is restricted to [`WriteReceiptIssuer::bind`]. The scheduling
/// class and both outbound identities are always derived from the exact owned
/// message; callers cannot supply them independently.
#[must_use = "a prepared write must be scheduled or explicitly rejected"]
#[derive(Debug)]
pub(crate) struct PreparedWrite {
    /// Unique write occurrence shared with the Core-side scheduling authority.
    occurrence: Arc<WriteOccurrence>,
    /// Exact typed protocol message the runtime must encode and write.
    message: ProtocolMessage,
}

impl PreparedWrite {
    /// Creates the runtime half of one already validated write registration.
    fn from_bound(occurrence: Arc<WriteOccurrence>, message: ProtocolMessage) -> Self {
        Self {
            occurrence,
            message,
        }
    }

    /// Returns the exact connection generation that must schedule this write.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Borrows the exact protocol message retained for runtime dispatch.
    pub(crate) fn message(&self) -> &ProtocolMessage {
        &self.message
    }

    /// Returns the Core-assigned write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns the operation that owns this write.
    pub(crate) fn operation_id(&self) -> OperationId {
        self.occurrence.operation_id
    }

    /// Returns the scheduling class derived from the exact message.
    pub(crate) fn class(&self) -> WriteClass {
        self.occurrence.class
    }

    /// Returns the complete outbound header identity derived from the message.
    pub(crate) fn identity(&self) -> OutboundHeaderIdentity {
        self.occurrence.identity
    }

    /// Returns the exact outbound operation kind derived from the message.
    pub(crate) fn kind(&self) -> OutboundOperationKind {
        self.occurrence.kind
    }

    /// Consumes the descriptor into the exact message and bound metadata.
    ///
    /// Returns `(generation, message, write_id, operation_id, class, identity,
    /// kind)`. The private occurrence identity is consumed and is never exposed.
    pub(crate) fn into_parts(
        self,
    ) -> (
        ConnectionGeneration,
        ProtocolMessage,
        WriteId,
        OperationId,
        WriteClass,
        OutboundHeaderIdentity,
        OutboundOperationKind,
    ) {
        (
            self.occurrence.generation,
            self.message,
            self.occurrence.write_id,
            self.occurrence.operation_id,
            self.occurrence.class,
            self.occurrence.identity,
            self.occurrence.kind,
        )
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        model::ids::{ConnectionGeneration, SystemBytes},
        protocol::header::{ControlMessage, SelectStatus},
    };

    use super::{PeerResponseCommitIssuer, PeerResponseIssueError};

    /// Rejects a successful Select response when no transition was requested.
    #[test]
    fn peer_response_issuer_rejects_inconsistent_kind_and_message() {
        let issuer = PeerResponseCommitIssuer::new_for_test(ConnectionGeneration::new(3));
        let response = ControlMessage::SelectResponse {
            session_id: u16::MAX,
            status: SelectStatus::SUCCESS,
            system_bytes: SystemBytes::new(7),
        };

        let error = issuer
            .issue_none(response)
            .expect_err("successful Select response requires an acceptance hook");
        assert_eq!(
            error,
            PeerResponseIssueError::NoTransitionResponse { response }
        );
    }

    /// Distinguishes repeated same-shaped responses by occurrence identity.
    #[test]
    fn repeated_peer_responses_have_distinct_occurrences() {
        let issuer = PeerResponseCommitIssuer::new_for_test(ConnectionGeneration::new(3));
        let response = ControlMessage::SelectResponse {
            session_id: u16::MAX,
            status: SelectStatus::SUCCESS,
            system_bytes: SystemBytes::new(7),
        };
        let first = issuer
            .issue_select_accepted(response)
            .expect("successful Select response is coherent");
        let second = issuer
            .issue_select_accepted(response)
            .expect("same-shaped issuance remains independently coherent");
        let first_expectation = first.commit.expectation();
        let second_receipt = issuer
            .commit(second.into_commit())
            .expect("issuer accepts its own second occurrence");

        assert!(!second_receipt.matches(&first_expectation));
    }
}
