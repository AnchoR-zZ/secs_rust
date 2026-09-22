//! Synchronous single-owner execution shell for one HSMS connection generation.
//!
//! The Driver serializes accepted Control/Data commands and protocol inputs into the
//! runtime-neutral [`SessionCore`], consumes each ordered action batch exactly
//! once, and owns command completion, Writer admission, and transport closing.
//! It deliberately contains no socket, task, channel, or asynchronous runtime.

use std::{
    collections::{BTreeMap, HashSet, VecDeque},
    sync::Arc,
};

use crate::hsms::{
    api::{
        ControlIntent, DataEventToken, InboundPrimary, InboundToken, MessageContext,
        PrimaryMessage, ReplyToken, SecondaryMessage, SendReceipt,
    },
    core::{CoreAction, CoreActions, CoreCommand, CoreCommandKind, CoreCommandResult, SessionCore},
    error::OperationError,
    generation::transport::writer::{
        DataPermitError, OutboundFrame, ReservedDataAdmissionError, WriteAdmissionError,
        WriterIngress,
    },
    lifecycle::SessionState,
    model::{
        ids::{CommandId, ConnectionGeneration, WriteId},
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
#[derive(Debug)]
pub(crate) enum DriverCommandResult {
    /// Pre-Core Primary failure returning the original message without cloning.
    PrimaryRejected {
        /// Original caller-owned header values and body.
        message: PrimaryMessage,
        /// Whether the caller requested a matched Secondary.
        #[cfg(test)]
        reply_expected: bool,
        /// Failure encountered before transferring ownership to Core.
        error: OperationError,
    },
    /// Pre-Core reply rejection returning every caller-owned value unchanged.
    ReplyRejected {
        /// Attempted reply operation, retained for retry or explicit disposal.
        intent: crate::hsms::ReplyIntent,
        /// Unconsumed, non-Clone reply capability.
        token: ReplyToken,
        /// Original Secondary content; absent for abort and abandonment.
        body: Option<crate::secs2::SecsItem>,
        /// Exact pre-Core failure, including structured encode/size errors.
        error: OperationError,
    },
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
    /// Receives a best-effort non-semantic observation after the full Core batch.
    fn notice(&mut self, _generation: ConnectionGeneration, _notice: crate::hsms::ProtocolNotice) {}
}

/// Synchronous boundary used when Driver has satisfied its close barrier.
pub(crate) trait TransportCloser {
    /// Closes the current generation transport exactly once.
    fn close(&mut self);
}

/// Stable reason a control intent did not enter the accepted-command queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlAdmissionErrorKind {
    /// Graceful Stop/Disconnect has closed new application control admission.
    Draining,
    /// The control intent is not represented by a Core command kind.
    UnsupportedIntent,
    /// Driver has entered closing and no longer accepts commands.
    Closing,
    /// The bounded FIFO has no remaining command slot.
    Full,
}

/// Stable reason a Data intent did not enter the accepted-command queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DataAdmissionErrorKind {
    /// Graceful Stop/Disconnect has closed new outbound Primary admission.
    Draining,
    /// Driver has entered closing and no longer accepts Data commands.
    Closing,
    /// The common command FIFO has no remaining slot.
    Full,
    /// The body cannot be represented by the endpoint's outbound wire bounds.
    Invalid(OperationError),
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
    #[cfg(test)]
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
    pub(crate) fn kind(&self) -> DataAdmissionErrorKind {
        self.kind.clone()
    }

    /// Borrows the original rejected Primary without cloning its body.
    #[cfg(test)]
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
    /// Peer response whose token remains intact until the Core admission turn.
    Reply {
        /// Normal response, header-only abort, or local capability release.
        intent: crate::hsms::ReplyIntent,
        /// Exclusive owner/generation-bound response authority.
        token: ReplyToken,
        /// Owned normal-response body awaiting Writer preflight.
        body: Option<crate::secs2::SecsItem>,
    },
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
            Self::Reply {
                intent: crate::hsms::ReplyIntent::Abandon,
                ..
            } => CompletionKind::Control,
            Self::Reply { .. } => CompletionKind::Send,
            Self::Control(_) => CompletionKind::Control,
            Self::Send(_) => CompletionKind::Send,
            Self::Request(_) => CompletionKind::Request,
        }
    }

    /// Returns whether this command requires a pre-Core Data reservation.
    const fn requires_data_permit(&self) -> bool {
        matches!(
            self,
            Self::Send(_)
                | Self::Request(_)
                | Self::Reply {
                    intent: crate::hsms::ReplyIntent::Secondary | crate::hsms::ReplyIntent::Abort,
                    ..
                }
        )
    }

    /// Borrows Message Text for pre-Core size validation and byte reservation.
    fn body(&self) -> Option<&crate::secs2::SecsItem> {
        match self {
            Self::Reply {
                intent: crate::hsms::ReplyIntent::Secondary,
                body,
                ..
            } => body.as_ref(),
            Self::Reply { .. } => None,
            Self::Send(message) | Self::Request(message) => message.body(),
            Self::Control(_) => None,
        }
    }

    /// Consumes the queued intent into Core's owned command vocabulary.
    fn into_core_kind(self) -> CoreCommandKind {
        match self {
            Self::Reply { .. } => unreachable!("reply intents enter Core via on_reply"),
            Self::Control(kind) => kind,
            Self::Send(message) => CoreCommandKind::Send(message),
            Self::Request(message) => CoreCommandKind::Request(message),
        }
    }

    /// Returns intact message or reply authority when Core has not taken ownership.
    fn rejected(self, error: OperationError) -> DriverCommandResult {
        match self {
            Self::Reply {
                intent,
                token,
                body,
            } => DriverCommandResult::ReplyRejected {
                intent,
                token,
                body,
                error,
            },
            Self::Send(message) => DriverCommandResult::PrimaryRejected {
                message,
                #[cfg(test)]
                reply_expected: false,
                error,
            },
            Self::Request(message) => DriverCommandResult::PrimaryRejected {
                message,
                #[cfg(test)]
                reply_expected: true,
                error,
            },
            Self::Control(_) => DriverCommandResult::Control(Err(error)),
        }
    }
}

/// One accepted command waiting for Driver to allocate its CommandId.
struct AcceptedCommand<Completion> {
    /// Encoded-byte reservation held until dequeue or shutdown settlement.
    byte_charge: usize,
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
    /// Application Primary delivery could not retain one more reliable event.
    InboundFull,
    /// Writer rejected ownership of one Core-produced Control frame.
    ControlWriteAdmission {
        /// Write identity rejected before the Writer accepted ownership.
        write_id: WriteId,
        /// Stable synchronous Writer rejection category.
        error: WriteAdmissionError,
    },
    /// Writer rejected a Data frame after a successful reservation.
    ReservedDataAdmission {
        /// Write identity rejected before the Writer accepted ownership.
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
    /// Private identity used by every capability minted by this generation.
    token_owner: Arc<()>,
    /// Bounded Primary queue waiting for transfer to the endpoint consumer.
    inbound: VecDeque<InboundPrimary>,
    /// Maximum reliable Primary envelopes retained by this Driver.
    inbound_capacity: usize,
    /// Reliable complete-frame protocol errors awaiting the application consumer.
    protocol_errors: VecDeque<crate::hsms::InboundProtocolError>,
    /// Independent bound that prevents malformed traffic from displacing Primaries.
    protocol_error_capacity: usize,
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
    /// Maximum accepted envelopes waiting for a synchronous Core turn.
    command_capacity: usize,
    /// Maximum aggregate encoded bytes retained by queued Data envelopes.
    command_byte_capacity: usize,
    /// Current encoded bytes charged to queued Data envelopes.
    command_bytes: usize,
    /// Stop/Disconnect admission latch; existing commands and replies keep running.
    shutdown_draining: bool,
    /// Maximum HSMS Message Length allowed during command preflight.
    maximum_message_length: usize,
    /// Typed unique completion endpoints for commands already presented to Core.
    completions: BTreeMap<CommandId, PendingCompletion<Completion>>,
    /// Successfully admitted writes awaiting terminal outcome processing.
    admitted_writes: HashSet<WriteId>,
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
    #[cfg(test)]
    pub(crate) fn new(
        generation: ConnectionGeneration,
        core: SessionCore,
        writer: Writer,
        observer: Observer,
        closer: Closer,
    ) -> Self {
        Self::with_command_capacity(
            generation,
            core,
            writer,
            observer,
            closer,
            std::num::NonZeroUsize::new(crate::hsms::EndpointLimits::default().command_capacity())
                .expect("default command capacity is nonzero"),
        )
    }

    /// Builds a fresh generation from one validated endpoint policy and its ports.
    pub(crate) fn from_config(
        generation: ConnectionGeneration,
        config: &crate::hsms::EndpointConfig,
        writer: Writer,
        observer: Observer,
        closer: Closer,
    ) -> Result<Self, crate::hsms::ConfigError> {
        config.validate()?;
        let core = SessionCore::new(crate::hsms::core::SessionCoreConfig::from_endpoint(config));
        let mut driver = Self::with_command_capacity(
            generation,
            core,
            writer,
            observer,
            closer,
            std::num::NonZeroUsize::new(config.limits().command_capacity())
                .expect("validated command capacity"),
        );
        driver.inbound_capacity = config.limits().application_event_capacity();
        driver.protocol_error_capacity = config.runtime().protocol_error_capacity();
        driver.command_byte_capacity = config.runtime().command_bytes() as usize;
        driver.maximum_message_length = config.limits().max_message_length();
        Ok(driver)
    }

    /// Creates a Driver whose common command FIFO is bounded by `capacity`.
    pub(crate) fn with_command_capacity(
        generation: ConnectionGeneration,
        core: SessionCore,
        writer: Writer,
        observer: Observer,
        closer: Closer,
        capacity: std::num::NonZeroUsize,
    ) -> Self {
        Self {
            generation,
            token_owner: Arc::new(()),
            inbound: VecDeque::new(),
            inbound_capacity: crate::hsms::EndpointLimits::default().application_event_capacity(),
            protocol_errors: VecDeque::new(),
            protocol_error_capacity: crate::hsms::EndpointLimits::default()
                .application_event_capacity(),
            core,
            writer,
            observer,
            closer,
            accepted_commands: VecDeque::new(),
            command_capacity: capacity.get(),
            command_byte_capacity: crate::hsms::RuntimePolicy::default().command_bytes() as usize,
            command_bytes: 0,
            shutdown_draining: false,
            maximum_message_length: crate::hsms::EndpointLimits::default().max_message_length(),
            completions: BTreeMap::new(),
            admitted_writes: HashSet::new(),
            next_command_id: Some(0),
            closing: None,
            transport_closed: false,
        }
    }

    /// Maps a public control intent into its implemented Core procedure.
    pub(crate) const fn core_command_kind(intent: ControlIntent) -> Option<CoreCommandKind> {
        match intent {
            ControlIntent::Select => Some(CoreCommandKind::Select),
            ControlIntent::Deselect => Some(CoreCommandKind::Deselect),
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
        if self.shutdown_draining {
            return Err(ControlAdmissionError {
                kind: ControlAdmissionErrorKind::Draining,
                intent,
                completion,
            });
        }
        if self.accepted_commands.len() >= self.command_capacity {
            return Err(ControlAdmissionError {
                kind: ControlAdmissionErrorKind::Full,
                intent,
                completion,
            });
        }
        self.accepted_commands.push_back(AcceptedCommand {
            byte_charge: 0,
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

    /// Queues reply work in the common FIFO while retaining the exclusive token.
    /// Immediate failure returns completion endpoint and all reply inputs; later
    /// pre-Core failures deliver ReplyRejected through the accepted endpoint.
    pub(crate) fn try_accept_reply(
        &mut self,
        intent: crate::hsms::ReplyIntent,
        token: ReplyToken,
        body: Option<crate::secs2::SecsItem>,
        completion: Completion,
    ) -> Result<(), (Completion, DriverCommandResult)> {
        let error = if self.closing.is_some() {
            Some(OperationError::ConnectionLost)
        } else if intent != crate::hsms::ReplyIntent::Abandon && self.core.replies_blocked() {
            Some(OperationError::Draining)
        } else if !token.belongs_to(&self.token_owner, self.generation) {
            Some(OperationError::ReplyCapabilityUnavailable)
        } else if intent == crate::hsms::ReplyIntent::Secondary
            && !token.normal_secondary_available()
        {
            Some(OperationError::ReplyRequiresAbort)
        } else if self.accepted_commands.len() >= self.command_capacity {
            Some(OperationError::Backpressure)
        } else {
            None
        };
        let kind = AcceptedCommandKind::Reply {
            intent,
            token,
            body,
        };
        if let Some(error) = error {
            return Err((completion, kind.rejected(error)));
        }
        // Charge every retained body, including one supplied to abort/abandon.
        let AcceptedCommandKind::Reply { body, .. } = &kind else {
            unreachable!()
        };
        let byte_charge = match self.command_charge(body.as_ref()) {
            Ok(charge) => charge,
            Err(error) => return Err((completion, kind.rejected(error))),
        };
        self.command_bytes += byte_charge;
        self.accepted_commands.push_back(AcceptedCommand {
            kind,
            completion,
            byte_charge,
        });
        Ok(())
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
        if self.shutdown_draining {
            return Err(DataAdmissionError {
                kind: DataAdmissionErrorKind::Draining,
                message,
                completion,
            });
        }
        if self.accepted_commands.len() >= self.command_capacity {
            return Err(DataAdmissionError {
                kind: DataAdmissionErrorKind::Full,
                message,
                completion,
            });
        }
        let byte_charge = match self.command_charge(message.body()) {
            Ok(charge) => charge,
            Err(error) => {
                return Err(DataAdmissionError {
                    kind: if error == OperationError::Backpressure {
                        DataAdmissionErrorKind::Full
                    } else {
                        DataAdmissionErrorKind::Invalid(error)
                    },
                    message,
                    completion,
                })
            }
        };
        let kind = if reply_expected {
            AcceptedCommandKind::Request(message)
        } else {
            AcceptedCommandKind::Send(message)
        };
        self.command_bytes += byte_charge;
        self.accepted_commands.push_back(AcceptedCommand {
            kind,
            completion,
            byte_charge,
        });
        Ok(())
    }

    /// Measures retained content without encoding or consuming any protocol ID.
    /// Returns the complete-frame charge or a precise validation/capacity error.
    fn command_charge(
        &self,
        body: Option<&crate::secs2::SecsItem>,
    ) -> Result<usize, OperationError> {
        use crate::hsms::profile::secs2::{Secs2Profile, StrictSecs2Profile};
        let profile = StrictSecs2Profile::new(crate::secs2::codec::Secs2Decoder::default());
        let text_length = profile.prepare_body(body)?.encoded_length();
        let message_length = text_length
            .checked_add(10)
            .filter(|length| *length <= self.maximum_message_length)
            .ok_or(OperationError::OutboundFrameTooLarge {
                text_length,
                maximum_message_length: self.maximum_message_length,
            })?;
        let charge = message_length
            .checked_add(4)
            .ok_or(OperationError::Backpressure)?;
        if charge
            > self
                .command_byte_capacity
                .saturating_sub(self.command_bytes)
        {
            return Err(OperationError::Backpressure);
        }
        Ok(charge)
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
        self.process_due_deadlines(now);
        if self.closing.is_some() {
            return false;
        }
        let Some(accepted) = self.accepted_commands.pop_front() else {
            return false;
        };
        self.command_bytes -= accepted.byte_charge;
        if matches!(
            &accepted.kind,
            AcceptedCommandKind::Reply {
                intent: crate::hsms::ReplyIntent::Secondary | crate::hsms::ReplyIntent::Abort,
                ..
            }
        ) && self.core.replies_blocked()
        {
            accepted
                .completion
                .complete(accepted.kind.rejected(OperationError::Draining));
            return true;
        }
        let completion_kind = accepted.kind.completion_kind();
        let data_permit = if accepted.kind.requires_data_permit() {
            match self.writer.try_reserve_message(accepted.kind.body()) {
                Ok(permit) => Some(permit),
                Err(OperationError::ConnectionLost) => {
                    accepted
                        .completion
                        .complete(accepted.kind.rejected(OperationError::ConnectionLost));
                    self.shutdown_after_driver_failure(
                        GenerationCloseReason::TransportLost,
                        None,
                        now,
                    );
                    self.finish_close_if_ready();
                    return true;
                }
                Err(error) => {
                    accepted.completion.complete(accepted.kind.rejected(error));
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
                .complete(accepted.kind.rejected(OperationError::ConnectionLost));
            self.fail_runtime_invariant(now);
            return true;
        };
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
        let actions = match accepted.kind {
            AcceptedCommandKind::Reply {
                intent,
                token,
                body,
            } => {
                let (_, capability, _, _) = token.into_claim();
                self.core
                    .on_reply(command_id, capability, intent, body, now)
            }
            kind => self
                .core
                .on_command(CoreCommand::new(command_id, kind.into_core_kind()), now),
        };
        self.apply_actions_with_data_permit(actions, data_permit, now);
        true
    }

    /// Presents one complete semantic peer message while Driver is running.
    ///
    /// Messages are refused after closing begins, which prevents post-close
    /// protocol transitions from entering Core.
    pub(crate) fn on_message(&mut self, message: ProtocolMessage, now: MonoTime) -> bool {
        self.process_due_deadlines(now);
        if self.closing.is_some() {
            return false;
        }
        let actions = self.core.on_message(message, now);
        self.apply_actions(actions, now);
        if let Some(notice) = self.core.take_notice() {
            self.observer.notice(self.generation, notice);
        }
        true
    }

    /// Routes one complete codec result, preserving malformed-frame context.
    /// Error reports have independent reliable capacity; overflow closes the
    /// generation instead of silently losing protocol-relevant information.
    #[cfg(test)]
    pub(crate) fn on_decode_step(
        &mut self,
        step: crate::hsms::codec::HsmsSsDecodeStep,
        now: MonoTime,
    ) -> bool {
        self.on_decode_step_with_header(step, None, now)
    }

    /// Routes decoded input with captured bytes for malformed-frame diagnostics.
    pub(crate) fn on_decode_step_with_header(
        &mut self,
        step: crate::hsms::codec::HsmsSsDecodeStep,
        raw_header: Option<[u8; 10]>,
        now: MonoTime,
    ) -> bool {
        use crate::hsms::{
            codec::HsmsSsDecodeStep, protocol::violation::InboundViolation, InboundProtocolError,
            InboundViolationKind,
        };
        if let HsmsSsDecodeStep::Message(message) = step {
            return self.on_message(message, now);
        }
        self.process_due_deadlines(now);
        if self.closing.is_some() {
            return false;
        }
        let (violation, context, kind, source) = match step {
            HsmsSsDecodeStep::HeaderViolation { violation } => (
                InboundViolation::Header(violation),
                MessageContext::from_header(self.generation, *violation.header().as_bytes()),
                InboundViolationKind::Header(violation.kind()),
                None,
            ),
            HsmsSsDecodeStep::PayloadViolation { violation, source } => (
                InboundViolation::Payload(violation),
                MessageContext::from_data(self.generation, violation.header()),
                InboundViolationKind::Payload(violation.kind()),
                Some(source),
            ),
            HsmsSsDecodeStep::NeedMore(_) => {
                self.fail_runtime_invariant(now);
                return false;
            }
            HsmsSsDecodeStep::Message(_) => unreachable!("valid messages handled above"),
        };
        if self.protocol_errors.len() >= self.protocol_error_capacity {
            self.on_shutdown(GenerationCloseReason::ApplicationBackpressure, None, now);
            return false;
        }
        let context = raw_header.map_or(context, |header| {
            MessageContext::from_header(self.generation, header)
        });
        self.protocol_errors
            .push_back(InboundProtocolError::new(context, kind, source));
        let actions = self.core.on_inbound_violation(violation, now);
        self.apply_actions(actions, now);
        true
    }

    /// Sets available downstream queue slots immediately before an input round.
    /// Zero rejects new delivery; retained reports must be drained after the round.
    pub(crate) fn set_delivery_capacity(&mut self, primaries: usize, errors: usize) {
        self.inbound_capacity = primaries;
        self.protocol_error_capacity = errors;
    }

    /// Transfers the oldest reliable protocol error without consuming Primary space.
    pub(crate) fn take_protocol_error(&mut self) -> Option<crate::hsms::InboundProtocolError> {
        self.protocol_errors.pop_front()
    }

    /// Presents a unique Writer terminal outcome to Core.
    ///
    /// Outcomes remain admissible while closing so an `AfterWrite` barrier can
    /// be satisfied and earlier accepted writes can reach their terminal fact.
    #[cfg(test)]
    pub(crate) fn on_write_outcome(
        &mut self,
        write_id: WriteId,
        outcome: WriteOutcome,
        now: MonoTime,
    ) {
        self.on_write_outcome_at(write_id, outcome, now, now);
    }

    /// Applies a Writer fact at `now`, retaining its actual `occurred_at` time.
    ///
    /// Full-write occurrence starts T3/T6 even when callback delivery is delayed.
    /// Terminal transport failures precede deadlines; ordinary successful writes
    /// follow already-due deadlines. Closing never blocks Writer settlement.
    pub(crate) fn on_write_outcome_at(
        &mut self,
        write_id: WriteId,
        outcome: WriteOutcome,
        occurred_at: MonoTime,
        now: MonoTime,
    ) {
        if outcome == WriteOutcome::Committed {
            self.process_due_deadlines(now);
        }
        let actions = self
            .core
            .on_write_outcome_at(write_id, outcome, occurred_at, now);
        self.apply_actions_without_finishing_close(actions, now);
        self.admitted_writes.remove(&write_id);
        self.satisfy_close_barrier(write_id);
        self.finish_close_if_ready();
    }

    /// Applies an already-due timer batch before accepting ordinary input.
    ///
    /// The comparison uses processing time, so queued responses cannot revive
    /// expired transactions. No timer batch runs during terminal cleanup.
    fn process_due_deadlines(&mut self, now: MonoTime) {
        if self.closing.is_none() && self.core.next_deadline().is_some_and(|due| due <= now) {
            let actions = self.core.advance_time(now);
            self.apply_actions(actions, now);
        }
    }

    /// Closes new Primary and application control admission for graceful shutdown.
    /// Existing queued commands, peer input and reply capabilities remain usable.
    /// Repeated calls are idempotent; only generation replacement reopens this gate.
    pub(crate) fn begin_shutdown_drain(&mut self) {
        self.shutdown_draining = true;
    }

    /// Returns whether all application work and pending wire writes have settled.
    /// The runtime bounds waiting for peer replies/capabilities with a fixed deadline.
    pub(crate) fn shutdown_drain_ready(&self) -> bool {
        self.accepted_commands.is_empty()
            && self.core.open_command_count() == 0
            && self.core.reply_capability_count() == 0
            && self.admitted_writes.is_empty()
    }

    /// Emits the internal tail Separate or closes an unselected transport.
    /// Already queued application commands are settled by the normal close path.
    pub(crate) fn finish_shutdown_drain(&mut self, reason: GenerationCloseReason, now: MonoTime) {
        self.shutdown_draining = true;
        let actions = self.core.finish_shutdown_drain(reason, now);
        // Core has already committed the lifecycle cause before emitting the
        // tail frame. Admission can fail before its Close action reaches Driver.
        if let Err(failure) = self.apply_action_batch(actions, None) {
            self.handle_action_failure_with_reason(failure, now, Some(reason));
        }
        self.finish_close_if_ready();
    }

    /// Presents a terminal shutdown reason to Core and enters fail-closed.
    ///
    /// `failed_write_id` is `Some` only for a synchronous Writer admission
    /// failure for which the Writer will not report a later outcome.
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
    /// no longer competes with terminal close cleanup.
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

    /// Transfers the oldest reliable Primary to the single application consumer.
    pub(crate) fn take_inbound(&mut self) -> Option<InboundPrimary> {
        self.inbound.pop_front()
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
    #[cfg(test)]
    pub(crate) fn pending_completion_count(&self) -> usize {
        self.completions.len()
    }

    /// Returns Core's count of live outbound Data response transactions.
    #[cfg(test)]
    pub(crate) fn pending_data_transaction_count(&self) -> usize {
        self.core.pending_data_transaction_count()
    }

    /// Returns Core's count of retained completed response contracts.
    #[cfg(test)]
    pub(crate) fn tombstone_count(&self) -> usize {
        self.core.tombstone_count()
    }

    /// Returns Core's count of accepted commands not yet completed there.
    #[cfg(test)]
    pub(crate) fn open_core_command_count(&self) -> usize {
        self.core.open_command_count()
    }

    /// Returns Core's count of writes awaiting their unique terminal outcome.
    #[cfg(test)]
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

    /// Returns whether the Writer accepted this still-pending write identity.
    pub(crate) fn has_admitted_write(&self, write_id: WriteId) -> bool {
        self.admitted_writes.contains(&write_id)
    }

    /// Borrows the Writer for generation orchestration and deterministic tests.
    #[cfg(test)]
    pub(crate) const fn writer(&self) -> &Writer {
        &self.writer
    }

    /// Mutably borrows the Writer for test-controlled outcome injection.
    #[cfg(test)]
    pub(crate) fn writer_mut(&mut self) -> &mut Writer {
        &mut self.writer
    }

    /// Borrows the transport closer for diagnostics and deterministic tests.
    #[cfg(test)]
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
                CoreAction::DeliverPrimary {
                    message,
                    capability,
                } => {
                    if self.inbound.len() >= self.inbound_capacity {
                        Err(ActionApplicationFailure::InboundFull)
                    } else {
                        let (header, body) = message.into_parts();
                        let context = MessageContext::from_data(self.generation, header);
                        let token = match capability {
                            Some(id) => InboundToken::Reply(ReplyToken::from_core(
                                self.token_owner.clone(),
                                id,
                                self.generation,
                                header.function().get() < u8::MAX,
                            )),
                            None => InboundToken::Data(DataEventToken::new()),
                        };
                        self.inbound.push_back(InboundPrimary::new(
                            PrimaryMessage::new(header.stream(), header.function(), body),
                            token,
                            context,
                        ));
                        Ok(())
                    }
                }
                CoreAction::SendFrame { write_id, message } => {
                    let frame = OutboundFrame::new(write_id, message);
                    let admission = match frame.message() {
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
                    match admission {
                        Ok(()) => (),
                        Err(failure) => {
                            application_result = Err(failure);
                            break;
                        }
                    };
                    if !self.admitted_writes.insert(write_id) {
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
                if !self.has_admitted_write(committed.write_id()) {
                    return None;
                }
                Some(DriverCommandResult::Send(Ok(SendReceipt::new(
                    self.generation,
                ))))
            }
            (CompletionKind::Send, CoreCommandResult::Send(Err(error))) => {
                Some(DriverCommandResult::Send(Err(error)))
            }
            (CompletionKind::Request, CoreCommandResult::Request(Ok(matched))) => {
                let (header, body) = matched.into_parts();
                Some(DriverCommandResult::Request(Ok(SecondaryMessage::new(
                    header.stream(),
                    header.function(),
                    body,
                    MessageContext::from_data(self.generation, header),
                ))))
            }
            (CompletionKind::Request, CoreCommandResult::Request(Err(error))) => {
                Some(DriverCommandResult::Request(Err(error)))
            }
            (CompletionKind::Request, CoreCommandResult::RequestTimedOut(header)) => Some(
                DriverCommandResult::Request(Err(OperationError::RequestTimeout {
                    context: MessageContext::from_data(self.generation, header),
                })),
            ),
            _ => None,
        }
    }

    /// Converts one action-application failure into a single Core shutdown call.
    fn handle_action_failure(&mut self, failure: ActionApplicationFailure, now: MonoTime) {
        self.handle_action_failure_with_reason(failure, now, None);
    }

    /// Preserves a lifecycle cause committed before a failed tail action batch.
    fn handle_action_failure_with_reason(
        &mut self,
        failure: ActionApplicationFailure,
        now: MonoTime,
        first_reason: Option<GenerationCloseReason>,
    ) {
        let (reason, failed_write_id) = match failure {
            ActionApplicationFailure::InboundFull => {
                (GenerationCloseReason::ApplicationBackpressure, None)
            }
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
        self.shutdown_after_driver_failure(first_reason.unwrap_or(reason), failed_write_id, now);
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

    /// Retains the first reason, allowing terminal shutdown to remove a barrier.
    fn begin_closing(&mut self, reason: GenerationCloseReason, barrier: CloseBarrier) {
        if let Some(closing) = &mut self.closing {
            if barrier == CloseBarrier::Immediate {
                closing.barrier_write_id = None;
            }
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

    /// Finalizes settlement only after Writer exit and draining its outcome FIFO.
    ///
    /// The runtime must prove task termination before calling. Any missing write
    /// outcome becomes conservatively indeterminate; queued work never wrote.
    pub(crate) fn on_writer_stopped(&mut self, now: MonoTime) {
        if self.closing.is_none() {
            self.on_shutdown(GenerationCloseReason::RuntimeInvariant, None, now);
        }
        let actions = self.core.finalize_writer(now);
        self.apply_actions_without_finishing_close(actions, now);
        self.admitted_writes.clear();
        if let Some(closing) = &mut self.closing {
            closing.barrier_write_id = None;
        }
        self.finish_close_if_ready();
        self.drain_accepted_commands();
    }

    /// Drains queued and orphaned completions, retaining Core-owned settlement.
    fn drain_accepted_commands(&mut self) {
        let orphaned: Vec<_> = self
            .completions
            .keys()
            .copied()
            .filter(|command_id| !self.core.owns_command(*command_id))
            .collect();
        for command_id in orphaned {
            let pending = self
                .completions
                .remove(&command_id)
                .expect("identified orphan remains present");
            pending
                .completion
                .complete(pending.kind.connection_lost_result());
        }
        while let Some(accepted) = self.accepted_commands.pop_front() {
            self.command_bytes -= accepted.byte_charge;
            accepted
                .completion
                .complete(accepted.kind.rejected(OperationError::ConnectionLost));
        }
    }
}
