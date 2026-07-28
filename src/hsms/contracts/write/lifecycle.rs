//! Linear typestates for one outbound write from registration to terminal result.
//!
//! A generation-local `WriteReceiptIssuer` binds an exact `WriteSpec` once.
//! Every transition consumes the previous authority, and only that chain can
//! issue runtime permissions or terminal evidence. Peer-response writes retain
//! their exact issuer occurrence until a genuine matching commit receipt
//! resolves the fence.

use std::sync::Arc;

use crate::hsms::{
    model::{
        ids::{ConnectionGeneration, OperationId, WireSequence, WriteId},
        runtime::{EffectiveWriteResult, WriteResult, WriteRuntimeViolation},
    },
    protocol::message::ProtocolMessage,
};

use super::{
    normalize_runtime_result, OutboundHeaderIdentity, OutboundMessageShapeError, OutboundRole,
    PeerResponseCommit, PeerResponseCommitReceipt, PeerResponseExpectation,
    PendingPeerResponseWrite, PreparedWrite, ScheduleFailure, WriteClass, WriteIssuerBrand,
    WriteOccurrence, WritePhase,
};

/// Sealed semantic input from which one write registration may be bound.
///
/// Private representation prevents crate callers from constructing either a
/// raw control-response write or an independently paired message and hook.
#[must_use = "a write specification must be bound or returned intact"]
#[derive(Debug)]
pub(crate) struct WriteSpec {
    /// Private semantic variant selected only by validated constructors.
    kind: WriteSpecKind,
}

/// Private semantic variant retained by one sealed [`WriteSpec`].
#[derive(Debug)]
enum WriteSpecKind {
    /// A write whose BeginWrite fence has no deferred semantic transition.
    NoHook {
        /// Exact protocol message that the runtime must schedule.
        message: ProtocolMessage,
    },
    /// An exact peer response and its one-shot issuer authority.
    PeerResponse {
        /// Inseparable response message and deferred commit authority.
        pending: PendingPeerResponseWrite,
    },
}

/// Stable reason a raw message cannot become a no-hook write specification.
#[must_use = "a write-specification error must be handled"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteSpecError {
    /// A peer control response must retain its exact issuer-created bundle.
    ControlResponseRequiresPeerBundle,
}

/// Move-only specification failure returning the original protocol message.
#[must_use = "a failed write specification contains the original message"]
#[derive(Debug)]
pub(crate) struct WriteSpecFailure {
    /// Stable validation error.
    error: WriteSpecError,
    /// Exact message rejected before a write specification was constructed.
    message: ProtocolMessage,
}

impl WriteSpecFailure {
    /// Returns the stable construction error without consuming the message.
    pub(crate) const fn error(&self) -> WriteSpecError {
        self.error
    }

    /// Borrows the exact rejected protocol message.
    pub(crate) const fn message(&self) -> &ProtocolMessage {
        &self.message
    }

    /// Consumes the failure into its error and original message.
    ///
    /// Returns `(error, message)` without reconstructing a write specification.
    pub(crate) fn into_parts(self) -> (WriteSpecError, ProtocolMessage) {
        (self.error, self.message)
    }
}

impl WriteSpec {
    /// Creates a write specification with no BeginWrite hook.
    ///
    /// `message` is retained exactly and later moved into [`PreparedWrite`].
    /// Peer control responses are deliberately rejected because only a
    /// [`PendingPeerResponseWrite`] can preserve their deferred semantic hook.
    ///
    /// # Errors
    ///
    /// Returns [`WriteSpecFailure`] with `message` intact when the message is a
    /// Select, Deselect, or Linktest response.
    pub(crate) fn no_hook(message: ProtocolMessage) -> Result<Self, WriteSpecFailure> {
        let is_control_response = OutboundHeaderIdentity::from_protocol_message(&message)
            .is_ok_and(|identity| identity.role() == OutboundRole::ControlResponse);
        if is_control_response {
            return Err(WriteSpecFailure {
                error: WriteSpecError::ControlResponseRequiresPeerBundle,
                message,
            });
        }
        Ok(Self {
            kind: WriteSpecKind::NoHook { message },
        })
    }

    /// Creates a write specification from one exact peer-response bundle.
    ///
    /// No independent message argument is accepted, so the scheduled response
    /// cannot diverge from the issuer authority.
    pub(crate) fn peer_response(pending: PendingPeerResponseWrite) -> Self {
        Self {
            kind: WriteSpecKind::PeerResponse { pending },
        }
    }

    /// Returns the generation carried by a peer response, if this write has one.
    pub(crate) fn peer_response_generation(&self) -> Option<ConnectionGeneration> {
        match &self.kind {
            WriteSpecKind::NoHook { .. } => None,
            WriteSpecKind::PeerResponse { pending } => Some(pending.generation()),
        }
    }

    /// Returns whether the sealed specification owns a peer-response bundle.
    #[cfg(test)]
    fn is_peer_response(&self) -> bool {
        matches!(&self.kind, WriteSpecKind::PeerResponse { .. })
    }
}

/// Stable reason a write specification could not be bound.
#[must_use = "a write-bind error must be handled"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteBindError {
    /// The exact protocol message has an invalid outbound semantic shape.
    InvalidMessageShape {
        /// Structured shape error derived from the complete message.
        error: OutboundMessageShapeError,
    },
    /// The peer response belongs to a different connection generation.
    PeerResponseGenerationMismatch {
        /// Generation owned by the WriteReceiptIssuer.
        expected: ConnectionGeneration,
        /// Generation carried by the pending peer response.
        actual: ConnectionGeneration,
    },
}

/// Move-only bind failure returning the complete original specification.
#[must_use = "a failed write bind returns its original specification"]
#[derive(Debug)]
pub(crate) struct WriteBindFailure {
    /// Stable validation error.
    error: WriteBindError,
    /// Exact unconsumed write specification.
    spec: WriteSpec,
}

impl WriteBindFailure {
    /// Returns the stable bind error without consuming the specification.
    pub(crate) const fn error(&self) -> WriteBindError {
        self.error
    }

    /// Borrows the exact write specification returned by validation.
    pub(crate) const fn spec(&self) -> &WriteSpec {
        &self.spec
    }

    /// Consumes the failure into its error and original specification.
    pub(crate) fn into_parts(self) -> (WriteBindError, WriteSpec) {
        (self.error, self.spec)
    }
}

/// Generation-local issuer that starts and verifies every write authority chain.
#[must_use = "a write receipt issuer owns one WriteLedger-instance brand"]
#[derive(Debug)]
pub(crate) struct WriteReceiptIssuer {
    /// Exact connection generation accepted by this issuer.
    generation: ConnectionGeneration,
    /// Private WriteLedger-instance identity.
    brand: Arc<WriteIssuerBrand>,
}

impl WriteReceiptIssuer {
    /// Creates a fresh issuer for one generation-local WriteLedger.
    pub(crate) fn new(generation: ConnectionGeneration) -> Self {
        Self {
            generation,
            brand: Arc::new(WriteIssuerBrand { private: () }),
        }
    }

    /// Binds an exact write specification to one fresh write occurrence.
    ///
    /// `write_id` identifies this attempt, `operation_id` identifies its owner,
    /// and `spec` supplies either a no-hook message or an exact peer-response
    /// bundle. The returned registration is the only authority capable of
    /// entering the scheduling lifecycle.
    ///
    /// # Errors
    ///
    /// Returns [`WriteBindFailure`] without mutation and with `spec` intact
    /// when the message shape is invalid or a peer response belongs to another
    /// generation.
    pub(crate) fn bind(
        &self,
        write_id: WriteId,
        operation_id: OperationId,
        spec: WriteSpec,
    ) -> Result<WriteRegistration, WriteBindFailure> {
        if let Some(actual) = spec.peer_response_generation() {
            if actual != self.generation {
                return Err(WriteBindFailure {
                    error: WriteBindError::PeerResponseGenerationMismatch {
                        expected: self.generation,
                        actual,
                    },
                    spec,
                });
            }
        }

        let identity_result = match &spec.kind {
            WriteSpecKind::NoHook { message } => {
                OutboundHeaderIdentity::from_protocol_message(message)
            }
            WriteSpecKind::PeerResponse { pending } => {
                OutboundHeaderIdentity::from_protocol_message(&ProtocolMessage::Control(
                    pending.response(),
                ))
            }
        };
        let identity = match identity_result {
            Ok(identity) => identity,
            Err(error) => {
                return Err(WriteBindFailure {
                    error: WriteBindError::InvalidMessageShape { error },
                    spec,
                });
            }
        };
        let kind = identity.kind();

        let (message, peer_response) = match spec.kind {
            WriteSpecKind::NoHook { message } => (message, None),
            WriteSpecKind::PeerResponse { pending } => {
                let response = pending.response();
                (
                    ProtocolMessage::Control(response),
                    Some(pending.into_commit()),
                )
            }
        };
        let class = WriteClass::from_message(&message);
        let occurrence = Arc::new(WriteOccurrence {
            issuer_brand: Arc::clone(&self.brand),
            generation: self.generation,
            write_id,
            operation_id,
            class,
            identity,
            kind,
        });
        let prepared = PreparedWrite::from_bound(Arc::clone(&occurrence), message);

        Ok(WriteRegistration {
            prepared,
            occurrence,
            peer_response,
        })
    }

    /// Returns whether `occurrence` belongs to this exact issuer and generation.
    fn owns_occurrence(&self, occurrence: &Arc<WriteOccurrence>) -> bool {
        occurrence.generation == self.generation
            && Arc::ptr_eq(&self.brand, &occurrence.issuer_brand)
    }

    /// Splits an exact peer-response fence for the CoreResources commit phase.
    ///
    /// The returned commit is consumed by ControlFsm. The continuation retains
    /// both the exact write occurrence and the exact expected peer occurrence.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidWriteAuthority`] with `fence` intact when it belongs to
    /// another WriteLedger instance or generation.
    pub(crate) fn split_peer_response_fence(
        &self,
        fence: PeerResponseFence,
    ) -> Result<
        (PeerResponseCommit, PeerResponseFenceContinuation),
        InvalidWriteAuthority<PeerResponseFence>,
    > {
        if !self.owns_occurrence(&fence.occurrence) {
            return Err(InvalidWriteAuthority { authority: fence });
        }
        let expectation = fence.commit.expectation();
        Ok((
            fence.commit,
            PeerResponseFenceContinuation {
                occurrence: fence.occurrence,
                wire_sequence: fence.wire_sequence,
                expectation,
            },
        ))
    }

    /// Resolves a split peer fence with the exact issuer success receipt.
    ///
    /// # Errors
    ///
    /// Returns [`ForeignPeerResponseResolution`] with both linear inputs intact
    /// when either the write occurrence is foreign or the control receipt proves
    /// another peer-response occurrence.
    pub(crate) fn resolve_peer_response(
        &self,
        continuation: PeerResponseFenceContinuation,
        receipt: PeerResponseCommitReceipt,
    ) -> Result<CommittedPeerResponseFence, ForeignPeerResponseResolution> {
        if !self.owns_occurrence(&continuation.occurrence) {
            return Err(ForeignPeerResponseResolution {
                reason: PeerResponseResolutionError::ForeignWriteOccurrence,
                continuation,
                receipt,
            });
        }
        if !receipt.matches(&continuation.expectation) {
            return Err(ForeignPeerResponseResolution {
                reason: PeerResponseResolutionError::ForeignPeerOccurrence,
                continuation,
                receipt,
            });
        }
        Ok(CommittedPeerResponseFence {
            occurrence: continuation.occurrence,
            wire_sequence: continuation.wire_sequence,
        })
    }

    /// Aborts an unresolved write after a committed receipt mismatched its fence.
    ///
    /// `failure` preserves the mismatching ControlFsm receipt. When its write
    /// continuation belongs to this issuer, the method converts that exact
    /// fence into runtime Abort permission plus a mandatory close marker while
    /// returning the foreign receipt for its rightful aggregate owner.
    ///
    /// # Errors
    ///
    /// Returns `failure` intact when its write continuation belongs to another
    /// WriteLedger instance or generation.
    pub(crate) fn abort_failed_peer_response_resolution(
        &self,
        failure: ForeignPeerResponseResolution,
    ) -> Result<PeerResponseResolutionAbort, ForeignPeerResponseResolution> {
        if !self.owns_occurrence(&failure.continuation.occurrence) {
            return Err(failure);
        }
        let continuation = failure.continuation;
        let must_close = MustCloseGeneration {
            generation: continuation.occurrence.generation,
            write_id: continuation.occurrence.write_id,
        };
        let (receipt, authority) =
            aborting(continuation.occurrence, continuation.wire_sequence, None);
        Ok(PeerResponseResolutionAbort {
            reason: failure.reason,
            abort_receipt: receipt,
            authority,
            must_close,
            foreign_receipt: failure.receipt,
        })
    }

    /// Converts an unsplit peer fence into an exact abort and must-close bundle.
    ///
    /// This is the zero-Control-mutation path used when hook preflight fails.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidWriteAuthority`] with `fence` intact when it belongs to
    /// another WriteLedger instance or generation.
    pub(crate) fn abort_peer_response_fence(
        &self,
        fence: PeerResponseFence,
    ) -> Result<PeerHookAbort, InvalidWriteAuthority<PeerResponseFence>> {
        if !self.owns_occurrence(&fence.occurrence) {
            return Err(InvalidWriteAuthority { authority: fence });
        }
        Ok(peer_hook_abort(
            fence.occurrence,
            fence.wire_sequence,
            fence.commit,
        ))
    }

    /// Converts a split continuation and returned exact commit into abort+close.
    ///
    /// This is the path used when ControlFsm commit fails and returns its
    /// one-shot authority without mutation.
    ///
    /// # Errors
    ///
    /// Returns [`PeerHookAbortRejection`] with both inputs intact if the write
    /// authority is foreign or `commit` is not the continuation's occurrence.
    pub(crate) fn abort_peer_response_continuation(
        &self,
        continuation: PeerResponseFenceContinuation,
        commit: PeerResponseCommit,
    ) -> Result<PeerHookAbort, PeerHookAbortRejection> {
        let reason = if !self.owns_occurrence(&continuation.occurrence) {
            Some(PeerHookAbortError::ForeignWriteOccurrence)
        } else if !commit
            .expectation()
            .same_occurrence(&continuation.expectation)
        {
            Some(PeerHookAbortError::ForeignPeerOccurrence)
        } else {
            None
        };
        if let Some(reason) = reason {
            return Err(PeerHookAbortRejection {
                reason,
                continuation,
                commit,
            });
        }
        Ok(peer_hook_abort(
            continuation.occurrence,
            continuation.wire_sequence,
            commit,
        ))
    }
}

/// Move-only rejection returning a foreign write authority intact.
#[must_use = "a rejected write authority must be recovered"]
#[derive(Debug)]
pub(crate) struct InvalidWriteAuthority<T> {
    /// Exact authority rejected by a WriteReceiptIssuer.
    authority: T,
}

impl<T> InvalidWriteAuthority<T> {
    /// Recovers the exact rejected authority.
    pub(crate) fn into_authority(self) -> T {
        self.authority
    }
}

/// First linear authority for one successfully bound write.
#[must_use = "a write registration must enter scheduling or be retained"]
#[derive(Debug)]
pub(crate) struct WriteRegistration {
    /// Exact runtime descriptor created during binding.
    prepared: PreparedWrite,
    /// Unique Core-side write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Exact peer-response commit, absent for no-hook writes.
    peer_response: Option<PeerResponseCommit>,
}

impl WriteRegistration {
    /// Returns the exact connection generation owning this registration.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns the owning operation identity.
    pub(crate) fn operation_id(&self) -> OperationId {
        self.occurrence.operation_id
    }

    /// Returns the scheduling class derived from the exact message.
    pub(crate) fn class(&self) -> WriteClass {
        self.occurrence.class
    }

    /// Borrows the exact prepared runtime descriptor.
    pub(crate) const fn prepared(&self) -> &PreparedWrite {
        &self.prepared
    }

    /// Consumes registration into the runtime descriptor and scheduling authority.
    ///
    /// The two returned values share one private occurrence. Neither value can
    /// independently reconstruct another authority chain.
    pub(crate) fn into_scheduling(self) -> (PreparedWrite, SchedulingAuthority) {
        (
            self.prepared,
            SchedulingAuthority {
                occurrence: self.occurrence,
                peer_response: self.peer_response,
            },
        )
    }
}

/// Authority retained by WriteLedger while runtime schedules a prepared write.
#[must_use = "a scheduling authority must be queued or terminalized"]
#[derive(Debug)]
pub(crate) struct SchedulingAuthority {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Exact uncommitted peer response, if present.
    peer_response: Option<PeerResponseCommit>,
}

impl SchedulingAuthority {
    /// Returns the exact connection generation owning this write.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns whether `prepared` is the runtime half of this exact occurrence.
    pub(crate) fn matches_prepared(&self, prepared: &PreparedWrite) -> bool {
        Arc::ptr_eq(&self.occurrence, &prepared.occurrence)
    }

    /// Consumes successful scheduling acknowledgement into queued authority.
    ///
    /// `wire_sequence` is the ordered writer position assigned by runtime and
    /// remains bound through every subsequent fence and receipt.
    pub(crate) fn acknowledge_queued(self, wire_sequence: WireSequence) -> QueuedAuthority {
        QueuedAuthority {
            occurrence: self.occurrence,
            wire_sequence,
            peer_response: self.peer_response,
        }
    }

    /// Consumes a scheduling rejection into its unique terminal transition.
    ///
    /// `failure` is the scheduler admission result. No wire sequence exists on
    /// this path. A retained peer hook also yields a must-close marker because
    /// its protocol response can no longer be committed.
    pub(crate) fn schedule_failed(self, failure: ScheduleFailure) -> TerminalWriteTransition {
        let must_close = must_close_for_hook(&self.occurrence, &self.peer_response);
        let abandoned_peer_response = self.peer_response.map(AbandonedPeerResponse::from_commit);
        TerminalWriteTransition {
            receipt: WriteTerminalReceipt {
                occurrence: self.occurrence,
                wire_sequence: None,
                outcome: WriteTerminalOutcome::ScheduleFailed {
                    failure,
                    abandoned_peer_response,
                },
                runtime_violation: None,
            },
            must_close,
        }
    }

    /// Consumes an impossible early WriteFinished callback into terminal evidence.
    ///
    /// `result` is retained unchanged and normalization records
    /// `TerminalWhileScheduling`. A pending peer hook additionally requires the
    /// generation to close.
    pub(crate) fn finish_unexpected(self, result: WriteResult) -> TerminalWriteTransition {
        terminal_finished(
            self.occurrence,
            None,
            WritePhase::Scheduling,
            false,
            result,
            self.peer_response,
            false,
        )
    }
}

/// Copyable runtime observation used to validate one queued BeginWrite callback.
///
/// This value is data only; the queued authority remains the sole permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BeginWriteObservation {
    /// Connection generation reporting the fence.
    generation: ConnectionGeneration,
    /// Exact write whose ordered writer position reached the fence.
    write_id: WriteId,
    /// Ordered writer sequence assigned at scheduling acknowledgement.
    wire_sequence: WireSequence,
}

impl BeginWriteObservation {
    /// Creates one typed BeginWrite observation from runtime callback fields.
    pub(crate) const fn new(
        generation: ConnectionGeneration,
        write_id: WriteId,
        wire_sequence: WireSequence,
    ) -> Self {
        Self {
            generation,
            write_id,
            wire_sequence,
        }
    }

    /// Returns the reporting connection generation.
    pub(crate) const fn generation(self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the reported write identity.
    pub(crate) const fn write_id(self) -> WriteId {
        self.write_id
    }

    /// Returns the reported ordered writer sequence.
    pub(crate) const fn wire_sequence(self) -> WireSequence {
        self.wire_sequence
    }
}

/// Authority retained after the runtime reserves an ordered writer position.
#[must_use = "a queued authority must reach BeginWrite or terminal completion"]
#[derive(Debug)]
pub(crate) struct QueuedAuthority {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Runtime-assigned ordered writer sequence.
    wire_sequence: WireSequence,
    /// Exact uncommitted peer response, if present.
    peer_response: Option<PeerResponseCommit>,
}

impl QueuedAuthority {
    /// Returns the exact connection generation owning this queued write.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact queued write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns the ordered writer sequence assigned by runtime.
    pub(crate) const fn wire_sequence(&self) -> WireSequence {
        self.wire_sequence
    }

    /// Consumes the exact BeginWrite callback into a hook-specific fence type.
    ///
    /// # Errors
    ///
    /// Returns [`BeginWriteFailure`] with both the queued authority and
    /// observation intact when generation, WriteId, or WireSequence differs.
    pub(crate) fn begin(
        self,
        observation: BeginWriteObservation,
    ) -> Result<BeginWriteFence, BeginWriteFailure> {
        if observation.generation != self.occurrence.generation
            || observation.write_id != self.occurrence.write_id
            || observation.wire_sequence != self.wire_sequence
        {
            return Err(BeginWriteFailure {
                authority: self,
                observation,
            });
        }
        Ok(match self.peer_response {
            None => BeginWriteFence::NoHook(NoHookFence {
                occurrence: self.occurrence,
                wire_sequence: self.wire_sequence,
            }),
            Some(commit) => BeginWriteFence::PeerResponse(PeerResponseFence {
                occurrence: self.occurrence,
                wire_sequence: self.wire_sequence,
                commit,
            }),
        })
    }

    /// Consumes an impossible pre-fence WriteFinished callback into terminal evidence.
    pub(crate) fn finish_unexpected(self, result: WriteResult) -> TerminalWriteTransition {
        terminal_finished(
            self.occurrence,
            Some(self.wire_sequence),
            WritePhase::Queued,
            false,
            result,
            self.peer_response,
            false,
        )
    }
}

/// Move-only BeginWrite mismatch returning both exact inputs.
#[must_use = "a rejected BeginWrite callback leaves the queued write pending"]
#[derive(Debug)]
pub(crate) struct BeginWriteFailure {
    /// Exact queued authority that rejected the observation.
    authority: QueuedAuthority,
    /// Exact mismatching runtime observation.
    observation: BeginWriteObservation,
}

impl BeginWriteFailure {
    /// Borrows the queued authority without consuming the failure.
    pub(crate) const fn authority(&self) -> &QueuedAuthority {
        &self.authority
    }

    /// Returns the mismatching runtime observation.
    pub(crate) const fn observation(&self) -> BeginWriteObservation {
        self.observation
    }

    /// Consumes the failure into the queued authority and observation.
    pub(crate) fn into_parts(self) -> (QueuedAuthority, BeginWriteObservation) {
        (self.authority, self.observation)
    }
}

/// Hook-specific result of consuming one exact queued BeginWrite observation.
#[must_use = "a BeginWrite fence must be resolved exactly once"]
#[derive(Debug)]
pub(crate) enum BeginWriteFence {
    /// Fence with no deferred semantic transition.
    NoHook(NoHookFence),
    /// Fence retaining an exact peer-response issuer authority.
    PeerResponse(PeerResponseFence),
}

/// Move-only BeginWrite fence with no semantic hook.
#[must_use = "a no-hook fence must proceed, abort, or terminalize"]
#[derive(Debug)]
pub(crate) struct NoHookFence {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Exact ordered writer sequence held at the fence.
    wire_sequence: WireSequence,
}

impl NoHookFence {
    /// Returns the exact connection generation owning this fence.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact fenced write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns the exact ordered writer sequence held at the fence.
    pub(crate) const fn wire_sequence(&self) -> WireSequence {
        self.wire_sequence
    }

    /// Consumes the fence into runtime Proceed permission and Core authority.
    ///
    /// The receipt is emitted to runtime; `ProceededAuthority` remains in
    /// WriteLedger and is the only path to terminal evidence.
    pub(crate) fn proceed(self) -> (ProceedWriteReceipt, ProceededAuthority) {
        proceeded(self.occurrence, self.wire_sequence, false)
    }

    /// Consumes the fence into runtime Abort permission and Core authority.
    ///
    /// Abort is permission, not a terminal result; runtime must still report the
    /// final `WriteResult`.
    pub(crate) fn abort(self) -> (AbortWriteReceipt, AbortingAuthority) {
        aborting(self.occurrence, self.wire_sequence, None)
    }

    /// Consumes a terminal callback that bypassed fence resolution.
    pub(crate) fn finish_unexpected(self, result: WriteResult) -> TerminalWriteTransition {
        terminal_finished(
            self.occurrence,
            Some(self.wire_sequence),
            WritePhase::Fenced,
            false,
            result,
            None,
            false,
        )
    }
}

/// Move-only BeginWrite fence retaining one exact peer-response commit.
#[must_use = "a peer-response fence must commit its exact hook or abort and close"]
#[derive(Debug)]
pub(crate) struct PeerResponseFence {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Exact ordered writer sequence held at the fence.
    wire_sequence: WireSequence,
    /// Exact peer-response authority issued with the scheduled message.
    commit: PeerResponseCommit,
}

impl PeerResponseFence {
    /// Returns the exact connection generation owning this fence.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact fenced write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns the exact ordered writer sequence held at the fence.
    pub(crate) const fn wire_sequence(&self) -> WireSequence {
        self.wire_sequence
    }

    /// Borrows the exact peer-response commit for side-effect-free preflight.
    pub(crate) const fn commit(&self) -> &PeerResponseCommit {
        &self.commit
    }

    /// Consumes a terminal callback that bypassed peer-hook commit.
    ///
    /// The result records an unresolved-fence violation and carries a must-close
    /// marker because the peer-response state can no longer be committed safely.
    pub(crate) fn finish_unexpected(self, result: WriteResult) -> TerminalWriteTransition {
        terminal_finished(
            self.occurrence,
            Some(self.wire_sequence),
            WritePhase::Fenced,
            false,
            result,
            Some(self.commit),
            false,
        )
    }
}

/// Opaque write continuation retained while ControlFsm consumes the exact commit.
#[must_use = "a peer-response continuation must be resolved or aborted with its returned commit"]
#[derive(Debug)]
pub(crate) struct PeerResponseFenceContinuation {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Exact ordered writer sequence held at the fence.
    wire_sequence: WireSequence,
    /// Exact peer-response occurrence required for success or returned abort.
    expectation: PeerResponseExpectation,
}

/// Peer-response fence resolved by its exact issuer commit receipt.
///
/// In the production FSM path, resolution follows a successful ControlFsm
/// transition. This typestate itself proves exact occurrence matching, not
/// generation-wide uniqueness of the ControlFsm owner.
///
/// This typestate deliberately exposes only [`Self::proceed`]. There is no
/// normal silent-abort transition after the semantic hook has committed.
#[must_use = "a committed peer-response fence must proceed"]
#[derive(Debug)]
pub(crate) struct CommittedPeerResponseFence {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Exact ordered writer sequence held at the fence.
    wire_sequence: WireSequence,
}

impl CommittedPeerResponseFence {
    /// Returns the exact connection generation owning this committed fence.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact committed write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Consumes the committed hook into Proceed permission and terminal authority.
    pub(crate) fn proceed(self) -> (ProceedWriteReceipt, ProceededAuthority) {
        proceeded(self.occurrence, self.wire_sequence, true)
    }

    /// Aborts this committed hook only as part of mandatory generation close.
    ///
    /// Unlike the ordinary no-hook abort, the returned bundle always includes
    /// [`MustCloseGeneration`]. The issuer receipt has committed and the
    /// production aggregate path may already have advanced its ControlFsm, so
    /// normal protocol work conservatively remains blocked if the response is
    /// cancelled.
    pub(crate) fn abort_for_generation_close(self) -> CommittedPeerHookAbort {
        let must_close = MustCloseGeneration {
            generation: self.occurrence.generation,
            write_id: self.occurrence.write_id,
        };
        let (receipt, authority) = aborting(self.occurrence, self.wire_sequence, None);
        CommittedPeerHookAbort {
            receipt,
            authority,
            must_close,
        }
    }
}

/// Stable reason an exact peer-response success receipt could not resolve a fence.
#[must_use = "a peer-response resolution error must be handled"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerResponseResolutionError {
    /// The continuation belongs to another WriteReceiptIssuer.
    ForeignWriteOccurrence,
    /// The receipt proves another peer-response issuance.
    ForeignPeerOccurrence,
}

/// Move-only resolution failure returning continuation and control receipt intact.
#[must_use = "a failed peer-response resolution returns both linear inputs"]
#[derive(Debug)]
pub(crate) struct ForeignPeerResponseResolution {
    /// Stable mismatch classification.
    reason: PeerResponseResolutionError,
    /// Exact unresolved write continuation.
    continuation: PeerResponseFenceContinuation,
    /// Exact mismatching issuer success receipt.
    receipt: PeerResponseCommitReceipt,
}

impl ForeignPeerResponseResolution {
    /// Returns the stable mismatch classification.
    pub(crate) const fn reason(&self) -> PeerResponseResolutionError {
        self.reason
    }

    /// Consumes the failure into its reason and both original authorities.
    pub(crate) fn into_parts(
        self,
    ) -> (
        PeerResponseResolutionError,
        PeerResponseFenceContinuation,
        PeerResponseCommitReceipt,
    ) {
        (self.reason, self.continuation, self.receipt)
    }
}

/// Abnormal abort after a committed receipt failed exact fence resolution.
///
/// The generation must close because an issuer receipt has committed while
/// this response write remains unresolved. The production aggregate path may
/// already have advanced its ControlFsm, so recovery conservatively aborts and
/// closes. The mismatching receipt is returned intact so its rightful write
/// occurrence may still account for the issuer evidence during shutdown.
#[must_use = "a peer-response resolution abort must be fully applied"]
#[derive(Debug)]
pub(crate) struct PeerResponseResolutionAbort {
    /// Stable reason the committed receipt did not resolve this write.
    reason: PeerResponseResolutionError,
    /// Runtime permission to cancel the still-fenced write.
    abort_receipt: AbortWriteReceipt,
    /// Core authority awaiting the exact abort completion.
    authority: AbortingAuthority,
    /// Proof that normal generation work may not resume.
    must_close: MustCloseGeneration,
    /// Committed receipt that belongs to another peer-response occurrence.
    foreign_receipt: PeerResponseCommitReceipt,
}

impl PeerResponseResolutionAbort {
    /// Consumes the abnormal abort into all required recovery authorities.
    ///
    /// Returns `(reason, abort_receipt, authority, must_close,
    /// foreign_receipt)` so CoreResources can abort, close, and retain the
    /// already committed issuer evidence in deterministic order.
    pub(crate) fn into_parts(
        self,
    ) -> (
        PeerResponseResolutionError,
        AbortWriteReceipt,
        AbortingAuthority,
        MustCloseGeneration,
        PeerResponseCommitReceipt,
    ) {
        (
            self.reason,
            self.abort_receipt,
            self.authority,
            self.must_close,
            self.foreign_receipt,
        )
    }
}

/// Stable reason returned commit authority could not abort its continuation.
#[must_use = "a peer-hook abort error must be handled"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerHookAbortError {
    /// The continuation belongs to another WriteReceiptIssuer.
    ForeignWriteOccurrence,
    /// The returned commit belongs to another peer-response issuance.
    ForeignPeerOccurrence,
}

/// Move-only abort rejection returning continuation and commit intact.
#[must_use = "a rejected peer-hook abort returns both exact authorities"]
#[derive(Debug)]
pub(crate) struct PeerHookAbortRejection {
    /// Stable mismatch classification.
    reason: PeerHookAbortError,
    /// Exact unresolved write continuation.
    continuation: PeerResponseFenceContinuation,
    /// Exact mismatching peer-response commit.
    commit: PeerResponseCommit,
}

impl PeerHookAbortRejection {
    /// Returns the stable mismatch classification.
    pub(crate) const fn reason(&self) -> PeerHookAbortError {
        self.reason
    }

    /// Consumes the rejection into its reason and both original authorities.
    pub(crate) fn into_parts(
        self,
    ) -> (
        PeerHookAbortError,
        PeerResponseFenceContinuation,
        PeerResponseCommit,
    ) {
        (self.reason, self.continuation, self.commit)
    }
}

/// Explicit requirement to close after peer-response semantics become unsafe.
///
/// The marker is produced when a peer hook cannot commit or when a committed
/// hook's response is not confirmed written. Either case forbids resuming the
/// generation because local selection state and peer-observed state may differ.
#[must_use = "a must-close marker must enter generation-close orchestration"]
#[derive(Debug)]
pub(crate) struct MustCloseGeneration {
    /// Exact generation that can no longer continue safely.
    generation: ConnectionGeneration,
    /// Exact write whose peer-response semantic/wire outcome forced closure.
    write_id: WriteId,
}

impl MustCloseGeneration {
    /// Returns the generation that must close.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Returns the peer-response write that forced closure.
    pub(crate) const fn write_id(&self) -> WriteId {
        self.write_id
    }

    /// Consumes the marker into its generation and write identities.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, WriteId) {
        (self.generation, self.write_id)
    }
}

/// Exact abort permission, Core terminal authority, and mandatory close marker.
#[must_use = "a failed peer hook must abort its fence and close the generation"]
#[derive(Debug)]
pub(crate) struct PeerHookAbort {
    /// Runtime permission to cancel the exact fenced write.
    receipt: AbortWriteReceipt,
    /// Core authority awaiting the exact terminal callback.
    authority: AbortingAuthority,
    /// Proof that this generation may not resume protocol work.
    must_close: MustCloseGeneration,
}

/// Explicit abort after a peer-response hook has already committed.
///
/// This bundle is reserved for generation-close recovery. It cannot represent
/// an ordinary write cancellation because it inseparably carries a close
/// requirement alongside runtime and Core write authorities.
#[must_use = "a committed peer-hook abort must close its generation"]
#[derive(Debug)]
pub(crate) struct CommittedPeerHookAbort {
    /// Runtime permission to cancel the exact fenced response write.
    receipt: AbortWriteReceipt,
    /// Core authority awaiting the exact abort completion.
    authority: AbortingAuthority,
    /// Proof that normal generation work may not resume.
    must_close: MustCloseGeneration,
}

impl CommittedPeerHookAbort {
    /// Consumes the recovery bundle into abort, accounting, and close authority.
    ///
    /// Returns `(receipt, authority, must_close)` in the order CoreResources
    /// must apply them during generation shutdown.
    pub(crate) fn into_parts(self) -> (AbortWriteReceipt, AbortingAuthority, MustCloseGeneration) {
        (self.receipt, self.authority, self.must_close)
    }
}

impl PeerHookAbort {
    /// Consumes the bundle in runtime, ledger, and close-orchestration order.
    pub(crate) fn into_parts(self) -> (AbortWriteReceipt, AbortingAuthority, MustCloseGeneration) {
        (self.receipt, self.authority, self.must_close)
    }
}

/// Move-only permission to release one exact BeginWrite fence.
#[must_use = "a proceed-write receipt must be emitted to its generation runtime"]
#[derive(Debug)]
pub(crate) struct ProceedWriteReceipt {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Ordered writer sequence whose fence must be released.
    wire_sequence: WireSequence,
}

impl ProceedWriteReceipt {
    /// Returns the owning connection generation.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns the owning operation identity.
    pub(crate) fn operation_id(&self) -> OperationId {
        self.occurrence.operation_id
    }

    /// Returns the scheduling class derived from the exact message.
    pub(crate) fn class(&self) -> WriteClass {
        self.occurrence.class
    }

    /// Returns the ordered writer sequence to release.
    pub(crate) const fn wire_sequence(&self) -> WireSequence {
        self.wire_sequence
    }

    /// Consumes the permission into its externally useful identity fields.
    ///
    /// The private occurrence is consumed and cannot be reconstructed.
    pub(crate) fn into_parts(
        self,
    ) -> (
        ConnectionGeneration,
        WriteId,
        OperationId,
        WriteClass,
        WireSequence,
    ) {
        (
            self.occurrence.generation,
            self.occurrence.write_id,
            self.occurrence.operation_id,
            self.occurrence.class,
            self.wire_sequence,
        )
    }
}

/// Move-only permission to cancel one exact BeginWrite fence.
#[must_use = "an abort-write receipt must be emitted to its generation runtime"]
#[derive(Debug)]
pub(crate) struct AbortWriteReceipt {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Ordered writer sequence whose fence must be cancelled.
    wire_sequence: WireSequence,
}

impl AbortWriteReceipt {
    /// Returns the owning connection generation.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns the owning operation identity.
    pub(crate) fn operation_id(&self) -> OperationId {
        self.occurrence.operation_id
    }

    /// Returns the scheduling class derived from the exact message.
    pub(crate) fn class(&self) -> WriteClass {
        self.occurrence.class
    }

    /// Returns the ordered writer sequence to cancel.
    pub(crate) const fn wire_sequence(&self) -> WireSequence {
        self.wire_sequence
    }

    /// Consumes the permission into its externally useful identity fields.
    ///
    /// The private occurrence is consumed and cannot be reconstructed.
    pub(crate) fn into_parts(
        self,
    ) -> (
        ConnectionGeneration,
        WriteId,
        OperationId,
        WriteClass,
        WireSequence,
    ) {
        (
            self.occurrence.generation,
            self.occurrence.write_id,
            self.occurrence.operation_id,
            self.occurrence.class,
            self.wire_sequence,
        )
    }
}

/// Core-side authority retained after runtime receives Proceed permission.
#[must_use = "a proceeded authority must consume visibility and terminal callbacks"]
#[derive(Debug)]
pub(crate) struct ProceededAuthority {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Ordered writer sequence released to runtime.
    wire_sequence: WireSequence,
    /// Whether runtime reported that bytes may already be visible.
    observed_may_be_visible: bool,
    /// Whether this write already committed a peer-response semantic hook.
    ///
    /// A non-committed terminal result after this point requires generation
    /// closure because the local FSM may have advanced without the peer seeing
    /// its response.
    committed_peer_response: bool,
}

impl ProceededAuthority {
    /// Returns the exact proceeded write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Consumes one visibility observation and returns the advanced authority.
    ///
    /// The WriteLedger remains responsible for rejecting duplicate runtime
    /// events; this method never manufactures a second terminal permission.
    pub(crate) fn observe_may_be_visible(mut self) -> Self {
        self.observed_may_be_visible = true;
        self
    }

    /// Consumes the unique proceeded authority into terminal evidence.
    ///
    /// `result` is retained unchanged and normalized conservatively using the
    /// previously observed visibility watermark.
    pub(crate) fn finish(self, result: WriteResult) -> TerminalWriteTransition {
        terminal_finished(
            self.occurrence,
            Some(self.wire_sequence),
            WritePhase::Proceeded,
            self.observed_may_be_visible,
            result,
            None,
            self.committed_peer_response,
        )
    }
}

/// Core-side authority retained after runtime receives Abort permission.
#[must_use = "an aborting authority must consume visibility and terminal callbacks"]
#[derive(Debug)]
pub(crate) struct AbortingAuthority {
    /// Unique bound write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Ordered writer sequence requested to cancel.
    wire_sequence: WireSequence,
    /// Whether runtime nevertheless reported possible byte visibility.
    observed_may_be_visible: bool,
    /// Exact uncommitted peer response retained until terminal accounting.
    peer_response: Option<PeerResponseCommit>,
}

impl AbortingAuthority {
    /// Returns the exact aborting write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Consumes an unexpected visibility observation and returns advanced authority.
    ///
    /// Terminal normalization will retain the resulting abort/visibility
    /// contradiction.
    pub(crate) fn observe_may_be_visible(mut self) -> Self {
        self.observed_may_be_visible = true;
        self
    }

    /// Consumes the unique aborting authority into terminal evidence.
    ///
    /// Only `NotWritten(Cancelled)` without prior visibility is a normal abort
    /// completion. Every other result retains a structured violation.
    pub(crate) fn finish(self, result: WriteResult) -> WriteTerminalReceipt {
        terminal_finished(
            self.occurrence,
            Some(self.wire_sequence),
            WritePhase::Aborting,
            self.observed_may_be_visible,
            result,
            self.peer_response,
            false,
        )
        .receipt
    }
}

/// Terminal diagnostic proving a peer response can no longer be scheduled.
///
/// This wrapper intentionally exposes no raw response and no commit authority.
/// Once constructed, the occurrence can be inspected for diagnostics but can
/// never be rebound through [`WriteSpec::peer_response`].
#[must_use = "an abandoned peer response must enter terminal accounting"]
#[derive(Debug)]
pub(crate) struct AbandonedPeerResponse {
    /// Consumed peer-response authority retained only as opaque diagnostics.
    commit: PeerResponseCommit,
}

impl AbandonedPeerResponse {
    /// Converts an uncommitted authority into a permanently terminal diagnostic.
    fn from_commit(commit: PeerResponseCommit) -> Self {
        Self { commit }
    }

    /// Returns the connection generation that abandoned the response.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.commit.generation()
    }

    /// Returns whether the abandoned response carried no semantic transition.
    pub(crate) fn is_none(&self) -> bool {
        self.commit.is_none()
    }

    /// Returns whether the abandoned response would have accepted Select.
    pub(crate) fn is_select_accepted(&self) -> bool {
        self.commit.is_select_accepted()
    }

    /// Returns whether the abandoned response would have accepted Deselect.
    pub(crate) fn is_deselect_accepted(&self) -> bool {
        self.commit.is_deselect_accepted()
    }
}

/// Move-only normalized terminal outcome retained by a terminal receipt.
#[must_use = "a write terminal outcome must be applied to operation accounting"]
#[derive(Debug)]
pub(crate) enum WriteTerminalOutcome {
    /// Runtime rejected scheduling before assigning a wire sequence.
    ScheduleFailed {
        /// Exact scheduling admission failure.
        failure: ScheduleFailure,
        /// Permanently abandoned peer response, when this was a response write.
        abandoned_peer_response: Option<AbandonedPeerResponse>,
    },
    /// Runtime produced a terminal result after scheduling began.
    Finished {
        /// Original result received from runtime without normalization.
        raw_result: WriteResult,
        /// Conservative result used for operation and transaction decisions.
        effective_result: EffectiveWriteResult,
        /// Permanently abandoned peer response whose hook never committed.
        abandoned_peer_response: Option<AbandonedPeerResponse>,
    },
}

/// Complete move-only evidence for one terminal write transition.
#[must_use = "a write terminal receipt must enter operation accounting"]
#[derive(Debug)]
pub(crate) struct WriteTerminalReceipt {
    /// Unique terminalized write occurrence.
    occurrence: Arc<WriteOccurrence>,
    /// Ordered writer sequence, absent only for scheduling rejection.
    wire_sequence: Option<WireSequence>,
    /// Original and conservative terminal outcome.
    outcome: WriteTerminalOutcome,
    /// Runtime-contract contradiction retained with the outcome.
    runtime_violation: Option<WriteRuntimeViolation>,
}

impl WriteTerminalReceipt {
    /// Returns the connection generation that produced this terminal transition.
    pub(crate) fn generation(&self) -> ConnectionGeneration {
        self.occurrence.generation
    }

    /// Returns the exact terminal write identity.
    pub(crate) fn write_id(&self) -> WriteId {
        self.occurrence.write_id
    }

    /// Returns the owning operation identity.
    pub(crate) fn operation_id(&self) -> OperationId {
        self.occurrence.operation_id
    }

    /// Returns the scheduling class derived from the exact message.
    pub(crate) fn class(&self) -> WriteClass {
        self.occurrence.class
    }

    /// Returns the assigned sequence, absent only for scheduling failure.
    pub(crate) const fn wire_sequence(&self) -> Option<WireSequence> {
        self.wire_sequence
    }

    /// Borrows the terminal outcome.
    pub(crate) const fn outcome(&self) -> &WriteTerminalOutcome {
        &self.outcome
    }

    /// Returns the retained runtime-contract violation, if any.
    pub(crate) const fn runtime_violation(&self) -> Option<WriteRuntimeViolation> {
        self.runtime_violation
    }

    /// Consumes the receipt into all externally useful terminal evidence.
    ///
    /// The private occurrence is consumed and cannot mint another receipt.
    pub(crate) fn into_parts(
        self,
    ) -> (
        ConnectionGeneration,
        WriteId,
        OperationId,
        WriteClass,
        Option<WireSequence>,
        WriteTerminalOutcome,
        Option<WriteRuntimeViolation>,
    ) {
        (
            self.occurrence.generation,
            self.occurrence.write_id,
            self.occurrence.operation_id,
            self.occurrence.class,
            self.wire_sequence,
            self.outcome,
            self.runtime_violation,
        )
    }
}

/// Terminal evidence plus an optional mandatory generation-close marker.
#[must_use = "a terminal write transition must be fully applied"]
#[derive(Debug)]
pub(crate) struct TerminalWriteTransition {
    /// Unique terminal receipt for operation and transaction accounting.
    receipt: WriteTerminalReceipt,
    /// Required close marker when a peer hook remained uncommitted.
    must_close: Option<MustCloseGeneration>,
}

impl TerminalWriteTransition {
    /// Borrows the terminal receipt.
    pub(crate) const fn receipt(&self) -> &WriteTerminalReceipt {
        &self.receipt
    }

    /// Borrows the must-close marker, if a peer hook was lost.
    pub(crate) const fn must_close(&self) -> Option<&MustCloseGeneration> {
        self.must_close.as_ref()
    }

    /// Consumes the transition into terminal evidence and close requirement.
    pub(crate) fn into_parts(self) -> (WriteTerminalReceipt, Option<MustCloseGeneration>) {
        (self.receipt, self.must_close)
    }
}

/// Creates the sole Proceed receipt and retained proceeded authority.
fn proceeded(
    occurrence: Arc<WriteOccurrence>,
    wire_sequence: WireSequence,
    committed_peer_response: bool,
) -> (ProceedWriteReceipt, ProceededAuthority) {
    (
        ProceedWriteReceipt {
            occurrence: Arc::clone(&occurrence),
            wire_sequence,
        },
        ProceededAuthority {
            occurrence,
            wire_sequence,
            observed_may_be_visible: false,
            committed_peer_response,
        },
    )
}

/// Creates the sole Abort receipt and retained aborting authority.
fn aborting(
    occurrence: Arc<WriteOccurrence>,
    wire_sequence: WireSequence,
    peer_response: Option<PeerResponseCommit>,
) -> (AbortWriteReceipt, AbortingAuthority) {
    (
        AbortWriteReceipt {
            occurrence: Arc::clone(&occurrence),
            wire_sequence,
        },
        AbortingAuthority {
            occurrence,
            wire_sequence,
            observed_may_be_visible: false,
            peer_response,
        },
    )
}

/// Creates exact abort permission and close proof for one uncommitted peer hook.
fn peer_hook_abort(
    occurrence: Arc<WriteOccurrence>,
    wire_sequence: WireSequence,
    commit: PeerResponseCommit,
) -> PeerHookAbort {
    let must_close = MustCloseGeneration {
        generation: occurrence.generation,
        write_id: occurrence.write_id,
    };
    let (receipt, authority) = aborting(occurrence, wire_sequence, Some(commit));
    PeerHookAbort {
        receipt,
        authority,
        must_close,
    }
}

/// Creates a close marker exactly when an authority retains a peer hook.
fn must_close_for_hook(
    occurrence: &Arc<WriteOccurrence>,
    peer_response: &Option<PeerResponseCommit>,
) -> Option<MustCloseGeneration> {
    peer_response.as_ref().map(|_| MustCloseGeneration {
        generation: occurrence.generation,
        write_id: occurrence.write_id,
    })
}

/// Consumes any state authority into one normalized terminal transition.
fn terminal_finished(
    occurrence: Arc<WriteOccurrence>,
    wire_sequence: Option<WireSequence>,
    phase: WritePhase,
    observed_may_be_visible: bool,
    raw_result: WriteResult,
    peer_response: Option<PeerResponseCommit>,
    committed_peer_response: bool,
) -> TerminalWriteTransition {
    let must_close = if peer_response.is_some()
        || committed_peer_response && !matches!(&raw_result, WriteResult::Committed)
    {
        Some(MustCloseGeneration {
            generation: occurrence.generation,
            write_id: occurrence.write_id,
        })
    } else {
        None
    };
    let (effective_result, runtime_violation) =
        normalize_runtime_result(phase, observed_may_be_visible, &raw_result);
    let abandoned_peer_response = peer_response.map(AbandonedPeerResponse::from_commit);
    TerminalWriteTransition {
        receipt: WriteTerminalReceipt {
            occurrence,
            wire_sequence,
            outcome: WriteTerminalOutcome::Finished {
                raw_result,
                effective_result,
                abandoned_peer_response,
            },
            runtime_violation,
        },
        must_close,
    }
}

impl PeerResponseExpectation {
    /// Returns whether two opaque expectations name one exact occurrence.
    fn same_occurrence(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.occurrence, &other.occurrence)
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        model::{
            ids::{ConnectionGeneration, OperationId, SystemBytes, WireSequence, WriteId},
            runtime::{TransportFault, TransportFaultKind, WriteResult},
        },
        protocol::{
            header::{ControlMessage, DeselectStatus, SelectStatus},
            message::ProtocolMessage,
        },
    };

    use super::{
        BeginWriteFence, BeginWriteObservation, CommittedPeerResponseFence, PeerResponseFence,
        PeerResponseResolutionError, ScheduleFailure, WriteBindError, WriteReceiptIssuer,
        WriteSpec, WriteSpecError, WriteTerminalOutcome,
    };
    use crate::hsms::contracts::write::{PeerResponseCommitIssuer, WriteClass};

    /// Builds one deterministic no-hook control specification.
    fn no_hook_spec() -> WriteSpec {
        WriteSpec::no_hook(ProtocolMessage::Control(ControlMessage::LinktestRequest {
            system_bytes: SystemBytes::new(7),
        }))
        .expect("a control request is a valid no-hook write")
    }

    /// Advances one registration to its exact BeginWrite fence.
    fn begin_no_hook(issuer: &WriteReceiptIssuer) -> BeginWriteFence {
        let registration = issuer
            .bind(WriteId::new(7), OperationId::new(9), no_hook_spec())
            .expect("typed control write binds");
        let (prepared, scheduling) = registration.into_scheduling();
        assert!(scheduling.matches_prepared(&prepared));
        assert_eq!(prepared.class(), WriteClass::Critical);
        let queued = scheduling.acknowledge_queued(WireSequence::new(11));
        queued
            .begin(BeginWriteObservation::new(
                ConnectionGeneration::new(3),
                WriteId::new(7),
                WireSequence::new(11),
            ))
            .expect("exact observation reaches the fence")
    }

    /// Advances one exact peer response to its unresolved BeginWrite fence.
    fn peer_response_fence(
        writes: &WriteReceiptIssuer,
        control: &PeerResponseCommitIssuer,
        generation: ConnectionGeneration,
        write_id: WriteId,
    ) -> PeerResponseFence {
        let pending = control
            .issue_select_accepted(ControlMessage::SelectResponse {
                session_id: u16::MAX,
                status: SelectStatus::SUCCESS,
                system_bytes: SystemBytes::new(write_id.get() as u32),
            })
            .expect("successful Select response is coherent");
        let registration = writes
            .bind(
                write_id,
                OperationId::new(write_id.get()),
                WriteSpec::peer_response(pending),
            )
            .expect("matching response generation binds");
        let (_, scheduling) = registration.into_scheduling();
        let wire_sequence = WireSequence::new(write_id.get());
        let queued = scheduling.acknowledge_queued(wire_sequence);
        let BeginWriteFence::PeerResponse(fence) = queued
            .begin(BeginWriteObservation::new(
                generation,
                write_id,
                wire_sequence,
            ))
            .expect("exact observation reaches peer fence")
        else {
            panic!("peer spec must create a peer fence");
        };
        fence
    }

    /// Advances one exact peer response through its committed BeginWrite hook.
    fn committed_peer_fence(
        generation: ConnectionGeneration,
        write_id: WriteId,
    ) -> CommittedPeerResponseFence {
        let writes = WriteReceiptIssuer::new(generation);
        let control = PeerResponseCommitIssuer::new_for_test(generation);
        let fence = peer_response_fence(&writes, &control, generation, write_id);
        let (commit, continuation) = writes
            .split_peer_response_fence(fence)
            .expect("issuer owns its fence");
        let receipt = control
            .commit(commit)
            .expect("ControlFsm issuer owns exact commit");
        writes
            .resolve_peer_response(continuation, receipt)
            .expect("exact occurrence resolves the fence")
    }

    /// Rejects every raw peer control response at the sealed no-hook constructor.
    #[test]
    fn no_hook_spec_rejects_all_control_responses_and_returns_message() {
        let responses = [
            ControlMessage::SelectResponse {
                session_id: u16::MAX,
                status: SelectStatus::SUCCESS,
                system_bytes: SystemBytes::new(1),
            },
            ControlMessage::DeselectResponse {
                session_id: u16::MAX,
                status: DeselectStatus::BUSY,
                system_bytes: SystemBytes::new(2),
            },
            ControlMessage::LinktestResponse {
                system_bytes: SystemBytes::new(3),
            },
        ];

        for response in responses {
            let message = ProtocolMessage::Control(response);
            let failure = WriteSpec::no_hook(message.clone())
                .expect_err("peer response must retain its ControlFsm bundle");
            assert_eq!(
                failure.error(),
                WriteSpecError::ControlResponseRequiresPeerBundle
            );
            assert_eq!(failure.message(), &message);
            let (error, returned) = failure.into_parts();
            assert_eq!(error, WriteSpecError::ControlResponseRequiresPeerBundle);
            assert_eq!(returned, message);
        }
    }

    /// Derives write class from the exact message and completes one linear chain.
    #[test]
    fn no_hook_write_advances_linearly_to_one_terminal_receipt() {
        let issuer = WriteReceiptIssuer::new(ConnectionGeneration::new(3));
        let BeginWriteFence::NoHook(fence) = begin_no_hook(&issuer) else {
            panic!("no-hook spec must create a no-hook fence");
        };
        let (proceed, authority) = fence.proceed();
        assert_eq!(proceed.write_id(), WriteId::new(7));
        let terminal = authority.finish(WriteResult::Committed);
        assert_eq!(terminal.receipt().write_id(), WriteId::new(7));
        assert_eq!(terminal.receipt().runtime_violation(), None);
        assert!(terminal.must_close().is_none());
    }

    /// Leaves an ordinary no-hook write open after a non-committed runtime result.
    #[test]
    fn no_hook_proceeded_failure_does_not_create_peer_close_marker() {
        let issuer = WriteReceiptIssuer::new(ConnectionGeneration::new(3));
        let BeginWriteFence::NoHook(fence) = begin_no_hook(&issuer) else {
            panic!("no-hook spec must create a no-hook fence");
        };
        let (_, authority) = fence.proceed();
        let terminal = authority.finish(WriteResult::NotWritten(TransportFault {
            kind: TransportFaultKind::BrokenPipe,
            context: "ordinary request failed",
        }));

        assert!(terminal.must_close().is_none());
    }

    /// Returns a foreign-generation peer specification intact during bind.
    #[test]
    fn bind_rejects_foreign_peer_response_generation() {
        let writes = WriteReceiptIssuer::new(ConnectionGeneration::new(3));
        let control = PeerResponseCommitIssuer::new_for_test(ConnectionGeneration::new(4));
        let pending = control
            .issue_select_accepted(ControlMessage::SelectResponse {
                session_id: u16::MAX,
                status: SelectStatus::SUCCESS,
                system_bytes: SystemBytes::new(7),
            })
            .expect("successful Select response is coherent");
        let failure = writes
            .bind(
                WriteId::new(7),
                OperationId::new(9),
                WriteSpec::peer_response(pending),
            )
            .expect_err("foreign response generation must be rejected");
        assert_eq!(
            failure.error(),
            WriteBindError::PeerResponseGenerationMismatch {
                expected: ConnectionGeneration::new(3),
                actual: ConnectionGeneration::new(4),
            }
        );
        assert!(failure.into_parts().1.is_peer_response());
    }

    /// Resolves a peer fence only with its exact control occurrence.
    #[test]
    fn exact_peer_commit_receipt_is_required_for_proceed() {
        let generation = ConnectionGeneration::new(3);
        let writes = WriteReceiptIssuer::new(generation);
        let control = PeerResponseCommitIssuer::new_for_test(generation);
        let response = ControlMessage::SelectResponse {
            session_id: u16::MAX,
            status: SelectStatus::SUCCESS,
            system_bytes: SystemBytes::new(7),
        };
        let pending = control
            .issue_select_accepted(response)
            .expect("successful Select response is coherent");
        let registration = writes
            .bind(
                WriteId::new(7),
                OperationId::new(9),
                WriteSpec::peer_response(pending),
            )
            .expect("matching response generation binds");
        let (_, scheduling) = registration.into_scheduling();
        let queued = scheduling.acknowledge_queued(WireSequence::new(11));
        let BeginWriteFence::PeerResponse(fence) = queued
            .begin(BeginWriteObservation::new(
                generation,
                WriteId::new(7),
                WireSequence::new(11),
            ))
            .expect("exact observation reaches peer fence")
        else {
            panic!("peer spec must create a peer fence");
        };
        let (commit, continuation) = writes
            .split_peer_response_fence(fence)
            .expect("issuer owns its fence");
        let receipt = control
            .commit(commit)
            .expect("ControlFsm issuer owns exact commit");
        let committed = writes
            .resolve_peer_response(continuation, receipt)
            .expect("exact occurrence resolves the fence");
        let (proceed, authority) = committed.proceed();
        assert_eq!(proceed.write_id(), WriteId::new(7));
        let terminal = authority.finish(WriteResult::Committed);
        assert!(terminal.must_close().is_none());
    }

    /// Converts cross-swapped committed receipts into abort-and-close recovery.
    #[test]
    fn cross_swapped_peer_receipts_cannot_release_either_write_fence() {
        let generation = ConnectionGeneration::new(3);
        let writes = WriteReceiptIssuer::new(generation);
        let control = PeerResponseCommitIssuer::new_for_test(generation);
        let first_fence = peer_response_fence(&writes, &control, generation, WriteId::new(71));
        let second_fence = peer_response_fence(&writes, &control, generation, WriteId::new(72));
        let (first_commit, first_continuation) = writes
            .split_peer_response_fence(first_fence)
            .expect("issuer owns first fence");
        let (second_commit, second_continuation) = writes
            .split_peer_response_fence(second_fence)
            .expect("issuer owns second fence");
        let first_receipt = control
            .commit(first_commit)
            .expect("control issuer owns first occurrence");
        let second_receipt = control
            .commit(second_commit)
            .expect("control issuer owns second occurrence");

        let first_failure = writes
            .resolve_peer_response(first_continuation, second_receipt)
            .expect_err("second receipt cannot release first fence");
        assert_eq!(
            first_failure.reason(),
            PeerResponseResolutionError::ForeignPeerOccurrence
        );
        let first_abort = writes
            .abort_failed_peer_response_resolution(first_failure)
            .expect("write issuer owns first failed continuation");
        let (
            first_reason,
            first_abort_receipt,
            first_authority,
            first_close,
            returned_second_receipt,
        ) = first_abort.into_parts();
        assert_eq!(
            first_reason,
            PeerResponseResolutionError::ForeignPeerOccurrence
        );
        assert_eq!(first_abort_receipt.write_id(), WriteId::new(71));
        assert_eq!(first_close.write_id(), WriteId::new(71));
        assert!(returned_second_receipt.is_select_accepted());

        let second_failure = writes
            .resolve_peer_response(second_continuation, first_receipt)
            .expect_err("first receipt cannot release second fence");
        let second_abort = writes
            .abort_failed_peer_response_resolution(second_failure)
            .expect("write issuer owns second failed continuation");
        let (_, second_abort_receipt, second_authority, second_close, returned_first_receipt) =
            second_abort.into_parts();
        assert_eq!(second_abort_receipt.write_id(), WriteId::new(72));
        assert_eq!(second_close.write_id(), WriteId::new(72));
        assert!(returned_first_receipt.is_select_accepted());

        let cancelled = || {
            WriteResult::NotWritten(TransportFault {
                kind: TransportFaultKind::Cancelled,
                context: "cross-swapped peer hook shutdown",
            })
        };
        assert_eq!(
            first_authority.finish(cancelled()).runtime_violation(),
            None
        );
        assert_eq!(
            second_authority.finish(cancelled()).runtime_violation(),
            None
        );
    }

    /// Makes every post-commit cancellation carry an inseparable close marker.
    #[test]
    fn committed_peer_fence_abnormal_abort_requires_close() {
        let generation = ConnectionGeneration::new(3);
        let committed = committed_peer_fence(generation, WriteId::new(73));
        let abort = committed.abort_for_generation_close();
        let (receipt, authority, must_close) = abort.into_parts();
        assert_eq!(receipt.write_id(), WriteId::new(73));
        assert_eq!(must_close.generation(), generation);
        assert_eq!(must_close.write_id(), WriteId::new(73));
        let terminal = authority.finish(WriteResult::NotWritten(TransportFault {
            kind: TransportFaultKind::Cancelled,
            context: "generation shutdown after committed hook",
        }));
        assert_eq!(terminal.runtime_violation(), None);
    }

    /// Requires closure when an already committed peer hook is not confirmed sent.
    #[test]
    fn committed_peer_hook_non_committed_terminal_results_require_close() {
        let generation = ConnectionGeneration::new(3);
        let results = [
            WriteResult::NotWritten(TransportFault {
                kind: TransportFaultKind::BrokenPipe,
                context: "peer response not written",
            }),
            WriteResult::Indeterminate(TransportFault {
                kind: TransportFaultKind::ConnectionReset,
                context: "peer response visibility unknown",
            }),
        ];

        for (offset, result) in results.into_iter().enumerate() {
            let write_id = WriteId::new(30 + offset as u64);
            let committed = committed_peer_fence(generation, write_id);
            let (_, authority) = committed.proceed();
            let terminal = authority.finish(result);
            let must_close = terminal
                .must_close()
                .expect("committed hook without committed write must close");
            assert_eq!(must_close.generation(), generation);
            assert_eq!(must_close.write_id(), write_id);
        }
    }

    /// Converts a failed peer-response schedule into non-rebindable diagnostics.
    #[test]
    fn schedule_failure_abandons_peer_response_without_retry_authority() {
        let generation = ConnectionGeneration::new(3);
        let writes = WriteReceiptIssuer::new(generation);
        let control = PeerResponseCommitIssuer::new_for_test(generation);
        let pending = control
            .issue_select_accepted(ControlMessage::SelectResponse {
                session_id: u16::MAX,
                status: SelectStatus::SUCCESS,
                system_bytes: SystemBytes::new(44),
            })
            .expect("successful Select response is coherent");
        let registration = writes
            .bind(
                WriteId::new(44),
                OperationId::new(44),
                WriteSpec::peer_response(pending),
            )
            .expect("matching response generation binds");
        let (_, scheduling) = registration.into_scheduling();
        let transition = scheduling.schedule_failed(ScheduleFailure::SchedulerStopped);
        let (receipt, must_close) = transition.into_parts();
        assert!(must_close.is_some());
        let (_, _, _, _, _, outcome, _) = receipt.into_parts();
        let WriteTerminalOutcome::ScheduleFailed {
            abandoned_peer_response: Some(abandoned),
            ..
        } = outcome
        else {
            panic!("peer response must become abandoned terminal evidence");
        };
        assert_eq!(abandoned.generation(), generation);
        assert!(abandoned.is_select_accepted());
        assert!(!abandoned.is_none());
        assert!(!abandoned.is_deselect_accepted());
    }

    /// Turns hook preflight failure into exact abort permission and mandatory close.
    #[test]
    fn peer_hook_failure_aborts_exact_fence_and_requires_close() {
        let generation = ConnectionGeneration::new(3);
        let writes = WriteReceiptIssuer::new(generation);
        let control = PeerResponseCommitIssuer::new_for_test(generation);
        let pending = control
            .issue_select_accepted(ControlMessage::SelectResponse {
                session_id: u16::MAX,
                status: SelectStatus::SUCCESS,
                system_bytes: SystemBytes::new(7),
            })
            .expect("successful Select response is coherent");
        let registration = writes
            .bind(
                WriteId::new(7),
                OperationId::new(9),
                WriteSpec::peer_response(pending),
            )
            .expect("matching response generation binds");
        let (_, scheduling) = registration.into_scheduling();
        let queued = scheduling.acknowledge_queued(WireSequence::new(11));
        let BeginWriteFence::PeerResponse(fence) = queued
            .begin(BeginWriteObservation::new(
                generation,
                WriteId::new(7),
                WireSequence::new(11),
            ))
            .expect("exact observation reaches peer fence")
        else {
            panic!("peer spec must create a peer fence");
        };
        let abort = writes
            .abort_peer_response_fence(fence)
            .expect("issuer owns exact peer fence");
        let (receipt, authority, must_close) = abort.into_parts();
        assert_eq!(receipt.write_id(), WriteId::new(7));
        assert_eq!(must_close.generation(), generation);
        let terminal = authority.finish(WriteResult::NotWritten(TransportFault {
            kind: TransportFaultKind::Cancelled,
            context: "abort acknowledgement",
        }));
        assert_eq!(terminal.runtime_violation(), None);
        assert!(matches!(
            terminal.outcome(),
            WriteTerminalOutcome::Finished {
                abandoned_peer_response: Some(_),
                ..
            }
        ));
    }
}
