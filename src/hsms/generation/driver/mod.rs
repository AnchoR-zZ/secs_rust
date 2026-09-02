//! Synchronous single-owner execution shell for one HSMS connection generation.
//!
//! The Driver serializes accepted control commands and protocol inputs into the
//! runtime-neutral [`SessionCore`], consumes each ordered action batch exactly
//! once, and owns command completion, Writer admission, and transport closing.
//! It deliberately contains no socket, task, channel, or asynchronous runtime.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::hsms::{
    api::ControlIntent,
    core::{CoreAction, CoreActions, CoreCommand, CoreCommandKind, CoreCommandResult, SessionCore},
    error::OperationError,
    generation::transport::writer::{OutboundFrame, WriteAdmissionError, WriterIngress},
    lifecycle::SessionState,
    model::{
        ids::{CommandId, WireSequence, WriteId},
        runtime::{CloseBarrier, GenerationCloseReason, MonoTime, WriteOutcome},
    },
    protocol::message::ProtocolMessage,
};

#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

/// Consuming endpoint for one accepted command's unique terminal result.
pub(crate) trait CommandCompletion {
    /// Consumes the endpoint and delivers `result` at most once.
    fn complete(self, result: CoreCommandResult);
}

/// Infallible observation boundary for committed HSMS selection-state changes.
pub(crate) trait SessionStateObserver {
    /// Records or publishes the committed `state` in Driver action order.
    fn observe(&mut self, state: SessionState);
}

/// Synchronous boundary used when Driver has satisfied its close barrier.
pub(crate) trait TransportCloser {
    /// Closes the current generation transport exactly once.
    fn close(&mut self);
}

/// Stable reason a control intent did not enter the accepted-command queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlAdmissionErrorKind {
    /// Deselect is intentionally outside the B1 control command vocabulary.
    UnsupportedIntent,
    /// Driver has entered closing and no longer accepts commands.
    Closing,
}

/// Rejected command retaining the caller-owned completion endpoint.
pub(crate) struct ControlAdmissionError<Completion> {
    /// Stable reason the command was not accepted.
    kind: ControlAdmissionErrorKind,
    /// Original public control intent supplied by the caller.
    intent: ControlIntent,
    /// Unconsumed completion endpoint returned to the caller.
    completion: Completion,
}

impl<Completion> ControlAdmissionError<Completion> {
    /// Returns the stable rejection category.
    pub(crate) const fn kind(&self) -> ControlAdmissionErrorKind {
        self.kind
    }

    /// Returns the original rejected public intent.
    pub(crate) const fn intent(&self) -> ControlIntent {
        self.intent
    }

    /// Returns ownership of the rejected intent and completion endpoint.
    pub(crate) fn into_parts(self) -> (ControlIntent, Completion) {
        (self.intent, self.completion)
    }
}

/// One accepted command waiting for Driver to allocate its CommandId.
struct AcceptedControlCommand<Completion> {
    /// Already translated B1 Core command kind.
    kind: CoreCommandKind,
    /// Completion endpoint retained while the envelope remains queued.
    completion: Completion,
}

/// First-reason-wins state retained while a generation is closing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClosingState {
    /// First stable close reason observed by Driver.
    reason: GenerationCloseReason,
    /// Accepted write whose terminal outcome must precede transport close.
    barrier_write_id: Option<WriteId>,
}

/// Failure encountered while synchronously applying one Core action batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionApplicationFailure {
    /// Writer rejected ownership of one Core-produced frame.
    WriteAdmission {
        /// Write identity rejected before a wire sequence was allocated.
        write_id: WriteId,
        /// Stable synchronous Writer rejection category.
        error: WriteAdmissionError,
    },
    /// Core attempted to complete a missing or already completed command.
    DuplicateOrUnknownCompletion,
}

/// Runtime-neutral single-owner Driver for one HSMS connection generation.
pub(crate) struct SessionDriver<Writer, Observer, Completion, Closer> {
    /// Exclusive owner of all generation-local HSMS protocol state.
    core: SessionCore,
    /// Synchronous ingress to the generation's single Writer.
    writer: Writer,
    /// Infallible sink for committed session-state observations.
    observer: Observer,
    /// Boundary that performs the single physical transport close.
    closer: Closer,
    /// Commands accepted but not yet assigned a CommandId.
    accepted_commands: VecDeque<AcceptedControlCommand<Completion>>,
    /// Unique completion endpoints for commands already presented to Core.
    completions: BTreeMap<CommandId, Completion>,
    /// Writer sequence retained for each successfully admitted WriteId.
    admitted_writes: HashMap<WriteId, WireSequence>,
    /// Next generation-local command number, or `None` after exhaustion.
    next_command_id: Option<u64>,
    /// First-reason-wins close state, including an optional write barrier.
    closing: Option<ClosingState>,
    /// Whether the physical transport close has already been performed.
    transport_closed: bool,
}

impl<Writer, Observer, Completion, Closer> SessionDriver<Writer, Observer, Completion, Closer>
where
    Writer: WriterIngress,
    Observer: SessionStateObserver,
    Completion: CommandCompletion,
    Closer: TransportCloser,
{
    /// Creates a running Driver from its exclusive Core and execution ports.
    pub(crate) fn new(
        core: SessionCore,
        writer: Writer,
        observer: Observer,
        closer: Closer,
    ) -> Self {
        Self {
            core,
            writer,
            observer,
            closer,
            accepted_commands: VecDeque::new(),
            completions: BTreeMap::new(),
            admitted_writes: HashMap::new(),
            next_command_id: Some(0),
            closing: None,
            transport_closed: false,
        }
    }

    /// Maps a public control intent into the B1 Core vocabulary.
    ///
    /// Deselect returns `None` because its protocol and drain policy are
    /// explicitly deferred beyond B1.
    pub(crate) const fn core_command_kind(intent: ControlIntent) -> Option<CoreCommandKind> {
        match intent {
            ControlIntent::Select => Some(CoreCommandKind::Select),
            ControlIntent::Deselect => None,
            ControlIntent::Linktest => Some(CoreCommandKind::Linktest),
            ControlIntent::Separate => Some(CoreCommandKind::Separate),
        }
    }

    /// Accepts a supported control command while Driver remains running.
    ///
    /// Success means the completion endpoint is owned by either the queued
    /// envelope or completion table until exactly one terminal result. Failure
    /// returns the endpoint without consuming it.
    pub(crate) fn try_accept_control(
        &mut self,
        intent: ControlIntent,
        completion: Completion,
    ) -> Result<(), ControlAdmissionError<Completion>> {
        if self.closing.is_some() {
            return Err(ControlAdmissionError {
                kind: ControlAdmissionErrorKind::Closing,
                intent,
                completion,
            });
        }
        let Some(kind) = Self::core_command_kind(intent) else {
            return Err(ControlAdmissionError {
                kind: ControlAdmissionErrorKind::UnsupportedIntent,
                intent,
                completion,
            });
        };
        self.accepted_commands
            .push_back(AcceptedControlCommand { kind, completion });
        Ok(())
    }

    /// Presents the initial connected input to Core while Driver is running.
    ///
    /// Returns `false` after closing begins because no new protocol input was
    /// delivered to Core.
    pub(crate) fn on_connected(&mut self, now: MonoTime) -> bool {
        if self.closing.is_some() {
            return false;
        }
        let actions = self.core.on_connected(now);
        self.apply_actions(actions, now);
        true
    }

    /// Dequeues and presents one accepted command to Core.
    ///
    /// CommandId allocation happens only after dequeue. Returns `false` when
    /// no command was processed or closing has already begun.
    pub(crate) fn drive_next_command(&mut self, now: MonoTime) -> bool {
        if self.closing.is_some() {
            return false;
        }
        let Some(accepted) = self.accepted_commands.pop_front() else {
            return false;
        };
        let Some(command_id) = self.allocate_command_id() else {
            accepted.completion.complete(CoreCommandResult::Control(Err(
                OperationError::ConnectionLost,
            )));
            self.fail_runtime_invariant(now);
            return true;
        };
        let previous = self.completions.insert(command_id, accepted.completion);
        debug_assert!(
            previous.is_none(),
            "fresh CommandId must not replace completion"
        );
        let actions = self
            .core
            .on_command(CoreCommand::new(command_id, accepted.kind), now);
        self.apply_actions(actions, now);
        true
    }

    /// Presents one complete semantic peer message while Driver is running.
    ///
    /// Messages are refused after closing begins, which prevents post-close
    /// protocol transitions from entering Core.
    pub(crate) fn on_message(&mut self, message: ProtocolMessage, now: MonoTime) -> bool {
        if self.closing.is_some() {
            return false;
        }
        let actions = self.core.on_message(message, now);
        self.apply_actions(actions, now);
        true
    }

    /// Presents a unique Writer terminal outcome to Core.
    ///
    /// Outcomes remain admissible while closing so an `AfterWrite` barrier can
    /// be satisfied and earlier accepted writes can reach their terminal fact.
    pub(crate) fn on_write_outcome(
        &mut self,
        write_id: WriteId,
        outcome: WriteOutcome,
        now: MonoTime,
    ) {
        let actions = self.core.on_write_outcome(write_id, outcome, now);
        self.apply_actions_without_finishing_close(actions, now);
        self.admitted_writes.remove(&write_id);
        self.satisfy_close_barrier(write_id);
        self.finish_close_if_ready();
    }

    /// Presents a terminal shutdown reason to Core and enters fail-closed.
    ///
    /// `failed_write_id` is `Some` only for a synchronous Writer admission
    /// failure that did not allocate a sequence or later outcome.
    pub(crate) fn on_shutdown(
        &mut self,
        reason: GenerationCloseReason,
        failed_write_id: Option<WriteId>,
        now: MonoTime,
    ) {
        let actions = self.core.on_shutdown(reason, failed_write_id, now);
        self.apply_actions_without_finishing_close(actions, now);
        self.begin_closing(reason, CloseBarrier::Immediate);
        self.finish_close_if_ready();
    }

    /// Advances Core logical time while Driver remains open to protocol work.
    ///
    /// Returns `false` after closing begins because regular deadline processing
    /// no longer competes with close cleanup in this B1 slice.
    pub(crate) fn advance_time(&mut self, now: MonoTime) -> bool {
        if self.closing.is_some() {
            return false;
        }
        let actions = self.core.advance_time(now);
        self.apply_actions(actions, now);
        true
    }

    /// Returns Core's state snapshot at the current Driver round boundary.
    pub(crate) fn state(&self) -> Option<SessionState> {
        self.core.state()
    }

    /// Returns Core's earliest logical deadline for external round scheduling.
    pub(crate) fn next_deadline(&self) -> Option<MonoTime> {
        self.core.next_deadline()
    }

    /// Returns the number of accepted envelopes not yet presented to Core.
    pub(crate) fn queued_command_count(&self) -> usize {
        self.accepted_commands.len()
    }

    /// Returns the number of commands whose completion endpoint is table-owned.
    pub(crate) fn pending_completion_count(&self) -> usize {
        self.completions.len()
    }

    /// Returns the first close reason, if closing has begun.
    pub(crate) fn close_reason(&self) -> Option<GenerationCloseReason> {
        self.closing.map(|closing| closing.reason)
    }

    /// Returns whether the physical transport close has occurred.
    pub(crate) const fn transport_closed(&self) -> bool {
        self.transport_closed
    }

    /// Returns the Writer sequence saved for one still-tracked admitted write.
    pub(crate) fn wire_sequence(&self, write_id: WriteId) -> Option<WireSequence> {
        self.admitted_writes.get(&write_id).copied()
    }

    /// Borrows the Writer for generation orchestration and deterministic tests.
    pub(crate) const fn writer(&self) -> &Writer {
        &self.writer
    }

    /// Mutably borrows the Writer for test-controlled outcome injection.
    pub(crate) fn writer_mut(&mut self) -> &mut Writer {
        &mut self.writer
    }

    /// Borrows the state observer for diagnostics and deterministic tests.
    pub(crate) const fn observer(&self) -> &Observer {
        &self.observer
    }

    /// Borrows the transport closer for diagnostics and deterministic tests.
    pub(crate) const fn closer(&self) -> &Closer {
        &self.closer
    }

    /// Allocates the next CommandId once and permanently stops after `u64::MAX`.
    fn allocate_command_id(&mut self) -> Option<CommandId> {
        let value = self.next_command_id?;
        self.next_command_id = value.checked_add(1);
        Some(CommandId::new(value))
    }

    /// Applies a complete Core action batch and then closes if its barrier allows.
    fn apply_actions(&mut self, actions: CoreActions, now: MonoTime) {
        self.apply_actions_without_finishing_close(actions, now);
        self.finish_close_if_ready();
    }

    /// Applies actions in order without yet performing the physical close.
    ///
    /// Keeping close finalization outside this method lets write-outcome rounds
    /// apply completion actions before satisfying an `AfterWrite` barrier.
    fn apply_actions_without_finishing_close(&mut self, actions: CoreActions, now: MonoTime) {
        if let Err(failure) = self.apply_action_batch(actions) {
            self.handle_action_failure(failure, now);
        }
    }

    /// Consumes actions in order and stops at the first synchronous failure.
    fn apply_action_batch(&mut self, actions: CoreActions) -> Result<(), ActionApplicationFailure> {
        for action in actions {
            match action {
                CoreAction::SendFrame { write_id, message } => {
                    let frame = OutboundFrame::new(write_id, message);
                    let sequence = self.writer.try_admit(frame).map_err(|error| {
                        ActionApplicationFailure::WriteAdmission { write_id, error }
                    })?;
                    if self.admitted_writes.insert(write_id, sequence).is_some() {
                        return Err(ActionApplicationFailure::DuplicateOrUnknownCompletion);
                    }
                }
                CoreAction::SessionStateChanged(state) => self.observer.observe(state),
                CoreAction::CompleteCommand { command_id, result } => {
                    let Some(completion) = self.completions.remove(&command_id) else {
                        return Err(ActionApplicationFailure::DuplicateOrUnknownCompletion);
                    };
                    completion.complete(result);
                }
                CoreAction::CloseGeneration { reason, barrier } => {
                    self.begin_closing(reason, barrier);
                }
            }
        }
        Ok(())
    }

    /// Converts one action-application failure into a single Core shutdown call.
    fn handle_action_failure(&mut self, failure: ActionApplicationFailure, now: MonoTime) {
        let (reason, failed_write_id) = match failure {
            ActionApplicationFailure::WriteAdmission { write_id, error } => match error {
                WriteAdmissionError::Full => {
                    (GenerationCloseReason::ControlBackpressure, Some(write_id))
                }
                WriteAdmissionError::Closed => {
                    (GenerationCloseReason::TransportLost, Some(write_id))
                }
            },
            ActionApplicationFailure::DuplicateOrUnknownCompletion => {
                (GenerationCloseReason::RuntimeInvariant, None)
            }
        };
        let shutdown_actions = self.core.on_shutdown(reason, failed_write_id, now);
        if self.apply_action_batch(shutdown_actions).is_err() {
            self.begin_closing(
                GenerationCloseReason::RuntimeInvariant,
                CloseBarrier::Immediate,
            );
        }
        self.begin_closing(reason, CloseBarrier::Immediate);
    }

    /// Fails closed after local CommandId exhaustion without wrapping allocation.
    fn fail_runtime_invariant(&mut self, now: MonoTime) {
        let actions = self
            .core
            .on_shutdown(GenerationCloseReason::RuntimeInvariant, None, now);
        self.apply_actions_without_finishing_close(actions, now);
        self.begin_closing(
            GenerationCloseReason::RuntimeInvariant,
            CloseBarrier::Immediate,
        );
        self.finish_close_if_ready();
    }

    /// Records the first close reason and its associated barrier.
    fn begin_closing(&mut self, reason: GenerationCloseReason, barrier: CloseBarrier) {
        if self.closing.is_some() {
            return;
        }
        let barrier_write_id = match barrier {
            CloseBarrier::Immediate => None,
            CloseBarrier::AfterWrite(write_id) => Some(write_id),
        };
        self.closing = Some(ClosingState {
            reason,
            barrier_write_id,
        });
    }

    /// Marks a matching terminal write outcome as satisfying the close barrier.
    fn satisfy_close_barrier(&mut self, write_id: WriteId) {
        if let Some(closing) = &mut self.closing {
            if closing.barrier_write_id == Some(write_id) {
                closing.barrier_write_id = None;
            }
        }
    }

    /// Performs the single transport close once no write barrier remains.
    fn finish_close_if_ready(&mut self) {
        let ready = matches!(
            self.closing,
            Some(ClosingState {
                barrier_write_id: None,
                ..
            })
        );
        if !ready || self.transport_closed {
            return;
        }
        self.closer.close();
        self.transport_closed = true;
        self.drain_accepted_commands();
    }

    /// Completes every table-owned and queued accepted command exactly once.
    fn drain_accepted_commands(&mut self) {
        let pending = std::mem::take(&mut self.completions);
        for (_, completion) in pending {
            completion.complete(CoreCommandResult::Control(Err(
                OperationError::ConnectionLost,
            )));
        }
        while let Some(accepted) = self.accepted_commands.pop_front() {
            accepted.completion.complete(CoreCommandResult::Control(Err(
                OperationError::ConnectionLost,
            )));
        }
    }
}
