//! Deterministic control-only HSMS Session Core for the B1 fake-transport slice.
//!
//! This module owns selection state, one active transactional-control request,
//! generation-local protocol identifiers, and produced-write correlation. It
//! accepts semantic inputs synchronously and returns ordered actions without
//! depending on a runtime, transport, channel, task, or physical clock.

use std::{
    collections::{BTreeSet, HashMap},
    num::NonZeroU8,
};

use crate::hsms::{
    error::{OperationError, ProtocolError},
    lifecycle::SessionState,
    model::{
        ids::{CommandId, SessionId, SystemBytes, WriteId},
        runtime::{CloseBarrier, GenerationCloseReason, MonoTime, WriteOutcome},
    },
    protocol::{
        header::{ControlMessage, RejectReason, SelectStatus},
        message::ProtocolMessage,
    },
};

use super::{CoreAction, CoreActions, CoreCommand, CoreCommandKind, CoreCommandResult};

/// Control-message Session ID required for locally generated HSMS-SS control traffic.
const CONTROL_SESSION_ID: u16 = u16::MAX;
/// SType carried by an HSMS `Select.req` header.
const SELECT_REQUEST_STYPE: u8 = 1;
/// SType carried by an HSMS `Select.rsp` header.
const SELECT_RESPONSE_STYPE: u8 = 2;
/// SType carried by an HSMS `Linktest.req` header.
const LINKTEST_REQUEST_STYPE: u8 = 5;
/// SType carried by an HSMS `Linktest.rsp` header.
const LINKTEST_RESPONSE_STYPE: u8 = 6;

/// Immutable configuration retained by one generation-local Session Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionCoreConfig {
    /// Data Session ID reserved for the later Data-message slice.
    session_id: SessionId,
}

impl SessionCoreConfig {
    /// Creates a Session Core configuration for the endpoint Data `session_id`.
    pub(crate) const fn new(session_id: SessionId) -> Self {
        Self { session_id }
    }

    /// Returns the configured Data Session ID without exposing control allocation.
    pub(crate) const fn session_id(self) -> SessionId {
        self.session_id
    }
}

/// Transactional control request types supported by the B1 Core slice.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransactionKind {
    /// Locally initiated `Select.req` waiting for `Select.rsp` or peer Reject.
    Select,
    /// Locally initiated `Linktest.req` waiting for `Linktest.rsp` or peer Reject.
    Linktest,
}

impl TransactionKind {
    /// Returns the request SType used to attribute an inbound peer Reject.
    const fn request_stype(self) -> u8 {
        match self {
            Self::Select => SELECT_REQUEST_STYPE,
            Self::Linktest => LINKTEST_REQUEST_STYPE,
        }
    }
}

/// One live local transactional-control request and its response contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ControlTransaction {
    /// Control procedure whose exact response type must match.
    kind: TransactionKind,
    /// Driver-assigned command awaiting one terminal completion.
    command_id: CommandId,
    /// Fixed or copied Control Session ID required by the matcher.
    session_id: u16,
    /// Core-assigned correlation value required by the matcher.
    system_bytes: SystemBytes,
    /// Core-assigned request write retained independently of transaction completion.
    write_id: WriteId,
    /// Whether Writer has reported the request fully committed locally.
    sent: bool,
}

/// Protocol purpose of one Core-produced write awaiting its unique terminal outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingWriteKind {
    /// A Select or Linktest request whose `Committed` outcome starts no B1 timer.
    TransactionRequest,
    /// A response or Reject with no directly associated application command.
    Response,
    /// A local Separate request whose terminal outcome satisfies its close barrier.
    Separate,
}

/// Correlation retained for one Core-produced frame until outcome or admission failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingWrite {
    /// Protocol purpose controlling terminal-outcome completion semantics.
    kind: PendingWriteKind,
    /// Still-open command completed by this write, if any.
    command_id: Option<CommandId>,
}

/// Stateful, runtime-neutral HSMS control protocol object for one TCP generation.
#[derive(Debug)]
pub(crate) struct SessionCore {
    /// Immutable endpoint values needed by this generation.
    config: SessionCoreConfig,
    /// Current selection state, or `None` before the first connected fact.
    state: Option<SessionState>,
    /// Most recent monotonic logical time accepted from the Driver.
    last_now: Option<MonoTime>,
    /// Next WriteId raw value, or `None` after the non-wrapping space is exhausted.
    next_write_id: Option<u64>,
    /// Next System Bytes raw value, or `None` after the non-wrapping space is exhausted.
    next_system_bytes: Option<u32>,
    /// Sole locally initiated Select or Linktest transaction.
    active_transaction: Option<ControlTransaction>,
    /// Core-produced writes retained until admission failure or one terminal outcome.
    pending_writes: HashMap<WriteId, PendingWrite>,
    /// Accepted commands that have not yet emitted their unique completion action.
    open_commands: BTreeSet<CommandId>,
    /// Whether protocol work must be isolated while the generation closes.
    closing: bool,
    /// Whether Core has already returned a close action on a successfully applied path.
    close_action_issued: bool,
}

impl SessionCore {
    /// Creates a disconnected Core with empty transactions and fresh identifier spaces.
    pub(crate) fn new(config: SessionCoreConfig) -> Self {
        Self {
            config,
            state: None,
            last_now: None,
            next_write_id: Some(0),
            next_system_bytes: Some(0),
            active_transaction: None,
            pending_writes: HashMap::new(),
            open_commands: BTreeSet::new(),
            closing: false,
            close_action_issued: false,
        }
    }

    /// Returns the committed selection state, or `None` before connection.
    pub(crate) const fn state(&self) -> Option<SessionState> {
        self.state
    }

    /// Commits the first connected fact and publishes the initial NotSelected state.
    ///
    /// `now` is generation-local monotonic time. A repeated connection fact or
    /// a backwards time input fails the generation closed as a runtime invariant.
    pub(crate) fn on_connected(&mut self, now: MonoTime) -> CoreActions {
        let mut actions = CoreActions::new();
        if !self.accept_time(now, &mut actions) {
            return actions;
        }
        if self.state.is_some() || self.closing {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        }

        self.state = Some(SessionState::NotSelected);
        actions.push(CoreAction::SessionStateChanged(SessionState::NotSelected));
        actions
    }

    /// Applies one accepted B1 control command at logical time `now`.
    ///
    /// `command` carries the Driver identity and typed Select, Linktest, or
    /// Separate intent. The return value atomically describes all ordered
    /// frame, state, completion, and close actions for this input.
    pub(crate) fn on_command(&mut self, command: CoreCommand, now: MonoTime) -> CoreActions {
        let mut actions = CoreActions::new();
        let command_id = command.command_id();
        if !self.open_commands.insert(command_id) {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        }
        if !self.accept_time(now, &mut actions) {
            return actions;
        }
        if self.state.is_none() {
            self.complete_error(command_id, OperationError::NotConnected, &mut actions);
            return actions;
        }
        if self.closing {
            self.complete_error(command_id, OperationError::ConnectionLost, &mut actions);
            return actions;
        }

        match command.kind() {
            CoreCommandKind::Select => self.start_select(command_id, &mut actions),
            CoreCommandKind::Linktest => self.start_linktest(command_id, &mut actions),
            CoreCommandKind::Separate => self.start_separate(command_id, &mut actions),
        }
        actions
    }

    /// Applies one structurally validated semantic peer message at logical time `now`.
    ///
    /// B1 deliberately isolates pre-connected messages, Data, and Deselect as
    /// empty turns. Those inputs belong to later slices and are not assigned
    /// accidental protocol semantics here.
    pub(crate) fn on_message(&mut self, message: ProtocolMessage, now: MonoTime) -> CoreActions {
        let mut actions = CoreActions::new();
        if !self.accept_time(now, &mut actions) {
            return actions;
        }
        if self.state.is_none() || self.closing {
            return actions;
        }

        match message {
            ProtocolMessage::Control(ControlMessage::SelectRequest {
                session_id,
                system_bytes,
            }) => self.receive_select_request(session_id, system_bytes, &mut actions),
            ProtocolMessage::Control(ControlMessage::SelectResponse {
                session_id,
                status,
                system_bytes,
            }) => self.receive_select_response(session_id, status, system_bytes, &mut actions),
            ProtocolMessage::Control(ControlMessage::LinktestRequest { system_bytes }) => {
                self.send_response(
                    ControlMessage::LinktestResponse { system_bytes },
                    &mut actions,
                );
            }
            ProtocolMessage::Control(ControlMessage::LinktestResponse { system_bytes }) => {
                self.receive_linktest_response(system_bytes, &mut actions);
            }
            ProtocolMessage::Control(ControlMessage::RejectRequest {
                session_id,
                header_byte_2,
                reason,
                system_bytes,
            }) => self.receive_reject(
                session_id,
                header_byte_2,
                reason,
                system_bytes,
                &mut actions,
            ),
            ProtocolMessage::Control(ControlMessage::SeparateRequest {
                session_id: _,
                system_bytes: _,
            }) => self.receive_separate(&mut actions),
            // Data and Deselect are explicitly outside B1. Returning no action
            // keeps this slice isolated without inventing later protocol policy.
            ProtocolMessage::Data(_)
            | ProtocolMessage::Control(ControlMessage::DeselectRequest { .. })
            | ProtocolMessage::Control(ControlMessage::DeselectResponse { .. }) => {}
        }
        actions
    }

    /// Applies the unique terminal outcome for one successfully admitted write.
    ///
    /// `write_id` identifies the Core-produced frame and `outcome` distinguishes
    /// committed, provably unwritten, and indeterminate delivery. Unknown or
    /// duplicate identities fail closed as runtime invariants.
    pub(crate) fn on_write_outcome(
        &mut self,
        write_id: WriteId,
        outcome: WriteOutcome,
        now: MonoTime,
    ) -> CoreActions {
        let mut actions = CoreActions::new();
        if !self.accept_time(now, &mut actions) {
            return actions;
        }
        let Some(write) = self.pending_writes.remove(&write_id) else {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        };

        match outcome {
            WriteOutcome::Committed => match write.kind {
                PendingWriteKind::TransactionRequest => {
                    if let Some(transaction) = self.active_transaction.as_mut() {
                        if transaction.write_id == write_id {
                            transaction.sent = true;
                        }
                    }
                }
                PendingWriteKind::Separate => {
                    if let Some(command_id) = write.command_id {
                        self.complete_ok(command_id, &mut actions);
                    }
                }
                PendingWriteKind::Response => {}
            },
            WriteOutcome::NotWritten(_) => self.finish_failed_write(
                write_id,
                write,
                OperationError::ConnectionLost,
                &mut actions,
            ),
            WriteOutcome::Indeterminate(_) => self.finish_failed_write(
                write_id,
                write,
                OperationError::DeliveryIndeterminate,
                &mut actions,
            ),
        }
        actions
    }

    /// Terminates protocol work because Driver observed shutdown or admission failure.
    ///
    /// `reason` is the stable generation-close classification. `failed_write_id`
    /// is `Some` only when synchronous Writer admission rejected that exact
    /// Core-produced frame; unknown identities are runtime invariants.
    pub(crate) fn on_shutdown(
        &mut self,
        reason: GenerationCloseReason,
        failed_write_id: Option<WriteId>,
        now: MonoTime,
    ) -> CoreActions {
        let mut actions = CoreActions::new();
        if !self.accept_time(now, &mut actions) {
            return actions;
        }

        if let Some(write_id) = failed_write_id {
            let Some(write) = self.pending_writes.remove(&write_id) else {
                self.fail_runtime_invariant(&mut actions);
                return actions;
            };
            if self
                .active_transaction
                .is_some_and(|transaction| transaction.write_id == write_id)
            {
                self.active_transaction = None;
            }
            if let Some(command_id) = write.command_id {
                let error = if reason == GenerationCloseReason::ControlBackpressure {
                    OperationError::Backpressure
                } else {
                    OperationError::ConnectionLost
                };
                self.complete_error(command_id, error, &mut actions);
            }
            self.complete_all(OperationError::ConnectionLost, &mut actions);
            self.closing = true;
            self.close_action_issued = true;
            // Admission failure short-circuits the original action batch. A
            // Separate batch may therefore have contained a later close action
            // that Driver never applied, so this shutdown turn always emits one.
            actions.push(CoreAction::CloseGeneration {
                reason,
                barrier: CloseBarrier::Immediate,
            });
            return actions;
        }

        self.complete_all(OperationError::ConnectionLost, &mut actions);
        self.request_close(reason, CloseBarrier::Immediate, &mut actions);
        actions
    }

    /// Advances logical time without implementing deferred T6 or T7 behavior.
    ///
    /// The return value is empty for equal or increasing time and fails closed
    /// for a backwards input. B1 intentionally schedules no protocol deadline.
    pub(crate) fn advance_time(&mut self, now: MonoTime) -> CoreActions {
        let mut actions = CoreActions::new();
        self.accept_time(now, &mut actions);
        actions
    }

    /// Returns the earliest logical protocol deadline, which is absent in B1.
    pub(crate) const fn next_deadline(&self) -> Option<MonoTime> {
        None
    }

    /// Starts a local Select request or completes the command with a state error.
    fn start_select(&mut self, command_id: CommandId, actions: &mut CoreActions) {
        match self.state {
            Some(SessionState::Selected) => {
                self.complete_error(command_id, OperationError::AlreadySelected, actions);
            }
            Some(SessionState::NotSelected) => {
                if self.active_transaction.is_some() {
                    self.complete_error(command_id, OperationError::ControlBusy, actions);
                } else {
                    self.start_transaction(TransactionKind::Select, command_id, actions);
                }
            }
            Some(SessionState::Closing | SessionState::Closed) | None => {
                self.complete_error(command_id, OperationError::ConnectionLost, actions);
            }
        }
    }

    /// Starts a local Linktest request in either connected selection state.
    fn start_linktest(&mut self, command_id: CommandId, actions: &mut CoreActions) {
        match self.state {
            Some(SessionState::NotSelected | SessionState::Selected) => {
                if self.active_transaction.is_some() {
                    self.complete_error(command_id, OperationError::ControlBusy, actions);
                } else {
                    self.start_transaction(TransactionKind::Linktest, command_id, actions);
                }
            }
            Some(SessionState::Closing | SessionState::Closed) | None => {
                self.complete_error(command_id, OperationError::ConnectionLost, actions);
            }
        }
    }

    /// Starts local Separate, preempting any live control transaction in order.
    fn start_separate(&mut self, command_id: CommandId, actions: &mut CoreActions) {
        if self.state != Some(SessionState::Selected) {
            self.complete_error(command_id, OperationError::NotSelected, actions);
            return;
        }
        let Some((system_bytes, write_id)) = self.allocate_request_identifiers() else {
            self.fail_runtime_invariant(actions);
            return;
        };

        self.state = Some(SessionState::NotSelected);
        self.closing = true;
        if let Some(transaction) = self.active_transaction.take() {
            self.complete_error(
                transaction.command_id,
                OperationError::Protocol(ProtocolError::TransactionAborted),
                actions,
            );
        }
        self.pending_writes.insert(
            write_id,
            PendingWrite {
                kind: PendingWriteKind::Separate,
                command_id: Some(command_id),
            },
        );
        actions.push(CoreAction::SendFrame {
            write_id,
            message: ProtocolMessage::Control(ControlMessage::SeparateRequest {
                session_id: CONTROL_SESSION_ID,
                system_bytes,
            }),
        });
        actions.push(CoreAction::SessionStateChanged(SessionState::NotSelected));
        self.request_close(
            GenerationCloseReason::LocalSeparate,
            CloseBarrier::AfterWrite(write_id),
            actions,
        );
    }

    /// Creates one active Select or Linktest response transaction and request write.
    fn start_transaction(
        &mut self,
        kind: TransactionKind,
        command_id: CommandId,
        actions: &mut CoreActions,
    ) {
        let Some((system_bytes, write_id)) = self.allocate_request_identifiers() else {
            self.fail_runtime_invariant(actions);
            return;
        };
        let message = match kind {
            TransactionKind::Select => ControlMessage::SelectRequest {
                session_id: CONTROL_SESSION_ID,
                system_bytes,
            },
            TransactionKind::Linktest => ControlMessage::LinktestRequest { system_bytes },
        };
        self.active_transaction = Some(ControlTransaction {
            kind,
            command_id,
            session_id: CONTROL_SESSION_ID,
            system_bytes,
            write_id,
            sent: false,
        });
        self.pending_writes.insert(
            write_id,
            PendingWrite {
                kind: PendingWriteKind::TransactionRequest,
                command_id: Some(command_id),
            },
        );
        actions.push(CoreAction::SendFrame {
            write_id,
            message: ProtocolMessage::Control(message),
        });
    }

    /// Handles an inbound Select request with SessionID-first readiness rules.
    fn receive_select_request(
        &mut self,
        session_id: u16,
        system_bytes: SystemBytes,
        actions: &mut CoreActions,
    ) {
        let status = if session_id != CONTROL_SESSION_ID {
            SelectStatus::NOT_READY
        } else if self.state == Some(SessionState::Selected) {
            SelectStatus::ALREADY_ACTIVE
        } else {
            self.state = Some(SessionState::Selected);
            SelectStatus::SUCCESS
        };
        self.send_response(
            ControlMessage::SelectResponse {
                session_id,
                status,
                system_bytes,
            },
            actions,
        );
        if status.is_success() && !self.closing {
            actions.push(CoreAction::SessionStateChanged(SessionState::Selected));
        }
    }

    /// Matches an inbound Select response or emits transaction-not-open Reject.
    fn receive_select_response(
        &mut self,
        session_id: u16,
        status: SelectStatus,
        system_bytes: SystemBytes,
        actions: &mut CoreActions,
    ) {
        let Some(transaction) = self.active_transaction else {
            self.send_transaction_not_open_reject(
                session_id,
                SELECT_RESPONSE_STYPE,
                system_bytes,
                actions,
            );
            return;
        };
        if transaction.kind != TransactionKind::Select
            || transaction.session_id != session_id
            || transaction.system_bytes != system_bytes
        {
            self.send_transaction_not_open_reject(
                session_id,
                SELECT_RESPONSE_STYPE,
                system_bytes,
                actions,
            );
            return;
        }

        self.active_transaction = None;
        if status.is_success() {
            if self.state != Some(SessionState::Selected) {
                self.state = Some(SessionState::Selected);
                actions.push(CoreAction::SessionStateChanged(SessionState::Selected));
            }
            self.complete_ok(transaction.command_id, actions);
        } else {
            let status = NonZeroU8::new(status.get())
                .expect("a non-success Select status is necessarily non-zero");
            self.complete_error(
                transaction.command_id,
                OperationError::SelectRejected { status },
                actions,
            );
        }
    }

    /// Matches an inbound Linktest response or emits transaction-not-open Reject.
    fn receive_linktest_response(&mut self, system_bytes: SystemBytes, actions: &mut CoreActions) {
        let Some(transaction) = self.active_transaction else {
            self.send_transaction_not_open_reject(
                CONTROL_SESSION_ID,
                LINKTEST_RESPONSE_STYPE,
                system_bytes,
                actions,
            );
            return;
        };
        if transaction.kind != TransactionKind::Linktest
            || transaction.session_id != CONTROL_SESSION_ID
            || transaction.system_bytes != system_bytes
        {
            self.send_transaction_not_open_reject(
                CONTROL_SESSION_ID,
                LINKTEST_RESPONSE_STYPE,
                system_bytes,
                actions,
            );
            return;
        }

        self.active_transaction = None;
        self.complete_ok(transaction.command_id, actions);
    }

    /// Attributes a peer Reject to the exact live Select or Linktest tuple.
    fn receive_reject(
        &mut self,
        session_id: u16,
        header_byte_2: u8,
        reason: RejectReason,
        system_bytes: SystemBytes,
        actions: &mut CoreActions,
    ) {
        let Some(transaction) = self.active_transaction else {
            return;
        };
        let header_matches = if reason == RejectReason::UNSUPPORTED_PTYPE {
            header_byte_2 == 0
        } else if matches!(reason.get(), 1 | 3 | 4) {
            header_byte_2 == transaction.kind.request_stype()
        } else {
            false
        };
        if !header_matches
            || transaction.session_id != session_id
            || transaction.system_bytes != system_bytes
        {
            return;
        }

        self.active_transaction = None;
        self.complete_error(
            transaction.command_id,
            OperationError::PeerRejected { reason },
            actions,
        );
    }

    /// Applies peer Separate in Selected and ignores it in NotSelected.
    fn receive_separate(&mut self, actions: &mut CoreActions) {
        if self.state != Some(SessionState::Selected) {
            return;
        }
        self.state = Some(SessionState::NotSelected);
        if let Some(transaction) = self.active_transaction.take() {
            self.complete_error(
                transaction.command_id,
                OperationError::Protocol(ProtocolError::TransactionAborted),
                actions,
            );
        }
        actions.push(CoreAction::SessionStateChanged(SessionState::NotSelected));
        self.request_close(
            GenerationCloseReason::SeparateReceived,
            CloseBarrier::Immediate,
            actions,
        );
    }

    /// Creates one response write with no directly associated application command.
    fn send_response(&mut self, message: ControlMessage, actions: &mut CoreActions) {
        let Some(write_id) = self.allocate_write_id() else {
            self.fail_runtime_invariant(actions);
            return;
        };
        self.pending_writes.insert(
            write_id,
            PendingWrite {
                kind: PendingWriteKind::Response,
                command_id: None,
            },
        );
        actions.push(CoreAction::SendFrame {
            write_id,
            message: ProtocolMessage::Control(message),
        });
    }

    /// Creates the exact transaction-not-open Reject for one unmatched response.
    fn send_transaction_not_open_reject(
        &mut self,
        session_id: u16,
        rejected_stype: u8,
        system_bytes: SystemBytes,
        actions: &mut CoreActions,
    ) {
        self.send_response(
            ControlMessage::RejectRequest {
                session_id,
                header_byte_2: rejected_stype,
                reason: RejectReason::TRANSACTION_NOT_OPEN,
                system_bytes,
            },
            actions,
        );
    }

    /// Completes one failed write and closes without repeating prior completion.
    fn finish_failed_write(
        &mut self,
        write_id: WriteId,
        write: PendingWrite,
        operation_error: OperationError,
        actions: &mut CoreActions,
    ) {
        if self
            .active_transaction
            .is_some_and(|transaction| transaction.write_id == write_id)
        {
            self.active_transaction = None;
        }
        if let Some(command_id) = write.command_id {
            self.complete_error(command_id, operation_error, actions);
        }
        self.complete_all(OperationError::ConnectionLost, actions);
        self.request_close(
            GenerationCloseReason::TransportLost,
            CloseBarrier::Immediate,
            actions,
        );
    }

    /// Allocates the System Bytes and WriteId for one locally generated request.
    fn allocate_request_identifiers(&mut self) -> Option<(SystemBytes, WriteId)> {
        let system_bytes = self.allocate_system_bytes()?;
        let write_id = self.allocate_write_id()?;
        Some((system_bytes, write_id))
    }

    /// Allocates one non-wrapping generation-local WriteId.
    fn allocate_write_id(&mut self) -> Option<WriteId> {
        let value = self.next_write_id?;
        self.next_write_id = value.checked_add(1);
        Some(WriteId::new(value))
    }

    /// Allocates one non-wrapping generation-local System Bytes value.
    fn allocate_system_bytes(&mut self) -> Option<SystemBytes> {
        let value = self.next_system_bytes?;
        self.next_system_bytes = value.checked_add(1);
        Some(SystemBytes::new(value))
    }

    /// Accepts equal or increasing logical time and fails closed on regression.
    fn accept_time(&mut self, now: MonoTime, actions: &mut CoreActions) -> bool {
        if self.last_now.is_some_and(|last_now| now < last_now) {
            self.fail_runtime_invariant(actions);
            return false;
        }
        self.last_now = Some(now);
        true
    }

    /// Completes one command successfully if it remains open.
    fn complete_ok(&mut self, command_id: CommandId, actions: &mut CoreActions) {
        self.complete_command(command_id, Ok(()), actions);
    }

    /// Completes one command with `error` if it remains open.
    fn complete_error(
        &mut self,
        command_id: CommandId,
        error: OperationError,
        actions: &mut CoreActions,
    ) {
        self.complete_command(command_id, Err(error), actions);
    }

    /// Emits one unique completion and detaches its command from retained writes.
    fn complete_command(
        &mut self,
        command_id: CommandId,
        result: Result<(), OperationError>,
        actions: &mut CoreActions,
    ) {
        if !self.open_commands.remove(&command_id) {
            self.fail_runtime_invariant(actions);
            return;
        }
        for write in self.pending_writes.values_mut() {
            if write.command_id == Some(command_id) {
                write.command_id = None;
            }
        }
        actions.push(CoreAction::CompleteCommand {
            command_id,
            result: CoreCommandResult::Control(result),
        });
    }

    /// Completes every still-open command deterministically in CommandId order.
    fn complete_all(&mut self, error: OperationError, actions: &mut CoreActions) {
        self.active_transaction = None;
        let command_ids: Vec<_> = self.open_commands.iter().copied().collect();
        for command_id in command_ids {
            self.complete_error(command_id, error.clone(), actions);
        }
    }

    /// Records closing and emits the first close action returned by normal Core flow.
    fn request_close(
        &mut self,
        reason: GenerationCloseReason,
        barrier: CloseBarrier,
        actions: &mut CoreActions,
    ) {
        self.closing = true;
        if self.close_action_issued {
            return;
        }
        self.close_action_issued = true;
        actions.push(CoreAction::CloseGeneration { reason, barrier });
    }

    /// Fails all undecided commands and closes for a local runtime invariant.
    fn fail_runtime_invariant(&mut self, actions: &mut CoreActions) {
        self.complete_all(OperationError::ConnectionLost, actions);
        self.request_close(
            GenerationCloseReason::RuntimeInvariant,
            CloseBarrier::Immediate,
            actions,
        );
    }
}

#[cfg(test)]
mod tests {
    //! Direct state-transition tests for the B1 runtime-neutral Session Core.

    use std::{num::NonZeroU8, time::Duration};

    use crate::hsms::{
        error::{OperationError, ProtocolError},
        lifecycle::SessionState,
        model::{
            ids::{CommandId, SessionId, SystemBytes, WriteId},
            runtime::{
                CloseBarrier, GenerationCloseReason, MonoTime, TransportFault, TransportFaultKind,
                WriteOutcome,
            },
        },
        protocol::{
            header::{ControlMessage, RejectReason, SelectStatus},
            message::ProtocolMessage,
        },
    };

    use super::{
        CoreAction, CoreActions, CoreCommand, CoreCommandKind, CoreCommandResult, SessionCore,
        SessionCoreConfig, CONTROL_SESSION_ID, LINKTEST_REQUEST_STYPE, LINKTEST_RESPONSE_STYPE,
        SELECT_REQUEST_STYPE, SELECT_RESPONSE_STYPE,
    };

    /// Creates logical time at `millis` after the generation-local epoch.
    fn now(millis: u64) -> MonoTime {
        MonoTime::from_elapsed(Duration::from_millis(millis))
    }

    /// Creates a disconnected Session Core with a valid future Data Session ID.
    fn core() -> SessionCore {
        let session_id = SessionId::new(7).expect("test Data Session ID is valid");
        SessionCore::new(SessionCoreConfig::new(session_id))
    }

    /// Connects `core` at time zero and asserts the unique initial state action.
    fn connect(core: &mut SessionCore) {
        assert_eq!(
            core.on_connected(now(0)).into_actions(),
            vec![CoreAction::SessionStateChanged(SessionState::NotSelected)]
        );
    }

    /// Builds one accepted Core command with a deterministic Driver identity.
    fn command(id: u64, kind: CoreCommandKind) -> CoreCommand {
        CoreCommand::new(CommandId::new(id), kind)
    }

    /// Returns the unique sent control frame from `actions`.
    fn sent_control(actions: &[CoreAction]) -> (WriteId, ControlMessage) {
        let sends: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                CoreAction::SendFrame {
                    write_id,
                    message: ProtocolMessage::Control(message),
                } => Some((*write_id, *message)),
                _ => None,
            })
            .collect();
        assert_eq!(sends.len(), 1, "expected exactly one sent control frame");
        sends[0]
    }

    /// Builds a stable transport fault for unsuccessful write-outcome tests.
    fn fault() -> TransportFault {
        TransportFault::new(TransportFaultKind::BrokenPipe)
    }

    /// Starts an active Select and returns its request correlation values.
    fn start_select(core: &mut SessionCore, command_id: u64) -> (WriteId, SystemBytes) {
        let actions = core
            .on_command(command(command_id, CoreCommandKind::Select), now(1))
            .into_actions();
        let (write_id, message) = sent_control(&actions);
        let ControlMessage::SelectRequest {
            session_id,
            system_bytes,
        } = message
        else {
            panic!("active Select must send Select.req");
        };
        assert_eq!(session_id, CONTROL_SESSION_ID);
        (write_id, system_bytes)
    }

    /// Makes `core` Selected through a successful passive Select request.
    fn select_passively(core: &mut SessionCore) -> WriteId {
        select_passively_at(core, 1)
    }

    /// Makes `core` Selected through passive Select at logical time `millis`.
    fn select_passively_at(core: &mut SessionCore, millis: u64) -> WriteId {
        let actions = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectRequest {
                    session_id: CONTROL_SESSION_ID,
                    system_bytes: SystemBytes::new(91),
                }),
                now(millis),
            )
            .into_actions();
        let (write_id, _) = sent_control(&actions);
        assert_eq!(
            actions[1],
            CoreAction::SessionStateChanged(SessionState::Selected)
        );
        write_id
    }

    /// Confirms configuration is retained and connection is explicit and unique.
    #[test]
    fn connected_publishes_once_and_repeated_connected_fails_closed() {
        let mut core = core();

        assert_eq!(core.state(), None);
        assert_eq!(core.config.session_id(), SessionId::new(7).unwrap());
        connect(&mut core);
        assert_eq!(core.state(), Some(SessionState::NotSelected));
        assert_eq!(
            core.on_connected(now(0)).into_actions(),
            vec![CoreAction::CloseGeneration {
                reason: GenerationCloseReason::RuntimeInvariant,
                barrier: CloseBarrier::Immediate,
            }]
        );
    }

    /// Confirms pre-connected and explicitly deferred B1 messages have no semantics.
    #[test]
    fn out_of_slice_messages_are_safely_isolated() {
        let mut core = core();

        assert!(core
            .on_message(
                ProtocolMessage::Control(ControlMessage::LinktestRequest {
                    system_bytes: SystemBytes::new(1),
                }),
                now(0),
            )
            .into_actions()
            .is_empty());
        assert_eq!(
            core.on_command(command(1, CoreCommandKind::Select), now(0))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(1),
                result: CoreCommandResult::Control(Err(OperationError::NotConnected)),
            }]
        );
        connect(&mut core);
        assert!(core
            .on_message(
                ProtocolMessage::Control(ControlMessage::DeselectRequest {
                    session_id: CONTROL_SESSION_ID,
                    system_bytes: SystemBytes::new(2),
                }),
                now(0),
            )
            .into_actions()
            .is_empty());
    }

    /// Confirms equal time is legal, deadlines are absent, and regression fails closed.
    #[test]
    fn logical_time_is_monotonic_without_b1_deadlines() {
        let mut core = core();
        connect(&mut core);

        assert_eq!(core.next_deadline(), None);
        assert!(core.advance_time(now(0)).into_actions().is_empty());
        assert!(core.advance_time(now(5)).into_actions().is_empty());
        assert_eq!(
            core.advance_time(now(4)).into_actions(),
            vec![CoreAction::CloseGeneration {
                reason: GenerationCloseReason::RuntimeInvariant,
                barrier: CloseBarrier::Immediate,
            }]
        );
    }

    /// Confirms a backwards command turn still completes that accepted command once.
    #[test]
    fn backwards_command_time_completes_the_current_command_before_close() {
        let mut core = core();
        connect(&mut core);
        assert!(core.advance_time(now(5)).into_actions().is_empty());

        assert_eq!(
            core.on_command(command(9, CoreCommandKind::Select), now(4))
                .into_actions(),
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(9),
                    result: CoreCommandResult::Control(Err(OperationError::ConnectionLost)),
                },
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::RuntimeInvariant,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
    }

    /// Confirms active Select matches exactly and publishes state before completion.
    #[test]
    fn active_select_success_orders_state_before_completion() {
        let mut core = core();
        connect(&mut core);
        let (write_id, system_bytes) = start_select(&mut core, 10);

        assert!(core
            .on_write_outcome(write_id, WriteOutcome::Committed, now(2))
            .into_actions()
            .is_empty());
        assert!(core.active_transaction.unwrap().sent);
        assert_eq!(
            core.on_message(
                ProtocolMessage::Control(ControlMessage::SelectResponse {
                    session_id: CONTROL_SESSION_ID,
                    status: SelectStatus::SUCCESS,
                    system_bytes,
                }),
                now(3),
            )
            .into_actions(),
            vec![
                CoreAction::SessionStateChanged(SessionState::Selected),
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(10),
                    result: CoreCommandResult::Control(Ok(())),
                },
            ]
        );
        assert_eq!(core.state(), Some(SessionState::Selected));
    }

    /// Confirms non-zero Select status is preserved and does not change state.
    #[test]
    fn active_select_rejection_preserves_raw_status() {
        let mut core = core();
        connect(&mut core);
        let (_, system_bytes) = start_select(&mut core, 11);

        assert_eq!(
            core.on_message(
                ProtocolMessage::Control(ControlMessage::SelectResponse {
                    session_id: CONTROL_SESSION_ID,
                    status: SelectStatus::new(0x80),
                    system_bytes,
                }),
                now(2),
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(11),
                result: CoreCommandResult::Control(Err(OperationError::SelectRejected {
                    status: NonZeroU8::new(0x80).unwrap(),
                })),
            }]
        );
        assert_eq!(core.state(), Some(SessionState::NotSelected));
    }

    /// Confirms active Select enforces the selected state and unique control slot.
    #[test]
    fn active_select_reports_already_selected_and_control_busy() {
        let mut core = core();
        connect(&mut core);
        let _ = start_select(&mut core, 20);

        assert_eq!(
            core.on_command(command(21, CoreCommandKind::Select), now(2))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(21),
                result: CoreCommandResult::Control(Err(OperationError::ControlBusy)),
            }]
        );
        let passive_write = select_passively_at(&mut core, 3);
        assert!(core
            .on_write_outcome(passive_write, WriteOutcome::Committed, now(4))
            .into_actions()
            .is_empty());
        assert_eq!(
            core.on_command(command(22, CoreCommandKind::Select), now(5))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(22),
                result: CoreCommandResult::Control(Err(OperationError::AlreadySelected)),
            }]
        );
    }

    /// Confirms an unmatched Select response is rejected without consuming the live tuple.
    #[test]
    fn unmatched_select_response_emits_reject_and_preserves_transaction() {
        let mut core = core();
        connect(&mut core);
        let (_, system_bytes) = start_select(&mut core, 30);

        let unmatched = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectResponse {
                    session_id: 9,
                    status: SelectStatus::SUCCESS,
                    system_bytes,
                }),
                now(2),
            )
            .into_actions();
        assert!(matches!(
            sent_control(&unmatched).1,
            ControlMessage::RejectRequest {
                session_id: 9,
                header_byte_2: SELECT_RESPONSE_STYPE,
                reason: RejectReason::TRANSACTION_NOT_OPEN,
                system_bytes: copied,
            } if copied == system_bytes
        ));
        assert_eq!(
            core.on_message(
                ProtocolMessage::Control(ControlMessage::SelectResponse {
                    session_id: CONTROL_SESSION_ID,
                    status: SelectStatus::SUCCESS,
                    system_bytes,
                }),
                now(3),
            )
            .into_actions()[1],
            CoreAction::CompleteCommand {
                command_id: CommandId::new(30),
                result: CoreCommandResult::Control(Ok(())),
            }
        );
    }

    /// Confirms passive Select applies SessionID-first status and strict action order.
    #[test]
    fn passive_select_copies_tuple_and_orders_response_before_state() {
        let mut core = core();
        connect(&mut core);

        let invalid = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectRequest {
                    session_id: 5,
                    system_bytes: SystemBytes::new(44),
                }),
                now(1),
            )
            .into_actions();
        assert!(matches!(
            sent_control(&invalid).1,
            ControlMessage::SelectResponse {
                session_id: 5,
                status: SelectStatus::NOT_READY,
                system_bytes,
            } if system_bytes == SystemBytes::new(44)
        ));
        assert_eq!(core.state(), Some(SessionState::NotSelected));

        let success = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectRequest {
                    session_id: CONTROL_SESSION_ID,
                    system_bytes: SystemBytes::new(45),
                }),
                now(2),
            )
            .into_actions();
        assert!(matches!(
            success.as_slice(),
            [
                CoreAction::SendFrame {
                    message: ProtocolMessage::Control(ControlMessage::SelectResponse {
                        session_id: CONTROL_SESSION_ID,
                        status: SelectStatus::SUCCESS,
                        system_bytes,
                    }),
                    ..
                },
                CoreAction::SessionStateChanged(SessionState::Selected),
            ] if *system_bytes == SystemBytes::new(45)
        ));

        let already = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectRequest {
                    session_id: CONTROL_SESSION_ID,
                    system_bytes: SystemBytes::new(46),
                }),
                now(3),
            )
            .into_actions();
        assert!(matches!(
            sent_control(&already).1,
            ControlMessage::SelectResponse {
                status: SelectStatus::ALREADY_ACTIVE,
                ..
            }
        ));
        assert_eq!(already.len(), 1);
    }

    /// Confirms an invalid Control Session ID takes priority over Selected state.
    #[test]
    fn passive_select_prioritizes_invalid_session_id_while_selected() {
        let mut core = core();
        connect(&mut core);
        let _ = select_passively(&mut core);

        let actions = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectRequest {
                    session_id: 9,
                    system_bytes: SystemBytes::new(47),
                }),
                now(2),
            )
            .into_actions();

        assert!(matches!(
            sent_control(&actions).1,
            ControlMessage::SelectResponse {
                session_id: 9,
                status: SelectStatus::NOT_READY,
                system_bytes,
            } if system_bytes == SystemBytes::new(47)
        ));
        assert_eq!(actions.len(), 1);
        assert_eq!(core.state(), Some(SessionState::Selected));
    }

    /// Confirms simultaneous passive Select retains and later completes active Select once.
    #[test]
    fn simultaneous_select_retains_local_transaction_and_publishes_selected_once() {
        let mut core = core();
        connect(&mut core);
        let (_, local_system_bytes) = start_select(&mut core, 40);

        let passive = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectRequest {
                    session_id: CONTROL_SESSION_ID,
                    system_bytes: SystemBytes::new(100),
                }),
                now(2),
            )
            .into_actions();
        assert_eq!(
            passive[1],
            CoreAction::SessionStateChanged(SessionState::Selected)
        );
        assert_eq!(
            core.on_message(
                ProtocolMessage::Control(ControlMessage::SelectResponse {
                    session_id: CONTROL_SESSION_ID,
                    status: SelectStatus::SUCCESS,
                    system_bytes: local_system_bytes,
                }),
                now(3),
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(40),
                result: CoreCommandResult::Control(Ok(())),
            }]
        );
    }

    /// Confirms a passive response admission failure attributes no Backpressure to another command.
    #[test]
    fn passive_select_admission_failure_closes_other_transaction_as_connection_lost() {
        let mut core = core();
        connect(&mut core);
        let _ = start_select(&mut core, 50);
        let passive = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectRequest {
                    session_id: CONTROL_SESSION_ID,
                    system_bytes: SystemBytes::new(101),
                }),
                now(2),
            )
            .into_actions();
        let (failed_write_id, _) = sent_control(&passive);

        assert_eq!(
            core.on_shutdown(
                GenerationCloseReason::ControlBackpressure,
                Some(failed_write_id),
                now(3),
            )
            .into_actions(),
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(50),
                    result: CoreCommandResult::Control(Err(OperationError::ConnectionLost)),
                },
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::ControlBackpressure,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
    }

    /// Confirms Linktest is legal in both states and matches only its exact response.
    #[test]
    fn linktest_works_in_both_states_and_unmatched_response_is_rejected() {
        for selected in [false, true] {
            let mut core = core();
            connect(&mut core);
            if selected {
                let _ = select_passively(&mut core);
            }
            let actions = core
                .on_command(command(60, CoreCommandKind::Linktest), now(2))
                .into_actions();
            let (_, request) = sent_control(&actions);
            let ControlMessage::LinktestRequest { system_bytes } = request else {
                panic!("Linktest command must send Linktest.req");
            };

            let unmatched = core
                .on_message(
                    ProtocolMessage::Control(ControlMessage::LinktestResponse {
                        system_bytes: SystemBytes::new(system_bytes.get() + 1),
                    }),
                    now(3),
                )
                .into_actions();
            assert!(matches!(
                sent_control(&unmatched).1,
                ControlMessage::RejectRequest {
                    session_id: CONTROL_SESSION_ID,
                    header_byte_2: LINKTEST_RESPONSE_STYPE,
                    reason: RejectReason::TRANSACTION_NOT_OPEN,
                    ..
                }
            ));
            assert_eq!(
                core.on_message(
                    ProtocolMessage::Control(ControlMessage::LinktestResponse { system_bytes }),
                    now(4),
                )
                .into_actions(),
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(60),
                    result: CoreCommandResult::Control(Ok(())),
                }]
            );
        }
    }

    /// Confirms Linktest respects the unique control slot and a fast response
    /// completes once even when its retained request later reports a fault.
    #[test]
    fn linktest_control_busy_and_fast_response_are_exactly_once() {
        let mut busy = core();
        connect(&mut busy);
        let _ = start_select(&mut busy, 61);
        assert_eq!(
            busy.on_command(command(62, CoreCommandKind::Linktest), now(2))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(62),
                result: CoreCommandResult::Control(Err(OperationError::ControlBusy)),
            }]
        );

        let mut fast = core();
        connect(&mut fast);
        let request = fast
            .on_command(command(63, CoreCommandKind::Linktest), now(1))
            .into_actions();
        let (write_id, message) = sent_control(&request);
        let ControlMessage::LinktestRequest { system_bytes } = message else {
            panic!("Linktest command must send Linktest.req");
        };
        assert_eq!(
            fast.on_message(
                ProtocolMessage::Control(ControlMessage::LinktestResponse { system_bytes }),
                now(2),
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(63),
                result: CoreCommandResult::Control(Ok(())),
            }]
        );
        assert_eq!(
            fast.on_write_outcome(write_id, WriteOutcome::NotWritten(fault()), now(3))
                .into_actions(),
            vec![CoreAction::CloseGeneration {
                reason: GenerationCloseReason::TransportLost,
                barrier: CloseBarrier::Immediate,
            }]
        );
    }

    /// Confirms peer Linktest requests receive a response despite a local busy slot.
    #[test]
    fn peer_linktest_request_bypasses_local_control_slot() {
        let mut core = core();
        connect(&mut core);
        let _ = start_select(&mut core, 70);

        let actions = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::LinktestRequest {
                    system_bytes: SystemBytes::new(700),
                }),
                now(2),
            )
            .into_actions();
        assert!(matches!(
            sent_control(&actions).1,
            ControlMessage::LinktestResponse { system_bytes }
                if system_bytes == SystemBytes::new(700)
        ));
        assert_eq!(actions.len(), 1);
    }

    /// Confirms base Reject reasons require exact tuple attribution and extensions do not complete.
    #[test]
    fn reject_attribution_uses_reason_specific_header_semantics() {
        for (reason, header_byte_2) in [
            (RejectReason::UNSUPPORTED_PTYPE, 0),
            (RejectReason::UNSUPPORTED_STYPE, SELECT_REQUEST_STYPE),
            (RejectReason::TRANSACTION_NOT_OPEN, SELECT_REQUEST_STYPE),
            (RejectReason::ENTITY_NOT_SELECTED, SELECT_REQUEST_STYPE),
        ] {
            let mut core = core();
            connect(&mut core);
            let (_, system_bytes) = start_select(&mut core, 80);
            assert_eq!(
                core.on_message(
                    ProtocolMessage::Control(ControlMessage::RejectRequest {
                        session_id: CONTROL_SESSION_ID,
                        header_byte_2,
                        reason,
                        system_bytes,
                    }),
                    now(2),
                )
                .into_actions(),
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(80),
                    result: CoreCommandResult::Control(Err(OperationError::PeerRejected {
                        reason,
                    })),
                }]
            );
        }

        let mut core = core();
        connect(&mut core);
        let (_, system_bytes) = start_select(&mut core, 81);
        assert!(core
            .on_message(
                ProtocolMessage::Control(ControlMessage::RejectRequest {
                    session_id: CONTROL_SESSION_ID,
                    header_byte_2: SELECT_REQUEST_STYPE,
                    reason: RejectReason::new(9).unwrap(),
                    system_bytes,
                }),
                now(2),
            )
            .into_actions()
            .is_empty());
        assert!(core.active_transaction.is_some());
    }

    /// Confirms Linktest Reject attribution uses its request SType and a late
    /// repeat cannot complete the command again or consume another transaction.
    #[test]
    fn linktest_reject_uses_exact_tuple_and_late_repeat_is_trace_only() {
        let mut core = core();
        connect(&mut core);
        let actions = core
            .on_command(command(82, CoreCommandKind::Linktest), now(1))
            .into_actions();
        let (write_id, request) = sent_control(&actions);
        let ControlMessage::LinktestRequest { system_bytes } = request else {
            panic!("Linktest command must send Linktest.req");
        };
        let wrong = ProtocolMessage::Control(ControlMessage::RejectRequest {
            session_id: CONTROL_SESSION_ID,
            header_byte_2: SELECT_REQUEST_STYPE,
            reason: RejectReason::TRANSACTION_NOT_OPEN,
            system_bytes,
        });
        assert!(core.on_message(wrong, now(2)).into_actions().is_empty());
        assert!(core.active_transaction.is_some());

        let matching = ProtocolMessage::Control(ControlMessage::RejectRequest {
            session_id: CONTROL_SESSION_ID,
            header_byte_2: LINKTEST_REQUEST_STYPE,
            reason: RejectReason::TRANSACTION_NOT_OPEN,
            system_bytes,
        });
        assert_eq!(
            core.on_message(matching.clone(), now(3)).into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(82),
                result: CoreCommandResult::Control(Err(OperationError::PeerRejected {
                    reason: RejectReason::TRANSACTION_NOT_OPEN,
                })),
            }]
        );
        assert!(core.on_message(matching, now(4)).into_actions().is_empty());
        assert!(core
            .on_write_outcome(write_id, WriteOutcome::Committed, now(5))
            .into_actions()
            .is_empty());
    }

    /// Confirms fast response completes once and its retained request accepts one late outcome.
    #[test]
    fn fast_response_retains_write_without_duplicate_completion() {
        let mut core = core();
        connect(&mut core);
        let (write_id, system_bytes) = start_select(&mut core, 90);

        let response = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::SelectResponse {
                    session_id: CONTROL_SESSION_ID,
                    status: SelectStatus::SUCCESS,
                    system_bytes,
                }),
                now(2),
            )
            .into_actions();
        assert_eq!(response.len(), 2);
        assert_eq!(
            core.on_write_outcome(write_id, WriteOutcome::NotWritten(fault()), now(3))
                .into_actions(),
            vec![CoreAction::CloseGeneration {
                reason: GenerationCloseReason::TransportLost,
                barrier: CloseBarrier::Immediate,
            }]
        );
    }

    /// Confirms request write failures use deterministic and indeterminate completion classes.
    #[test]
    fn request_write_failures_complete_once_and_close_transport() {
        for (outcome, error) in [
            (
                WriteOutcome::NotWritten(fault()),
                OperationError::ConnectionLost,
            ),
            (
                WriteOutcome::Indeterminate(fault()),
                OperationError::DeliveryIndeterminate,
            ),
        ] {
            let mut core = core();
            connect(&mut core);
            let (write_id, _) = start_select(&mut core, 100);
            assert_eq!(
                core.on_write_outcome(write_id, outcome, now(2))
                    .into_actions(),
                vec![
                    CoreAction::CompleteCommand {
                        command_id: CommandId::new(100),
                        result: CoreCommandResult::Control(Err(error)),
                    },
                    CoreAction::CloseGeneration {
                        reason: GenerationCloseReason::TransportLost,
                        barrier: CloseBarrier::Immediate,
                    },
                ]
            );
        }
    }

    /// Confirms local Separate preempts a transaction before send and installs its barrier.
    #[test]
    fn local_separate_orders_preemption_send_state_and_barrier() {
        let mut core = core();
        connect(&mut core);
        let _ = start_select(&mut core, 110);
        let passive_write = select_passively(&mut core);
        assert!(core
            .on_write_outcome(passive_write, WriteOutcome::Committed, now(2))
            .into_actions()
            .is_empty());

        let actions = core
            .on_command(command(111, CoreCommandKind::Separate), now(3))
            .into_actions();
        let (write_id, message) = sent_control(&actions);
        assert!(matches!(
            message,
            ControlMessage::SeparateRequest {
                session_id: CONTROL_SESSION_ID,
                ..
            }
        ));
        assert_eq!(
            actions,
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(110),
                    result: CoreCommandResult::Control(Err(OperationError::Protocol(
                        ProtocolError::TransactionAborted,
                    ))),
                },
                CoreAction::SendFrame {
                    write_id,
                    message: ProtocolMessage::Control(message),
                },
                CoreAction::SessionStateChanged(SessionState::NotSelected),
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::LocalSeparate,
                    barrier: CloseBarrier::AfterWrite(write_id),
                },
            ]
        );
        assert_eq!(core.state(), Some(SessionState::NotSelected));
    }

    /// Confirms Separate preemption leaves the old request WriteId valid until
    /// its unique late outcome and does not repeat the aborted completion.
    #[test]
    fn separate_preemption_retains_the_old_request_write_outcome() {
        let mut core = core();
        connect(&mut core);
        let _ = select_passively(&mut core);
        let linktest = core
            .on_command(command(112, CoreCommandKind::Linktest), now(2))
            .into_actions();
        let (old_write_id, _) = sent_control(&linktest);
        let separate = core
            .on_command(command(113, CoreCommandKind::Separate), now(3))
            .into_actions();
        let (separate_write_id, _) = sent_control(&separate);
        assert!(matches!(
            separate.first(),
            Some(CoreAction::CompleteCommand {
                command_id,
                result: CoreCommandResult::Control(Err(OperationError::Protocol(
                    ProtocolError::TransactionAborted,
                ))),
            }) if *command_id == CommandId::new(112)
        ));

        assert!(core
            .on_write_outcome(old_write_id, WriteOutcome::Committed, now(4))
            .into_actions()
            .is_empty());
        assert_eq!(
            core.on_write_outcome(separate_write_id, WriteOutcome::Committed, now(5))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(113),
                result: CoreCommandResult::Control(Ok(())),
            }]
        );
    }

    /// Confirms every Separate terminal outcome satisfies the barrier but only commit succeeds.
    #[test]
    fn separate_barrier_completes_from_all_terminal_outcomes() {
        for (outcome, result) in [
            (WriteOutcome::Committed, Ok(())),
            (
                WriteOutcome::NotWritten(fault()),
                Err(OperationError::ConnectionLost),
            ),
            (
                WriteOutcome::Indeterminate(fault()),
                Err(OperationError::DeliveryIndeterminate),
            ),
        ] {
            let mut core = core();
            connect(&mut core);
            let _ = select_passively(&mut core);
            let separate = core
                .on_command(command(120, CoreCommandKind::Separate), now(2))
                .into_actions();
            let (write_id, _) = sent_control(&separate);
            assert_eq!(
                core.on_write_outcome(write_id, outcome, now(3))
                    .into_actions(),
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(120),
                    result: CoreCommandResult::Control(result),
                }]
            );
        }
    }

    /// Confirms Separate admission failure completes it and emits an applied immediate close.
    #[test]
    fn separate_admission_failure_replaces_short_circuited_barrier_close() {
        let mut core = core();
        connect(&mut core);
        let _ = select_passively(&mut core);
        let separate = core
            .on_command(command(130, CoreCommandKind::Separate), now(2))
            .into_actions();
        let (failed_write_id, _) = sent_control(&separate);

        assert_eq!(
            core.on_shutdown(
                GenerationCloseReason::ControlBackpressure,
                Some(failed_write_id),
                now(3),
            )
            .into_actions(),
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(130),
                    result: CoreCommandResult::Control(Err(OperationError::Backpressure)),
                },
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::ControlBackpressure,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
    }

    /// Confirms peer Separate is ignored in NotSelected and terminates Selected once.
    #[test]
    fn peer_separate_transitions_selected_and_aborts_local_transaction() {
        let mut core = core();
        connect(&mut core);
        let peer_separate = ProtocolMessage::Control(ControlMessage::SeparateRequest {
            session_id: 3,
            system_bytes: SystemBytes::new(3),
        });
        assert!(core
            .on_message(peer_separate.clone(), now(1))
            .into_actions()
            .is_empty());
        let _ = select_passively(&mut core);
        let linktest = core
            .on_command(command(140, CoreCommandKind::Linktest), now(2))
            .into_actions();
        assert!(matches!(
            sent_control(&linktest).1,
            ControlMessage::LinktestRequest { .. }
        ));

        assert_eq!(
            core.on_message(peer_separate, now(3)).into_actions(),
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(140),
                    result: CoreCommandResult::Control(Err(OperationError::Protocol(
                        ProtocolError::TransactionAborted,
                    ))),
                },
                CoreAction::SessionStateChanged(SessionState::NotSelected),
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::SeparateReceived,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
    }

    /// Confirms admission failure attributes only the failed command as Backpressure.
    #[test]
    fn shutdown_failed_write_attributes_backpressure_and_unknown_id_is_invariant() {
        let mut session = core();
        connect(&mut session);
        let (write_id, _) = start_select(&mut session, 150);
        assert_eq!(
            session
                .on_shutdown(
                    GenerationCloseReason::ControlBackpressure,
                    Some(write_id),
                    now(2),
                )
                .into_actions(),
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(150),
                    result: CoreCommandResult::Control(Err(OperationError::Backpressure)),
                },
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::ControlBackpressure,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );

        let mut unknown = core();
        connect(&mut unknown);
        assert_eq!(
            unknown
                .on_shutdown(
                    GenerationCloseReason::ControlBackpressure,
                    Some(WriteId::new(999)),
                    now(1),
                )
                .into_actions(),
            vec![CoreAction::CloseGeneration {
                reason: GenerationCloseReason::RuntimeInvariant,
                barrier: CloseBarrier::Immediate,
            }]
        );
    }

    /// Confirms a second outcome is an invariant and cannot repeat completion.
    #[test]
    fn duplicate_write_outcome_fails_closed_without_second_completion() {
        let mut core = core();
        connect(&mut core);
        let (write_id, _) = start_select(&mut core, 160);
        assert!(core
            .on_write_outcome(write_id, WriteOutcome::Committed, now(2))
            .into_actions()
            .is_empty());
        assert_eq!(
            core.on_write_outcome(write_id, WriteOutcome::Committed, now(3))
                .into_actions(),
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(160),
                    result: CoreCommandResult::Control(Err(OperationError::ConnectionLost)),
                },
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::RuntimeInvariant,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
    }

    /// Confirms identifier allocators expose their final value once and never wrap.
    #[test]
    fn identifier_allocators_do_not_wrap() {
        let mut core = core();
        core.next_write_id = Some(u64::MAX);
        assert_eq!(core.allocate_write_id(), Some(WriteId::new(u64::MAX)));
        assert_eq!(core.allocate_write_id(), None);

        core.next_system_bytes = Some(u32::MAX);
        assert_eq!(
            core.allocate_system_bytes(),
            Some(SystemBytes::new(u32::MAX))
        );
        assert_eq!(core.allocate_system_bytes(), None);
    }

    /// Confirms allocation exhaustion closes and completes the accepted command once.
    #[test]
    fn request_identifier_exhaustion_fails_closed() {
        let mut core = core();
        connect(&mut core);
        core.next_system_bytes = None;

        assert_eq!(
            core.on_command(command(170, CoreCommandKind::Select), now(1))
                .into_actions(),
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(170),
                    result: CoreCommandResult::Control(Err(OperationError::ConnectionLost)),
                },
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::RuntimeInvariant,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
    }

    /// Confirms normal external shutdown completes the live command before close.
    #[test]
    fn external_shutdown_completes_pending_command_before_close() {
        let mut core = core();
        connect(&mut core);
        let _ = core.on_command(command(180, CoreCommandKind::Linktest), now(1));

        assert_eq!(
            core.on_shutdown(GenerationCloseReason::LocalDisconnect, None, now(2))
                .into_actions(),
            vec![
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(180),
                    result: CoreCommandResult::Control(Err(OperationError::ConnectionLost)),
                },
                CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::LocalDisconnect,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
    }

    /// Confirms unmatched Linktest Reject uses the exact response SType constant.
    #[test]
    fn unmatched_linktest_response_copies_tuple_into_reject() {
        let mut core = core();
        connect(&mut core);
        let system_bytes = SystemBytes::new(303);

        let actions = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::LinktestResponse { system_bytes }),
                now(1),
            )
            .into_actions();
        assert!(matches!(
            sent_control(&actions).1,
            ControlMessage::RejectRequest {
                session_id: CONTROL_SESSION_ID,
                header_byte_2: LINKTEST_RESPONSE_STYPE,
                reason: RejectReason::TRANSACTION_NOT_OPEN,
                system_bytes: copied,
            } if copied == system_bytes
        ));
        assert_eq!(LINKTEST_REQUEST_STYPE, 5);
    }
}
