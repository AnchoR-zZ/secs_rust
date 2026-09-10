//! Deterministic B1/B2 fake ports and one-input-at-a-time Driver harness.
//!
//! The fakes share one ordered trace so tests can prove ordering across Writer
//! admission, state publication, command completion, and transport close without
//! threads, sleeps, sockets, or an asynchronous runtime.

use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
    rc::Rc,
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, Sender},
    },
};

use crate::hsms::{
    api::{ControlIntent, PrimaryMessage, SecondaryMessage, SendReceipt},
    core::{SessionCore, SessionCoreConfig},
    error::OperationError,
    generation::transport::writer::{
        DataPermitError, DataReserveError, OutboundFrame, ReservedDataAdmissionError,
        WriteAdmissionError, WriterIngress,
    },
    lifecycle::SessionState,
    model::{
        ids::{ConnectionGeneration, SessionId, SystemBytes, WireSequence, WriteId},
        runtime::{GenerationCloseReason, MonoTime, WriteOutcome},
    },
    protocol::{
        header::{ControlMessage, RejectReason},
        message::ProtocolMessage,
    },
};

use super::{
    CommandCompletion, DriverCommandResult, SessionDriver, SessionStateObserver, TransportCloser,
};

/// Next process-local FakeWriter identity used to reject cross-writer permits.
static NEXT_FAKE_WRITER_ID: AtomicU64 = AtomicU64::new(1);

/// One cross-component event retained in deterministic Driver execution order.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum TraceEvent {
    /// Reply failed before Core and returned its exclusive inputs to the caller.
    ReplyRejected {
        /// Stable completion label used for order assertions.
        label: &'static str,
        /// Attempted reply operation without token identity or body contents.
        intent: crate::hsms::ReplyIntent,
        /// Exact pre-Core failure category.
        error: OperationError,
    },
    /// Fake Writer reserved one Data-lane slot before Core execution.
    DataPermitReserved {
        /// Fake-local reservation identity used to prove unique retirement.
        reservation_id: u64,
    },
    /// Fake Writer rejected one pre-Core Data reservation attempt.
    DataPermitRejected {
        /// Stable Full or Closed reservation failure.
        error: DataReserveError,
    },
    /// Fake Writer consumed one permit while admitting its sole Data frame.
    DataPermitConsumed {
        /// Fake-local reservation identity retired by Data admission.
        reservation_id: u64,
        /// Core-assigned identity of the admitted Data frame.
        write_id: WriteId,
    },
    /// Fake Writer released an unused permit after Core emitted no Data frame.
    DataPermitReleased {
        /// Fake-local reservation identity retired by release.
        reservation_id: u64,
    },
    /// Writer synchronously accepted a semantic frame and assigned wire order.
    WriterAdmitted {
        /// Core-assigned identity of the accepted frame.
        write_id: WriteId,
        /// Writer-assigned generation-local total order.
        sequence: WireSequence,
        /// Complete semantic message transferred to Writer ownership.
        message: ProtocolMessage,
    },
    /// Writer rejected a frame without taking ownership or assigning order.
    WriterRejected {
        /// Core-assigned identity rejected at ingress.
        write_id: WriteId,
        /// Immediate admission failure injected by the fake.
        error: WriteAdmissionError,
    },
    /// Writer rejected a Data frame after its slot had been reserved.
    ReservedDataRejected {
        /// Core-assigned identity rejected at reserved Data ingress.
        write_id: WriteId,
        /// Closed or invariant post-reservation failure injected by the fake.
        error: ReservedDataAdmissionError,
    },
    /// Fake Writer reported the unique terminal fact for an admitted frame.
    WriterOutcome {
        /// Core-assigned identity whose terminal fact became available.
        write_id: WriteId,
        /// Exact committed, not-written, or indeterminate outcome.
        outcome: WriteOutcome,
    },
    /// A deliberately invalid callback injected to test Driver fail-closed behavior.
    InvalidWriterOutcomeInjected {
        /// Write identity that a conforming Writer would not report this way.
        write_id: WriteId,
        /// Terminal fact deliberately injected outside the normal fake seam.
        outcome: WriteOutcome,
    },
    /// Driver received a peer Reject, including trace-only diagnostics.
    RejectReceived {
        /// Exact Control Session ID copied from the peer Reject.
        session_id: u16,
        /// Header Byte 2 interpreted according to the Reject reason.
        header_byte_2: u8,
        /// Exact base or extension Reject reason received from the peer.
        reason: RejectReason,
        /// Exact System Bytes copied from the peer Reject.
        system_bytes: SystemBytes,
    },
    /// Driver received Separate with a nonstandard Control Session ID.
    NonstandardSeparateReceived {
        /// Exact non-`0xFFFF` Session ID retained for diagnostics.
        session_id: u16,
        /// Exact System Bytes carried by the peer Separate.
        system_bytes: SystemBytes,
    },
    /// Driver published one committed selection-state transition.
    StateObserved(SessionState),
    /// Driver consumed one completion endpoint with its terminal control result.
    CommandCompleted {
        /// Stable test label identifying the accepted command.
        label: &'static str,
        /// Exact success or stable operation error delivered to the caller.
        result: Result<(), OperationError>,
    },
    /// Driver consumed one Send completion endpoint with its typed result.
    SendCommandCompleted {
        /// Stable test label identifying the accepted Send command.
        label: &'static str,
        /// Exact receipt or stable operation error delivered to the caller.
        result: Result<SendReceipt, OperationError>,
    },
    /// Driver consumed one Request completion endpoint with its typed result.
    RequestCommandCompleted {
        /// Stable test label identifying the accepted Request command.
        label: &'static str,
        /// Exact matched Secondary or stable operation error delivered.
        result: Result<SecondaryMessage, OperationError>,
    },
    /// Driver performed the physical transport close.
    TransportClosed,
}

/// Cloneable ordered log shared by every fake port in one harness.
#[derive(Clone, Debug, Default)]
pub(super) struct SharedTrace {
    /// Interior-mutable event sequence owned only by the current test thread.
    events: Rc<RefCell<Vec<TraceEvent>>>,
}

impl SharedTrace {
    /// Appends one event at the end of the global fake execution order.
    fn push(&self, event: TraceEvent) {
        self.events.borrow_mut().push(event);
    }

    /// Returns a snapshot of all events in their observed order.
    pub(super) fn events(&self) -> Vec<TraceEvent> {
        self.events.borrow().clone()
    }

    /// Removes every previously retained trace event.
    pub(super) fn clear(&self) {
        self.events.borrow_mut().clear();
    }
}

/// One frame retained after successful fake Writer admission.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct AdmittedFrame {
    /// Core-assigned write identity retained for outcome injection.
    pub(super) write_id: WriteId,
    /// Writer-assigned total wire order.
    pub(super) sequence: WireSequence,
    /// Owned semantic message accepted by the fake Writer.
    pub(super) message: ProtocolMessage,
}

/// Opaque single-use proof that this fake reserved one Data-lane slot.
#[derive(Debug)]
pub(super) struct FakeDataPermit {
    /// Identity of the FakeWriter that created this permit.
    writer_id: u64,
    /// Monotonic reservation identity unique within the fake Writer.
    reservation_id: u64,
}

/// Synchronous Writer fake with controllable admission failures and order.
#[derive(Debug)]
pub(super) struct FakeWriter {
    /// Stable owner identity embedded in every permit created by this fake.
    writer_id: u64,
    /// Next sequence number allocated by a successful admission.
    next_sequence: Option<u64>,
    /// Next Data reservation identity, or `None` after exhaustion.
    next_reservation_id: Option<u64>,
    /// Currently available Control-lane queue slots.
    available_control_capacity: usize,
    /// Currently available Data-lane queue slots.
    available_data_capacity: usize,
    /// Reservations that have not yet been consumed or released.
    open_data_reservations: BTreeSet<u64>,
    /// Successfully admitted frames in total wire order.
    admitted: Vec<AdmittedFrame>,
    /// Failures consumed before the next otherwise valid Control admission.
    control_failures: VecDeque<WriteAdmissionError>,
    /// Failures consumed before the next otherwise valid Data reservation.
    data_reserve_failures: VecDeque<DataReserveError>,
    /// Failures consumed before the next otherwise valid reserved admission.
    reserved_data_failures: VecDeque<ReservedDataAdmissionError>,
    /// Write identities for which the fake already emitted the unique outcome.
    outcomes: BTreeSet<WriteId>,
    /// Global execution trace shared with the other fake ports.
    trace: SharedTrace,
}

impl FakeWriter {
    /// Creates an empty Writer with independently bounded Control and Data lanes.
    pub(super) fn with_capacities(
        trace: SharedTrace,
        control_capacity: usize,
        data_capacity: usize,
    ) -> Self {
        Self {
            writer_id: NEXT_FAKE_WRITER_ID.fetch_add(1, Ordering::Relaxed),
            next_sequence: Some(0),
            next_reservation_id: Some(0),
            available_control_capacity: control_capacity,
            available_data_capacity: data_capacity,
            open_data_reservations: BTreeSet::new(),
            admitted: Vec::new(),
            control_failures: VecDeque::new(),
            data_reserve_failures: VecDeque::new(),
            reserved_data_failures: VecDeque::new(),
            outcomes: BTreeSet::new(),
            trace,
        }
    }

    /// Injects one immediate failure for the next admission attempt.
    pub(super) fn fail_next_with(&mut self, error: WriteAdmissionError) {
        self.control_failures.push_back(error);
    }

    /// Injects one Full or Closed result for the next Data reservation attempt.
    pub(super) fn fail_next_data_reserve_with(&mut self, error: DataReserveError) {
        self.data_reserve_failures.push_back(error);
    }

    /// Injects one post-reservation failure for the next Data admission.
    pub(super) fn fail_next_reserved_data_with(&mut self, error: ReservedDataAdmissionError) {
        self.reserved_data_failures.push_back(error);
    }

    /// Returns successfully admitted frames in their assigned wire order.
    pub(super) fn admitted(&self) -> &[AdmittedFrame] {
        &self.admitted
    }

    /// Emits one valid terminal outcome for a successfully admitted write.
    ///
    /// Returns [`FakeWriterOutcomeError::NotAdmitted`] when `write_id` was
    /// never accepted and [`FakeWriterOutcomeError::Duplicate`] after the fake
    /// has already emitted that write's unique terminal fact.
    pub(super) fn emit_outcome(
        &mut self,
        write_id: WriteId,
        outcome: WriteOutcome,
    ) -> Result<(), FakeWriterOutcomeError> {
        if !self
            .admitted
            .iter()
            .any(|admitted| admitted.write_id == write_id)
        {
            return Err(FakeWriterOutcomeError::NotAdmitted);
        }
        if !self.outcomes.insert(write_id) {
            return Err(FakeWriterOutcomeError::Duplicate);
        }
        let admitted = self
            .admitted
            .iter()
            .find(|admitted| admitted.write_id == write_id)
            .expect("admitted identity was validated above");
        match admitted.message {
            ProtocolMessage::Control(_) => {
                self.available_control_capacity = self
                    .available_control_capacity
                    .checked_add(1)
                    .expect("fake Control capacity must remain bounded");
            }
            ProtocolMessage::Data(_) => {
                self.available_data_capacity = self
                    .available_data_capacity
                    .checked_add(1)
                    .expect("fake Data capacity must remain bounded");
            }
        }
        self.trace
            .push(TraceEvent::WriterOutcome { write_id, outcome });
        Ok(())
    }

    /// Allocates the next total wire sequence without wrapping.
    fn allocate_sequence(&mut self) -> Option<WireSequence> {
        let value = self.next_sequence?;
        self.next_sequence = value.checked_add(1);
        Some(WireSequence::new(value))
    }

    /// Retires a permit owned by this Writer and restores no capacity itself.
    fn take_reservation(&mut self, permit: &FakeDataPermit) -> Result<(), DataPermitError> {
        if permit.writer_id != self.writer_id
            || !self.open_data_reservations.remove(&permit.reservation_id)
        {
            return Err(DataPermitError::Invariant);
        }
        Ok(())
    }

    /// Restores one Data slot when no frame took ownership of a reservation.
    fn restore_data_capacity(&mut self) -> Result<(), DataPermitError> {
        self.available_data_capacity = self
            .available_data_capacity
            .checked_add(1)
            .ok_or(DataPermitError::Invariant)?;
        Ok(())
    }
}

/// Invalid terminal-outcome injection rejected by the conforming FakeWriter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FakeWriterOutcomeError {
    /// The fake never accepted the supplied WriteId.
    NotAdmitted,
    /// The fake already emitted the supplied WriteId's unique terminal outcome.
    Duplicate,
}

impl WriterIngress for FakeWriter {
    type DataPermit = FakeDataPermit;

    /// Reserves one Data slot independently from the Control lane.
    fn try_reserve_data(&mut self) -> Result<Self::DataPermit, DataReserveError> {
        if let Some(error) = self.data_reserve_failures.pop_front() {
            self.trace.push(TraceEvent::DataPermitRejected { error });
            return Err(error);
        }
        if self.available_data_capacity == 0 {
            self.trace.push(TraceEvent::DataPermitRejected {
                error: DataReserveError::Full,
            });
            return Err(DataReserveError::Full);
        }
        let Some(reservation_id) = self.next_reservation_id else {
            self.trace.push(TraceEvent::DataPermitRejected {
                error: DataReserveError::Closed,
            });
            return Err(DataReserveError::Closed);
        };
        self.next_reservation_id = reservation_id.checked_add(1);
        self.available_data_capacity -= 1;
        let inserted = self.open_data_reservations.insert(reservation_id);
        debug_assert!(inserted, "fresh reservation identity must be unique");
        self.trace
            .push(TraceEvent::DataPermitReserved { reservation_id });
        Ok(FakeDataPermit {
            writer_id: self.writer_id,
            reservation_id,
        })
    }

    /// Releases one unused permit and restores its reserved Data slot.
    fn release_data(&mut self, permit: Self::DataPermit) -> Result<(), DataPermitError> {
        self.take_reservation(&permit)?;
        self.restore_data_capacity()?;
        self.trace.push(TraceEvent::DataPermitReleased {
            reservation_id: permit.reservation_id,
        });
        Ok(())
    }

    /// Consumes one permit while admitting exactly one Data frame.
    fn admit_reserved_data(
        &mut self,
        permit: Self::DataPermit,
        frame: OutboundFrame,
    ) -> Result<WireSequence, ReservedDataAdmissionError> {
        self.take_reservation(&permit)
            .map_err(|DataPermitError::Invariant| ReservedDataAdmissionError::Invariant)?;
        if !matches!(frame.message(), ProtocolMessage::Data(_)) {
            self.restore_data_capacity()
                .map_err(|DataPermitError::Invariant| ReservedDataAdmissionError::Invariant)?;
            return Err(ReservedDataAdmissionError::Invariant);
        }
        if let Some(error) = self.reserved_data_failures.pop_front() {
            self.restore_data_capacity()
                .map_err(|DataPermitError::Invariant| ReservedDataAdmissionError::Invariant)?;
            self.trace.push(TraceEvent::ReservedDataRejected {
                write_id: frame.write_id(),
                error,
            });
            return Err(error);
        }
        let Some(sequence) = self.allocate_sequence() else {
            self.restore_data_capacity()
                .map_err(|DataPermitError::Invariant| ReservedDataAdmissionError::Invariant)?;
            return Err(ReservedDataAdmissionError::Invariant);
        };
        let (write_id, message) = frame.into_parts();
        self.trace.push(TraceEvent::DataPermitConsumed {
            reservation_id: permit.reservation_id,
            write_id,
        });
        self.trace.push(TraceEvent::WriterAdmitted {
            write_id,
            sequence,
            message: message.clone(),
        });
        self.admitted.push(AdmittedFrame {
            write_id,
            sequence,
            message,
        });
        Ok(sequence)
    }

    /// Accepts one frame with total order or rejects it without Writer state.
    fn try_admit(&mut self, frame: OutboundFrame) -> Result<WireSequence, WriteAdmissionError> {
        if !matches!(frame.message(), ProtocolMessage::Control(_)) {
            return Err(WriteAdmissionError::Invariant);
        }
        if let Some(error) = self.control_failures.pop_front() {
            self.trace.push(TraceEvent::WriterRejected {
                write_id: frame.write_id(),
                error,
            });
            return Err(error);
        }
        if self.available_control_capacity == 0 {
            self.trace.push(TraceEvent::WriterRejected {
                write_id: frame.write_id(),
                error: WriteAdmissionError::Full,
            });
            return Err(WriteAdmissionError::Full);
        }
        let Some(sequence) = self.allocate_sequence() else {
            return Err(WriteAdmissionError::Invariant);
        };
        self.available_control_capacity -= 1;
        let (write_id, message) = frame.into_parts();
        self.trace.push(TraceEvent::WriterAdmitted {
            write_id,
            sequence,
            message: message.clone(),
        });
        self.admitted.push(AdmittedFrame {
            write_id,
            sequence,
            message,
        });
        Ok(sequence)
    }
}

/// FIFO input emitted by the complete-message Reader fake.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum FakeReaderInput {
    /// One complete semantic message requiring no byte framing.
    Message(ProtocolMessage),
}

/// FIFO Reader fake that never parses bytes or owns protocol state.
#[derive(Debug, Default)]
pub(super) struct FakeReader {
    /// Complete messages and terminal faults waiting for Driver delivery.
    inputs: VecDeque<FakeReaderInput>,
}

impl FakeReader {
    /// Queues one complete semantic peer message.
    pub(super) fn push_message(&mut self, message: ProtocolMessage) {
        self.inputs.push_back(FakeReaderInput::Message(message));
    }

    /// Removes the oldest queued Reader input.
    fn pop(&mut self) -> Option<FakeReaderInput> {
        self.inputs.pop_front()
    }
}

/// Infallible state-observer fake writing into the shared trace.
#[derive(Clone, Debug)]
pub(super) struct FakeStateObserver {
    /// Global execution trace shared with Writer, completion, and close fakes.
    trace: SharedTrace,
}

impl FakeStateObserver {
    /// Creates an observer attached to `trace`.
    fn new(trace: SharedTrace) -> Self {
        Self { trace }
    }
}

impl SessionStateObserver for FakeStateObserver {
    /// Records one committed selection state without a fallible side effect.
    fn observe(&mut self, state: SessionState) {
        self.trace.push(TraceEvent::StateObserved(state));
    }
}

/// Explicit generation-local logical clock used without sleeping.
#[derive(Clone, Copy, Debug)]
pub(super) struct FakeClock {
    /// Current monotonic value injected at the next Driver round.
    now: MonoTime,
}

impl Default for FakeClock {
    /// Creates a fake clock at the generation-local epoch.
    fn default() -> Self {
        Self {
            now: MonoTime::ZERO,
        }
    }
}

impl FakeClock {
    /// Returns the current logical clock value.
    pub(super) const fn now(self) -> MonoTime {
        self.now
    }

    /// Sets the logical time directly, including regressions used by tests.
    pub(super) fn set(&mut self, now: MonoTime) {
        self.now = now;
    }
}

/// Completion endpoint backed by a test receiver and shared trace.
#[derive(Debug)]
pub(super) struct FakeCompletion {
    /// Stable label identifying the command in trace assertions.
    label: &'static str,
    /// Sender whose receiver may deliberately be dropped by a test.
    sender: Sender<DriverCommandResult>,
    /// Global execution trace shared with the other fake ports.
    trace: SharedTrace,
}

impl FakeCompletion {
    /// Creates a completion endpoint and its single-consumer receiver.
    pub(super) fn channel(
        label: &'static str,
        trace: SharedTrace,
    ) -> (Self, Receiver<DriverCommandResult>) {
        let (sender, receiver) = mpsc::channel();
        (
            Self {
                label,
                sender,
                trace,
            },
            receiver,
        )
    }
}

impl CommandCompletion for FakeCompletion {
    /// Records and attempts the unique completion; a dropped receiver is benign.
    fn complete(self, result: DriverCommandResult) {
        let event = match &result {
            DriverCommandResult::PrimaryRejected {
                reply_expected,
                error,
                ..
            } => {
                if *reply_expected {
                    TraceEvent::RequestCommandCompleted {
                        label: self.label,
                        result: Err(error.clone()),
                    }
                } else {
                    TraceEvent::SendCommandCompleted {
                        label: self.label,
                        result: Err(error.clone()),
                    }
                }
            }
            DriverCommandResult::ReplyRejected { intent, error, .. } => TraceEvent::ReplyRejected {
                label: self.label,
                intent: *intent,
                error: error.clone(),
            },
            DriverCommandResult::Control(control_result) => TraceEvent::CommandCompleted {
                label: self.label,
                result: control_result.clone(),
            },
            DriverCommandResult::Send(send_result) => TraceEvent::SendCommandCompleted {
                label: self.label,
                result: send_result.clone(),
            },
            DriverCommandResult::Request(request_result) => TraceEvent::RequestCommandCompleted {
                label: self.label,
                result: request_result.clone(),
            },
        };
        self.trace.push(event);
        let _ = self.sender.send(result);
    }
}

/// Fake physical transport close counter attached to the shared trace.
#[derive(Clone, Debug)]
pub(super) struct FakeTransportCloser {
    /// Number of physical close calls performed by Driver.
    count: usize,
    /// Global execution trace shared with the other fake ports.
    trace: SharedTrace,
}

impl FakeTransportCloser {
    /// Creates an open fake transport attached to `trace`.
    fn new(trace: SharedTrace) -> Self {
        Self { count: 0, trace }
    }

    /// Returns the number of physical close calls observed.
    pub(super) const fn count(&self) -> usize {
        self.count
    }
}

impl TransportCloser for FakeTransportCloser {
    /// Records one physical close and increments the close counter.
    fn close(&mut self) {
        self.count += 1;
        self.trace.push(TraceEvent::TransportClosed);
    }
}

/// Concrete Driver type assembled entirely from deterministic B1 fakes.
type FakeDriver = SessionDriver<FakeWriter, FakeStateObserver, FakeCompletion, FakeTransportCloser>;

/// One explicit input consumed by [`DriverHarness::drive_one`].
pub(super) enum HarnessInput {
    /// Initial TCP-connected notification.
    Connected,
    /// Oldest already accepted control command.
    AcceptedCommand,
    /// Oldest complete message or terminal fault queued in FakeReader.
    Reader,
    /// Unique terminal outcome for one successfully admitted WriteId.
    WriteOutcome {
        /// Core-assigned write identity whose outcome became available.
        write_id: WriteId,
        /// Exact committed, not-written, or indeterminate terminal fact.
        outcome: WriteOutcome,
    },
    /// Deliberately bypasses FakeWriter validation to test defensive shutdown.
    InvalidWriteOutcome {
        /// Unknown or already completed write identity to inject.
        write_id: WriteId,
        /// Terminal fact delivered by the deliberately malformed callback.
        outcome: WriteOutcome,
    },
    /// Explicit logical-time advance round at the clock's current value.
    AdvanceTime,
    /// External shutdown input with no failed Writer admission identity.
    Shutdown(GenerationCloseReason),
}

/// Deterministic single-generation harness exposing one-input Driver rounds.
pub(super) struct DriverHarness {
    /// Real SessionDriver under test, assembled with deterministic fake ports.
    pub(super) driver: FakeDriver,
    /// Complete-message FIFO controlled directly by each scenario.
    pub(super) reader: FakeReader,
    /// Explicit logical clock injected into every Driver round.
    pub(super) clock: FakeClock,
    /// Shared total-order trace used for cross-component assertions.
    pub(super) trace: SharedTrace,
}

impl DriverHarness {
    /// Creates a harness around a real SessionCore for Data Session ID 7.
    pub(super) fn new() -> Self {
        Self::with_limits(256, 512, 1_024, 1_024)
    }

    /// Creates a harness with explicit Core and Writer lane capacities.
    pub(super) fn with_limits(
        transaction_capacity: usize,
        tombstone_capacity: usize,
        control_capacity: usize,
        data_capacity: usize,
    ) -> Self {
        let trace = SharedTrace::default();
        let session_id = SessionId::new(7).expect("fixture Session ID must be valid");
        let core = SessionCore::new(
            SessionCoreConfig::new(session_id)
                .with_transaction_capacity(transaction_capacity)
                .with_tombstone_capacity(tombstone_capacity),
        );
        let driver = SessionDriver::new(
            ConnectionGeneration::new(17),
            core,
            FakeWriter::with_capacities(trace.clone(), control_capacity, data_capacity),
            FakeStateObserver::new(trace.clone()),
            FakeTransportCloser::new(trace.clone()),
        );
        Self {
            driver,
            reader: FakeReader::default(),
            clock: FakeClock::default(),
            trace,
        }
    }

    /// Accepts one command and returns its completion receiver.
    pub(super) fn accept(
        &mut self,
        label: &'static str,
        intent: ControlIntent,
    ) -> Receiver<DriverCommandResult> {
        let (completion, receiver) = FakeCompletion::channel(label, self.trace.clone());
        self.driver
            .try_accept_control(intent, completion)
            .unwrap_or_else(|_| panic!("test command {label} must be accepted"));
        receiver
    }

    /// Accepts one outbound Send and returns its typed completion receiver.
    pub(super) fn accept_send(
        &mut self,
        label: &'static str,
        message: PrimaryMessage,
    ) -> Receiver<DriverCommandResult> {
        let (completion, receiver) = FakeCompletion::channel(label, self.trace.clone());
        self.driver
            .try_accept_send(message, completion)
            .unwrap_or_else(|_| panic!("test Send {label} must be accepted"));
        receiver
    }

    /// Accepts one outbound Request and returns its typed completion receiver.
    pub(super) fn accept_request(
        &mut self,
        label: &'static str,
        message: PrimaryMessage,
    ) -> Receiver<DriverCommandResult> {
        let (completion, receiver) = FakeCompletion::channel(label, self.trace.clone());
        self.driver
            .try_accept_request(message, completion)
            .unwrap_or_else(|_| panic!("test Request {label} must be accepted"));
        receiver
    }

    /// Consumes exactly one explicit input without polling any other source.
    pub(super) fn drive_one(&mut self, input: HarnessInput) -> bool {
        let now = self.clock.now();
        match input {
            HarnessInput::Connected => self.driver.on_connected(now),
            HarnessInput::AcceptedCommand => self.driver.drive_next_command(now),
            HarnessInput::Reader => match self.reader.pop() {
                Some(FakeReaderInput::Message(message)) => {
                    if let ProtocolMessage::Control(ControlMessage::RejectRequest {
                        session_id,
                        header_byte_2,
                        reason,
                        system_bytes,
                    }) = &message
                    {
                        self.trace.push(TraceEvent::RejectReceived {
                            session_id: *session_id,
                            header_byte_2: *header_byte_2,
                            reason: *reason,
                            system_bytes: *system_bytes,
                        });
                    }
                    if let ProtocolMessage::Control(ControlMessage::SeparateRequest {
                        session_id,
                        system_bytes,
                    }) = &message
                    {
                        if *session_id != u16::MAX {
                            self.trace.push(TraceEvent::NonstandardSeparateReceived {
                                session_id: *session_id,
                                system_bytes: *system_bytes,
                            });
                        }
                    }
                    self.driver.on_message(message, now)
                }
                None => false,
            },
            HarnessInput::WriteOutcome { write_id, outcome } => {
                self.driver
                    .writer_mut()
                    .emit_outcome(write_id, outcome)
                    .expect("normal fake outcomes require one admitted unfinished WriteId");
                self.driver.on_write_outcome(write_id, outcome, now);
                true
            }
            HarnessInput::InvalidWriteOutcome { write_id, outcome } => {
                self.trace
                    .push(TraceEvent::InvalidWriterOutcomeInjected { write_id, outcome });
                self.driver.on_write_outcome(write_id, outcome, now);
                true
            }
            HarnessInput::AdvanceTime => self.driver.advance_time(now),
            HarnessInput::Shutdown(reason) => {
                self.driver.on_shutdown(reason, None, now);
                true
            }
        }
    }
}
