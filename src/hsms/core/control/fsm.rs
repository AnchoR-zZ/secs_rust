//! Implements the generation-local HSMS selection reducer.
//!
//! This module converts typed local and peer control inputs into ordered,
//! runtime-neutral actions. It deliberately does not allocate identifiers,
//! correlate System Bytes, own T6, schedule writes, or execute effects; those
//! responsibilities remain with `HsmsCore` and `TransactionRegistry`.

use std::num::NonZeroU8;

use crate::hsms::{
    contracts::{
        peer_response::{
            PeerResponseCommit, PeerResponseCommitIssuer, PeerResponseCommitReceipt,
            PendingPeerResponseWrite,
        },
        ControlIntent, DataGateState,
    },
    core::transaction::ControlKind,
    model::{
        ids::{ConnectionGeneration, OperationId, SystemBytes},
        runtime::{CommunicationsTimeoutKind, GenerationCloseReason, TimerToken},
    },
    protocol::header::{ControlMessage, DeselectStatus, SelectStatus},
    OperationError, SessionState, TimeoutKind,
};

/// Private projection of a locally initiated selection procedure.
///
/// System Bytes, T6, and the live control slot remain owned solely by
/// `TransactionRegistry`; this overlay retains only the Core operation needed
/// to apply a matched terminal result to selection state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SelectionOverlay {
    /// No local Select or Deselect procedure is projected into the FSM.
    Idle,
    /// A locally initiated `Select.req` has not reached its terminal result.
    Selecting {
        /// Core operation that owns the pending Select procedure.
        operation_id: OperationId,
    },
    /// A locally initiated `Deselect.req` has not reached its terminal result.
    Deselecting {
        /// Core operation that owns the pending Deselect procedure.
        operation_id: OperationId,
    },
}

/// Read-only plan returned before Core allocates and reserves an operation.
#[must_use = "Core must reserve and commit an accepted local control plan"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalControlPlan {
    /// A Select, Deselect, or Linktest request that uses the Registry control slot.
    Transactional {
        /// Transaction kind Core must reserve before committing this plan.
        kind: ControlKind,
    },
    /// A one-way local `Separate.req` that closes after its write terminates.
    Separate,
}

impl LocalControlPlan {
    /// Returns the transactional kind, or `None` for a one-way Separate plan.
    pub(crate) const fn transactional_kind(self) -> Option<ControlKind> {
        match self {
            Self::Transactional { kind } => Some(kind),
            Self::Separate => None,
        }
    }
}

/// Runtime boundary that must be crossed before transport closure may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseBarrier {
    /// Transport closure may start as soon as Core applies the decision.
    Immediate,
    /// Core must resolve this semantic operation to its exact active `WriteId`
    /// before Drain starts; only that write's scheduler/writer terminal outcome
    /// may release transport close.
    AfterOperation(
        /// Semantic operation whose exact active write must replace this
        /// placeholder; operation completion never releases the barrier.
        OperationId,
    ),
}

/// Ordered, runtime-neutral action emitted by the control reducer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ControlAction {
    /// Apply the Data scheduler gate synchronously.
    SetDataGate(
        /// Gate state that must take effect before the following action.
        DataGateState,
    ),
    /// Atomically remove selected-session Data work, inbound reply
    /// capabilities, and their pending deliveries without closing
    /// generation-wide admission, so the same TCP connection may reselect.
    ResetSelectedSession,
    /// Publish one stable selection-state transition.
    SessionStateChanged(
        /// Stable state committed internally before this action was emitted.
        SessionState,
    ),
    /// Arm the exact T7 registration for the current NotSelected tenure.
    ArmT7(
        /// Core-allocated token TimerDriver must register.
        TimerToken,
    ),
    /// Cancel an exact timer registration that is no longer current.
    CancelTimer(
        /// Core-allocated token TimerDriver must cancel.
        TimerToken,
    ),
    /// Begin the generation's idempotent closing procedure.
    BeginGenerationClose {
        /// Stable reason retained through SessionDriver and Supervisor.
        reason: GenerationCloseReason,
        /// Write boundary that controls when transport closure may start.
        barrier: CloseBarrier,
    },
}

/// Ordered action vector produced by one serialized FSM decision.
#[must_use = "HsmsCore must apply every ordered control action"]
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ControlDecision {
    /// Actions Core must translate and execute in this exact order.
    actions: Vec<ControlAction>,
}

impl ControlDecision {
    /// Creates a decision containing no external actions.
    const fn empty() -> Self {
        Self {
            actions: Vec::new(),
        }
    }

    /// Creates a decision from an already ordered action vector.
    fn new(actions: Vec<ControlAction>) -> Self {
        Self { actions }
    }

    /// Borrows actions in the order Core must preserve.
    #[cfg(test)]
    pub(crate) fn actions(&self) -> &[ControlAction] {
        &self.actions
    }

    /// Consumes the decision and returns its ordered actions.
    pub(crate) fn into_actions(self) -> Vec<ControlAction> {
        self.actions
    }

    /// Returns whether applying the decision requires no external work.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }
}

/// Programmer-contract failure detected without panicking or mutating state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlInvariantError {
    /// A T7 registration used another semantic timer kind.
    WrongTimerKind {
        /// Timer kind required by the attempted FSM transition.
        expected: TimeoutKind,
        /// Timer kind carried by the rejected token.
        actual: TimeoutKind,
    },
    /// State changed between read-only planning and reservation commit.
    LocalPlanNoLongerValid {
        /// Plan that could not be committed safely.
        plan: LocalControlPlan,
        /// Stable state observed at commit time.
        state: SessionState,
        /// Selection overlay observed at commit time.
        overlay: SelectionOverlay,
        /// Whether a peer Deselect response barrier was active.
        peer_deselect_pending: bool,
    },
    /// A matched Select or Deselect response did not own the projected overlay.
    ResponseWithoutMatchingOverlay {
        /// Operation supplied by the exact Registry response decision.
        operation_id: OperationId,
        /// Control kind whose overlay was required.
        kind: ControlKind,
        /// Overlay that was preserved instead of being cleared.
        overlay: SelectionOverlay,
    },
    /// A real Selected-to-NotSelected transition lacked its new T7 token.
    MissingT7ForDowngrade,
    /// A real NotSelected-to-Selected transition lacked its current T7 token.
    MissingT7ForUpgrade,
    /// A transition supplied a T7 token even though no new tenure began.
    UnexpectedT7ForState {
        /// Stable state that did not require the supplied registration.
        state: SessionState,
    },
    /// A selected-state transition found an impossible retained T7 token.
    T7AlreadyPresentWhileSelected,
    /// The accepted peer Deselect response fence arrived without its barrier.
    PeerDeselectCommitWithoutBarrier,
    /// Peer-response authority belongs to another issuer instance or generation.
    ForeignPeerResponseCommit,
}

/// Private selector used only while building a branded peer-response plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PeerResponseCommitIntent {
    /// Response carries no deferred selection-state transition.
    None,
    /// Response commits peer Select acceptance.
    SelectAccepted,
    /// Response commits peer Deselect acceptance.
    DeselectAccepted,
}

/// Successful FSM transition paired with its exact issuer-occurrence receipt.
#[must_use = "the control decision and peer-response receipt must both be handled"]
#[derive(Debug)]
pub(crate) struct CommittedPeerResponse {
    /// Control effects produced by the unchanged state-transition logic.
    decision: ControlDecision,
    /// Branded proof of the exact committed peer-response authority.
    receipt: PeerResponseCommitReceipt,
}

impl CommittedPeerResponse {
    /// Consumes the result into its decision and success proof.
    ///
    /// Returns `(decision, receipt)` for ordered Core application.
    pub(crate) fn into_parts(self) -> (ControlDecision, PeerResponseCommitReceipt) {
        (self.decision, self.receipt)
    }
}

/// Move-only peer-response failure that returns authority without mutation loss.
#[must_use = "a failed peer-response commit contains recoverable authority"]
#[derive(Debug)]
pub(crate) struct PeerResponseCommitFailure {
    /// Exact invariant or instance-brand validation failure.
    error: ControlInvariantError,
    /// Original one-shot authority rejected by the ControlFsm.
    commit: PeerResponseCommit,
}

impl PeerResponseCommitFailure {
    /// Creates a failure while preserving the original authority.
    const fn new(error: ControlInvariantError, commit: PeerResponseCommit) -> Self {
        Self { error, commit }
    }

    /// Returns the structured invariant error without consuming authority.
    pub(crate) const fn error(&self) -> ControlInvariantError {
        self.error
    }

    /// Borrows the exact authority returned by the failed commit.
    pub(crate) const fn commit(&self) -> &PeerResponseCommit {
        &self.commit
    }

    /// Consumes the failure into its error and original authority.
    pub(crate) fn into_parts(self) -> (ControlInvariantError, PeerResponseCommit) {
        (self.error, self.commit)
    }
}

/// Typed response already correlated exactly by `TransactionRegistry`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MatchedControlResponse {
    /// Exact response to the projected local Select procedure.
    Select {
        /// Raw, lossless `Select.rsp` status.
        status: SelectStatus,
    },
    /// Exact response to the projected local Deselect procedure.
    Deselect {
        /// Raw, lossless `Deselect.rsp` status.
        status: DeselectStatus,
    },
    /// Exact response to a local Linktest procedure.
    Linktest,
}

/// Terminal result and state actions for one exactly matched local response.
#[must_use = "the operation result and ordered state actions must both be handled"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct LocalResponseDecision {
    /// Ordered state and timer actions caused by the response.
    state: ControlDecision,
    /// Successful control completion or stable non-success status.
    result: Result<(), OperationError>,
}

impl LocalResponseDecision {
    /// Borrows ordered state actions for focused reducer assertions.
    #[cfg(test)]
    const fn state_decision(&self) -> &ControlDecision {
        &self.state
    }

    /// Borrows the operation result for focused reducer assertions.
    #[cfg(test)]
    const fn result(&self) -> &Result<(), OperationError> {
        &self.result
    }

    /// Consumes the response decision in the order Core must preserve.
    ///
    /// Core must apply `state` completely before making `result` visible to
    /// the command completion boundary.
    pub(crate) fn into_ordered_parts(self) -> (ControlDecision, Result<(), OperationError>) {
        (self.state, self.result)
    }
}

/// Immediate state work plus an inseparable peer-response write bundle.
#[must_use = "the immediate actions and pending peer response must both be handled"]
#[derive(Debug)]
pub(crate) struct PeerResponsePlan {
    /// Actions that must complete before Core schedules the response.
    immediate: ControlDecision,
    /// Inseparable response message and transition authority for WriteLedger.
    pending: PendingPeerResponseWrite,
}

impl PeerResponsePlan {
    /// Borrows immediate actions for focused reducer assertions.
    #[cfg(test)]
    const fn immediate_decision(&self) -> &ControlDecision {
        &self.immediate
    }

    /// Borrows the typed response for focused reducer assertions.
    #[cfg(test)]
    fn response(&self) -> ControlMessage {
        self.pending.response_for_test()
    }

    /// Borrows the pending bundle for focused reducer assertions.
    #[cfg(test)]
    const fn pending(&self) -> &PendingPeerResponseWrite {
        &self.pending
    }

    /// Consumes the plan in the order Core must preserve.
    ///
    /// Core must apply `immediate` completely before allocating and scheduling
    /// the returned bundle. Only `WriteSpec::peer_response` may bind that bundle
    /// to a write, so the exact response cannot be separated from its hook.
    pub(crate) fn into_ordered_parts(self) -> (ControlDecision, PendingPeerResponseWrite) {
        (self.immediate, self.pending)
    }
}

/// Complete FSM treatment of one peer control request.
#[must_use = "peer requests require their response or action decision to be applied"]
#[derive(Debug)]
pub(crate) enum PeerRequestDecision {
    /// Schedule a mandatory typed response with its split transition plan.
    Respond {
        /// Response and receipt/fence actions produced for the request.
        plan: PeerResponsePlan,
    },
    /// Send no response and apply only the attached actions, if any.
    NoResponse {
        /// Ignore decision or immediate generation-close decision.
        decision: ControlDecision,
    },
}

impl PeerRequestDecision {
    /// Borrows a response plan when the request requires one.
    #[cfg(test)]
    pub(crate) const fn response_plan(&self) -> Option<&PeerResponsePlan> {
        match self {
            Self::Respond { plan } => Some(plan),
            Self::NoResponse { .. } => None,
        }
    }

    /// Borrows no-response actions when the request does not produce a frame.
    #[cfg(test)]
    pub(crate) const fn no_response_decision(&self) -> Option<&ControlDecision> {
        match self {
            Self::Respond { .. } => None,
            Self::NoResponse { decision } => Some(decision),
        }
    }
}

/// Result of projecting a local transaction's terminal Registry decision.
#[must_use = "terminal overlay cleanup must be reconciled with transaction completion"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OverlayTerminalDecision {
    /// The exact Select or Deselect overlay was cleared.
    Cleared,
    /// Linktest or another non-selection operation has no FSM overlay.
    NoOverlay,
    /// A stale or mismatched operation left the current overlay unchanged.
    Stale {
        /// Overlay deliberately preserved by the stale terminal input.
        current: SelectionOverlay,
    },
}

/// T6 result combining overlay cleanup classification and close actions.
#[must_use = "T6 cleanup and generation-close actions must both be applied"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ControlTimeoutDecision {
    /// Whether the timed-out operation cleared an exact selection overlay.
    overlay: OverlayTerminalDecision,
    /// Idempotent T6 generation-close decision.
    close: ControlDecision,
}

impl ControlTimeoutDecision {
    /// Returns how the T6 operation affected the selection overlay.
    pub(crate) const fn overlay(&self) -> OverlayTerminalDecision {
        self.overlay
    }

    /// Borrows ordered actions caused by the communications timeout.
    #[cfg(test)]
    pub(crate) const fn close_decision(&self) -> &ControlDecision {
        &self.close
    }

    /// Consumes the timeout result into overlay and close decisions.
    pub(crate) fn into_parts(self) -> (OverlayTerminalDecision, ControlDecision) {
        (self.overlay, self.close)
    }
}

/// Generation-local, single-threaded HSMS selection reducer.
#[derive(Debug)]
pub(crate) struct ControlFsm {
    /// Exact connection generation served by this FSM instance.
    generation: ConnectionGeneration,
    /// Instance-local issuer for every deferred peer-response authority.
    peer_response_issuer: PeerResponseCommitIssuer,
    /// Stable selection state exposed outside the reducer.
    state: SessionState,
    /// Projection of a locally initiated Select or Deselect operation.
    overlay: SelectionOverlay,
    /// Whether an accepted peer Deselect is waiting for its response fence.
    peer_deselect_pending: bool,
    /// Exact T7 registration for the current NotSelected tenure.
    t7: Option<TimerToken>,
}

impl ControlFsm {
    /// Starts one connected generation in NotSelected with an exact T7 token.
    ///
    /// Returns the initialized FSM and ordered initial gate, state, and timer
    /// actions. A non-T7 token returns a structured error without constructing
    /// partial state.
    pub(crate) fn start(
        generation: ConnectionGeneration,
        initial_t7: TimerToken,
    ) -> Result<(Self, ControlDecision), ControlInvariantError> {
        Self::validate_t7(initial_t7)?;
        let fsm = Self {
            generation,
            peer_response_issuer: PeerResponseCommitIssuer::new(generation),
            state: SessionState::NotSelected,
            overlay: SelectionOverlay::Idle,
            peer_deselect_pending: false,
            t7: Some(initial_t7),
        };
        let decision = ControlDecision::new(vec![
            ControlAction::SetDataGate(DataGateState::Closed),
            ControlAction::SessionStateChanged(SessionState::NotSelected),
            ControlAction::ArmT7(initial_t7),
        ]);
        Ok((fsm, decision))
    }

    /// Returns the stable selection state currently committed by the FSM.
    pub(crate) const fn state(&self) -> SessionState {
        self.state
    }

    /// Returns the private local selection projection.
    pub(crate) const fn overlay(&self) -> SelectionOverlay {
        self.overlay
    }

    /// Returns whether an accepted peer Deselect still owns a response barrier.
    pub(crate) const fn peer_deselect_pending(&self) -> bool {
        self.peer_deselect_pending
    }

    /// Returns the authoritative Data-admission gate derived from FSM state.
    ///
    /// A peer Deselect closes Data admission immediately even though the
    /// stable session state remains Selected until the response BeginWrite
    /// fence. Core must use this projection instead of testing `state` alone
    /// when admitting or scheduling Data messages.
    pub(crate) const fn data_gate_state(&self) -> DataGateState {
        if matches!(self.state, SessionState::Selected) && !self.peer_deselect_pending {
            DataGateState::Open
        } else {
            DataGateState::Closed
        }
    }

    /// Returns the exact current T7 token, if the session is NotSelected.
    pub(crate) const fn t7(&self) -> Option<TimerToken> {
        self.t7
    }

    /// Checks a local control intent without allocating IDs or changing state.
    ///
    /// `control_slot_free` is the Registry's read-only control-slot snapshot.
    /// A successful plan must be reserved atomically by Core before
    /// [`Self::commit_local_started`] is called.
    pub(crate) fn plan_local(
        &self,
        intent: ControlIntent,
        control_slot_free: bool,
    ) -> Result<LocalControlPlan, OperationError> {
        match self.state {
            SessionState::Closing => return Err(OperationError::Draining),
            SessionState::Closed => return Err(OperationError::NotConnected),
            SessionState::NotSelected | SessionState::Selected => {}
        }

        let plan = match self.state {
            SessionState::NotSelected => match intent {
                ControlIntent::Select => LocalControlPlan::Transactional {
                    kind: ControlKind::Select,
                },
                ControlIntent::Linktest => LocalControlPlan::Transactional {
                    kind: ControlKind::Linktest,
                },
                ControlIntent::Deselect | ControlIntent::Separate => {
                    return Err(OperationError::NotSelected);
                }
            },
            SessionState::Selected => match intent {
                ControlIntent::Deselect => LocalControlPlan::Transactional {
                    kind: ControlKind::Deselect,
                },
                ControlIntent::Linktest => LocalControlPlan::Transactional {
                    kind: ControlKind::Linktest,
                },
                ControlIntent::Select => return Err(OperationError::AlreadySelected),
                ControlIntent::Separate => return Ok(LocalControlPlan::Separate),
            },
            SessionState::Closing => return Err(OperationError::Draining),
            SessionState::Closed => return Err(OperationError::NotConnected),
        };

        if self.overlay != SelectionOverlay::Idle || !control_slot_free {
            return Err(OperationError::ControlBusy);
        }
        if self.peer_deselect_pending
            && matches!(intent, ControlIntent::Select | ControlIntent::Deselect)
        {
            return Err(OperationError::ControlBusy);
        }
        Ok(plan)
    }

    /// Commits a plan only after Core successfully reserves every required
    /// operation, Registry, and write resource.
    ///
    /// `operation_id` is allocated by Core. Invalid or replayed commits return
    /// a structured invariant error without partially replacing an overlay.
    /// For local Separate, Core must resolve the returned semantic
    /// [`CloseBarrier::AfterOperation`] to its exact active write before
    /// beginning Drain; a failed translation is an internal invariant failure.
    pub(crate) fn commit_local_started(
        &mut self,
        operation_id: OperationId,
        plan: LocalControlPlan,
    ) -> Result<ControlDecision, ControlInvariantError> {
        match plan {
            LocalControlPlan::Separate if self.state == SessionState::Selected => Ok(self
                .begin_close(
                    GenerationCloseReason::LocalSeparate,
                    CloseBarrier::AfterOperation(operation_id),
                )),
            LocalControlPlan::Transactional {
                kind: ControlKind::Select,
            } if self.state == SessionState::NotSelected
                && self.overlay == SelectionOverlay::Idle
                && !self.peer_deselect_pending =>
            {
                self.overlay = SelectionOverlay::Selecting { operation_id };
                Ok(ControlDecision::empty())
            }
            LocalControlPlan::Transactional {
                kind: ControlKind::Deselect,
            } if self.state == SessionState::Selected
                && self.overlay == SelectionOverlay::Idle
                && !self.peer_deselect_pending =>
            {
                self.overlay = SelectionOverlay::Deselecting { operation_id };
                Ok(ControlDecision::empty())
            }
            LocalControlPlan::Transactional {
                kind: ControlKind::Linktest,
            } if matches!(
                self.state,
                SessionState::NotSelected | SessionState::Selected
            ) && self.overlay == SelectionOverlay::Idle =>
            {
                Ok(ControlDecision::empty())
            }
            _ => Err(ControlInvariantError::LocalPlanNoLongerValid {
                plan,
                state: self.state,
                overlay: self.overlay,
                peer_deselect_pending: self.peer_deselect_pending,
            }),
        }
    }

    /// Clears only an exact projected Select or Deselect terminal operation.
    ///
    /// Linktest has no FSM overlay. Stale or mismatched identities leave the
    /// current overlay unchanged for the actual Registry owner.
    pub(crate) fn finish_local_transaction(
        &mut self,
        operation_id: OperationId,
        kind: ControlKind,
    ) -> OverlayTerminalDecision {
        let exact = match (kind, self.overlay) {
            (
                ControlKind::Select,
                SelectionOverlay::Selecting {
                    operation_id: current,
                },
            )
            | (
                ControlKind::Deselect,
                SelectionOverlay::Deselecting {
                    operation_id: current,
                },
            ) => current == operation_id,
            _ => false,
        };
        if exact {
            self.overlay = SelectionOverlay::Idle;
            OverlayTerminalDecision::Cleared
        } else if kind == ControlKind::Linktest {
            OverlayTerminalDecision::NoOverlay
        } else {
            OverlayTerminalDecision::Stale {
                current: self.overlay,
            }
        }
    }

    /// Applies one exact Registry-matched local control response.
    ///
    /// `next_t7` is required only when a successful Deselect response actually
    /// begins a new NotSelected tenure. The returned result completes the
    /// operation; the action vector contains only selection-state work.
    pub(crate) fn on_matched_response(
        &mut self,
        operation_id: OperationId,
        response: MatchedControlResponse,
        next_t7: Option<TimerToken>,
    ) -> Result<LocalResponseDecision, ControlInvariantError> {
        match response {
            MatchedControlResponse::Select { status } => {
                Self::reject_unexpected_t7(next_t7, self.state)?;
                self.require_overlay(operation_id, ControlKind::Select)?;
                let rejection = NonZeroU8::new(status.get());
                let result = match rejection {
                    Some(status) => Err(OperationError::SelectRejected { status }),
                    None => Ok(()),
                };
                let state = if rejection.is_none()
                    && self.state == SessionState::NotSelected
                    && !self.peer_deselect_pending
                {
                    self.upgrade()?
                } else {
                    ControlDecision::empty()
                };
                self.overlay = SelectionOverlay::Idle;
                Ok(LocalResponseDecision { result, state })
            }
            MatchedControlResponse::Deselect { status } => {
                self.require_overlay(operation_id, ControlKind::Deselect)?;
                if let Some(status) = NonZeroU8::new(status.get()) {
                    Self::reject_unexpected_t7(next_t7, self.state)?;
                    self.overlay = SelectionOverlay::Idle;
                    Ok(LocalResponseDecision {
                        result: Err(OperationError::DeselectRejected { status }),
                        state: ControlDecision::empty(),
                    })
                } else {
                    let state = if self.state == SessionState::Selected {
                        let token = next_t7.ok_or(ControlInvariantError::MissingT7ForDowngrade)?;
                        self.downgrade(token)?
                    } else {
                        Self::reject_unexpected_t7(next_t7, self.state)?;
                        ControlDecision::empty()
                    };
                    self.overlay = SelectionOverlay::Idle;
                    Ok(LocalResponseDecision {
                        result: Ok(()),
                        state,
                    })
                }
            }
            MatchedControlResponse::Linktest => {
                Self::reject_unexpected_t7(next_t7, self.state)?;
                Ok(LocalResponseDecision {
                    result: Ok(()),
                    state: ControlDecision::empty(),
                })
            }
        }
    }

    /// Classifies `Select.req` and builds its typed response write bundle.
    pub(crate) fn on_select_request(
        &self,
        session_id: u16,
        system_bytes: SystemBytes,
    ) -> PeerRequestDecision {
        if matches!(self.state, SessionState::Closing | SessionState::Closed) {
            return Self::ignored_peer_request();
        }

        let (status, commit) = if self.peer_deselect_pending
            || matches!(self.overlay, SelectionOverlay::Deselecting { .. })
                && self.state == SessionState::NotSelected
        {
            (SelectStatus::NOT_READY, PeerResponseCommitIntent::None)
        } else if self.state == SessionState::Selected {
            (SelectStatus::ALREADY_ACTIVE, PeerResponseCommitIntent::None)
        } else {
            (
                SelectStatus::SUCCESS,
                PeerResponseCommitIntent::SelectAccepted,
            )
        };
        self.peer_response(
            ControlDecision::empty(),
            ControlMessage::SelectResponse {
                session_id,
                status,
                system_bytes,
            },
            commit,
        )
    }

    /// Classifies `Deselect.req`, closing the Data gate immediately on accept.
    pub(crate) fn on_deselect_request(
        &mut self,
        session_id: u16,
        system_bytes: SystemBytes,
    ) -> PeerRequestDecision {
        if matches!(self.state, SessionState::Closing | SessionState::Closed) {
            return Self::ignored_peer_request();
        }

        let (status, commit, immediate) = if self.peer_deselect_pending {
            (
                DeselectStatus::BUSY,
                PeerResponseCommitIntent::None,
                ControlDecision::empty(),
            )
        } else if self.state == SessionState::Selected {
            self.peer_deselect_pending = true;
            (
                DeselectStatus::SUCCESS,
                PeerResponseCommitIntent::DeselectAccepted,
                ControlDecision::new(vec![ControlAction::SetDataGate(DataGateState::Closed)]),
            )
        } else {
            (
                DeselectStatus::NOT_SELECTED,
                PeerResponseCommitIntent::None,
                ControlDecision::empty(),
            )
        };
        self.peer_response(
            immediate,
            ControlMessage::DeselectResponse {
                session_id,
                status,
                system_bytes,
            },
            commit,
        )
    }

    /// Builds `Linktest.rsp` in either open stable state without changing FSM state.
    pub(crate) fn on_linktest_request(&self, system_bytes: SystemBytes) -> PeerRequestDecision {
        if matches!(self.state, SessionState::Closing | SessionState::Closed) {
            Self::ignored_peer_request()
        } else {
            self.peer_response(
                ControlDecision::empty(),
                ControlMessage::LinktestResponse { system_bytes },
                PeerResponseCommitIntent::None,
            )
        }
    }

    /// Applies peer `Separate.req` semantics without producing a response.
    pub(crate) fn on_separate_request(&mut self) -> PeerRequestDecision {
        if self.state == SessionState::Selected {
            PeerRequestDecision::NoResponse {
                decision: self.begin_close(
                    GenerationCloseReason::SeparateReceived,
                    CloseBarrier::Immediate,
                ),
            }
        } else {
            Self::ignored_peer_request()
        }
    }

    /// Commits a peer response's deferred selection transition at BeginWrite.
    ///
    /// A successful peer Deselect already closed the Data gate when received.
    /// Its response fence clears the barrier and commits reset/state/T7 work.
    pub(crate) fn commit_peer_response(
        &mut self,
        commit: PeerResponseCommit,
        next_t7: Option<TimerToken>,
    ) -> Result<CommittedPeerResponse, PeerResponseCommitFailure> {
        if !self.peer_response_issuer.owns(&commit) {
            return Err(PeerResponseCommitFailure::new(
                ControlInvariantError::ForeignPeerResponseCommit,
                commit,
            ));
        }

        let decision = if commit.is_none() {
            Self::reject_unexpected_t7(next_t7, self.state).map(|()| ControlDecision::empty())
        } else if commit.is_select_accepted() {
            let validation = Self::reject_unexpected_t7(next_t7, self.state);
            validation.and_then(|()| {
                if self.state == SessionState::NotSelected && !self.peer_deselect_pending {
                    self.upgrade()
                } else {
                    Ok(ControlDecision::empty())
                }
            })
        } else {
            debug_assert!(commit.is_deselect_accepted());
            if matches!(self.state, SessionState::Closing | SessionState::Closed) {
                Self::reject_unexpected_t7(next_t7, self.state).map(|()| ControlDecision::empty())
            } else if !self.peer_deselect_pending {
                Err(ControlInvariantError::PeerDeselectCommitWithoutBarrier)
            } else if self.state == SessionState::Selected {
                match next_t7 {
                    Some(token) => self.commit_peer_deselect(token),
                    None => Err(ControlInvariantError::MissingT7ForDowngrade),
                }
            } else {
                Self::reject_unexpected_t7(next_t7, self.state).map(|()| {
                    self.peer_deselect_pending = false;
                    ControlDecision::empty()
                })
            }
        };

        match decision {
            Ok(decision) => {
                let receipt = self
                    .peer_response_issuer
                    .commit(commit)
                    .expect("peer-response brand was validated before mutation");
                Ok(CommittedPeerResponse { decision, receipt })
            }
            Err(error) => Err(PeerResponseCommitFailure::new(error, commit)),
        }
    }

    /// Applies an exact Registry T6 expiry and begins idempotent close.
    pub(crate) fn on_t6_expired(
        &mut self,
        operation_id: OperationId,
        kind: ControlKind,
    ) -> ControlTimeoutDecision {
        let overlay = self.finish_local_transaction(operation_id, kind);
        let close = self.begin_close(
            GenerationCloseReason::CommunicationsTimeout(CommunicationsTimeoutKind::T6),
            CloseBarrier::Immediate,
        );
        ControlTimeoutDecision { overlay, close }
    }

    /// Applies only the exact T7 expiry for the current NotSelected tenure.
    pub(crate) fn on_t7_expired(&mut self, token: TimerToken) -> ControlDecision {
        if self.state != SessionState::NotSelected || self.t7 != Some(token) {
            return ControlDecision::empty();
        }
        self.t7 = None;
        self.begin_close(
            GenerationCloseReason::CommunicationsTimeout(CommunicationsTimeoutKind::T7),
            CloseBarrier::Immediate,
        )
    }

    /// Starts the generation's first close and returns ordered close actions.
    ///
    /// Repeated calls in Closing or Closed are idempotent and return no work.
    pub(crate) fn begin_close(
        &mut self,
        reason: GenerationCloseReason,
        barrier: CloseBarrier,
    ) -> ControlDecision {
        if matches!(self.state, SessionState::Closing | SessionState::Closed) {
            return ControlDecision::empty();
        }

        self.state = SessionState::Closing;
        self.overlay = SelectionOverlay::Idle;
        self.peer_deselect_pending = false;
        let t7 = self.t7.take();
        let mut actions = vec![ControlAction::SetDataGate(DataGateState::Closed)];
        if let Some(token) = t7 {
            actions.push(ControlAction::CancelTimer(token));
        }
        actions.push(ControlAction::SessionStateChanged(SessionState::Closing));
        actions.push(ControlAction::BeginGenerationClose { reason, barrier });
        ControlDecision::new(actions)
    }

    /// Commits terminal Closed state without reopening or starting another close.
    pub(crate) fn transport_closed(&mut self) -> ControlDecision {
        if self.state == SessionState::Closed {
            return ControlDecision::empty();
        }

        self.state = SessionState::Closed;
        self.overlay = SelectionOverlay::Idle;
        self.peer_deselect_pending = false;
        let t7 = self.t7.take();
        let mut actions = vec![ControlAction::SetDataGate(DataGateState::Closed)];
        if let Some(token) = t7 {
            actions.push(ControlAction::CancelTimer(token));
        }
        actions.push(ControlAction::SessionStateChanged(SessionState::Closed));
        ControlDecision::new(actions)
    }

    /// Requires the exact selection overlay without changing reducer state.
    fn require_overlay(
        &self,
        operation_id: OperationId,
        kind: ControlKind,
    ) -> Result<(), ControlInvariantError> {
        let exact = match (kind, self.overlay) {
            (
                ControlKind::Select,
                SelectionOverlay::Selecting {
                    operation_id: current,
                },
            )
            | (
                ControlKind::Deselect,
                SelectionOverlay::Deselecting {
                    operation_id: current,
                },
            ) => current == operation_id,
            _ => false,
        };
        if exact {
            Ok(())
        } else {
            Err(ControlInvariantError::ResponseWithoutMatchingOverlay {
                operation_id,
                kind,
                overlay: self.overlay,
            })
        }
    }

    /// Commits a real NotSelected-to-Selected transition.
    fn upgrade(&mut self) -> Result<ControlDecision, ControlInvariantError> {
        if self.state != SessionState::NotSelected || self.peer_deselect_pending {
            return Ok(ControlDecision::empty());
        }
        let Some(t7) = self.t7.take() else {
            return Err(ControlInvariantError::MissingT7ForUpgrade);
        };
        self.state = SessionState::Selected;
        Ok(ControlDecision::new(vec![
            ControlAction::CancelTimer(t7),
            ControlAction::SessionStateChanged(SessionState::Selected),
            ControlAction::SetDataGate(DataGateState::Open),
        ]))
    }

    /// Commits a real Selected-to-NotSelected transition with a fresh T7 token.
    fn downgrade(&mut self, next_t7: TimerToken) -> Result<ControlDecision, ControlInvariantError> {
        Self::validate_t7(next_t7)?;
        if self.state != SessionState::Selected {
            return Err(ControlInvariantError::UnexpectedT7ForState { state: self.state });
        }
        if self.t7.is_some() {
            return Err(ControlInvariantError::T7AlreadyPresentWhileSelected);
        }

        self.state = SessionState::NotSelected;
        self.t7 = Some(next_t7);
        Ok(ControlDecision::new(vec![
            ControlAction::SetDataGate(DataGateState::Closed),
            ControlAction::ResetSelectedSession,
            ControlAction::SessionStateChanged(SessionState::NotSelected),
            ControlAction::ArmT7(next_t7),
        ]))
    }

    /// Commits one accepted peer Deselect after a successful downgrade.
    ///
    /// `next_t7` starts the resulting NotSelected tenure. The pending peer
    /// barrier is cleared only after [`Self::downgrade`] has completed all
    /// validation and state mutation successfully.
    fn commit_peer_deselect(
        &mut self,
        next_t7: TimerToken,
    ) -> Result<ControlDecision, ControlInvariantError> {
        let decision = self.downgrade(next_t7)?;
        self.peer_deselect_pending = false;
        Ok(decision)
    }

    /// Validates that `token` represents an exact T7 registration.
    fn validate_t7(token: TimerToken) -> Result<(), ControlInvariantError> {
        if token.kind() == TimeoutKind::T7 {
            Ok(())
        } else {
            Err(ControlInvariantError::WrongTimerKind {
                expected: TimeoutKind::T7,
                actual: token.kind(),
            })
        }
    }

    /// Rejects a token supplied where no new NotSelected tenure begins.
    fn reject_unexpected_t7(
        token: Option<TimerToken>,
        state: SessionState,
    ) -> Result<(), ControlInvariantError> {
        if token.is_none() {
            Ok(())
        } else {
            Err(ControlInvariantError::UnexpectedT7ForState { state })
        }
    }

    /// Builds a peer response decision with one exact typed write bundle.
    fn peer_response(
        &self,
        immediate: ControlDecision,
        response: ControlMessage,
        commit: PeerResponseCommitIntent,
    ) -> PeerRequestDecision {
        let pending = match commit {
            PeerResponseCommitIntent::None => self.peer_response_issuer.issue_none(response),
            PeerResponseCommitIntent::SelectAccepted => {
                self.peer_response_issuer.issue_select_accepted(response)
            }
            PeerResponseCommitIntent::DeselectAccepted => {
                self.peer_response_issuer.issue_deselect_accepted(response)
            }
        }
        .expect("ControlFsm response and deferred transition must agree");
        debug_assert_eq!(pending.generation(), self.generation);
        PeerRequestDecision::Respond {
            plan: PeerResponsePlan { immediate, pending },
        }
    }

    /// Builds an ignored peer-request decision with no response or actions.
    fn ignored_peer_request() -> PeerRequestDecision {
        PeerRequestDecision::NoResponse {
            decision: ControlDecision::empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU8;

    use super::{
        CloseBarrier, ControlAction, ControlDecision, ControlFsm, ControlInvariantError,
        LocalControlPlan, MatchedControlResponse, OverlayTerminalDecision, PeerRequestDecision,
        PeerResponsePlan, SelectionOverlay,
    };
    use crate::hsms::{
        contracts::{peer_response::PeerResponseCommit, ControlIntent, DataGateState},
        core::transaction::ControlKind,
        model::{
            ids::{OperationId, SystemBytes, TimerId},
            runtime::{CommunicationsTimeoutKind, GenerationCloseReason, TimerToken},
        },
        protocol::header::{ControlMessage, DeselectStatus, SelectStatus},
        OperationError, SessionState, TimeoutKind,
    };

    /// Constructs a deterministic operation identity for one test scenario.
    const fn operation(value: u64) -> OperationId {
        OperationId::new(value)
    }

    /// Constructs deterministic System Bytes for one peer-control scenario.
    const fn system_bytes(value: u32) -> SystemBytes {
        SystemBytes::new(value)
    }

    /// Constructs an exact timer token with the requested semantic kind.
    const fn timer(value: u64, kind: TimeoutKind) -> TimerToken {
        TimerToken::new(TimerId::new(value), kind)
    }

    /// Constructs an exact T7 token for a NotSelected tenure.
    const fn t7(value: u64) -> TimerToken {
        timer(value, TimeoutKind::T7)
    }

    /// Starts one valid NotSelected FSM and discards its already-asserted actions.
    /// Returns the fixed generation used by isolated ControlFsm tests.
    fn generation() -> crate::hsms::model::ids::ConnectionGeneration {
        crate::hsms::model::ids::ConnectionGeneration::new(7)
    }

    /// Starts a NotSelected ControlFsm in the fixed test generation.
    fn started() -> ControlFsm {
        let (fsm, decision) = ControlFsm::start(generation(), t7(1)).expect("valid initial T7");
        assert_eq!(
            decision.actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::SessionStateChanged(SessionState::NotSelected),
                ControlAction::ArmT7(t7(1)),
            ]
        );
        fsm
    }

    /// Plans and commits one local transactional control operation.
    fn start_transaction(fsm: &mut ControlFsm, intent: ControlIntent, id: OperationId) {
        let plan = fsm
            .plan_local(intent, true)
            .expect("transaction must be admissible");
        assert!(plan.transactional_kind().is_some());
        assert!(fsm
            .commit_local_started(id, plan)
            .expect("reserved transaction must commit")
            .is_empty());
    }

    /// Builds a stable Selected FSM through the public local-Select transition.
    fn selected() -> ControlFsm {
        let mut fsm = started();
        let id = operation(10);
        start_transaction(&mut fsm, ControlIntent::Select, id);
        let response = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Select {
                    status: SelectStatus::SUCCESS,
                },
                None,
            )
            .expect("successful Select response");
        assert_eq!(response.result(), &Ok(()));
        assert_eq!(fsm.state(), SessionState::Selected);
        assert_eq!(fsm.data_gate_state(), DataGateState::Open);
        fsm
    }

    /// Extracts a mandatory peer response plan or fails the test.
    fn response_plan(decision: PeerRequestDecision) -> PeerResponsePlan {
        match decision {
            PeerRequestDecision::Respond { plan } => plan,
            PeerRequestDecision::NoResponse { .. } => {
                panic!("peer request unexpectedly produced no response")
            }
        }
    }

    /// Splits a peer plan through the test-only hook escape used by FSM tests.
    ///
    /// Production code cannot perform this split; CoreResources must first bind
    /// the pending bundle to a write and reach its exact BeginWrite fence.
    fn split_response_plan(plan: PeerResponsePlan) -> (ControlDecision, PeerResponseCommit) {
        let (immediate, pending) = plan.into_ordered_parts();
        (immediate, pending.into_commit_for_test())
    }

    /// Extracts the action-only branch of a peer request or fails the test.
    fn no_response(decision: PeerRequestDecision) -> ControlDecision {
        match decision {
            PeerRequestDecision::NoResponse { decision } => decision,
            PeerRequestDecision::Respond { .. } => {
                panic!("peer request unexpectedly produced a response")
            }
        }
    }

    /// Verifies construction rejects non-T7 tokens and emits exact initial actions.
    #[test]
    fn start_requires_t7_and_emits_ordered_initial_actions() {
        assert_eq!(
            ControlFsm::start(generation(), timer(1, TimeoutKind::T6))
                .expect_err("T6 is not an initial T7"),
            ControlInvariantError::WrongTimerKind {
                expected: TimeoutKind::T7,
                actual: TimeoutKind::T6,
            }
        );

        let (fsm, decision) = ControlFsm::start(generation(), t7(7)).expect("valid T7");
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
        assert!(!fsm.peer_deselect_pending());
        assert_eq!(fsm.data_gate_state(), DataGateState::Closed);
        assert_eq!(fsm.t7(), Some(t7(7)));
        assert_eq!(
            decision.into_actions(),
            vec![
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::SessionStateChanged(SessionState::NotSelected),
                ControlAction::ArmT7(t7(7)),
            ]
        );
    }

    /// Locks the local NotSelected intent matrix and semantic error precedence.
    #[test]
    fn not_selected_local_planning_uses_the_frozen_intent_matrix() {
        let fsm = started();
        assert_eq!(
            fsm.plan_local(ControlIntent::Select, true),
            Ok(LocalControlPlan::Transactional {
                kind: ControlKind::Select,
            })
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Linktest, true),
            Ok(LocalControlPlan::Transactional {
                kind: ControlKind::Linktest,
            })
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Deselect, false),
            Err(OperationError::NotSelected)
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Separate, false),
            Err(OperationError::NotSelected)
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Select, false),
            Err(OperationError::ControlBusy)
        );
    }

    /// Locks the local Selected intent matrix and Separate preemption behavior.
    #[test]
    fn selected_local_planning_uses_the_frozen_intent_matrix() {
        let fsm = selected();
        assert_eq!(
            fsm.plan_local(ControlIntent::Deselect, true),
            Ok(LocalControlPlan::Transactional {
                kind: ControlKind::Deselect,
            })
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Linktest, true),
            Ok(LocalControlPlan::Transactional {
                kind: ControlKind::Linktest,
            })
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Select, false),
            Err(OperationError::AlreadySelected)
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Separate, false),
            Ok(LocalControlPlan::Separate)
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Deselect, false),
            Err(OperationError::ControlBusy)
        );
    }

    /// Confirms a projected selection operation owns the sole transactional slot.
    #[test]
    fn selection_overlay_blocks_transactions_but_not_selected_separate() {
        let mut fsm = selected();
        start_transaction(&mut fsm, ControlIntent::Deselect, operation(20));

        assert_eq!(
            fsm.plan_local(ControlIntent::Linktest, true),
            Err(OperationError::ControlBusy)
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Deselect, true),
            Err(OperationError::ControlBusy)
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Select, true),
            Err(OperationError::AlreadySelected)
        );
        assert_eq!(
            fsm.plan_local(ControlIntent::Separate, true),
            Ok(LocalControlPlan::Separate)
        );
    }

    /// Confirms commit revalidates state after Core performs external reservation.
    #[test]
    fn local_commit_rejects_stale_or_replayed_plans_without_mutation() {
        let mut fsm = started();
        let plan = fsm
            .plan_local(ControlIntent::Select, true)
            .expect("Select plan");
        fsm.overlay = SelectionOverlay::Selecting {
            operation_id: operation(21),
        };

        assert_eq!(
            fsm.commit_local_started(operation(22), plan),
            Err(ControlInvariantError::LocalPlanNoLongerValid {
                plan,
                state: SessionState::NotSelected,
                overlay: SelectionOverlay::Selecting {
                    operation_id: operation(21),
                },
                peer_deselect_pending: false,
            })
        );
        assert_eq!(
            fsm.overlay(),
            SelectionOverlay::Selecting {
                operation_id: operation(21),
            }
        );
    }

    /// Confirms terminal cleanup clears only the exact operation and kind.
    #[test]
    fn terminal_cleanup_preserves_mismatched_selection_overlays() {
        let mut fsm = started();
        start_transaction(&mut fsm, ControlIntent::Select, operation(30));

        assert_eq!(
            fsm.finish_local_transaction(operation(31), ControlKind::Select),
            OverlayTerminalDecision::Stale {
                current: SelectionOverlay::Selecting {
                    operation_id: operation(30),
                },
            }
        );
        assert_eq!(
            fsm.finish_local_transaction(operation(30), ControlKind::Deselect),
            OverlayTerminalDecision::Stale {
                current: SelectionOverlay::Selecting {
                    operation_id: operation(30),
                },
            }
        );
        assert_eq!(
            fsm.finish_local_transaction(operation(99), ControlKind::Linktest),
            OverlayTerminalDecision::NoOverlay
        );
        assert_eq!(
            fsm.finish_local_transaction(operation(30), ControlKind::Select),
            OverlayTerminalDecision::Cleared
        );
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
    }

    /// Confirms a successful local Select commits state, T7, and gate in order.
    #[test]
    fn local_select_success_upgrades_once_at_the_matched_response() {
        let mut fsm = started();
        let id = operation(40);
        start_transaction(&mut fsm, ControlIntent::Select, id);

        let decision = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Select {
                    status: SelectStatus::SUCCESS,
                },
                None,
            )
            .expect("matching Select response");
        assert_eq!(decision.result(), &Ok(()));
        assert_eq!(
            decision.state_decision().actions(),
            [
                ControlAction::CancelTimer(t7(1)),
                ControlAction::SessionStateChanged(SessionState::Selected),
                ControlAction::SetDataGate(DataGateState::Open),
            ]
        );
        assert_eq!(fsm.state(), SessionState::Selected);
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
        assert_eq!(fsm.t7(), None);
    }

    /// Confirms a non-success Select preserves its exact extension status.
    #[test]
    fn local_select_rejection_clears_only_its_overlay() {
        let mut fsm = started();
        let id = operation(41);
        start_transaction(&mut fsm, ControlIntent::Select, id);

        let (state, result) = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Select {
                    status: SelectStatus::new(0x80),
                },
                None,
            )
            .expect("matching rejection")
            .into_ordered_parts();
        assert_eq!(
            result,
            Err(OperationError::SelectRejected {
                status: NonZeroU8::new(0x80).expect("non-zero status"),
            })
        );
        assert!(state.is_empty());
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
        assert_eq!(fsm.t7(), Some(t7(1)));
    }

    /// Confirms an incorrectly attributed response cannot consume another overlay.
    #[test]
    fn mismatched_local_response_returns_an_invariant_without_mutation() {
        let mut fsm = started();
        start_transaction(&mut fsm, ControlIntent::Select, operation(42));

        assert_eq!(
            fsm.on_matched_response(
                operation(43),
                MatchedControlResponse::Select {
                    status: SelectStatus::SUCCESS,
                },
                None,
            ),
            Err(ControlInvariantError::ResponseWithoutMatchingOverlay {
                operation_id: operation(43),
                kind: ControlKind::Select,
                overlay: SelectionOverlay::Selecting {
                    operation_id: operation(42),
                },
            })
        );
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(
            fsm.overlay(),
            SelectionOverlay::Selecting {
                operation_id: operation(42),
            }
        );
        assert_eq!(fsm.t7(), Some(t7(1)));
    }

    /// Confirms a corrupt missing T7 cannot partially consume a Select response.
    #[test]
    fn select_upgrade_invariant_failure_preserves_the_pending_overlay() {
        let mut fsm = started();
        let id = operation(44);
        start_transaction(&mut fsm, ControlIntent::Select, id);
        fsm.t7 = None;

        assert_eq!(
            fsm.on_matched_response(
                id,
                MatchedControlResponse::Select {
                    status: SelectStatus::SUCCESS,
                },
                None,
            ),
            Err(ControlInvariantError::MissingT7ForUpgrade)
        );
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(
            fsm.overlay(),
            SelectionOverlay::Selecting { operation_id: id }
        );
    }

    /// Confirms a successful local Deselect closes/reset/rearms in exact order.
    #[test]
    fn local_deselect_success_starts_one_fresh_not_selected_tenure() {
        let mut fsm = selected();
        let id = operation(50);
        start_transaction(&mut fsm, ControlIntent::Deselect, id);

        let decision = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Deselect {
                    status: DeselectStatus::SUCCESS,
                },
                Some(t7(2)),
            )
            .expect("matching Deselect response");
        assert_eq!(decision.result(), &Ok(()));
        assert_eq!(
            decision.state_decision().actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::ResetSelectedSession,
                ControlAction::SessionStateChanged(SessionState::NotSelected),
                ControlAction::ArmT7(t7(2)),
            ]
        );
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
        assert_eq!(fsm.t7(), Some(t7(2)));
    }

    /// Confirms non-success Deselect preserves status and the Selected state.
    #[test]
    fn local_deselect_rejection_does_not_close_the_data_gate() {
        let mut fsm = selected();
        let id = operation(51);
        start_transaction(&mut fsm, ControlIntent::Deselect, id);

        let decision = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Deselect {
                    status: DeselectStatus::new(0x81),
                },
                None,
            )
            .expect("matching Deselect rejection");
        assert_eq!(
            decision.result(),
            &Err(OperationError::DeselectRejected {
                status: NonZeroU8::new(0x81).expect("non-zero status"),
            })
        );
        assert!(decision.state_decision().is_empty());
        assert_eq!(fsm.state(), SessionState::Selected);
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
    }

    /// Confirms missing or mistyped next-T7 tokens never partially downgrade.
    #[test]
    fn local_deselect_t7_errors_preserve_selected_state_and_overlay() {
        let mut fsm = selected();
        let id = operation(52);
        start_transaction(&mut fsm, ControlIntent::Deselect, id);

        assert_eq!(
            fsm.on_matched_response(
                id,
                MatchedControlResponse::Deselect {
                    status: DeselectStatus::SUCCESS,
                },
                None,
            ),
            Err(ControlInvariantError::MissingT7ForDowngrade)
        );
        assert_eq!(fsm.state(), SessionState::Selected);
        assert_eq!(
            fsm.overlay(),
            SelectionOverlay::Deselecting { operation_id: id }
        );

        assert_eq!(
            fsm.on_matched_response(
                id,
                MatchedControlResponse::Deselect {
                    status: DeselectStatus::BUSY,
                },
                Some(t7(4)),
            ),
            Err(ControlInvariantError::UnexpectedT7ForState {
                state: SessionState::Selected,
            })
        );
        assert_eq!(fsm.state(), SessionState::Selected);
        assert_eq!(
            fsm.overlay(),
            SelectionOverlay::Deselecting { operation_id: id }
        );

        assert_eq!(
            fsm.on_matched_response(
                id,
                MatchedControlResponse::Deselect {
                    status: DeselectStatus::SUCCESS,
                },
                Some(timer(3, TimeoutKind::T6)),
            ),
            Err(ControlInvariantError::WrongTimerKind {
                expected: TimeoutKind::T7,
                actual: TimeoutKind::T6,
            })
        );
        assert_eq!(fsm.state(), SessionState::Selected);
        assert_eq!(
            fsm.overlay(),
            SelectionOverlay::Deselecting { operation_id: id }
        );
    }

    /// Confirms peer Select requests map stable and collision states to E37 status.
    #[test]
    fn peer_select_request_matrix_preserves_status_and_fence_semantics() {
        let fsm = started();
        let plan = response_plan(fsm.on_select_request(7, system_bytes(60)));
        assert_eq!(
            plan.response(),
            ControlMessage::SelectResponse {
                session_id: 7,
                status: SelectStatus::SUCCESS,
                system_bytes: system_bytes(60),
            }
        );
        assert!(plan.pending().is_select_accepted());
        assert!(plan.immediate_decision().is_empty());

        let fsm = selected();
        let plan = response_plan(fsm.on_select_request(8, system_bytes(61)));
        assert_eq!(
            plan.response(),
            ControlMessage::SelectResponse {
                session_id: 8,
                status: SelectStatus::ALREADY_ACTIVE,
                system_bytes: system_bytes(61),
            }
        );
        assert!(plan.pending().is_none());

        let mut fsm = selected();
        let _ = fsm.on_deselect_request(9, system_bytes(62));
        let plan = response_plan(fsm.on_select_request(9, system_bytes(63)));
        assert_eq!(
            plan.response(),
            ControlMessage::SelectResponse {
                session_id: 9,
                status: SelectStatus::NOT_READY,
                system_bytes: system_bytes(63),
            }
        );
        assert!(plan.pending().is_none());
    }

    /// Confirms accepted peer Select upgrades only when its response begins writing.
    #[test]
    fn peer_select_response_fence_commits_the_upgrade() {
        let mut fsm = started();
        let plan = response_plan(fsm.on_select_request(10, system_bytes(64)));
        assert_eq!(fsm.state(), SessionState::NotSelected);

        let response = plan.response();
        let (immediate, commit) = split_response_plan(plan);
        assert!(immediate.is_empty());
        assert!(matches!(response, ControlMessage::SelectResponse { .. }));
        let committed = fsm
            .commit_peer_response(commit, None)
            .expect("accepted peer Select fence");
        let (decision, receipt) = committed.into_parts();
        assert!(receipt.is_select_accepted());
        assert_eq!(
            decision.actions(),
            [
                ControlAction::CancelTimer(t7(1)),
                ControlAction::SessionStateChanged(SessionState::Selected),
                ControlAction::SetDataGate(DataGateState::Open),
            ]
        );
        assert_eq!(fsm.state(), SessionState::Selected);
    }

    /// Confirms simultaneous Select handshakes converge without duplicate transitions.
    #[test]
    fn simultaneous_select_preserves_local_overlay_across_peer_upgrade() {
        let mut fsm = started();
        let id = operation(65);
        start_transaction(&mut fsm, ControlIntent::Select, id);
        let plan = response_plan(fsm.on_select_request(11, system_bytes(65)));
        let (_, commit) = split_response_plan(plan);

        let committed = fsm
            .commit_peer_response(commit, None)
            .expect("peer Select response fence");
        let (peer, receipt) = committed.into_parts();
        assert!(receipt.is_select_accepted());
        assert!(!peer.is_empty());
        assert_eq!(fsm.state(), SessionState::Selected);
        assert_eq!(
            fsm.overlay(),
            SelectionOverlay::Selecting { operation_id: id }
        );

        let local = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Select {
                    status: SelectStatus::SUCCESS,
                },
                None,
            )
            .expect("local Select response");
        assert_eq!(local.result(), &Ok(()));
        assert!(local.state_decision().is_empty());
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
        assert_eq!(fsm.state(), SessionState::Selected);
    }

    /// Confirms accepted peer Deselect closes the gate immediately and owns a barrier.
    #[test]
    fn peer_deselect_request_closes_gate_before_scheduling_its_response() {
        let mut fsm = selected();
        let plan = response_plan(fsm.on_deselect_request(12, system_bytes(70)));
        assert_eq!(
            plan.response(),
            ControlMessage::DeselectResponse {
                session_id: 12,
                status: DeselectStatus::SUCCESS,
                system_bytes: system_bytes(70),
            }
        );
        assert!(plan.pending().is_deselect_accepted());
        assert_eq!(
            plan.immediate_decision().actions(),
            [ControlAction::SetDataGate(DataGateState::Closed)]
        );
        assert!(fsm.peer_deselect_pending());
        assert_eq!(fsm.state(), SessionState::Selected);
        assert_eq!(fsm.data_gate_state(), DataGateState::Closed);

        let duplicate = response_plan(fsm.on_deselect_request(12, system_bytes(71)));
        assert_eq!(
            duplicate.response(),
            ControlMessage::DeselectResponse {
                session_id: 12,
                status: DeselectStatus::BUSY,
                system_bytes: system_bytes(71),
            }
        );
        assert!(duplicate.pending().is_none());
        assert!(duplicate.immediate_decision().is_empty());
    }

    /// Confirms a peer Deselect in NotSelected reports status without rearming T7.
    #[test]
    fn peer_deselect_while_not_selected_is_a_no_transition_response() {
        let mut fsm = started();
        let plan = response_plan(fsm.on_deselect_request(13, system_bytes(72)));
        assert_eq!(
            plan.response(),
            ControlMessage::DeselectResponse {
                session_id: 13,
                status: DeselectStatus::NOT_SELECTED,
                system_bytes: system_bytes(72),
            }
        );
        assert!(plan.pending().is_none());
        assert!(plan.immediate_decision().is_empty());
        assert!(!fsm.peer_deselect_pending());
        assert_eq!(fsm.t7(), Some(t7(1)));
    }

    /// Confirms accepted peer Deselect commits reset/state/T7 at BeginWrite.
    #[test]
    fn peer_deselect_response_fence_commits_one_downgrade() {
        let mut fsm = selected();
        let plan = response_plan(fsm.on_deselect_request(14, system_bytes(73)));
        let (_, commit) = split_response_plan(plan);

        let committed = fsm
            .commit_peer_response(commit, Some(t7(4)))
            .expect("accepted peer Deselect fence");
        let (decision, receipt) = committed.into_parts();
        assert!(receipt.is_deselect_accepted());
        assert_eq!(
            decision.actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::ResetSelectedSession,
                ControlAction::SessionStateChanged(SessionState::NotSelected),
                ControlAction::ArmT7(t7(4)),
            ]
        );
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.t7(), Some(t7(4)));
        assert!(!fsm.peer_deselect_pending());
        assert_eq!(fsm.data_gate_state(), DataGateState::Closed);
    }

    /// Confirms peer Deselect fence validation leaves its gate barrier intact.
    #[test]
    fn peer_deselect_fence_t7_errors_do_not_partially_clear_the_barrier() {
        let mut missing_t7 = selected();
        let plan = response_plan(missing_t7.on_deselect_request(15, system_bytes(74)));
        let (_, commit) = split_response_plan(plan);
        let failure = missing_t7
            .commit_peer_response(commit, None)
            .expect_err("accepted Deselect requires a replacement T7");
        assert_eq!(
            failure.error(),
            ControlInvariantError::MissingT7ForDowngrade
        );
        let (_, returned_commit) = failure.into_parts();
        assert!(returned_commit.is_deselect_accepted());
        assert_eq!(missing_t7.state(), SessionState::Selected);
        assert!(missing_t7.peer_deselect_pending());

        let mut wrong_kind = selected();
        let plan = response_plan(wrong_kind.on_deselect_request(15, system_bytes(74)));
        let (_, commit) = split_response_plan(plan);
        let failure = wrong_kind
            .commit_peer_response(commit, Some(timer(5, TimeoutKind::Linktest)))
            .expect_err("accepted Deselect requires a T7 timer");
        assert_eq!(
            failure.error(),
            ControlInvariantError::WrongTimerKind {
                expected: TimeoutKind::T7,
                actual: TimeoutKind::Linktest,
            }
        );
        let (_, returned_commit) = failure.into_parts();
        assert!(returned_commit.is_deselect_accepted());
        assert_eq!(wrong_kind.state(), SessionState::Selected);
        assert!(wrong_kind.peer_deselect_pending());
    }

    /// Confirms peer-first simultaneous Deselect leaves the local result transition-free.
    #[test]
    fn simultaneous_deselect_peer_fence_can_win_without_duplicate_t7() {
        let mut fsm = selected();
        let id = operation(75);
        start_transaction(&mut fsm, ControlIntent::Deselect, id);
        let peer = response_plan(fsm.on_deselect_request(16, system_bytes(75)));
        let (_, commit) = split_response_plan(peer);

        let committed = fsm
            .commit_peer_response(commit, Some(t7(6)))
            .expect("peer Deselect fence");
        let (decision, receipt) = committed.into_parts();
        assert!(receipt.is_deselect_accepted());
        assert!(!decision.is_empty());
        assert_eq!(
            fsm.overlay(),
            SelectionOverlay::Deselecting { operation_id: id }
        );

        let local = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Deselect {
                    status: DeselectStatus::SUCCESS,
                },
                None,
            )
            .expect("local Deselect response");
        assert!(local.state_decision().is_empty());
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.t7(), Some(t7(6)));
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
    }

    /// Confirms local-first simultaneous Deselect leaves the peer fence transition-free.
    #[test]
    fn simultaneous_deselect_local_response_can_win_without_duplicate_reset() {
        let mut fsm = selected();
        let id = operation(76);
        start_transaction(&mut fsm, ControlIntent::Deselect, id);
        let peer = response_plan(fsm.on_deselect_request(17, system_bytes(76)));
        let (_, commit) = split_response_plan(peer);

        let local = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Deselect {
                    status: DeselectStatus::SUCCESS,
                },
                Some(t7(7)),
            )
            .expect("local Deselect response");
        assert!(!local.state_decision().is_empty());
        assert!(fsm.peer_deselect_pending());

        let committed = fsm
            .commit_peer_response(commit, None)
            .expect("peer Deselect response fence");
        let (peer_decision, receipt) = committed.into_parts();
        assert!(receipt.is_deselect_accepted());
        assert!(peer_decision.is_empty());
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.t7(), Some(t7(7)));
        assert!(!fsm.peer_deselect_pending());
    }

    /// Confirms Linktest responds in open states and is ignored after closing.
    #[test]
    fn peer_linktest_request_is_limited_to_open_generation_states() {
        let fsm = started();
        let plan = response_plan(fsm.on_linktest_request(system_bytes(80)));
        assert_eq!(
            plan.response(),
            ControlMessage::LinktestResponse {
                system_bytes: system_bytes(80),
            }
        );
        assert!(plan.pending().is_none());

        let mut fsm = selected();
        let _ = fsm.begin_close(
            GenerationCloseReason::LocalDisconnect,
            CloseBarrier::Immediate,
        );
        let ignored = fsm.on_linktest_request(system_bytes(81));
        assert!(ignored.response_plan().is_none());
        assert!(ignored
            .no_response_decision()
            .expect("ignored decision")
            .is_empty());
    }

    /// Confirms local Separate waits for its write while peer Separate closes now.
    #[test]
    fn separate_uses_direction_specific_close_barriers() {
        let mut local = selected();
        let plan = local
            .plan_local(ControlIntent::Separate, false)
            .expect("Selected Separate plan");
        let decision = local
            .commit_local_started(operation(90), plan)
            .expect("Separate commit");
        assert_eq!(
            decision.actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::SessionStateChanged(SessionState::Closing),
                ControlAction::BeginGenerationClose {
                    reason: GenerationCloseReason::LocalSeparate,
                    barrier: CloseBarrier::AfterOperation(operation(90)),
                },
            ]
        );

        let mut peer = selected();
        let decision = no_response(peer.on_separate_request());
        assert_eq!(
            decision.actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::SessionStateChanged(SessionState::Closing),
                ControlAction::BeginGenerationClose {
                    reason: GenerationCloseReason::SeparateReceived,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );

        let mut not_selected = started();
        assert!(no_response(not_selected.on_separate_request()).is_empty());
        assert_eq!(not_selected.state(), SessionState::NotSelected);
    }

    /// Confirms stale T7 tokens are ignored and the exact token closes once.
    #[test]
    fn t7_expiry_is_correlated_to_one_not_selected_tenure() {
        let mut fsm = started();
        assert!(
            fsm.on_t7_expired(t7(99)).is_empty(),
            "stale T7 must not change the generation"
        );
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.t7(), Some(t7(1)));

        let decision = fsm.on_t7_expired(t7(1));
        assert_eq!(
            decision.actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::SessionStateChanged(SessionState::Closing),
                ControlAction::BeginGenerationClose {
                    reason: GenerationCloseReason::CommunicationsTimeout(
                        CommunicationsTimeoutKind::T7,
                    ),
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
        assert_eq!(fsm.t7(), None);
        assert!(fsm.on_t7_expired(t7(1)).is_empty());
    }

    /// Confirms a late T7 from an earlier tenure cannot close a later tenure.
    #[test]
    fn late_t7_from_an_earlier_tenure_is_ignored_after_reselection_cycle() {
        let mut fsm = selected();
        let id = operation(95);
        start_transaction(&mut fsm, ControlIntent::Deselect, id);
        let _ = fsm
            .on_matched_response(
                id,
                MatchedControlResponse::Deselect {
                    status: DeselectStatus::SUCCESS,
                },
                Some(t7(2)),
            )
            .expect("successful Deselect");

        assert!(fsm.on_t7_expired(t7(1)).is_empty());
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.t7(), Some(t7(2)));
    }

    /// Confirms exact T6 expiry clears its projection and begins immediate close.
    #[test]
    fn t6_expiry_classifies_overlay_cleanup_and_closes_the_generation() {
        let mut fsm = started();
        let id = operation(100);
        start_transaction(&mut fsm, ControlIntent::Select, id);

        let timeout = fsm.on_t6_expired(id, ControlKind::Select);
        assert_eq!(timeout.overlay(), OverlayTerminalDecision::Cleared);
        assert_eq!(
            timeout.close_decision().actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::CancelTimer(t7(1)),
                ControlAction::SessionStateChanged(SessionState::Closing),
                ControlAction::BeginGenerationClose {
                    reason: GenerationCloseReason::CommunicationsTimeout(
                        CommunicationsTimeoutKind::T6,
                    ),
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
        let (overlay, close) = timeout.into_parts();
        assert_eq!(overlay, OverlayTerminalDecision::Cleared);
        assert!(!close.is_empty());
        assert_eq!(fsm.state(), SessionState::Closing);
        assert_eq!(fsm.overlay(), SelectionOverlay::Idle);
    }

    /// Confirms the first close cancels live T7 and later close requests are inert.
    #[test]
    fn generation_close_is_ordered_and_idempotent() {
        let mut fsm = started();
        let first = fsm.begin_close(
            GenerationCloseReason::ProtocolViolation,
            CloseBarrier::Immediate,
        );
        assert_eq!(
            first.actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::CancelTimer(t7(1)),
                ControlAction::SessionStateChanged(SessionState::Closing),
                ControlAction::BeginGenerationClose {
                    reason: GenerationCloseReason::ProtocolViolation,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
        assert!(fsm
            .begin_close(
                GenerationCloseReason::TransportLost,
                CloseBarrier::Immediate,
            )
            .is_empty());
        assert_eq!(fsm.state(), SessionState::Closing);
    }

    /// Confirms transport closure is terminal and cannot emit duplicate state.
    #[test]
    fn transport_closed_commits_one_terminal_state_transition() {
        let mut fsm = started();
        let decision = fsm.transport_closed();
        assert_eq!(
            decision.actions(),
            [
                ControlAction::SetDataGate(DataGateState::Closed),
                ControlAction::CancelTimer(t7(1)),
                ControlAction::SessionStateChanged(SessionState::Closed),
            ]
        );
        assert_eq!(fsm.state(), SessionState::Closed);
        assert!(fsm.transport_closed().is_empty());
        assert_eq!(
            fsm.plan_local(ControlIntent::Linktest, true),
            Err(OperationError::NotConnected)
        );
    }

    /// Confirms Closing rejects new local work and ignores every peer request.
    #[test]
    fn closing_state_is_an_inert_control_boundary() {
        let mut fsm = started();
        let stale = response_plan(fsm.on_select_request(18, system_bytes(129)));
        let (_, commit) = split_response_plan(stale);
        let _ = fsm.begin_close(
            GenerationCloseReason::LocalDisconnect,
            CloseBarrier::Immediate,
        );

        assert_eq!(
            fsm.plan_local(ControlIntent::Separate, true),
            Err(OperationError::Draining)
        );
        assert!(no_response(fsm.on_select_request(18, system_bytes(130))).is_empty());
        assert!(no_response(fsm.on_deselect_request(18, system_bytes(131))).is_empty());
        assert!(no_response(fsm.on_separate_request()).is_empty());
        let committed = fsm
            .commit_peer_response(commit, None)
            .expect("stale Select fence is inert");
        let (decision, receipt) = committed.into_parts();
        assert!(receipt.is_select_accepted());
        assert!(decision.is_empty());
        assert_eq!(fsm.state(), SessionState::Closing);
    }

    /// Confirms non-transition response commits reject accidental T7 allocation.
    #[test]
    fn response_commits_reject_t7_when_no_new_tenure_begins() {
        let mut fsm = started();
        let plan = response_plan(fsm.on_linktest_request(system_bytes(109)));
        let (_, commit) = split_response_plan(plan);
        let failure = fsm
            .commit_peer_response(commit, Some(t7(110)))
            .expect_err("non-transition response cannot allocate T7");
        assert_eq!(
            failure.error(),
            ControlInvariantError::UnexpectedT7ForState {
                state: SessionState::NotSelected,
            }
        );
        let (_, returned_commit) = failure.into_parts();
        assert!(returned_commit.is_none());
        assert_eq!(fsm.state(), SessionState::NotSelected);
        assert_eq!(fsm.t7(), Some(t7(1)));

        assert_eq!(
            fsm.on_matched_response(
                operation(999),
                MatchedControlResponse::Linktest,
                Some(t7(111)),
            ),
            Err(ControlInvariantError::UnexpectedT7ForState {
                state: SessionState::NotSelected,
            })
        );
        assert_eq!(fsm.t7(), Some(t7(1)));
    }

    /// Confirms a peer Deselect fence cannot be fabricated without its barrier.
    #[test]
    fn peer_deselect_commit_requires_an_accepted_request_barrier() {
        let mut target = selected();
        let mut foreign = selected();
        let plan = response_plan(foreign.on_deselect_request(19, system_bytes(132)));
        let (_, commit) = split_response_plan(plan);
        let failure = target
            .commit_peer_response(commit, Some(t7(120)))
            .expect_err("another issuer brand cannot commit this barrier");
        assert_eq!(
            failure.error(),
            ControlInvariantError::ForeignPeerResponseCommit
        );
        let (_, returned_commit) = failure.into_parts();
        assert!(returned_commit.is_deselect_accepted());
        assert_eq!(target.state(), SessionState::Selected);
        assert_eq!(target.t7(), None);
        assert!(!target.peer_deselect_pending());
    }
}
