//! Deterministic B1 fake ports and one-input-at-a-time Driver harness.
//!
//! The fakes share one ordered trace so tests can prove ordering across Writer
//! admission, state publication, command completion, and transport close without
//! threads, sleeps, sockets, or an asynchronous runtime.

use std::{
    cell::RefCell,
    collections::{BTreeSet, VecDeque},
    rc::Rc,
    sync::mpsc::{self, Receiver, Sender},
    time::Duration,
};

use crate::hsms::{
    api::ControlIntent,
    core::{CoreCommandResult, SessionCore, SessionCoreConfig},
    error::OperationError,
    generation::transport::writer::{OutboundFrame, WriteAdmissionError, WriterIngress},
    lifecycle::SessionState,
    model::{
        ids::{SessionId, SystemBytes, WireSequence, WriteId},
        runtime::{GenerationCloseReason, MonoTime, WriteOutcome},
    },
    protocol::{
        header::{ControlMessage, RejectReason},
        message::ProtocolMessage,
    },
};

use super::{CommandCompletion, SessionDriver, SessionStateObserver, TransportCloser};

/// One cross-component event retained in deterministic Driver execution order.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum TraceEvent {
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

/// Synchronous Writer fake with controllable admission failures and order.
#[derive(Debug)]
pub(super) struct FakeWriter {
    /// Next sequence number allocated by a successful admission.
    next_sequence: Option<u64>,
    /// Successfully admitted frames in total wire order.
    admitted: Vec<AdmittedFrame>,
    /// Failures consumed before the next otherwise successful admission.
    failures: VecDeque<WriteAdmissionError>,
    /// Write identities for which the fake already emitted the unique outcome.
    outcomes: BTreeSet<WriteId>,
    /// Global execution trace shared with the other fake ports.
    trace: SharedTrace,
}

impl FakeWriter {
    /// Creates an empty Writer whose first successful sequence is zero.
    pub(super) fn new(trace: SharedTrace) -> Self {
        Self {
            next_sequence: Some(0),
            admitted: Vec::new(),
            failures: VecDeque::new(),
            outcomes: BTreeSet::new(),
            trace,
        }
    }

    /// Injects one immediate failure for the next admission attempt.
    pub(super) fn fail_next_with(&mut self, error: WriteAdmissionError) {
        self.failures.push_back(error);
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
        self.trace
            .push(TraceEvent::WriterOutcome { write_id, outcome });
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
    /// Accepts one frame with total order or rejects it without Writer state.
    fn try_admit(&mut self, frame: OutboundFrame) -> Result<WireSequence, WriteAdmissionError> {
        if let Some(error) = self.failures.pop_front() {
            self.trace.push(TraceEvent::WriterRejected {
                write_id: frame.write_id(),
                error,
            });
            return Err(error);
        }
        let value = self
            .next_sequence
            .expect("fake wire-sequence exhaustion must be explicit in a test");
        self.next_sequence = value.checked_add(1);
        let sequence = WireSequence::new(value);
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
    /// One terminal generation failure passed to Core shutdown.
    TerminalFault(GenerationCloseReason),
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

    /// Queues one terminal generation failure.
    pub(super) fn push_terminal_fault(&mut self, reason: GenerationCloseReason) {
        self.inputs
            .push_back(FakeReaderInput::TerminalFault(reason));
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

    /// Advances logical time with checked arithmetic.
    pub(super) fn advance(&mut self, duration: Duration) {
        self.now = self
            .now
            .checked_add(duration)
            .expect("fake clock advance must not overflow");
    }
}

/// Completion endpoint backed by a test receiver and shared trace.
#[derive(Debug)]
pub(super) struct FakeCompletion {
    /// Stable label identifying the command in trace assertions.
    label: &'static str,
    /// Sender whose receiver may deliberately be dropped by a test.
    sender: Sender<CoreCommandResult>,
    /// Global execution trace shared with the other fake ports.
    trace: SharedTrace,
}

impl FakeCompletion {
    /// Creates a completion endpoint and its single-consumer receiver.
    pub(super) fn channel(
        label: &'static str,
        trace: SharedTrace,
    ) -> (Self, Receiver<CoreCommandResult>) {
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
    fn complete(self, result: CoreCommandResult) {
        let CoreCommandResult::Control(control_result) = &result;
        self.trace.push(TraceEvent::CommandCompleted {
            label: self.label,
            result: control_result.clone(),
        });
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
        let trace = SharedTrace::default();
        let session_id = SessionId::new(7).expect("fixture Session ID must be valid");
        let core = SessionCore::new(SessionCoreConfig::new(session_id));
        let driver = SessionDriver::new(
            core,
            FakeWriter::new(trace.clone()),
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
    ) -> Receiver<CoreCommandResult> {
        let (completion, receiver) = FakeCompletion::channel(label, self.trace.clone());
        self.driver
            .try_accept_control(intent, completion)
            .unwrap_or_else(|_| panic!("test command {label} must be accepted"));
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
                Some(FakeReaderInput::TerminalFault(reason)) => {
                    self.driver.on_shutdown(reason, None, now);
                    true
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
