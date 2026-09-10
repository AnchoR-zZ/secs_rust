//! Deterministic HSMS Session Core for B1 control and B2 Data traffic.
//!
//! This module owns selection state, control and Data transactions, bounded
//! response tombstones, generation-local identifiers, and write correlation.
//! It accepts semantic inputs synchronously and emits ordered actions without
//! depending on a runtime, transport, channel, socket, or physical clock.

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroU8,
};

use crate::hsms::{
    config::HsmsTimeouts,
    error::{OperationError, ProtocolError},
    lifecycle::SessionState,
    model::{
        ids::{CommandId, ReplyCapabilityId, SessionId, SystemBytes, WriteId},
        runtime::{CloseBarrier, GenerationCloseReason, MonoTime, WriteOutcome},
    },
    protocol::{
        header::{ControlMessage, DataHeader, RejectReason, SelectStatus},
        message::{DataMessage, ProtocolMessage},
    },
};

mod deselect;
mod inbound;
mod timing;

use super::{
    transaction::{matches_data_reject, ResponseContract, ResponseMatch, TransactionTombstones},
    CommittedWrite, CoreAction, CoreActions, CoreCommand, CoreCommandKind, CoreCommandResult,
    MatchedSecondary, OutboundPrimary,
};

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
/// B2 Data transaction capacity matching `EndpointLimits::default()`.
const DEFAULT_TRANSACTION_CAPACITY: usize = 256;
/// B2 tombstone capacity matching `EndpointLimits::default()`.
const DEFAULT_TOMBSTONE_CAPACITY: usize = 512;

/// Immutable configuration retained by one generation-local Session Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionCoreConfig {
    /// Data Session ID copied into locally generated Data headers.
    session_id: SessionId,
    /// Maximum number of concurrently pending outbound Data requests.
    transaction_capacity: usize,
    /// Maximum number of completed response contracts retained in FIFO order.
    tombstone_capacity: usize,
    /// Protocol reply, control, selection and idle-probe timer policy.
    timeouts: HsmsTimeouts,
    /// Maximum live inbound W=1 response contracts retained by this generation.
    reply_capacity: usize,
    /// Absolute local Deselect drain bound, separate from the peer T6 timer.
    drain_timeout: std::time::Duration,
}

impl SessionCoreConfig {
    /// Copies every Core-owned timer and registry bound from endpoint policy.
    pub(crate) fn from_endpoint(config: &crate::hsms::EndpointConfig) -> Self {
        Self::new(config.session_id())
            .with_timeouts(config.timeouts())
            .with_transaction_capacity(config.limits().transaction_capacity())
            .with_tombstone_capacity(config.limits().tombstone_capacity())
            .with_reply_capacity(config.limits().reply_capability_capacity())
            .with_drain_timeout(config.runtime().drain())
    }
    /// Creates a Session Core configuration for the endpoint Data `session_id`.
    pub(crate) fn new(session_id: SessionId) -> Self {
        Self {
            session_id,
            transaction_capacity: DEFAULT_TRANSACTION_CAPACITY,
            tombstone_capacity: DEFAULT_TOMBSTONE_CAPACITY,
            timeouts: HsmsTimeouts::default(),
            reply_capacity: 256,
            drain_timeout: std::time::Duration::from_secs(5),
        }
    }

    /// Returns the configured Data Session ID without exposing control allocation.
    pub(crate) const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Sets the maximum live inbound reply contracts before fail-closed pressure.
    pub(crate) const fn with_reply_capacity(mut self, capacity: usize) -> Self {
        self.reply_capacity = capacity;
        self
    }

    /// Sets the local drain interval used before sending Deselect.req.
    pub(crate) const fn with_drain_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Overrides the maximum number of concurrent outbound Data requests.
    pub(crate) const fn with_transaction_capacity(mut self, capacity: usize) -> Self {
        self.transaction_capacity = capacity;
        self
    }

    /// Overrides the number of completed response contracts retained in FIFO order.
    pub(crate) const fn with_tombstone_capacity(mut self, capacity: usize) -> Self {
        self.tombstone_capacity = capacity;
        self
    }

    /// Installs the validated protocol timer policy for this generation.
    pub(crate) const fn with_timeouts(mut self, timeouts: HsmsTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Returns the configured concurrent outbound Data-request capacity.
    pub(crate) const fn transaction_capacity(self) -> usize {
        self.transaction_capacity
    }

    /// Returns the configured completed response-contract capacity.
    pub(crate) const fn tombstone_capacity(self) -> usize {
        self.tombstone_capacity
    }
}

/// Transactional control request types supported by Session Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransactionKind {
    /// Locally initiated Deselect awaiting its exact response or peer Reject.
    Deselect,
    /// Locally initiated `Select.req` waiting for `Select.rsp` or peer Reject.
    Select,
    /// Locally initiated `Linktest.req` waiting for `Linktest.rsp` or peer Reject.
    Linktest,
}

impl TransactionKind {
    /// Returns the request SType used to attribute an inbound peer Reject.
    const fn request_stype(self) -> u8 {
        match self {
            Self::Deselect => 3,
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
    /// Application command, absent for an autonomous control procedure.
    command_id: Option<CommandId>,
    /// Fixed or copied Control Session ID required by the matcher.
    session_id: u16,
    /// Core-assigned correlation value required by the matcher.
    system_bytes: SystemBytes,
    /// Core-assigned request write retained independently of transaction completion.
    write_id: WriteId,
    /// Absolute T6 deadline, absent until the request commits locally.
    deadline: Option<MonoTime>,
}

/// One live outbound Data request and its immutable response contract.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DataTransaction {
    /// Driver-assigned request command awaiting one terminal completion.
    command_id: CommandId,
    /// Core-produced request write retained independently of command completion.
    write_id: WriteId,
    /// Full normal-Secondary and header-only F0 matching contract.
    response_contract: ResponseContract,
    /// Absolute T3 deadline, absent until the request commits locally.
    deadline: Option<MonoTime>,
}

/// Correlation tuple used to attribute a peer Reject to an outbound Data write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DataWriteCorrelation {
    /// Data Session ID copied into the outbound Primary.
    session_id: SessionId,
    /// Core-assigned System Bytes copied into the outbound Primary.
    system_bytes: SystemBytes,
}

impl DataWriteCorrelation {
    /// Returns whether a Reject header names this exact Data transaction tuple.
    fn matches(
        self,
        session_id: u16,
        header_byte_2: u8,
        reason: RejectReason,
        system_bytes: SystemBytes,
    ) -> bool {
        matches_data_reject(
            self.session_id,
            self.system_bytes,
            session_id,
            header_byte_2,
            reason,
            system_bytes,
        )
    }
}

/// Typed class retained for every Core-accepted command until completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenCommandKind {
    /// Select, Linktest, or Separate control operation.
    Control,
    /// W=0 outbound Data Primary.
    Send,
    /// W=1 outbound Data Primary awaiting a matched response.
    Request,
}

/// Protocol purpose of one Core-produced write awaiting its unique terminal outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingWriteKind {
    /// A Select or Linktest request whose `Committed` outcome starts no B1 timer.
    TransactionRequest,
    /// A W=0 Data Primary whose commit completes its Send command.
    DataSend {
        /// Reject-correlation tuple retained until the unique write outcome.
        correlation: DataWriteCorrelation,
    },
    /// A W=1 Data Primary whose response transaction may outlive its write.
    DataRequest {
        /// System Bytes selecting the corresponding live Data transaction.
        system_bytes: SystemBytes,
    },
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

/// Exact live operation selected by one attributable base-standard Reject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RejectTarget {
    /// The sole active B1 Select or Linktest control transaction.
    Control(ControlTransaction),
    /// A B2 Data request selected by its response-contract correlation.
    DataRequest(DataTransaction),
    /// A B2 Data send selected while its command is still open.
    DataSend {
        /// Driver command completed by the peer rejection.
        command_id: CommandId,
    },
}

/// Internal classification of a Data input, without public delivery semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DataInputClassification {
    /// An exact normal Secondary consumed one live request.
    MatchedSecondary,
    /// An exact header-only F0 aborted one live request.
    MatchedAbort,
    /// System Bytes selected a live request but its full contract did not match.
    LiveMismatch,
    /// An exact terminal response matched a retained completed contract.
    RetainedTombstone,
    /// No live or retained contract matched, including evicted responses.
    Unmatched,
}

/// Internal Reject classification used by the B2 deterministic test seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RejectInputClassification {
    /// An exact Reject retired one autonomous control procedure.
    MatchedAutonomousOperation,
    /// One exact live control or Data operation consumed the Reject.
    MatchedLiveOperation,
    /// An exact Reject matched a retained completed Data request.
    RetainedTombstone,
    /// No live or retained contract matched, including evicted Rejects.
    Unmatched,
    /// An extension reason has no B2 terminal operation semantics.
    ExtensionReason,
    /// Conflicting live targets prevented unique attribution.
    Ambiguous,
}

/// Stateful, runtime-neutral HSMS control and Data protocol object for one generation.
#[derive(Debug)]
pub(crate) struct SessionCore {
    /// Immutable endpoint values needed by this generation.
    config: SessionCoreConfig,
    /// Current selection state, or `None` before the first connected fact.
    state: Option<SessionState>,
    /// Most recent monotonic logical time accepted from the Driver.
    last_now: Option<MonoTime>,
    /// At most one non-semantic observation from the current inbound input round.
    notice: Option<crate::hsms::ProtocolNotice>,
    /// Absolute expiry of the current uninterrupted NotSelected tenure.
    selection_deadline: Option<MonoTime>,
    /// Latest input or local write activity, using the generation epoch.
    last_activity: Option<MonoTime>,
    /// Next idle-probe deadline, suppressed while a control transaction is open.
    idle_deadline: Option<MonoTime>,
    /// Next WriteId raw value, or `None` after the non-wrapping space is exhausted.
    next_write_id: Option<u64>,
    /// Next System Bytes raw value, or `None` after the non-wrapping space is exhausted.
    next_system_bytes: Option<u32>,
    /// Sole locally initiated Select or Linktest transaction.
    active_transaction: Option<ControlTransaction>,
    /// Pending local Deselect waiting for existing Data work to drain.
    deselect_drain: Option<deselect::DeselectDrain>,
    /// Concurrent outbound Data requests keyed by their unique System Bytes.
    data_transactions: HashMap<SystemBytes, DataTransaction>,
    /// Bounded FIFO of completed response contracts for late-input isolation.
    tombstones: TransactionTombstones,
    /// Core-produced writes retained until admission failure or one terminal outcome.
    pending_writes: HashMap<WriteId, PendingWrite>,
    /// Independent peer transaction contracts; outbound System Bytes may overlap.
    reply_contracts: BTreeMap<ReplyCapabilityId, DataHeader>,
    /// Next single-use capability identity, absent after checked exhaustion.
    next_reply_id: Option<u64>,
    /// Accepted commands that have not yet emitted their unique completion action.
    open_commands: BTreeMap<CommandId, OpenCommandKind>,
    /// Terminal protocol errors waiting for an unresolved write's visibility fact.
    deferred_errors: BTreeMap<CommandId, OperationError>,
    /// Whether protocol work must be isolated while the generation closes.
    closing: bool,
    /// Whether Core has already returned a close action on a successfully applied path.
    close_action_issued: bool,
}

impl SessionCore {
    /// Creates a disconnected Core with empty transactions and fresh identifier spaces.
    pub(crate) fn new(config: SessionCoreConfig) -> Self {
        let tombstones = TransactionTombstones::new(config.tombstone_capacity());
        Self {
            config,
            state: None,
            last_now: None,
            notice: None,
            selection_deadline: None,
            last_activity: None,
            idle_deadline: None,
            next_write_id: Some(0),
            next_system_bytes: Some(0),
            active_transaction: None,
            deselect_drain: None,
            data_transactions: HashMap::new(),
            tombstones,
            pending_writes: HashMap::new(),
            reply_contracts: BTreeMap::new(),
            next_reply_id: Some(0),
            open_commands: BTreeMap::new(),
            deferred_errors: BTreeMap::new(),
            closing: false,
            close_action_issued: false,
        }
    }

    /// Returns the committed selection state, or `None` before connection.
    pub(crate) const fn state(&self) -> Option<SessionState> {
        self.state
    }

    /// Returns the number of live outbound Data requests awaiting a response.
    #[cfg(test)]
    pub(crate) fn pending_data_transaction_count(&self) -> usize {
        self.data_transactions.len()
    }

    /// Returns the number of retained completed Data response contracts.
    #[cfg(test)]
    pub(crate) fn tombstone_count(&self) -> usize {
        self.tombstones.len()
    }

    /// Returns the number of commands accepted by Core but not yet completed.
    pub(crate) fn open_command_count(&self) -> usize {
        self.open_commands.len()
    }

    /// Reports whether Core still owns settlement of `command_id`.
    pub(crate) fn owns_command(&self, command_id: CommandId) -> bool {
        self.open_commands.contains_key(&command_id)
    }

    /// Returns the number of Core-produced writes awaiting a unique outcome.
    #[cfg(test)]
    pub(crate) fn pending_write_count(&self) -> usize {
        self.pending_writes.len()
    }

    /// Seeds non-wrapping allocators for boundary-focused fake-driver tests.
    ///
    /// `next_write_id` and `next_system_bytes` are the exact next raw values;
    /// `None` represents a permanently exhausted identifier space.
    #[cfg(test)]
    pub(crate) fn seed_identifiers(
        &mut self,
        next_write_id: Option<u64>,
        next_system_bytes: Option<u32>,
    ) {
        self.next_write_id = next_write_id;
        self.next_system_bytes = next_system_bytes;
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
        self.arm_selection_timeout(now, &mut actions);
        self.record_activity(now, &mut actions);
        if self.closing {
            return actions;
        }
        actions.push(CoreAction::SessionStateChanged(SessionState::NotSelected));
        actions
    }

    /// Applies one accepted B1 control or B2 Data command at logical time `now`.
    ///
    /// `command` carries the Driver identity and typed control or owned Data
    /// intent. The return value atomically describes all ordered
    /// frame, state, completion, and close actions for this input.
    pub(crate) fn on_command(&mut self, command: CoreCommand, now: MonoTime) -> CoreActions {
        let mut actions = CoreActions::new();
        let (command_id, kind) = command.into_parts();
        let open_kind = match &kind {
            CoreCommandKind::Select
            | CoreCommandKind::Deselect
            | CoreCommandKind::Linktest
            | CoreCommandKind::Separate => OpenCommandKind::Control,
            CoreCommandKind::Send(_) => OpenCommandKind::Send,
            CoreCommandKind::Request(_) => OpenCommandKind::Request,
        };
        if self.open_commands.contains_key(&command_id) {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        }
        self.open_commands.insert(command_id, open_kind);
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

        if self.deselect_drain.is_some()
            && matches!(
                kind,
                CoreCommandKind::Select | CoreCommandKind::Deselect | CoreCommandKind::Linktest
            )
        {
            self.complete_error(command_id, OperationError::ControlBusy, &mut actions);
            return actions;
        }

        match kind {
            CoreCommandKind::Deselect => self.start_deselect(command_id, now, &mut actions),
            CoreCommandKind::Select => self.start_select(command_id, &mut actions),
            CoreCommandKind::Linktest => self.start_linktest(command_id, &mut actions),
            CoreCommandKind::Separate => self.start_separate(
                Some(command_id),
                GenerationCloseReason::LocalSeparate,
                &mut actions,
            ),
            CoreCommandKind::Send(primary) => {
                self.start_data(command_id, primary, false, &mut actions);
            }
            CoreCommandKind::Request(primary) => {
                self.start_data(command_id, primary, true, &mut actions);
            }
        }
        actions
    }

    /// Applies one structurally validated semantic peer message at logical time `now`.
    ///
    /// B2 matches Data against live responses and retained tombstones, or
    /// delivers inbound Primaries with their optional reply authority. B1
    /// control messages retain their state and transaction handling here.
    pub(crate) fn on_message(&mut self, message: ProtocolMessage, now: MonoTime) -> CoreActions {
        self.notice = None;
        let mut actions = CoreActions::new();
        if !self.accept_time(now, &mut actions) {
            return actions;
        }
        if self.state.is_none() || self.closing {
            return actions;
        }

        self.record_activity(now, &mut actions);
        if self.closing {
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
            }) => {
                let classification = self.receive_reject(
                    session_id,
                    header_byte_2,
                    reason,
                    system_bytes,
                    &mut actions,
                );
                use crate::hsms::{
                    PeerRejectDisposition as Disposition, PeerRejectNotice, ProtocolNotice,
                };
                let disposition = match classification {
                    RejectInputClassification::MatchedLiveOperation => {
                        Disposition::OperationRejected
                    }
                    RejectInputClassification::MatchedAutonomousOperation => {
                        Disposition::AutonomousRejected
                    }
                    RejectInputClassification::RetainedTombstone => Disposition::Late,
                    RejectInputClassification::Unmatched => Disposition::Unknown,
                    RejectInputClassification::ExtensionReason => Disposition::UnsupportedExtension,
                    RejectInputClassification::Ambiguous => Disposition::Ambiguous,
                };
                self.notice = Some(ProtocolNotice::PeerReject(PeerRejectNotice::new(
                    reason,
                    disposition,
                )));
            }
            ProtocolMessage::Control(ControlMessage::SeparateRequest {
                session_id: _,
                system_bytes: _,
            }) => self.receive_separate(&mut actions),
            ProtocolMessage::Data(message) => {
                self.receive_inbound_data(message, &mut actions);
            }
            ProtocolMessage::Control(ControlMessage::DeselectRequest {
                session_id,
                system_bytes,
            }) => self.receive_deselect_request(session_id, system_bytes, now, &mut actions),
            ProtocolMessage::Control(ControlMessage::DeselectResponse {
                session_id,
                status,
                system_bytes,
            }) => {
                self.receive_deselect_response(session_id, status, system_bytes, now, &mut actions)
            }
        }
        self.progress_deselect(now, &mut actions);
        actions
    }

    /// Applies the unique terminal outcome for one successfully admitted write.
    ///
    /// `write_id` identifies the Core-produced frame and `outcome` distinguishes
    /// committed, provably unwritten, and indeterminate delivery. Unknown or
    /// duplicate identities fail closed as runtime invariants.
    #[cfg(test)]
    pub(crate) fn on_write_outcome(
        &mut self,
        write_id: WriteId,
        outcome: WriteOutcome,
        now: MonoTime,
    ) -> CoreActions {
        self.on_write_outcome_at(write_id, outcome, now, now)
    }

    /// Applies a write fact with its actual `occurred_at` and monotonic `now`.
    ///
    /// Callback latency must not extend T3/T6. Future occurrence timestamps
    /// fail closed; already expired commit-derived deadlines fire this turn.
    pub(crate) fn on_write_outcome_at(
        &mut self,
        write_id: WriteId,
        outcome: WriteOutcome,
        occurred_at: MonoTime,
        now: MonoTime,
    ) -> CoreActions {
        let mut actions = CoreActions::new();
        if !self.accept_time(now, &mut actions) {
            return actions;
        }
        if occurred_at > now {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        }
        let Some(write) = self.pending_writes.remove(&write_id) else {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        };

        if outcome == WriteOutcome::Committed && !self.closing {
            self.record_activity(occurred_at, &mut actions);
        }

        // A closing Request cannot await a peer response, but Send/Separate can
        // still prove local commit. Failed writes retain their actual visibility.
        if outcome == WriteOutcome::Committed
            && matches!(
                write.kind,
                PendingWriteKind::TransactionRequest | PendingWriteKind::DataRequest { .. }
            )
        {
            if let Some(command_id) = write.command_id {
                if let Some(error) = self.deferred_errors.remove(&command_id) {
                    self.complete_error(command_id, error, &mut actions);
                    return actions;
                }
            }
        }

        match outcome {
            WriteOutcome::Committed => match write.kind {
                PendingWriteKind::TransactionRequest => {
                    if let Some(transaction) = self.active_transaction.as_mut() {
                        if transaction.write_id == write_id {
                            transaction.deadline =
                                occurred_at.checked_add(self.config.timeouts.t6());
                            if transaction.deadline.is_none() {
                                self.fail_runtime_invariant(&mut actions);
                            }
                        }
                    }
                }
                PendingWriteKind::DataSend { correlation: _ } => {
                    if let Some(command_id) = write.command_id {
                        self.complete_send(
                            command_id,
                            Ok(CommittedWrite::new(write_id)),
                            &mut actions,
                        );
                    }
                }
                PendingWriteKind::DataRequest { system_bytes } => {
                    if let Some(transaction) = self.data_transactions.get_mut(&system_bytes) {
                        if transaction.write_id == write_id {
                            transaction.deadline =
                                occurred_at.checked_add(self.config.timeouts.t3());
                            if transaction.deadline.is_none() {
                                self.fail_runtime_invariant(&mut actions);
                            }
                        }
                    }
                }
                PendingWriteKind::Separate => {
                    if let Some(command_id) = write.command_id {
                        self.complete_control(command_id, Ok(()), &mut actions);
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
        self.expire_deadlines(now, &mut actions);
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
            self.remove_transaction_for_write(write_id, write.kind);
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

    /// Expires due protocol timers at monotonic `now` in deterministic order.
    pub(crate) fn advance_time(&mut self, now: MonoTime) -> CoreActions {
        let mut actions = CoreActions::new();
        if self.accept_time(now, &mut actions) {
            self.expire_deadlines(now, &mut actions);
        }
        actions
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
                    self.start_transaction(TransactionKind::Select, Some(command_id), actions);
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
                    self.start_transaction(TransactionKind::Linktest, Some(command_id), actions);
                }
            }
            Some(SessionState::Closing | SessionState::Closed) | None => {
                self.complete_error(command_id, OperationError::ConnectionLost, actions);
            }
        }
    }

    /// Starts local Separate, preempting live control and Data commands in order.
    fn start_separate(
        &mut self,
        command_id: Option<CommandId>,
        reason: GenerationCloseReason,
        actions: &mut CoreActions,
    ) {
        if self.state != Some(SessionState::Selected) {
            if let Some(command_id) = command_id {
                self.complete_error(command_id, OperationError::NotSelected, actions);
            }
            return;
        }
        let Some((system_bytes, write_id)) = self.allocate_request_identifiers(actions) else {
            return;
        };

        self.state = Some(SessionState::NotSelected);
        self.closing = true;
        if let Some(drain) = self.deselect_drain.take() {
            self.complete_error(
                drain.command_id,
                ProtocolError::TransactionAborted.into(),
                actions,
            );
        }
        if let Some(transaction) = self.active_transaction.take() {
            self.complete_control_transaction(
                transaction,
                Err(ProtocolError::TransactionAborted.into()),
                actions,
            );
        }
        self.complete_data_for_deselection(actions);
        self.pending_writes.insert(
            write_id,
            PendingWrite {
                kind: PendingWriteKind::Separate,
                command_id,
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
        self.request_close(reason, CloseBarrier::AfterWrite(write_id), actions);
    }

    /// Ends a finite Stop/Disconnect drain with a tail Separate when Selected.
    /// Runtime supplies the original lifecycle reason; no application command or
    /// completion is invented for this internal final frame.
    pub(crate) fn finish_shutdown_drain(
        &mut self,
        reason: GenerationCloseReason,
        now: MonoTime,
    ) -> CoreActions {
        if self.state != Some(SessionState::Selected) || self.closing {
            return self.on_shutdown(reason, None, now);
        }
        let mut actions = CoreActions::new();
        if self.accept_time(now, &mut actions) {
            self.start_separate(None, reason, &mut actions);
        }
        actions
    }

    /// Creates one active Select or Linktest response transaction and request write.
    fn start_transaction(
        &mut self,
        kind: TransactionKind,
        command_id: Option<CommandId>,
        actions: &mut CoreActions,
    ) {
        let Some((system_bytes, write_id)) = self.allocate_request_identifiers(actions) else {
            return;
        };
        let message = match kind {
            TransactionKind::Deselect => ControlMessage::DeselectRequest {
                session_id: CONTROL_SESSION_ID,
                system_bytes,
            },
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
            deadline: None,
        });
        self.pending_writes.insert(
            write_id,
            PendingWrite {
                kind: PendingWriteKind::TransactionRequest,
                command_id,
            },
        );
        actions.push(CoreAction::SendFrame {
            write_id,
            message: ProtocolMessage::Control(message),
        });
    }

    /// Validates and starts one outbound Data Send or Request.
    ///
    /// `reply_expected` fixes the W-bit and selects typed completion behavior.
    /// Invalid state, function, or request capacity completes without allocating
    /// protocol identifiers and without emitting a Data frame.
    fn start_data(
        &mut self,
        command_id: CommandId,
        primary: OutboundPrimary,
        reply_expected: bool,
        actions: &mut CoreActions,
    ) {
        if self.deselect_drain.is_some() || self.replies_blocked() {
            self.complete_error(command_id, OperationError::Draining, actions);
            return;
        }
        if self.state != Some(SessionState::Selected) {
            self.complete_error(command_id, OperationError::NotSelected, actions);
            return;
        }

        let (stream, function, body) = primary.into_parts();
        if function.get() == 0 || function.get() % 2 == 0 {
            self.complete_error(
                command_id,
                OperationError::Protocol(ProtocolError::InvalidPrimaryFunction { function }),
                actions,
            );
            return;
        }
        if reply_expected && self.data_transactions.len() >= self.config.transaction_capacity() {
            self.complete_error(command_id, OperationError::Backpressure, actions);
            return;
        }

        let Some((system_bytes, write_id)) = self.allocate_request_identifiers(actions) else {
            return;
        };
        let correlation = DataWriteCorrelation {
            session_id: self.config.session_id(),
            system_bytes,
        };
        let write_kind = if reply_expected {
            let response_contract = ResponseContract::for_request(
                self.config.session_id(),
                system_bytes,
                stream,
                function,
            );
            let transaction = DataTransaction {
                command_id,
                write_id,
                response_contract,
                deadline: None,
            };
            if self
                .data_transactions
                .insert(system_bytes, transaction)
                .is_some()
            {
                self.fail_runtime_invariant(actions);
                return;
            }
            PendingWriteKind::DataRequest { system_bytes }
        } else {
            PendingWriteKind::DataSend { correlation }
        };
        self.pending_writes.insert(
            write_id,
            PendingWrite {
                kind: write_kind,
                command_id: Some(command_id),
            },
        );
        actions.push(CoreAction::SendFrame {
            write_id,
            message: ProtocolMessage::Data(DataMessage::new(
                DataHeader::new(
                    self.config.session_id(),
                    stream,
                    function,
                    reply_expected,
                    system_bytes,
                ),
                body,
            )),
        });
    }

    /// Matches one inbound Data message against a live request or tombstone.
    ///
    /// Mismatches retain the live transaction unchanged. Exact normal and F0
    /// matches consume it, insert its full contract into the bounded FIFO, and
    /// complete the Request without waiting for the writer outcome. The return
    /// value exposes only internal classification for deterministic Core tests.
    fn receive_data(
        &mut self,
        message: DataMessage,
        actions: &mut CoreActions,
    ) -> DataInputClassification {
        let system_bytes = message.header().system_bytes();
        let Some(transaction) = self.data_transactions.get(&system_bytes).copied() else {
            return if self.tombstones.contains_message(&message) {
                DataInputClassification::RetainedTombstone
            } else {
                DataInputClassification::Unmatched
            };
        };

        match transaction.response_contract.classify(&message) {
            ResponseMatch::Mismatch => DataInputClassification::LiveMismatch,
            ResponseMatch::Normal => {
                self.data_transactions.remove(&system_bytes);
                self.tombstones.insert(transaction.response_contract);
                let (header, body) = message.into_parts();
                self.complete_request(
                    transaction.command_id,
                    Ok(MatchedSecondary::new(header, body)),
                    actions,
                );
                DataInputClassification::MatchedSecondary
            }
            ResponseMatch::Abort => {
                self.data_transactions.remove(&system_bytes);
                self.tombstones.insert(transaction.response_contract);
                self.complete_request(
                    transaction.command_id,
                    Err(OperationError::Protocol(ProtocolError::TransactionAborted)),
                    actions,
                );
                DataInputClassification::MatchedAbort
            }
        }
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
            self.selection_deadline = None;
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
                self.selection_deadline = None;
                actions.push(CoreAction::SessionStateChanged(SessionState::Selected));
            }
            self.complete_control_transaction(transaction, Ok(()), actions);
        } else {
            let status = NonZeroU8::new(status.get())
                .expect("a non-success Select status is necessarily non-zero");
            self.complete_control_transaction(
                transaction,
                Err(OperationError::SelectRejected { status }),
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
        self.complete_control_transaction(transaction, Ok(()), actions);
    }

    /// Attributes a base-standard peer Reject to exactly one live operation.
    ///
    /// Control SType and Data PType/SType interpretation remains reason-aware.
    /// Extension, unmatched, and ambiguous Rejects leave all live commands
    /// intact. The return value exposes internal classification to Core tests,
    /// without adding public events or persistent protocol state.
    fn receive_reject(
        &mut self,
        session_id: u16,
        header_byte_2: u8,
        reason: RejectReason,
        system_bytes: SystemBytes,
        actions: &mut CoreActions,
    ) -> RejectInputClassification {
        if !reason.is_base_standard() {
            return RejectInputClassification::ExtensionReason;
        }

        let mut targets = Vec::new();
        if let Some(transaction) = self.active_transaction {
            let expected_header_byte_2 = if reason == RejectReason::UNSUPPORTED_PTYPE {
                0
            } else {
                transaction.kind.request_stype()
            };
            if transaction.session_id == session_id
                && transaction.system_bytes == system_bytes
                && header_byte_2 == expected_header_byte_2
            {
                targets.push(RejectTarget::Control(transaction));
            }
        }
        for transaction in self.data_transactions.values().copied() {
            if transaction.response_contract.matches_reject(
                session_id,
                header_byte_2,
                reason,
                system_bytes,
            ) {
                targets.push(RejectTarget::DataRequest(transaction));
            }
        }
        for write in self.pending_writes.values() {
            if let PendingWriteKind::DataSend { correlation } = write.kind {
                if let Some(command_id) = write.command_id {
                    if correlation.matches(session_id, header_byte_2, reason, system_bytes) {
                        targets.push(RejectTarget::DataSend { command_id });
                    }
                }
            }
        }
        let target = match targets.as_slice() {
            [target] => *target,
            [] => {
                return if self.tombstones.contains_reject(
                    session_id,
                    header_byte_2,
                    reason,
                    system_bytes,
                ) {
                    RejectInputClassification::RetainedTombstone
                } else {
                    RejectInputClassification::Unmatched
                };
            }
            _ => return RejectInputClassification::Ambiguous,
        };

        let autonomous = matches!(
            target,
            RejectTarget::Control(ControlTransaction {
                command_id: None,
                ..
            })
        );
        match target {
            RejectTarget::Control(transaction) => {
                self.active_transaction = None;
                self.complete_control_transaction(
                    transaction,
                    Err(OperationError::PeerRejected { reason }),
                    actions,
                );
            }
            RejectTarget::DataRequest(transaction) => {
                self.data_transactions
                    .remove(&transaction.response_contract.system_bytes());
                self.tombstones.insert(transaction.response_contract);
                self.complete_error(
                    transaction.command_id,
                    OperationError::PeerRejected { reason },
                    actions,
                );
            }
            RejectTarget::DataSend { command_id } => {
                self.complete_error(command_id, OperationError::PeerRejected { reason }, actions);
            }
        }
        if autonomous {
            RejectInputClassification::MatchedAutonomousOperation
        } else {
            RejectInputClassification::MatchedLiveOperation
        }
    }

    /// Transfers this input round's diagnostic after all ordered actions apply.
    /// Driver consumes it once per inbound round; it never owns protocol work.
    pub(crate) fn take_notice(&mut self) -> Option<crate::hsms::ProtocolNotice> {
        self.notice.take()
    }

    /// Applies peer Separate in Selected and ignores it in NotSelected.
    fn receive_separate(&mut self, actions: &mut CoreActions) {
        if self.state != Some(SessionState::Selected) {
            return;
        }
        self.state = Some(SessionState::NotSelected);
        if let Some(transaction) = self.active_transaction.take() {
            self.complete_control_transaction(
                transaction,
                Err(ProtocolError::TransactionAborted.into()),
                actions,
            );
        }
        self.complete_data_for_deselection(actions);
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
        self.remove_transaction_for_write(write_id, write.kind);
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

    /// Removes the live transaction, if any, correlated with a failed write.
    ///
    /// A missing Data transaction is valid after a fast Secondary, F0, or
    /// Reject completed its command before the retained write outcome arrived.
    fn remove_transaction_for_write(&mut self, write_id: WriteId, kind: PendingWriteKind) {
        match kind {
            PendingWriteKind::TransactionRequest => {
                if self
                    .active_transaction
                    .is_some_and(|transaction| transaction.write_id == write_id)
                {
                    self.active_transaction = None;
                }
            }
            PendingWriteKind::DataRequest { system_bytes } => {
                if self
                    .data_transactions
                    .get(&system_bytes)
                    .is_some_and(|transaction| transaction.write_id == write_id)
                {
                    self.data_transactions.remove(&system_bytes);
                }
            }
            PendingWriteKind::DataSend { correlation: _ }
            | PendingWriteKind::Response
            | PendingWriteKind::Separate => {}
        }
    }

    /// Allocates a fresh tuple or emits shutdown without wrapping either ID.
    ///
    /// Exhausting the protocol's 32-bit space is normal generation retirement.
    /// Internal 64-bit WriteId exhaustion remains a local runtime failure.
    fn allocate_request_identifiers(
        &mut self,
        actions: &mut CoreActions,
    ) -> Option<(SystemBytes, WriteId)> {
        if self.next_write_id.is_none() {
            self.fail_runtime_invariant(actions);
            return None;
        }
        if self.next_system_bytes.is_none() {
            self.complete_all(OperationError::ConnectionLost, actions);
            self.request_close(
                GenerationCloseReason::SystemBytesExhausted,
                CloseBarrier::Immediate,
                actions,
            );
            return None;
        }
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

    /// Completes one control command successfully if it remains open.
    fn complete_ok(&mut self, command_id: CommandId, actions: &mut CoreActions) {
        self.complete_control(command_id, Ok(()), actions);
    }

    /// Completes one command with a result typed from its retained command kind.
    fn complete_error(
        &mut self,
        command_id: CommandId,
        error: OperationError,
        actions: &mut CoreActions,
    ) {
        let Some(kind) = self.open_commands.get(&command_id).copied() else {
            self.fail_runtime_invariant(actions);
            return;
        };
        let result = match kind {
            OpenCommandKind::Control => CoreCommandResult::Control(Err(error)),
            OpenCommandKind::Send => CoreCommandResult::Send(Err(error)),
            OpenCommandKind::Request => CoreCommandResult::Request(Err(error)),
        };
        self.emit_completion(command_id, result, actions);
    }

    /// Completes one control command with `result`, validating its retained kind.
    fn complete_control(
        &mut self,
        command_id: CommandId,
        result: Result<(), OperationError>,
        actions: &mut CoreActions,
    ) {
        if self.open_commands.get(&command_id) != Some(&OpenCommandKind::Control) {
            self.fail_runtime_invariant(actions);
            return;
        }
        self.emit_completion(command_id, CoreCommandResult::Control(result), actions);
    }

    /// Completes one Send command with Core-owned commit proof or stable error.
    fn complete_send(
        &mut self,
        command_id: CommandId,
        result: Result<CommittedWrite, OperationError>,
        actions: &mut CoreActions,
    ) {
        if self.open_commands.get(&command_id) != Some(&OpenCommandKind::Send) {
            self.fail_runtime_invariant(actions);
            return;
        }
        self.emit_completion(command_id, CoreCommandResult::Send(result), actions);
    }

    /// Completes one Request with a matched Secondary or stable error.
    fn complete_request(
        &mut self,
        command_id: CommandId,
        result: Result<MatchedSecondary, OperationError>,
        actions: &mut CoreActions,
    ) {
        if self.open_commands.get(&command_id) != Some(&OpenCommandKind::Request) {
            self.fail_runtime_invariant(actions);
            return;
        }
        self.emit_completion(command_id, CoreCommandResult::Request(result), actions);
    }

    /// Emits one unique typed completion and detaches retained write correlation.
    fn emit_completion(
        &mut self,
        command_id: CommandId,
        result: CoreCommandResult,
        actions: &mut CoreActions,
    ) {
        if self.open_commands.remove(&command_id).is_none() {
            self.fail_runtime_invariant(actions);
            return;
        }
        self.deferred_errors.remove(&command_id);
        for write in self.pending_writes.values_mut() {
            if write.command_id == Some(command_id) {
                write.command_id = None;
            }
        }
        actions.push(CoreAction::CompleteCommand { command_id, result });
    }

    /// Ends Data transactions, deferring deselection until unresolved writes settle.
    fn complete_data_for_deselection(&mut self, actions: &mut CoreActions) {
        self.data_transactions.clear();
        let command_ids: Vec<_> = self
            .open_commands
            .iter()
            .filter_map(|(command_id, kind)| {
                matches!(kind, OpenCommandKind::Send | OpenCommandKind::Request)
                    .then_some(*command_id)
            })
            .collect();
        for command_id in command_ids {
            self.complete_or_defer(command_id, OperationError::SessionDeselected, actions);
        }
    }

    /// Ends protocol transactions while retaining commands with unresolved writes.
    fn complete_all(&mut self, error: OperationError, actions: &mut CoreActions) {
        self.active_transaction = None;
        self.data_transactions.clear();
        let command_ids: Vec<_> = self.open_commands.keys().copied().collect();
        for command_id in command_ids {
            self.complete_or_defer(command_id, error.clone(), actions);
        }
    }

    /// Defers `error` if visibility is unresolved; otherwise completes immediately.
    fn complete_or_defer(
        &mut self,
        command_id: CommandId,
        error: OperationError,
        actions: &mut CoreActions,
    ) {
        if self
            .pending_writes
            .values()
            .any(|write| write.command_id == Some(command_id))
        {
            self.deferred_errors.entry(command_id).or_insert(error);
        } else {
            self.complete_error(command_id, error, actions);
        }
    }

    /// Conservatively settles missing outcomes after Writer termination is proven.
    ///
    /// Driver must first drain all actual outcomes and ensure no Writer can ever
    /// touch this generation again. Missing visibility proof is Indeterminate,
    /// never NotWritten. This operation is idempotent during terminal cleanup.
    pub(crate) fn finalize_writer(&mut self, now: MonoTime) -> CoreActions {
        let mut actions = CoreActions::new();
        if !self.accept_time(now, &mut actions) {
            return actions;
        }
        if !self.closing {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        }
        let mut writes: Vec<_> = self.pending_writes.drain().collect();
        writes.sort_unstable_by_key(|(write_id, _)| *write_id);
        for (_, write) in writes {
            if let Some(command_id) = write.command_id {
                self.complete_error(
                    command_id,
                    OperationError::DeliveryIndeterminate,
                    &mut actions,
                );
            }
        }
        self.complete_all(OperationError::ConnectionLost, &mut actions);
        actions
    }

    /// Records closing and emits the first close action returned by normal Core flow.
    fn request_close(
        &mut self,
        reason: GenerationCloseReason,
        barrier: CloseBarrier,
        actions: &mut CoreActions,
    ) {
        self.closing = true;
        self.deselect_drain = None;
        self.reply_contracts.clear();
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
    //! Direct B1 control and B2 Data state-transition tests for Session Core.

    use std::{num::NonZeroU8, time::Duration};

    use crate::hsms::{
        error::{OperationError, ProtocolError},
        lifecycle::SessionState,
        model::{
            ids::{CommandId, Function, SessionId, Stream, SystemBytes, WriteId},
            runtime::{
                CloseBarrier, GenerationCloseReason, MonoTime, TransportFault, TransportFaultKind,
                WriteOutcome,
            },
        },
        protocol::{
            header::{ControlMessage, DataHeader, RejectReason, SelectStatus},
            message::{DataMessage, ProtocolMessage},
        },
    };
    use crate::secs2::SecsItem;

    use super::{
        CommittedWrite, CoreAction, CoreActions, CoreCommand, CoreCommandKind, CoreCommandResult,
        DataInputClassification, MatchedSecondary, OutboundPrimary, PendingWriteKind,
        RejectInputClassification, SessionCore, SessionCoreConfig, CONTROL_SESSION_ID,
        LINKTEST_REQUEST_STYPE, LINKTEST_RESPONSE_STYPE, SELECT_REQUEST_STYPE,
        SELECT_RESPONSE_STYPE,
    };

    /// Creates logical time at `millis` after the generation-local epoch.
    fn now(millis: u64) -> MonoTime {
        MonoTime::from_elapsed(Duration::from_millis(millis))
    }

    /// Creates a disconnected Session Core with a valid Data Session ID.
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

    /// Builds owned Data Primary content for one test stream and function.
    fn primary(stream: u8, function: u8, body: Option<SecsItem>) -> OutboundPrimary {
        OutboundPrimary::new(
            Stream::new(stream).expect("fixture stream must be valid"),
            Function::new(function),
            body,
        )
    }

    /// Returns the unique sent Data frame from `actions`.
    fn sent_data(actions: &[CoreAction]) -> (WriteId, DataMessage) {
        let sends: Vec<_> = actions
            .iter()
            .filter_map(|action| match action {
                CoreAction::SendFrame {
                    write_id,
                    message: ProtocolMessage::Data(message),
                } => Some((*write_id, message.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(sends.len(), 1, "expected exactly one sent Data frame");
        sends
            .into_iter()
            .next()
            .expect("one Data frame was counted")
    }

    /// Builds one inbound Data candidate using explicit matcher fields.
    fn inbound_data(
        session_id: SessionId,
        stream: u8,
        function: u8,
        reply_expected: bool,
        system_bytes: SystemBytes,
        body: Option<SecsItem>,
    ) -> ProtocolMessage {
        ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                session_id,
                Stream::new(stream).expect("fixture stream must be valid"),
                Function::new(function),
                reply_expected,
                system_bytes,
            ),
            body,
        ))
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

    /// Pre-connect input is isolated; connected NotSelected handles Deselect.
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
        let response = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::DeselectRequest {
                    session_id: CONTROL_SESSION_ID,
                    system_bytes: SystemBytes::new(2),
                }),
                now(0),
            )
            .into_actions();
        assert!(matches!(response.as_slice(), [CoreAction::SendFrame {
            message: ProtocolMessage::Control(ControlMessage::DeselectResponse { status, .. }), ..
        }] if *status == crate::hsms::protocol::header::DeselectStatus::NOT_SELECTED));
    }

    /// Confirms T7 is armed, equal time is legal, and regression fails closed.
    #[test]
    fn logical_time_is_monotonic_without_b1_deadlines() {
        let mut core = core();
        connect(&mut core);

        assert_eq!(core.next_deadline(), Some(now(10_000)));
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
        assert!(core.active_transaction.unwrap().deadline.is_some());
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
        let (local_write, _) = start_select(&mut core, 50);
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
            vec![CoreAction::CloseGeneration {
                reason: GenerationCloseReason::ControlBackpressure,
                barrier: CloseBarrier::Immediate,
            },]
        );
        assert_eq!(core.open_command_count(), 1);
        assert_eq!(
            core.on_write_outcome(local_write, WriteOutcome::Committed, now(4))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(50),
                result: CoreCommandResult::Control(Err(OperationError::ConnectionLost)),
            }]
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
    fn system_bytes_exhaustion_requests_generation_retirement() {
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
                    reason: GenerationCloseReason::SystemBytesExhausted,
                    barrier: CloseBarrier::Immediate,
                },
            ]
        );
    }

    /// Shutdown retains unresolved command visibility until Writer termination.
    #[test]
    fn external_shutdown_retains_pending_command_until_writer_finalization() {
        let mut core = core();
        connect(&mut core);
        let _ = core.on_command(command(180, CoreCommandKind::Linktest), now(1));

        assert_eq!(
            core.on_shutdown(GenerationCloseReason::LocalDisconnect, None, now(2))
                .into_actions(),
            vec![CoreAction::CloseGeneration {
                reason: GenerationCloseReason::LocalDisconnect,
                barrier: CloseBarrier::Immediate,
            },]
        );
        assert_eq!(core.open_command_count(), 1);
        assert_eq!(
            core.finalize_writer(now(3)).into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(180),
                result: CoreCommandResult::Control(Err(OperationError::DeliveryIndeterminate)),
            }]
        );
        assert!(core.finalize_writer(now(3)).into_actions().is_empty());
        assert_eq!(core.pending_write_count(), 0);
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

    /// Confirms Send and Request emit owned Data with fixed W-bit policy and
    /// preserve absent versus typed-empty Message Text.
    #[test]
    fn data_commands_build_headers_and_preserve_body_ownership() {
        let mut core = core();
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        assert!(core
            .on_write_outcome(passive_write, WriteOutcome::Committed, now(2))
            .into_actions()
            .is_empty());

        let send_actions = core
            .on_command(
                command(200, CoreCommandKind::Send(primary(3, 1, None))),
                now(3),
            )
            .into_actions();
        let (send_write, send_message) = sent_data(&send_actions);
        assert_eq!(
            send_message.header().session_id(),
            SessionId::new(7).unwrap()
        );
        assert_eq!(send_message.header().stream(), Stream::new(3).unwrap());
        assert_eq!(send_message.header().function(), Function::new(1));
        assert!(!send_message.header().reply_expected());
        assert_eq!(send_message.body(), None);
        assert_eq!(
            core.on_write_outcome(send_write, WriteOutcome::Committed, now(4))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(200),
                result: CoreCommandResult::Send(Ok(CommittedWrite::new(send_write))),
            }]
        );

        let typed_empty = Some(SecsItem::List(Vec::new()));
        let request_actions = core
            .on_command(
                command(
                    201,
                    CoreCommandKind::Request(primary(4, 3, typed_empty.clone())),
                ),
                now(5),
            )
            .into_actions();
        let (_, request_message) = sent_data(&request_actions);
        assert!(request_message.header().reply_expected());
        assert_eq!(request_message.body(), typed_empty.as_ref());
    }

    /// Confirms state, primary validation, and request capacity reject before
    /// allocating protocol identifiers or producing a Data frame.
    #[test]
    fn data_preconditions_and_capacity_do_not_consume_identifiers() {
        let session_id = SessionId::new(7).unwrap();
        let mut core =
            SessionCore::new(SessionCoreConfig::new(session_id).with_transaction_capacity(1));
        connect(&mut core);
        assert_eq!(
            core.on_command(
                command(210, CoreCommandKind::Send(primary(1, 1, None))),
                now(1),
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(210),
                result: CoreCommandResult::Send(Err(OperationError::NotSelected)),
            }]
        );
        let passive_write = select_passively_at(&mut core, 2);
        assert!(core
            .on_write_outcome(passive_write, WriteOutcome::Committed, now(3))
            .into_actions()
            .is_empty());

        for (command_id, function) in [(211, 0), (212, 2)] {
            assert_eq!(
                core.on_command(
                    command(
                        command_id,
                        CoreCommandKind::Request(primary(1, function, None)),
                    ),
                    now(4),
                )
                .into_actions(),
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(command_id),
                    result: CoreCommandResult::Request(Err(OperationError::Protocol(
                        ProtocolError::InvalidPrimaryFunction {
                            function: Function::new(function),
                        },
                    ))),
                }]
            );
        }

        let first = core
            .on_command(
                command(213, CoreCommandKind::Request(primary(1, 1, None))),
                now(5),
            )
            .into_actions();
        let (_, first_message) = sent_data(&first);
        assert_eq!(first_message.header().system_bytes(), SystemBytes::new(0));
        assert_eq!(
            core.on_command(
                command(214, CoreCommandKind::Request(primary(1, 3, None))),
                now(6),
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(214),
                result: CoreCommandResult::Request(Err(OperationError::Backpressure)),
            }]
        );
        assert_eq!(core.pending_data_transaction_count(), 1);
    }

    /// Confirms a full-field mismatch leaves a request live and a later exact
    /// Secondary completes immediately while the write outcome stays tracked.
    #[test]
    fn request_mismatch_then_fast_secondary_completes_once() {
        let mut core = core();
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
        let actions = core
            .on_command(
                command(220, CoreCommandKind::Request(primary(3, 5, None))),
                now(3),
            )
            .into_actions();
        let (write_id, request) = sent_data(&actions);
        let system_bytes = request.header().system_bytes();

        assert!(core
            .on_message(
                inbound_data(SessionId::new(7).unwrap(), 3, 6, true, system_bytes, None,),
                now(4),
            )
            .into_actions()
            .is_empty());
        assert_eq!(core.pending_data_transaction_count(), 1);

        let body = Some(SecsItem::U1(Vec::new()));
        assert_eq!(
            core.on_message(
                inbound_data(
                    SessionId::new(7).unwrap(),
                    3,
                    6,
                    false,
                    system_bytes,
                    body.clone(),
                ),
                now(5),
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(220),
                result: CoreCommandResult::Request(Ok(MatchedSecondary::new(
                    DataHeader::new(
                        SessionId::new(7).unwrap(),
                        Stream::new(3).unwrap(),
                        Function::new(6),
                        false,
                        system_bytes
                    ),
                    body,
                ))),
            }]
        );
        assert_eq!(core.pending_data_transaction_count(), 0);
        assert_eq!(core.tombstone_count(), 1);
        assert_eq!(core.pending_write_count(), 1);
        assert!(core
            .on_write_outcome(write_id, WriteOutcome::Committed, now(6))
            .into_actions()
            .is_empty());
        assert_eq!(core.open_command_count(), 0);
    }

    /// Confirms F255 requests accept only header-only F0 and retain their
    /// completed contract as a tombstone; T3 is absent before write commit.
    #[test]
    fn f255_request_is_abort_only_and_waits_for_commit_before_t3() {
        let mut core = core();
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
        let actions = core
            .on_command(
                command(230, CoreCommandKind::Request(primary(9, 255, None))),
                now(3),
            )
            .into_actions();
        let (_, request) = sent_data(&actions);
        let system_bytes = request.header().system_bytes();

        assert!(core
            .on_message(
                inbound_data(
                    SessionId::new(7).unwrap(),
                    9,
                    0,
                    false,
                    system_bytes,
                    Some(SecsItem::List(Vec::new())),
                ),
                now(4),
            )
            .into_actions()
            .is_empty());
        assert!(core
            .data_transactions
            .values()
            .all(|transaction| transaction.deadline.is_none()));
        assert_eq!(core.next_deadline(), Some(now(30_004)));
        assert!(core.advance_time(now(10_000)).into_actions().is_empty());
        assert_eq!(
            core.on_message(
                inbound_data(SessionId::new(7).unwrap(), 9, 0, false, system_bytes, None,),
                now(10_001),
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(230),
                result: CoreCommandResult::Request(Err(OperationError::Protocol(
                    ProtocolError::TransactionAborted,
                ))),
            }]
        );
        assert_eq!(core.tombstone_count(), 1);
    }

    /// Confirms exact Data Rejects complete Send commands while late
    /// outcomes remain unique and extension reasons remain trace-only.
    #[test]
    fn data_reject_attribution_retains_write_outcomes() {
        let mut core = core();
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
        let actions = core
            .on_command(
                command(240, CoreCommandKind::Send(primary(2, 1, None))),
                now(3),
            )
            .into_actions();
        let (write_id, send) = sent_data(&actions);
        let reject = |reason| {
            ProtocolMessage::Control(ControlMessage::RejectRequest {
                session_id: SessionId::new(7).unwrap().get(),
                header_byte_2: 0,
                reason,
                system_bytes: send.header().system_bytes(),
            })
        };
        assert!(core
            .on_message(reject(RejectReason::new(9).unwrap()), now(4))
            .into_actions()
            .is_empty());
        assert_eq!(
            core.on_message(reject(RejectReason::TRANSACTION_NOT_OPEN), now(5))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(240),
                result: CoreCommandResult::Send(Err(OperationError::PeerRejected {
                    reason: RejectReason::TRANSACTION_NOT_OPEN,
                })),
            }]
        );
        assert!(core
            .on_write_outcome(write_id, WriteOutcome::Committed, now(6))
            .into_actions()
            .is_empty());
    }

    /// Confirms all base Reject reasons terminate only the exact live Request,
    /// free transaction capacity, retain its write, and isolate late inputs.
    #[test]
    fn request_reject_releases_capacity_and_isolates_late_inputs() {
        let session_id = SessionId::new(7).unwrap();
        for reason in [
            RejectReason::UNSUPPORTED_STYPE,
            RejectReason::UNSUPPORTED_PTYPE,
            RejectReason::TRANSACTION_NOT_OPEN,
            RejectReason::ENTITY_NOT_SELECTED,
        ] {
            let mut core =
                SessionCore::new(SessionCoreConfig::new(session_id).with_transaction_capacity(1));
            connect(&mut core);
            let passive_write = select_passively(&mut core);
            let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
            let first = core
                .on_command(
                    command(260, CoreCommandKind::Request(primary(2, 1, None))),
                    now(3),
                )
                .into_actions();
            let (first_write, first_request) = sent_data(&first);
            let first_system_bytes = first_request.header().system_bytes();
            let reject = |session_id, header_byte_2, reason, system_bytes| {
                ProtocolMessage::Control(ControlMessage::RejectRequest {
                    session_id,
                    header_byte_2,
                    reason,
                    system_bytes,
                })
            };

            for mismatched in [
                reject(8, 0, reason, first_system_bytes),
                reject(session_id.get(), 1, reason, first_system_bytes),
                reject(session_id.get(), 0, reason, SystemBytes::new(999)),
                reject(
                    session_id.get(),
                    0,
                    RejectReason::new(9).unwrap(),
                    first_system_bytes,
                ),
            ] {
                assert!(core
                    .on_message(mismatched, now(4))
                    .into_actions()
                    .is_empty());
                assert_eq!(core.pending_data_transaction_count(), 1);
                assert_eq!(core.open_command_count(), 1);
                assert_eq!(core.pending_write_count(), 1);
                assert_eq!(core.tombstone_count(), 0);
            }
            let next_identifiers = (core.next_write_id, core.next_system_bytes);
            assert_eq!(
                core.on_command(
                    command(261, CoreCommandKind::Request(primary(2, 1, None))),
                    now(5),
                )
                .into_actions(),
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(261),
                    result: CoreCommandResult::Request(Err(OperationError::Backpressure)),
                }]
            );
            assert_eq!(
                (core.next_write_id, core.next_system_bytes),
                next_identifiers
            );

            let exact_reject = reject(session_id.get(), 0, reason, first_system_bytes);
            assert_eq!(
                core.on_message(exact_reject.clone(), now(6)).into_actions(),
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(260),
                    result: CoreCommandResult::Request(Err(OperationError::PeerRejected {
                        reason
                    })),
                }]
            );
            assert_eq!(core.pending_data_transaction_count(), 0);
            assert_eq!(core.open_command_count(), 0);
            assert_eq!(core.pending_write_count(), 1);
            assert_eq!(core.tombstone_count(), 1);
            assert!(core.tombstones.contains_reject(
                session_id.get(),
                0,
                reason,
                first_system_bytes,
            ));

            let second = core
                .on_command(
                    command(262, CoreCommandKind::Request(primary(2, 1, None))),
                    now(7),
                )
                .into_actions();
            let (second_write, second_request) = sent_data(&second);
            let second_system_bytes = second_request.header().system_bytes();
            assert_ne!(first_system_bytes, second_system_bytes);
            assert_eq!(core.pending_data_transaction_count(), 1);
            assert_eq!(core.open_command_count(), 1);
            assert_eq!(core.pending_write_count(), 2);

            for late in [
                exact_reject.clone(),
                inbound_data(session_id, 2, 2, false, first_system_bytes, None),
                inbound_data(session_id, 2, 0, false, first_system_bytes, None),
            ] {
                assert!(core.on_message(late, now(8)).into_actions().is_empty());
                assert_eq!(core.pending_data_transaction_count(), 1);
                assert_eq!(core.open_command_count(), 1);
                assert_eq!(core.pending_write_count(), 2);
                assert_eq!(core.tombstone_count(), 1);
            }
            assert!(core
                .on_write_outcome(first_write, WriteOutcome::Committed, now(9))
                .into_actions()
                .is_empty());
            assert_eq!(core.pending_write_count(), 1);
            assert!(core
                .on_message(exact_reject, now(10))
                .into_actions()
                .is_empty());
            assert_eq!(core.pending_data_transaction_count(), 1);
            assert_eq!(
                core.on_message(
                    inbound_data(session_id, 2, 2, false, second_system_bytes, None),
                    now(11),
                )
                .into_actions(),
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(262),
                    result: CoreCommandResult::Request(Ok(MatchedSecondary::new(
                        DataHeader::new(
                            session_id,
                            Stream::new(2).unwrap(),
                            Function::new(2),
                            false,
                            second_system_bytes
                        ),
                        None,
                    ))),
                }]
            );
            assert!(core
                .on_write_outcome(second_write, WriteOutcome::Committed, now(12))
                .into_actions()
                .is_empty());
            assert_eq!(core.pending_write_count(), 0);
            assert_eq!(core.open_command_count(), 0);
            assert_eq!(core.tombstone_count(), 2);
            assert_eq!(
                core.on_write_outcome(first_write, WriteOutcome::Committed, now(13))
                    .into_actions(),
                vec![CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::RuntimeInvariant,
                    barrier: CloseBarrier::Immediate,
                }]
            );
        }
    }

    /// Confirms a committed Request remains Reject-attributable even though
    /// its unique Writer outcome has already removed the pending-write record.
    #[test]
    fn committed_request_reject_uses_live_transaction_contract() {
        let mut core = core();
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
        let actions = core
            .on_command(
                command(270, CoreCommandKind::Request(primary(3, 5, None))),
                now(3),
            )
            .into_actions();
        let (write_id, request) = sent_data(&actions);
        assert!(core
            .on_write_outcome(write_id, WriteOutcome::Committed, now(4))
            .into_actions()
            .is_empty());
        assert_eq!(core.pending_write_count(), 0);
        assert_eq!(core.pending_data_transaction_count(), 1);
        assert_eq!(
            core.on_message(
                ProtocolMessage::Control(ControlMessage::RejectRequest {
                    session_id: request.header().session_id().get(),
                    header_byte_2: 0,
                    reason: RejectReason::TRANSACTION_NOT_OPEN,
                    system_bytes: request.header().system_bytes(),
                }),
                now(5),
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(270),
                result: CoreCommandResult::Request(Err(OperationError::PeerRejected {
                    reason: RejectReason::TRANSACTION_NOT_OPEN,
                })),
            }]
        );
        assert_eq!(core.pending_data_transaction_count(), 0);
        assert_eq!(core.open_command_count(), 0);
        assert_eq!(core.tombstone_count(), 1);
    }

    /// Confirms the private Data seam distinguishes live mismatch, completion,
    /// retained late responses, and FIFO-evicted responses without public events.
    #[test]
    fn data_classification_distinguishes_retained_and_evicted_responses() {
        let session_id = SessionId::new(7).unwrap();
        let mut core =
            SessionCore::new(SessionCoreConfig::new(session_id).with_tombstone_capacity(1));
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
        let first = core
            .on_command(
                command(280, CoreCommandKind::Request(primary(2, 1, None))),
                now(3),
            )
            .into_actions();
        let (_, first_request) = sent_data(&first);
        let first_system_bytes = first_request.header().system_bytes();
        let candidate = |system_bytes, function, reply_expected| {
            DataMessage::new(
                DataHeader::new(
                    session_id,
                    Stream::new(2).unwrap(),
                    Function::new(function),
                    reply_expected,
                    system_bytes,
                ),
                None,
            )
        };
        let mut actions = CoreActions::new();
        assert_eq!(
            core.receive_data(candidate(first_system_bytes, 2, true), &mut actions),
            DataInputClassification::LiveMismatch,
        );
        assert!(actions.into_actions().is_empty());
        assert_eq!(core.pending_data_transaction_count(), 1);

        let first_response = candidate(first_system_bytes, 2, false);
        let mut actions = CoreActions::new();
        assert_eq!(
            core.receive_data(first_response.clone(), &mut actions),
            DataInputClassification::MatchedSecondary,
        );
        assert!(matches!(
            actions.into_actions().as_slice(),
            [CoreAction::CompleteCommand {
                result: CoreCommandResult::Request(Ok(_)),
                ..
            }]
        ));
        let mut actions = CoreActions::new();
        assert_eq!(
            core.receive_data(first_response.clone(), &mut actions),
            DataInputClassification::RetainedTombstone,
        );
        assert!(actions.into_actions().is_empty());

        let second = core
            .on_command(
                command(281, CoreCommandKind::Request(primary(2, 255, None))),
                now(4),
            )
            .into_actions();
        let (_, second_request) = sent_data(&second);
        let second_abort = candidate(second_request.header().system_bytes(), 0, false);
        let mut actions = CoreActions::new();
        assert_eq!(
            core.receive_data(second_abort.clone(), &mut actions),
            DataInputClassification::MatchedAbort,
        );
        assert!(matches!(
            actions.into_actions().as_slice(),
            [CoreAction::CompleteCommand {
                result: CoreCommandResult::Request(Err(OperationError::Protocol(
                    ProtocolError::TransactionAborted,
                ))),
                ..
            }]
        ));
        assert_eq!(core.tombstone_count(), 1);
        for (message, expected) in [
            (first_response, DataInputClassification::Unmatched),
            (second_abort, DataInputClassification::RetainedTombstone),
            (
                candidate(SystemBytes::new(999), 2, false),
                DataInputClassification::Unmatched,
            ),
        ] {
            let mut actions = CoreActions::new();
            assert_eq!(core.receive_data(message, &mut actions), expected);
            assert!(actions.into_actions().is_empty());
            assert_eq!(core.open_command_count(), 0);
            assert_eq!(core.pending_write_count(), 2);
        }
    }

    /// Confirms Reject classification distinguishes extension, live, retained,
    /// and FIFO-evicted inputs while keeping ignored inputs action-free.
    #[test]
    fn reject_classification_distinguishes_retained_and_evicted_requests() {
        let session_id = SessionId::new(7).unwrap();
        let mut core =
            SessionCore::new(SessionCoreConfig::new(session_id).with_tombstone_capacity(1));
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
        let mut completed_system_bytes = Vec::new();
        for command_id in [290, 291] {
            let request = core
                .on_command(
                    command(command_id, CoreCommandKind::Request(primary(2, 1, None))),
                    now(3),
                )
                .into_actions();
            let (_, request) = sent_data(&request);
            let system_bytes = request.header().system_bytes();
            for (header_byte_2, reason, expected) in [
                (
                    0,
                    RejectReason::new(9).unwrap(),
                    RejectInputClassification::ExtensionReason,
                ),
                (
                    1,
                    RejectReason::TRANSACTION_NOT_OPEN,
                    RejectInputClassification::Unmatched,
                ),
            ] {
                let mut actions = CoreActions::new();
                assert_eq!(
                    core.receive_reject(
                        session_id.get(),
                        header_byte_2,
                        reason,
                        system_bytes,
                        &mut actions,
                    ),
                    expected,
                );
                assert!(actions.into_actions().is_empty());
                assert_eq!(core.pending_data_transaction_count(), 1);
            }
            let mut actions = CoreActions::new();
            assert_eq!(
                core.receive_reject(
                    session_id.get(),
                    0,
                    RejectReason::TRANSACTION_NOT_OPEN,
                    system_bytes,
                    &mut actions,
                ),
                RejectInputClassification::MatchedLiveOperation,
            );
            assert_eq!(
                actions.into_actions(),
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(command_id),
                    result: CoreCommandResult::Request(Err(OperationError::PeerRejected {
                        reason: RejectReason::TRANSACTION_NOT_OPEN,
                    })),
                }]
            );
            let mut actions = CoreActions::new();
            assert_eq!(
                core.receive_reject(
                    session_id.get(),
                    0,
                    RejectReason::TRANSACTION_NOT_OPEN,
                    system_bytes,
                    &mut actions,
                ),
                RejectInputClassification::RetainedTombstone,
            );
            assert!(actions.into_actions().is_empty());
            completed_system_bytes.push(system_bytes);
        }

        assert_eq!(core.tombstone_count(), 1);
        for (system_bytes, expected) in [
            (
                completed_system_bytes[0],
                RejectInputClassification::Unmatched,
            ),
            (
                completed_system_bytes[1],
                RejectInputClassification::RetainedTombstone,
            ),
        ] {
            let mut actions = CoreActions::new();
            assert_eq!(
                core.receive_reject(
                    session_id.get(),
                    0,
                    RejectReason::TRANSACTION_NOT_OPEN,
                    system_bytes,
                    &mut actions,
                ),
                expected,
            );
            assert!(actions.into_actions().is_empty());
            assert_eq!(core.open_command_count(), 0);
            assert_eq!(core.pending_write_count(), 2);
        }
    }

    /// Confirms a deliberately conflicting private correlation is classified
    /// as ambiguous and cannot consume either live Data command.
    #[test]
    fn ambiguous_reject_classification_preserves_live_commands() {
        let mut core = core();
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
        let request = core
            .on_command(
                command(300, CoreCommandKind::Request(primary(2, 1, None))),
                now(3),
            )
            .into_actions();
        let (_, request) = sent_data(&request);
        let send = core
            .on_command(
                command(301, CoreCommandKind::Send(primary(2, 1, None))),
                now(4),
            )
            .into_actions();
        let (send_write, _) = sent_data(&send);
        let send_record = core.pending_writes.get_mut(&send_write).unwrap();
        let PendingWriteKind::DataSend { correlation } = &mut send_record.kind else {
            panic!("fixture must retain a Data Send write");
        };
        // This conflict cannot arise from the non-reusing allocator. Inject it
        // only to prove that unique-attribution protection is non-destructive.
        correlation.system_bytes = request.header().system_bytes();
        let mut actions = CoreActions::new();
        assert_eq!(
            core.receive_reject(
                request.header().session_id().get(),
                0,
                RejectReason::TRANSACTION_NOT_OPEN,
                request.header().system_bytes(),
                &mut actions,
            ),
            RejectInputClassification::Ambiguous,
        );
        assert!(actions.into_actions().is_empty());
        assert_eq!(core.open_command_count(), 2);
        assert_eq!(core.pending_data_transaction_count(), 1);
        assert_eq!(core.pending_write_count(), 2);
        assert_eq!(core.tombstone_count(), 0);
    }

    /// Separate preserves Send commit proof and defers Request deselection until commit.
    #[test]
    fn separate_preempts_data_commands_without_dropping_write_tracking() {
        let mut core = core();
        connect(&mut core);
        let passive_write = select_passively(&mut core);
        let _ = core.on_write_outcome(passive_write, WriteOutcome::Committed, now(2));
        let send = core
            .on_command(
                command(250, CoreCommandKind::Send(primary(1, 1, None))),
                now(3),
            )
            .into_actions();
        let (send_write, _) = sent_data(&send);
        let request = core
            .on_command(
                command(251, CoreCommandKind::Request(primary(1, 3, None))),
                now(4),
            )
            .into_actions();
        let (request_write, _) = sent_data(&request);

        let separate = core
            .on_command(command(252, CoreCommandKind::Separate), now(5))
            .into_actions();
        assert!(!separate
            .iter()
            .any(|action| matches!(action, CoreAction::CompleteCommand { .. })));
        assert_eq!(
            core.on_write_outcome(send_write, WriteOutcome::Committed, now(6))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(250),
                result: CoreCommandResult::Send(Ok(super::CommittedWrite::new(send_write))),
            }]
        );
        assert_eq!(
            core.on_write_outcome(request_write, WriteOutcome::Committed, now(7))
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(251),
                result: CoreCommandResult::Request(Err(OperationError::SessionDeselected)),
            }]
        );
    }
}
