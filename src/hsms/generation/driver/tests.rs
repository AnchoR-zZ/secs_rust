//! B1 vertical-slice tests for ordered SessionDriver execution with real Core.
//!
//! These scenarios use deterministic fake ports and explicit one-input rounds
//! to prove cross-component ordering, fail-closed behavior, write barriers, and
//! local command completion exactly once.

use std::{sync::mpsc::Receiver, time::Duration};

use crate::hsms::{
    api::ControlIntent,
    core::{CoreAction, CoreActions, CoreCommandKind, CoreCommandResult},
    error::OperationError,
    generation::transport::writer::WriteAdmissionError,
    lifecycle::SessionState,
    model::{
        ids::{CommandId, SystemBytes, WriteId},
        runtime::{
            GenerationCloseReason, MonoTime, TransportFault, TransportFaultKind, WriteOutcome,
        },
    },
    protocol::{
        header::{ControlMessage, RejectReason, SelectStatus},
        message::ProtocolMessage,
    },
};

use super::test_support::{DriverHarness, FakeWriterOutcomeError, HarnessInput, TraceEvent};
use super::{ControlAdmissionErrorKind, DriverCommandResult, SessionDriver};

/// Builds a terminal transport fault with a stable test category.
fn transport_fault() -> TransportFault {
    TransportFault::new(TransportFaultKind::BrokenPipe)
}

/// Extracts a control completion result received without blocking.
fn completion_result(receiver: &Receiver<DriverCommandResult>) -> Result<(), OperationError> {
    let result = receiver
        .try_recv()
        .expect("command must have one completion available");
    let DriverCommandResult::Control(result) = result else {
        panic!("control command must retain its Driver result type");
    };
    result
}

/// Returns the latest admitted frame identity from the fake Writer.
fn last_write_id(harness: &DriverHarness) -> WriteId {
    harness
        .driver
        .writer()
        .admitted()
        .last()
        .expect("scenario must admit one frame")
        .write_id
}

/// Extracts the Session ID and System Bytes from an admitted Select request.
fn active_select_contract(harness: &DriverHarness) -> (u16, SystemBytes, WriteId) {
    let admitted = harness
        .driver
        .writer()
        .admitted()
        .last()
        .expect("active Select must admit its request");
    let ProtocolMessage::Control(ControlMessage::SelectRequest {
        session_id,
        system_bytes,
    }) = admitted.message
    else {
        panic!("latest frame must be Select.req");
    };
    (session_id, system_bytes, admitted.write_id)
}

/// Drives a legal passive Select and commits its response write.
fn enter_selected(harness: &mut DriverHarness) {
    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(91),
        }));
    assert!(harness.drive_one(HarnessInput::Reader));
    let response_write = last_write_id(harness);
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: response_write,
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(harness.driver.state(), Some(SessionState::Selected));
}

/// Confirms public B1 intents map exactly and deferred Deselect is not accepted.
#[test]
fn control_intents_map_without_expanding_the_b1_vocabulary() {
    type Driver = SessionDriver<
        super::test_support::FakeWriter,
        super::test_support::FakeStateObserver,
        super::test_support::FakeCompletion,
        super::test_support::FakeTransportCloser,
    >;

    assert_eq!(
        Driver::core_command_kind(ControlIntent::Select),
        Some(CoreCommandKind::Select)
    );
    assert_eq!(
        Driver::core_command_kind(ControlIntent::Linktest),
        Some(CoreCommandKind::Linktest)
    );
    assert_eq!(
        Driver::core_command_kind(ControlIntent::Separate),
        Some(CoreCommandKind::Separate)
    );
    assert_eq!(Driver::core_command_kind(ControlIntent::Deselect), None);
}

/// Confirms first connected publication is unique and repetition fails closed.
#[test]
fn connected_publishes_once_and_repeated_connected_fails_closed() {
    let mut harness = DriverHarness::new();

    assert!(harness.drive_one(HarnessInput::Connected));
    assert_eq!(harness.driver.state(), Some(SessionState::NotSelected));
    assert_eq!(
        harness.trace.events(),
        vec![TraceEvent::StateObserved(SessionState::NotSelected)]
    );

    assert!(harness.drive_one(HarnessInput::Connected));
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert_eq!(harness.driver.closer().count(), 1);
    assert_eq!(
        harness
            .trace
            .events()
            .iter()
            .filter(|event| matches!(event, TraceEvent::StateObserved(_)))
            .count(),
        1
    );
}

/// Confirms active Select publishes Selected before completing its command.
#[test]
fn active_select_success_orders_state_before_completion_and_accepts_fast_response() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    let receiver = harness.accept("select", ControlIntent::Select);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (session_id, system_bytes, request_write) = active_select_contract(&harness);
    assert_eq!(session_id, u16::MAX);
    harness.trace.clear();

    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::SelectResponse {
            session_id,
            status: SelectStatus::SUCCESS,
            system_bytes,
        }));
    assert!(harness.drive_one(HarnessInput::Reader));

    assert_eq!(completion_result(&receiver), Ok(()));
    assert_eq!(
        harness.trace.events(),
        vec![
            TraceEvent::StateObserved(SessionState::Selected),
            TraceEvent::CommandCompleted {
                label: "select",
                result: Ok(()),
            },
        ]
    );
    harness.trace.clear();
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: request_write,
        outcome: WriteOutcome::Committed,
    }));
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        harness.trace.events(),
        vec![TraceEvent::WriterOutcome {
            write_id: request_write,
            outcome: WriteOutcome::Committed,
        }]
    );
}

/// Confirms passive Select response admission precedes its Selected observation.
#[test]
fn passive_select_admission_precedes_selected_observation() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    harness.trace.clear();

    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(17),
        }));
    assert!(harness.drive_one(HarnessInput::Reader));

    let events = harness.trace.events();
    assert!(matches!(events[0], TraceEvent::WriterAdmitted { .. }));
    assert_eq!(events[1], TraceEvent::StateObserved(SessionState::Selected));
    assert_eq!(harness.driver.state(), Some(SessionState::Selected));
}

/// Confirms failed passive response admission short-circuits state publication.
#[test]
fn passive_select_admission_failure_closes_without_selected_observation() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    harness.trace.clear();
    harness
        .driver
        .writer_mut()
        .fail_next_with(WriteAdmissionError::Full);
    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(19),
        }));

    assert!(harness.drive_one(HarnessInput::Reader));

    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::ControlBackpressure)
    );
    assert_eq!(harness.driver.closer().count(), 1);
    assert!(!harness
        .trace
        .events()
        .iter()
        .any(|event| matches!(event, TraceEvent::StateObserved(SessionState::Selected))));
    assert!(matches!(
        harness.trace.events().first(),
        Some(TraceEvent::WriterRejected {
            error: WriteAdmissionError::Full,
            ..
        })
    ));
}

/// Confirms a terminal response-write failure closes without retracting the
/// Selected observation already published after successful admission.
#[test]
fn passive_select_not_written_preserves_historical_selected_observation() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    harness.trace.clear();
    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(21),
        }));
    assert!(harness.drive_one(HarnessInput::Reader));
    let response_write = last_write_id(&harness);

    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: response_write,
        outcome: WriteOutcome::NotWritten(transport_fault()),
    }));

    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::TransportLost)
    );
    assert_eq!(harness.driver.closer().count(), 1);
    assert_eq!(
        harness
            .trace
            .events()
            .iter()
            .filter(|event| { **event == TraceEvent::StateObserved(SessionState::Selected) })
            .count(),
        1
    );
}

/// Confirms Full and Closed admission failures map both command completion and
/// generation close reason according to the frozen Control policy.
#[test]
fn request_admission_failures_use_frozen_completion_and_close_mapping() {
    let cases = [
        (
            WriteAdmissionError::Full,
            OperationError::Backpressure,
            GenerationCloseReason::ControlBackpressure,
        ),
        (
            WriteAdmissionError::Closed,
            OperationError::ConnectionLost,
            GenerationCloseReason::TransportLost,
        ),
    ];

    for (admission_error, operation_error, close_reason) in cases {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        let receiver = harness.accept("linktest", ControlIntent::Linktest);
        harness.driver.writer_mut().fail_next_with(admission_error);

        assert!(harness.drive_one(HarnessInput::AcceptedCommand));

        assert_eq!(completion_result(&receiver), Err(operation_error));
        assert_eq!(harness.driver.close_reason(), Some(close_reason));
        assert_eq!(harness.driver.closer().count(), 1);
    }
}

/// Confirms an admission failure for a WriteId unknown to Core locks the
/// RuntimeInvariant reason returned by Core rather than Driver's initial map.
#[test]
fn unknown_failed_write_id_locks_runtime_invariant_as_first_reason() {
    let mut harness = DriverHarness::new();
    harness
        .driver
        .writer_mut()
        .fail_next_with(WriteAdmissionError::Full);
    let mut actions = CoreActions::new();
    actions.push(CoreAction::SendFrame {
        write_id: WriteId::new(777),
        message: ProtocolMessage::Control(ControlMessage::LinktestResponse {
            system_bytes: SystemBytes::new(41),
        }),
    });

    harness.driver.apply_actions(actions, harness.clock.now());

    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert_eq!(harness.driver.closer().count(), 1);
}

/// Confirms response-frame backpressure does not misattribute Backpressure to
/// a simultaneous locally initiated Select command.
#[test]
fn simultaneous_select_response_failure_completes_local_command_as_connection_lost() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    let receiver = harness.accept("local-select", ControlIntent::Select);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    harness
        .driver
        .writer_mut()
        .fail_next_with(WriteAdmissionError::Full);
    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(23),
        }));

    assert!(harness.drive_one(HarnessInput::Reader));

    assert_eq!(
        completion_result(&receiver),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::ControlBackpressure)
    );
}

/// Confirms every Separate terminal outcome releases the barrier only after
/// completion actions have been fully applied.
#[test]
fn separate_three_way_terminal_outcomes_complete_before_transport_close() {
    let cases = [
        (WriteOutcome::Committed, Ok(())),
        (
            WriteOutcome::NotWritten(transport_fault()),
            Err(OperationError::ConnectionLost),
        ),
        (
            WriteOutcome::Indeterminate(transport_fault()),
            Err(OperationError::DeliveryIndeterminate),
        ),
    ];

    for (outcome, expected) in cases {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        enter_selected(&mut harness);
        let receiver = harness.accept("separate", ControlIntent::Separate);
        harness.trace.clear();

        assert!(harness.drive_one(HarnessInput::AcceptedCommand));
        let separate_write = last_write_id(&harness);
        assert_eq!(
            harness.driver.close_reason(),
            Some(GenerationCloseReason::LocalSeparate)
        );
        assert_eq!(harness.driver.closer().count(), 0);
        assert!(receiver.try_recv().is_err());

        assert!(harness.drive_one(HarnessInput::WriteOutcome {
            write_id: separate_write,
            outcome,
        }));

        assert_eq!(completion_result(&receiver), expected.clone());
        assert_eq!(harness.driver.closer().count(), 1);
        let events = harness.trace.events();
        let outcome_index = events
            .iter()
            .position(|event| matches!(event, TraceEvent::WriterOutcome { .. }))
            .expect("Separate terminal outcome must enter the shared trace");
        let completion_index = events
            .iter()
            .position(|event| matches!(event, TraceEvent::CommandCompleted { .. }))
            .expect("Separate outcome must complete its command");
        let close_index = events
            .iter()
            .position(|event| *event == TraceEvent::TransportClosed)
            .expect("Separate outcome must release transport close");
        assert!(outcome_index < completion_index);
        assert!(completion_index < close_index);
    }
}

/// Confirms Separate aborts an older transaction before attempting its own
/// admission, and admission failure short-circuits NotSelected publication.
#[test]
fn separate_preemption_completion_precedes_failed_admission() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let old_receiver = harness.accept("old-linktest", ControlIntent::Linktest);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let separate_receiver = harness.accept("separate", ControlIntent::Separate);
    harness
        .driver
        .writer_mut()
        .fail_next_with(WriteAdmissionError::Full);
    harness.trace.clear();

    assert!(harness.drive_one(HarnessInput::AcceptedCommand));

    assert_eq!(
        completion_result(&old_receiver),
        Err(OperationError::Protocol(
            crate::hsms::ProtocolError::TransactionAborted,
        ))
    );
    assert_eq!(
        completion_result(&separate_receiver),
        Err(OperationError::Backpressure)
    );
    let events = harness.trace.events();
    let old_completion = events
        .iter()
        .position(|event| {
            matches!(
                event,
                TraceEvent::CommandCompleted {
                    label: "old-linktest",
                    ..
                }
            )
        })
        .expect("preempted command must complete");
    let rejected = events
        .iter()
        .position(|event| matches!(event, TraceEvent::WriterRejected { .. }))
        .expect("Separate admission must fail");
    assert!(old_completion < rejected);
    assert!(!events
        .iter()
        .any(|event| matches!(event, TraceEvent::StateObserved(SessionState::NotSelected))));
}

/// Confirms a later terminal fault cannot replace LocalSeparate or clear its
/// still-unsatisfied write barrier.
#[test]
fn later_fault_preserves_first_close_reason_and_separate_barrier() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let receiver = harness.accept("separate", ControlIntent::Separate);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let separate_write = last_write_id(&harness);

    assert!(harness.drive_one(HarnessInput::Shutdown(GenerationCloseReason::TransportLost,)));
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::LocalSeparate)
    );
    assert_eq!(harness.driver.closer().count(), 0);
    assert_eq!(
        completion_result(&receiver),
        Err(OperationError::ConnectionLost)
    );

    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: separate_write,
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(harness.driver.closer().count(), 1);
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::LocalSeparate)
    );
    assert!(receiver.try_recv().is_err());
}

/// Confirms a Reject naming the local Separate remains a trace-only diagnostic
/// while the write barrier and command completion remain unchanged.
#[test]
fn separate_reject_is_traced_without_changing_the_barrier() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let receiver = harness.accept("separate", ControlIntent::Separate);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let admitted = harness
        .driver
        .writer()
        .admitted()
        .last()
        .expect("Separate request must be admitted");
    let ProtocolMessage::Control(ControlMessage::SeparateRequest {
        session_id,
        system_bytes,
    }) = admitted.message
    else {
        panic!("latest frame must be Separate.req");
    };
    let separate_write = admitted.write_id;
    harness.trace.clear();
    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::RejectRequest {
            session_id,
            header_byte_2: 9,
            reason: RejectReason::TRANSACTION_NOT_OPEN,
            system_bytes,
        }));

    assert!(!harness.drive_one(HarnessInput::Reader));

    assert_eq!(
        harness.trace.events(),
        vec![TraceEvent::RejectReceived {
            session_id,
            header_byte_2: 9,
            reason: RejectReason::TRANSACTION_NOT_OPEN,
            system_bytes,
        }]
    );
    assert!(receiver.try_recv().is_err());
    assert_eq!(harness.driver.closer().count(), 0);
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::LocalSeparate)
    );

    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: separate_write,
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(completion_result(&receiver), Ok(()));
    assert_eq!(harness.driver.closer().count(), 1);
}

/// Confirms extension and otherwise unknown peer Rejects remain observable
/// diagnostics without producing a frame, completion, state, or close action.
#[test]
fn unknown_extension_reject_is_trace_only() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    harness.trace.clear();
    let reason = RejectReason::new(0x80).expect("extension Reject reason is non-zero");
    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::RejectRequest {
            session_id: u16::MAX,
            header_byte_2: 0x7F,
            reason,
            system_bytes: SystemBytes::new(0x1020_3040),
        }));

    assert!(harness.drive_one(HarnessInput::Reader));

    assert_eq!(
        harness.trace.events(),
        vec![TraceEvent::RejectReceived {
            session_id: u16::MAX,
            header_byte_2: 0x7F,
            reason,
            system_bytes: SystemBytes::new(0x1020_3040),
        }]
    );
    assert_eq!(harness.driver.close_reason(), None);
    assert_eq!(harness.driver.closer().count(), 0);
}

/// Confirms a nonstandard Separate Session ID remains diagnostic while the
/// Selected peer-separation behavior still transitions and closes normally.
#[test]
fn nonstandard_peer_separate_is_traced_and_still_terminates_selected() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    harness.trace.clear();
    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::SeparateRequest {
            session_id: 7,
            system_bytes: SystemBytes::new(0x5566_7788),
        }));

    assert!(harness.drive_one(HarnessInput::Reader));

    assert_eq!(
        harness.trace.events(),
        vec![
            TraceEvent::NonstandardSeparateReceived {
                session_id: 7,
                system_bytes: SystemBytes::new(0x5566_7788),
            },
            TraceEvent::StateObserved(SessionState::NotSelected),
            TraceEvent::TransportClosed,
        ]
    );
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::SeparateReceived)
    );
    assert_eq!(harness.driver.closer().count(), 1);
}

/// Confirms each harness turn consumes at most one FIFO Reader input.
#[test]
fn drive_one_consumes_exactly_one_reader_input() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    for system_bytes in [401, 402] {
        harness
            .reader
            .push_message(ProtocolMessage::Control(ControlMessage::LinktestRequest {
                system_bytes: SystemBytes::new(system_bytes),
            }));
    }

    assert!(harness.drive_one(HarnessInput::Reader));
    assert_eq!(harness.driver.writer().admitted().len(), 1);
    assert!(harness.drive_one(HarnessInput::Reader));
    assert_eq!(harness.driver.writer().admitted().len(), 2);
    assert!(!harness.drive_one(HarnessInput::Reader));
}

/// Confirms unknown and duplicate write outcomes fail closed as runtime invariants.
#[test]
fn unknown_and_duplicate_write_outcomes_fail_closed() {
    let mut unknown = DriverHarness::new();
    assert!(unknown.drive_one(HarnessInput::Connected));
    assert_eq!(
        unknown
            .driver
            .writer_mut()
            .emit_outcome(WriteId::new(999), WriteOutcome::Committed),
        Err(FakeWriterOutcomeError::NotAdmitted)
    );
    assert!(unknown.drive_one(HarnessInput::InvalidWriteOutcome {
        write_id: WriteId::new(999),
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(
        unknown.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert_eq!(unknown.driver.closer().count(), 1);

    let mut duplicate = DriverHarness::new();
    assert!(duplicate.drive_one(HarnessInput::Connected));
    let receiver = duplicate.accept("linktest", ControlIntent::Linktest);
    assert!(duplicate.drive_one(HarnessInput::AcceptedCommand));
    let write_id = last_write_id(&duplicate);
    assert!(duplicate.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(
        duplicate
            .driver
            .writer_mut()
            .emit_outcome(write_id, WriteOutcome::Committed),
        Err(FakeWriterOutcomeError::Duplicate)
    );
    assert!(duplicate.drive_one(HarnessInput::InvalidWriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(
        duplicate.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert_eq!(
        completion_result(&receiver),
        Err(OperationError::ConnectionLost)
    );
}

/// Confirms a dropped receiver is benign and mixed queued/table drain completes
/// every other accepted command exactly once in stable order.
#[test]
fn receiver_drop_and_mixed_drain_preserve_exactly_once_completion() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    let dropped = harness.accept("table", ControlIntent::Linktest);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    drop(dropped);
    let queued = harness.accept("queued", ControlIntent::Select);
    harness.trace.clear();

    assert!(harness.drive_one(HarnessInput::Shutdown(GenerationCloseReason::TransportLost,)));

    assert_eq!(harness.driver.pending_completion_count(), 0);
    assert_eq!(harness.driver.queued_command_count(), 0);
    assert_eq!(
        completion_result(&queued),
        Err(OperationError::ConnectionLost)
    );
    let completions: Vec<_> = harness
        .trace
        .events()
        .into_iter()
        .filter(|event| matches!(event, TraceEvent::CommandCompleted { .. }))
        .collect();
    assert_eq!(completions.len(), 2);
    assert!(matches!(
        completions[0],
        TraceEvent::CommandCompleted { label: "table", .. }
    ));
    assert!(matches!(
        completions[1],
        TraceEvent::CommandCompleted {
            label: "queued",
            ..
        }
    ));
}

/// Confirms CommandId allocation uses `u64::MAX` once and then fails closed
/// instead of wrapping the next accepted command to zero.
#[test]
fn command_id_exhaustion_never_wraps_and_drains_every_command_once() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    harness.driver.next_command_id = Some(u64::MAX);
    let maximum = harness.accept("maximum", ControlIntent::Linktest);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let exhausted = harness.accept("exhausted", ControlIntent::Select);

    assert!(harness.drive_one(HarnessInput::AcceptedCommand));

    assert_eq!(harness.driver.next_command_id, None);
    assert_eq!(
        completion_result(&exhausted),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(
        completion_result(&maximum),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert!(maximum.try_recv().is_err());
    assert!(exhausted.try_recv().is_err());
}

/// Confirms duplicate completion fails closed, emits no second notification,
/// and short-circuits ordinary actions later in the injected batch.
#[test]
fn duplicate_completion_is_single_delivery_and_short_circuits_the_batch() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    let receiver = harness.accept("linktest", ControlIntent::Linktest);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    harness.trace.clear();
    let mut actions = CoreActions::new();
    actions.push(CoreAction::CompleteCommand {
        command_id: CommandId::new(0),
        result: CoreCommandResult::Control(Ok(())),
    });
    actions.push(CoreAction::CompleteCommand {
        command_id: CommandId::new(0),
        result: CoreCommandResult::Control(Ok(())),
    });
    actions.push(CoreAction::SessionStateChanged(SessionState::Selected));

    harness.driver.apply_actions(actions, harness.clock.now());

    assert_eq!(completion_result(&receiver), Ok(()));
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert!(!harness
        .trace
        .events()
        .iter()
        .any(|event| matches!(event, TraceEvent::StateObserved(SessionState::Selected))));
}

/// Confirms closing refuses new commands and messages without entering Core.
#[test]
fn closing_refuses_new_commands_and_messages() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    assert!(harness.drive_one(HarnessInput::Shutdown(
        GenerationCloseReason::LocalDisconnect,
    )));
    let (completion, _receiver) =
        super::test_support::FakeCompletion::channel("late", harness.trace.clone());

    let error = harness
        .driver
        .try_accept_control(ControlIntent::Select, completion)
        .expect_err("closing Driver must reject command admission");
    assert_eq!(error.kind(), ControlAdmissionErrorKind::Closing);
    assert_eq!(error.intent(), ControlIntent::Select);
    let message = ProtocolMessage::Control(ControlMessage::LinktestRequest {
        system_bytes: SystemBytes::new(31),
    });
    assert!(!harness.driver.on_message(message, harness.clock.now()));
}

/// Confirms equal time is legal while a backwards clock value fails closed.
#[test]
fn fake_clock_proves_equal_and_backwards_time_behavior_without_sleeping() {
    let mut harness = DriverHarness::new();
    harness
        .clock
        .set(MonoTime::from_elapsed(Duration::from_secs(2)));
    assert!(harness.drive_one(HarnessInput::Connected));
    assert_eq!(harness.driver.next_deadline(), None);
    assert!(harness.drive_one(HarnessInput::AdvanceTime));
    harness
        .clock
        .set(MonoTime::from_elapsed(Duration::from_secs(1)));
    assert!(harness.drive_one(HarnessInput::AdvanceTime));
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
}
