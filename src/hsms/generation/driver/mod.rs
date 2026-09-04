//! Synchronous single-owner execution shell for one HSMS connection generation.
//!
//! The Driver serializes accepted Control/Data commands and protocol inputs into the
//! runtime-neutral [`SessionCore`], consumes each ordered action batch exactly
//! once, and owns command completion, Writer admission, and transport closing.
//! It deliberately contains no socket, task, channel, or asynchronous runtime.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, VecDeque};

use crate::hsms::{
    api::{ControlIntent, PrimaryMessage, SecondaryMessage, SendReceipt},
    core::{
        CoreAction, CoreActions, CoreCommand, CoreCommandKind, CoreCommandResult, OutboundPrimary,
        SessionCore,
    },
    error::OperationError,
    generation::transport::writer::{
        DataPermitError, DataReserveError, OutboundFrame, ReservedDataAdmissionError,
        WriteAdmissionError, WriterIngress,
    },
    lifecycle::SessionState,
    model::{
        ids::{CommandId, ConnectionGeneration, WireSequence, WriteId},
        runtime::{CloseBarrier, GenerationCloseReason, MonoTime, WriteOutcome},
    },
    protocol::message::ProtocolMessage,
};

#[cfg(test)]
mod b2_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;

/// Public-API-shaped terminal result owned and delivered by the Driver.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DriverCommandResult {
    /// Completion of a Select, Linktest, or Separate operation.
    Control(Result<(), OperationError>),
    /// Completion of an outbound W=0 Primary with its local commit receipt.
    Send(Result<SendReceipt, OperationError>),
    /// Completion of an outbound W=1 Primary with its matched Secondary.
    Request(Result<SecondaryMessage, OperationError>),
}

/// Consuming endpoint for one accepted command's unique terminal result.
pub(crate) trait CommandCompletion {
    /// Consumes the endpoint and delivers `result` at most once.
    fn complete(self, result: DriverCommandResult);
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

/// Stable reason a Data intent did not enter the accepted-command queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataAdmissionErrorKind {
    /// Driver has entered closing and no longer accepts Data commands.
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

/// Rejected Data command retaining all caller-owned values.
pub(crate) struct DataAdmissionError<Completion> {
    /// Stable reason the command was not accepted.
    kind: DataAdmissionErrorKind,
    /// Original public Primary supplied by the caller.
    message: PrimaryMessage,
    /// Unconsumed completion endpoint returned to the caller.
    completion: Completion,
}

impl<Completion> DataAdmissionError<Completion> {
    /// Returns the stable Data admission rejection category.
    pub(crate) const fn kind(&self) -> DataAdmissionErrorKind {
        self.kind
    }

    /// Borrows the original rejected Primary without cloning its body.
    pub(crate) const fn message(&self) -> &PrimaryMessage {
        &self.message
    }

    /// Returns ownership of the rejected Primary and completion endpoint.
    pub(crate) fn into_parts(self) -> (PrimaryMessage, Completion) {
        (self.message, self.completion)
    }
}

/// Driver-owned intent retained before it is presented to Core.
enum AcceptedCommandKind {
    /// Already translated B1 Control command.
    Control(CoreCommandKind),
    /// Public W=0 Primary whose body remains owned by the queue.
    Send(PrimaryMessage),
    /// Public W=1 Primary whose body remains owned by the queue.
    Request(PrimaryMessage),
}

impl AcceptedCommandKind {
    /// Returns the typed completion class retained after Core takes the intent.
    const fn completion_kind(&self) -> CompletionKind {
        match self {
            Self::Control(_) => CompletionKind::Control,
            Self::Send(_) => CompletionKind::Send,
            Self::Request(_) => CompletionKind::Request,
        }
    }

    /// Returns whether this command requires a pre-Core Data reservation.
    const fn requires_data_permit(&self) -> bool {
        matches!(self, Self::Send(_) | Self::Request(_))
    }

    /// Consumes the queued intent into Core's owned command vocabulary.
    fn into_core_kind(self) -> CoreCommandKind {
        match self {
            Self::Control(kind) => kind,
            Self::Send(message) => {
                let (stream, function, body) = message.into_parts();
                CoreCommandKind::Send(OutboundPrimary::new(stream, function, body))
            }
            Self::Request(message) => {
                let (stream, function, body) = message.into_parts();
                CoreCommandKind::Request(OutboundPrimary::new(stream, function, body))
            }
        }
    }
}

/// One accepted command waiting for Driver to allocate its CommandId.
struct AcceptedCommand<Completion> {
    /// Owned Control, Send, or Request intent.
    kind: AcceptedCommandKind,
    /// Completion endpoint retained while the envelope remains queued.
    completion: Completion,
}

/// Typed completion class retained after the command payload enters Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionKind {
    /// Select, Linktest, or Separate completion.
    Control,
    /// Outbound W=0 Primary completion.
    Send,
    /// Outbound W=1 Primary completion.
    Request,
}

impl CompletionKind {
    /// Constructs this class's stable shutdown result.
    fn connection_lost_result(self) -> DriverCommandResult {
        match self {
            Self::Control => DriverCommandResult::Control(Err(OperationError::ConnectionLost)),
            Self::Send => DriverCommandResult::Send(Err(OperationError::ConnectionLost)),
            Self::Request => DriverCommandResult::Request(Err(OperationError::ConnectionLost)),
        }
    }

    /// Constructs this Data class's immediate pre-Core admission failure.
    fn data_error_result(self, error: OperationError) -> DriverCommandResult {
        match self {
            Self::Send => DriverCommandResult::Send(Err(error)),
            Self::Request => DriverCommandResult::Request(Err(error)),
            Self::Control => {
                debug_assert!(false, "Control command cannot fail Data reservation");
                DriverCommandResult::Control(Err(OperationError::ConnectionLost))
            }
        }
    }
}

/// Completion endpoint paired with the typed result shape expected from Core.
struct PendingCompletion<Completion> {
    /// Control, Send, or Request result class used for validation and drain.
    kind: CompletionKind,
    /// Unique endpoint consumed only after successful result materialization.
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
    /// Writer rejected ownership of one Core-produced Control frame.
    ControlWriteAdmission {
        /// Write identity rejected before a wire sequence was allocated.
        write_id: WriteId,
        /// Stable synchronous Writer rejection category.
        error: WriteAdmissionError,
    },
    /// Writer rejected a Data frame after a successful reservation.
    ReservedDataAdmission {
        /// Write identity rejected before a wire sequence was allocated.
        write_id: WriteId,
        /// Stable post-reservation rejection category.
        error: ReservedDataAdmissionError,
    },
    /// Core emitted a Data frame without exactly one available command permit.
    MissingOrMultipleDataFrame {
        /// Write identity of the impossible Data action.
        write_id: WriteId,
    },
    /// Writer rejected release of an otherwise unused Data permit.
    DataPermitRelease(DataPermitError),
    /// Core attempted to complete a missing or already completed command.
    DuplicateOrUnknownCompletion,
    /// Core returned the wrong completion type or an unmapped committed write.
    InvalidCompletionResult,
}

/// Runtime-neutral single-owner Driver for one HSMS connection generation.
pub(crate) struct SessionDriver<Writer, Observer, Completion, Closer> {
    /// TCP incarnation included in every materialized Send receipt.
    generation: ConnectionGeneration,
    /// Exclusive owner of all generation-local HSMS protocol state.
    core: SessionCore,
    /// Synchronous ingress to the generation's single Writer.
    writer: Writer,
    /// Infallible sink for committed session-state observations.
    observer: Observer,
    /// Boundary that performs the single physical transport close.
    closer: Closer,
    /// Commands accepted but not yet assigned a CommandId.
    accepted_commands: VecDeque<AcceptedCommand<Completion>>,
    /// Typed unique completion endpoints for commands already presented to Core.
    completions: BTreeMap<CommandId, PendingCompletion<Completion>>,
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
        generation: ConnectionGeneration,
        core: SessionCore,
        writer: Writer,
        observer: Observer,
        closer: Closer,
    ) -> Self {
        Self {
            generation,
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
        self.accepted_commands.push_back(AcceptedCommand {
            kind: AcceptedCommandKind::Control(kind),
            completion,
        });
        Ok(())
    }

    /// Accepts one outbound W=0 Primary while Driver remains running.
    ///
    /// Success transfers the owned `message` and `completion` into the common
    /// FIFO. Failure returns both values unchanged to the caller.
    pub(crate) fn try_accept_send(
        &mut self,
        message: PrimaryMessage,
        completion: Completion,
    ) -> Result<(), DataAdmissionError<Completion>> {
        self.try_accept_data(message, completion, false)
    }

    /// Accepts one outbound W=1 Primary while Driver remains running.
    ///
    /// Success transfers the owned `message` and `completion` into the common
    /// FIFO. Failure returns both values unchanged to the caller.
    pub(crate) fn try_accept_request(
        &mut self,
        message: PrimaryMessage,
        completion: Completion,
    ) -> Result<(), DataAdmissionError<Completion>> {
        self.try_accept_data(message, completion, true)
    }

    /// Enqueues one typed Data command without cloning its optional body.
    fn try_accept_data(
        &mut self,
        message: PrimaryMessage,
        completion: Completion,
        reply_expected: bool,
    ) -> Result<(), DataAdmissionError<Completion>> {
        if self.closing.is_some() {
            return Err(DataAdmissionError {
                kind: DataAdmissionErrorKind::Closing,
                message,
                completion,
            });
        }
        let kind = if reply_expected {
            AcceptedCommandKind::Request(message)
        } else {
            AcceptedCommandKind::Send(message)
        };
        self.accepted_commands
            .push_back(AcceptedCommand { kind, completion });
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
        let completion_kind = accepted.kind.completion_kind();
        let data_permit = if accepted.kind.requires_data_permit() {
            match self.writer.try_reserve_data() {
                Ok(permit) => Some(permit),
                Err(DataReserveError::Full) => {
                    accepted
                        .completion
                        .complete(completion_kind.data_error_result(OperationError::Backpressure));
                    return true;
                }
                Err(DataReserveError::Closed) => {
                    accepted.completion.complete(
                        completion_kind.data_error_result(OperationError::ConnectionLost),
                    );
                    self.shutdown_after_driver_failure(
                        GenerationCloseReason::TransportLost,
                        None,
                        now,
                    );
                    self.finish_close_if_ready();
                    return true;
                }
            }
        } else {
            None
        };
        let Some(command_id) = self.allocate_command_id() else {
            if let Some(permit) = data_permit {
                let _ = self.writer.release_data(permit);
            }
            accepted
                .completion
                .complete(completion_kind.connection_lost_result());
            self.fail_runtime_invariant(now);
            return true;
        };
        let core_kind = accepted.kind.into_core_kind();
        let previous = self.completions.insert(
            command_id,
            PendingCompletion {
                kind: completion_kind,
                completion: accepted.completion,
            },
        );
        debug_assert!(
            previous.is_none(),
            "fresh CommandId must not replace completion"
        );
        let actions = self
            .core
            .on_command(CoreCommand::new(command_id, core_kind), now);
        self.apply_actions_with_data_permit(actions, data_permit, now);
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
    /// no longer competes with close cleanup in these minimal B1/B2 slices.
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

    /// Returns Core's count of live outbound Data response transactions.
    pub(crate) fn pending_data_transaction_count(&self) -> usize {
        self.core.pending_data_transaction_count()
    }

    /// Returns Core's count of retained completed response contracts.
    pub(crate) fn tombstone_count(&self) -> usize {
        self.core.tombstone_count()
    }

    /// Returns Core's count of accepted commands not yet completed there.
    pub(crate) fn open_core_command_count(&self) -> usize {
        self.core.open_command_count()
    }

    /// Returns Core's count of writes awaiting their unique terminal outcome.
    pub(crate) fn pending_write_count(&self) -> usize {
        self.core.pending_write_count()
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

    /// Applies one Data-command batch with its pre-Core Writer reservation.
    fn apply_actions_with_data_permit(
        &mut self,
        actions: CoreActions,
        data_permit: Option<Writer::DataPermit>,
        now: MonoTime,
    ) {
        if let Err(failure) = self.apply_action_batch(actions, data_permit) {
            self.handle_action_failure(failure, now);
        }
        self.finish_close_if_ready();
    }

    /// Applies actions in order without yet performing the physical close.
    ///
    /// Keeping close finalization outside this method lets write-outcome rounds
    /// apply completion actions before satisfying an `AfterWrite` barrier.
    fn apply_actions_without_finishing_close(&mut self, actions: CoreActions, now: MonoTime) {
        if let Err(failure) = self.apply_action_batch(actions, None) {
            self.handle_action_failure(failure, now);
        }
    }

    /// Consumes actions in order and retires an optional Data permit exactly once.
    fn apply_action_batch(
        &mut self,
        actions: CoreActions,
        mut data_permit: Option<Writer::DataPermit>,
    ) -> Result<(), ActionApplicationFailure> {
        let mut application_result = Ok(());
        for action in actions {
            let step_result = match action {
                CoreAction::SendFrame { write_id, message } => {
                    let frame = OutboundFrame::new(write_id, message);
                    let sequence = match frame.message() {
                        ProtocolMessage::Control(_) => {
                            self.writer.try_admit(frame).map_err(|error| {
                                ActionApplicationFailure::ControlWriteAdmission { write_id, error }
                            })
                        }
                        ProtocolMessage::Data(_) => {
                            let Some(permit) = data_permit.take() else {
                                application_result =
                                    Err(ActionApplicationFailure::MissingOrMultipleDataFrame {
                                        write_id,
                                    });
                                break;
                            };
                            self.writer
                                .admit_reserved_data(permit, frame)
                                .map_err(|error| ActionApplicationFailure::ReservedDataAdmission {
                                    write_id,
                                    error,
                                })
                        }
                    };
                    let sequence = match sequence {
                        Ok(sequence) => sequence,
                        Err(failure) => {
                            application_result = Err(failure);
                            break;
                        }
                    };
                    if self.admitted_writes.insert(write_id, sequence).is_some() {
                        Err(ActionApplicationFailure::DuplicateOrUnknownCompletion)
                    } else {
                        Ok(())
                    }
                }
                CoreAction::SessionStateChanged(state) => {
                    self.observer.observe(state);
                    Ok(())
                }
                CoreAction::CompleteCommand { command_id, result } => {
                    let Some(pending) = self.completions.get(&command_id) else {
                        application_result =
                            Err(ActionApplicationFailure::DuplicateOrUnknownCompletion);
                        break;
                    };
                    let Some(result) = self.materialize_result(pending.kind, result) else {
                        application_result = Err(ActionApplicationFailure::InvalidCompletionResult);
                        break;
                    };
                    let pending = self
                        .completions
                        .remove(&command_id)
                        .expect("completion was just validated as present");
                    pending.completion.complete(result);
                    Ok(())
                }
                CoreAction::CloseGeneration { reason, barrier } => {
                    self.begin_closing(reason, barrier);
                    Ok(())
                }
            };
            if let Err(failure) = step_result {
                application_result = Err(failure);
                break;
            }
        }

        if let Some(permit) = data_permit {
            if let Err(error) = self.writer.release_data(permit) {
                return Err(ActionApplicationFailure::DataPermitRelease(error));
            }
        }
        application_result
    }

    /// Converts Core-owned completion facts into API-shaped Driver results.
    fn materialize_result(
        &self,
        kind: CompletionKind,
        result: CoreCommandResult,
    ) -> Option<DriverCommandResult> {
        match (kind, result) {
            (CompletionKind::Control, CoreCommandResult::Control(result)) => {
                Some(DriverCommandResult::Control(result))
            }
            (CompletionKind::Send, CoreCommandResult::Send(Ok(committed))) => {
                let sequence = self.admitted_writes.get(&committed.write_id()).copied()?;
                Some(DriverCommandResult::Send(Ok(SendReceipt::new(
                    self.generation,
                    sequence,
                ))))
            }
            (CompletionKind::Send, CoreCommandResult::Send(Err(error))) => {
                Some(DriverCommandResult::Send(Err(error)))
            }
            (CompletionKind::Request, CoreCommandResult::Request(Ok(matched))) => {
                let (stream, function, body) = matched.into_parts();
                Some(DriverCommandResult::Request(Ok(SecondaryMessage::new(
                    stream, function, body,
                ))))
            }
            (CompletionKind::Request, CoreCommandResult::Request(Err(error))) => {
                Some(DriverCommandResult::Request(Err(error)))
            }
            _ => None,
        }
    }

    /// Converts one action-application failure into a single Core shutdown call.
    fn handle_action_failure(&mut self, failure: ActionApplicationFailure, now: MonoTime) {
        let (reason, failed_write_id) = match failure {
            ActionApplicationFailure::ControlWriteAdmission { write_id, error } => match error {
                WriteAdmissionError::Full => {
                    (GenerationCloseReason::ControlBackpressure, Some(write_id))
                }
                WriteAdmissionError::Closed => {
                    (GenerationCloseReason::TransportLost, Some(write_id))
                }
                WriteAdmissionError::Invariant => {
                    (GenerationCloseReason::RuntimeInvariant, Some(write_id))
                }
            },
            ActionApplicationFailure::ReservedDataAdmission { write_id, error } => match error {
                ReservedDataAdmissionError::Closed => {
                    (GenerationCloseReason::TransportLost, Some(write_id))
                }
                ReservedDataAdmissionError::Invariant => {
                    (GenerationCloseReason::RuntimeInvariant, Some(write_id))
                }
            },
            ActionApplicationFailure::MissingOrMultipleDataFrame { write_id } => {
                (GenerationCloseReason::RuntimeInvariant, Some(write_id))
            }
            ActionApplicationFailure::DataPermitRelease(DataPermitError::Invariant)
            | ActionApplicationFailure::DuplicateOrUnknownCompletion
            | ActionApplicationFailure::InvalidCompletionResult => {
                (GenerationCloseReason::RuntimeInvariant, None)
            }
        };
        self.shutdown_after_driver_failure(reason, failed_write_id, now);
    }

    /// Shuts Core down after a Driver-owned failure and locks first close reason.
    fn shutdown_after_driver_failure(
        &mut self,
        reason: GenerationCloseReason,
        failed_write_id: Option<WriteId>,
        now: MonoTime,
    ) {
        let shutdown_actions = self.core.on_shutdown(reason, failed_write_id, now);
        if self.apply_action_batch(shutdown_actions, None).is_err() {
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
        for (_, pending) in pending {
            pending
                .completion
                .complete(pending.kind.connection_lost_result());
        }
        while let Some(accepted) = self.accepted_commands.pop_front() {
            accepted
                .completion
                .complete(accepted.kind.completion_kind().connection_lost_result());
        }
    }
}
