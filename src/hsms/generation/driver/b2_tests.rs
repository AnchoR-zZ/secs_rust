//! B2 vertical-slice tests for Data permits and typed Driver completions.
//!
//! Scenarios in this module use the real Session Core with deterministic fake
//! ports to verify end-to-end Send and Request behavior without a runtime.

use std::{sync::mpsc::Receiver, time::Duration};

use crate::{
    hsms::{
        api::{ControlIntent, PrimaryMessage, SecondaryMessage, SendReceipt},
        core::{CommittedWrite, CoreAction, CoreActions, CoreCommand, CoreCommandResult},
        error::{OperationError, ProtocolError},
        generation::transport::writer::{
            DataReserveError, ReservedDataAdmissionError, WriterIngress,
        },
        lifecycle::SessionState,
        model::{
            ids::{CommandId, Function, SessionId, Stream, SystemBytes, WriteId},
            runtime::{
                GenerationCloseReason, MonoTime, TransportFault, TransportFaultKind, WriteOutcome,
            },
        },
        protocol::{
            header::{ControlMessage, DataHeader, RejectReason},
            message::{DataMessage, ProtocolMessage},
        },
    },
    secs2::SecsItem,
};

use super::{
    test_support::{DriverHarness, HarnessInput, TraceEvent},
    DriverCommandResult, PendingCompletion,
};

/// Builds one public Primary with validated stream and the supplied function.
fn primary(stream: u8, function: u8, body: Option<SecsItem>) -> PrimaryMessage {
    PrimaryMessage::new(
        Stream::new(stream).expect("fixture stream must fit seven bits"),
        Function::new(function),
        body,
    )
}

/// Returns a deterministic transport fault for Writer terminal-outcome tests.
fn transport_fault() -> TransportFault {
    TransportFault::new(TransportFaultKind::BrokenPipe)
}

/// Extracts the sole available Control result without blocking.
fn control_result(receiver: &Receiver<DriverCommandResult>) -> Result<(), OperationError> {
    let DriverCommandResult::Control(result) = receiver
        .try_recv()
        .expect("Control command must have one completion available")
    else {
        panic!("Control command must retain its typed Driver result");
    };
    result
}

/// Extracts the sole available Send result without blocking.
fn send_result(receiver: &Receiver<DriverCommandResult>) -> Result<SendReceipt, OperationError> {
    let DriverCommandResult::Send(result) = receiver
        .try_recv()
        .expect("Send command must have one completion available")
    else {
        panic!("Send command must retain its typed Driver result");
    };
    result
}

/// Extracts the sole available Request result without blocking.
fn request_result(
    receiver: &Receiver<DriverCommandResult>,
) -> Result<SecondaryMessage, OperationError> {
    let DriverCommandResult::Request(result) = receiver
        .try_recv()
        .expect("Request command must have one completion available")
    else {
        panic!("Request command must retain its typed Driver result");
    };
    result
}

/// Drives a passive Select and commits the response frame.
fn enter_selected(harness: &mut DriverHarness) {
    harness
        .reader
        .push_message(ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(91),
        }));
    assert!(harness.drive_one(HarnessInput::Reader));
    let response_write = harness
        .driver
        .writer()
        .admitted()
        .last()
        .expect("passive Select must admit its response")
        .write_id;
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: response_write,
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(harness.driver.state(), Some(SessionState::Selected));
}

/// Returns one admitted Data frame by its zero-based Data-lane order.
fn admitted_data(harness: &DriverHarness, index: usize) -> (WriteId, DataMessage) {
    harness
        .driver
        .writer()
        .admitted()
        .iter()
        .filter_map(|frame| match &frame.message {
            ProtocolMessage::Data(message) => Some((frame.write_id, message.clone())),
            ProtocolMessage::Control(_) => None,
        })
        .nth(index)
        .expect("scenario must admit the requested Data frame")
}

/// Builds an inbound Data message from explicit matcher fields.
fn inbound_data(
    session_id: u16,
    stream: u8,
    function: u8,
    reply_expected: bool,
    system_bytes: u32,
    body: Option<SecsItem>,
) -> ProtocolMessage {
    ProtocolMessage::Data(DataMessage::new(
        DataHeader::new(
            SessionId::new(session_id).expect("fixture Data Session ID must be valid"),
            Stream::new(stream).expect("fixture stream must fit seven bits"),
            Function::new(function),
            reply_expected,
            SystemBytes::new(system_bytes),
        ),
        body,
    ))
}

/// Queues and drives one inbound message as a separate Driver round.
fn drive_message(harness: &mut DriverHarness, message: ProtocolMessage) {
    harness.reader.push_message(message);
    assert!(harness.drive_one(HarnessInput::Reader));
}

/// Transfers the oldest accepted command into Core but returns its unapplied
/// actions, exposing the normal completion-table boundary for malformed-batch
/// tests. The caller supplies or deliberately omits the Data permit separately.
fn prepare_command_actions(harness: &mut DriverHarness) -> CoreActions {
    let accepted = harness
        .driver
        .accepted_commands
        .pop_front()
        .expect("malformed-batch fixture must have an accepted command");
    let command_id = harness
        .driver
        .allocate_command_id()
        .expect("malformed-batch fixture must have a fresh CommandId");
    let previous = harness.driver.completions.insert(
        command_id,
        PendingCompletion {
            kind: accepted.kind.completion_kind(),
            completion: accepted.completion,
        },
    );
    assert!(previous.is_none());
    harness.driver.core.on_command(
        CoreCommand::new(command_id, accepted.kind.into_core_kind()),
        harness.clock.now(),
    )
}

/// Confirms disconnected and NotSelected Data commands reserve locally but
/// never admit a frame, and release the unused permit after Core rejection.
#[test]
fn state_rejected_data_commands_release_permit_without_admission() {
    let cases = [
        (false, OperationError::NotConnected),
        (true, OperationError::NotSelected),
    ];

    for (connect_first, expected) in cases {
        let mut harness = DriverHarness::new();
        if connect_first {
            assert!(harness.drive_one(HarnessInput::Connected));
            harness.trace.clear();
        }
        let send = harness.accept_send("send", primary(1, 1, None));

        assert!(harness.drive_one(HarnessInput::AcceptedCommand));

        assert_eq!(send_result(&send), Err(expected));
        assert!(harness.driver.writer().admitted().is_empty());
        assert!(matches!(
            harness.trace.events().as_slice(),
            [
                TraceEvent::DataPermitReserved { .. },
                TraceEvent::SendCommandCompleted { .. },
                TraceEvent::DataPermitReleased { .. }
            ]
        ));
    }
}

/// Confirms Send and Request translate public Primary ownership into Data
/// headers with configured Session ID, allocated System Bytes, and fixed W-bit.
#[test]
fn send_and_request_translate_headers_and_preserve_bodies() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let typed_empty = Some(SecsItem::U1(Vec::new()));
    let send = harness.accept_send("send", primary(3, 1, None));
    let request = harness.accept_request("request", primary(5, 3, typed_empty.clone()));

    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));

    let (send_write, sent) = admitted_data(&harness, 0);
    let (request_write, requested) = admitted_data(&harness, 1);
    assert_eq!(sent.header().session_id().get(), 7);
    assert_eq!(sent.header().stream().get(), 3);
    assert_eq!(sent.header().function().get(), 1);
    assert!(!sent.header().reply_expected());
    assert_eq!(sent.header().system_bytes().get(), 0);
    assert_eq!(sent.body(), None);
    assert_eq!(requested.header().session_id().get(), 7);
    assert_eq!(requested.header().stream().get(), 5);
    assert_eq!(requested.header().function().get(), 3);
    assert!(requested.header().reply_expected());
    assert_eq!(requested.header().system_bytes().get(), 1);
    assert_eq!(requested.body(), typed_empty.as_ref());
    let events = harness.trace.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TraceEvent::DataPermitReserved { .. }))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, TraceEvent::DataPermitConsumed { .. }))
            .count(),
        2
    );
    assert!(!events
        .iter()
        .any(|event| matches!(event, TraceEvent::DataPermitReleased { .. })));

    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: send_write,
        outcome: WriteOutcome::Committed,
    }));
    let receipt = send_result(&send).expect("committed Send must return a receipt");
    assert_eq!(receipt.generation().get(), 17);
    assert_eq!(receipt.wire_sequence(), 1);
    assert!(send.try_recv().is_err());
    assert!(request.try_recv().is_err());
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: request_write,
        outcome: WriteOutcome::Committed,
    }));
    assert!(request.try_recv().is_err());
}

/// Confirms invalid Primary functions consume no Core protocol identifiers,
/// emit no frame, and release the permit for a following valid command.
#[test]
fn invalid_primary_function_releases_permit_without_allocating_protocol_ids() {
    for function in [0, 2] {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        enter_selected(&mut harness);
        harness.trace.clear();
        let invalid = harness.accept_send("invalid", primary(1, function, None));
        assert!(harness.drive_one(HarnessInput::AcceptedCommand));
        assert_eq!(
            send_result(&invalid),
            Err(OperationError::Protocol(
                ProtocolError::InvalidPrimaryFunction {
                    function: Function::new(function),
                },
            ))
        );
        assert!(harness
            .driver
            .writer()
            .admitted()
            .iter()
            .all(|frame| !matches!(frame.message, ProtocolMessage::Data(_))));
        assert!(harness
            .trace
            .events()
            .iter()
            .any(|event| matches!(event, TraceEvent::DataPermitReleased { .. })));

        let valid = harness.accept_send("valid", primary(1, 1, None));
        assert!(harness.drive_one(HarnessInput::AcceptedCommand));
        let (_, data) = admitted_data(&harness, 0);
        assert_eq!(data.header().system_bytes().get(), 0);
        assert!(valid.try_recv().is_err());
    }
}

/// Confirms Data reservation Full completes only that command with
/// Backpressure, while Closed completes it as ConnectionLost and closes.
#[test]
fn data_reserve_full_and_closed_have_distinct_stable_mappings() {
    let mut full = DriverHarness::new();
    assert!(full.drive_one(HarnessInput::Connected));
    enter_selected(&mut full);
    full.trace.clear();
    full.driver
        .writer_mut()
        .fail_next_data_reserve_with(DataReserveError::Full);
    let full_receiver = full.accept_send("full", primary(1, 1, None));
    assert!(full.drive_one(HarnessInput::AcceptedCommand));
    assert_eq!(
        send_result(&full_receiver),
        Err(OperationError::Backpressure)
    );
    assert_eq!(full.driver.close_reason(), None);
    assert_eq!(
        full.trace.events()[0],
        TraceEvent::DataPermitRejected {
            error: DataReserveError::Full,
        }
    );
    let following = full.accept_send("following", primary(1, 1, None));
    assert!(full.drive_one(HarnessInput::AcceptedCommand));
    let (_, following_data) = admitted_data(&full, 0);
    assert_eq!(following_data.header().system_bytes().get(), 0);
    assert!(following.try_recv().is_err());

    let mut closed = DriverHarness::new();
    assert!(closed.drive_one(HarnessInput::Connected));
    enter_selected(&mut closed);
    closed
        .driver
        .writer_mut()
        .fail_next_data_reserve_with(DataReserveError::Closed);
    let closed_receiver = closed.accept_request("closed", primary(1, 1, None));
    assert!(closed.drive_one(HarnessInput::AcceptedCommand));
    assert_eq!(
        request_result(&closed_receiver),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(
        closed.driver.close_reason(),
        Some(GenerationCloseReason::TransportLost)
    );
    assert_eq!(closed.driver.closer().count(), 1);
}

/// Confirms closing rejects new Data admission without consuming either the
/// caller's Primary body or its completion endpoint.
#[test]
fn closing_returns_unconsumed_data_admission_values() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    assert!(harness.drive_one(HarnessInput::Shutdown(
        GenerationCloseReason::LocalDisconnect,
    )));
    let body = Some(SecsItem::U1(vec![7]));
    let message = primary(2, 1, body.clone());
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("late", harness.trace.clone());

    let error = harness
        .driver
        .try_accept_request(message, completion)
        .expect_err("closing Driver must return Data admission ownership");

    assert_eq!(error.kind(), super::DataAdmissionErrorKind::Closing);
    assert_eq!(error.message().body(), body.as_ref());
    let (returned, completion) = error.into_parts();
    assert_eq!(returned.body(), body.as_ref());
    drop(completion);
    assert!(receiver.try_recv().is_err());
}

/// Confirms post-reservation Closed and Invariant failures consume the permit,
/// complete the Data command once, and fail the generation closed as specified.
#[test]
fn reserved_data_admission_failures_close_with_stable_reason() {
    let cases = [
        (
            ReservedDataAdmissionError::Closed,
            GenerationCloseReason::TransportLost,
        ),
        (
            ReservedDataAdmissionError::Invariant,
            GenerationCloseReason::RuntimeInvariant,
        ),
    ];

    for (admission_error, close_reason) in cases {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        enter_selected(&mut harness);
        harness
            .driver
            .writer_mut()
            .fail_next_reserved_data_with(admission_error);
        let receiver = harness.accept_send("send", primary(1, 1, None));

        assert!(harness.drive_one(HarnessInput::AcceptedCommand));

        assert_eq!(send_result(&receiver), Err(OperationError::ConnectionLost));
        assert_eq!(harness.driver.close_reason(), Some(close_reason));
        assert_eq!(harness.driver.closer().count(), 1);
        assert!(receiver.try_recv().is_err());
    }
}

/// Confirms Data reservation never consumes Control capacity and both lanes
/// allocate from one monotonically ordered WireSequence space.
#[test]
fn control_and_data_lanes_are_isolated_but_share_wire_order() {
    let mut harness = DriverHarness::with_limits(256, 512, 1, 1);
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let send = harness.accept_send("send", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    drive_message(
        &mut harness,
        ProtocolMessage::Control(ControlMessage::LinktestRequest {
            system_bytes: SystemBytes::new(77),
        }),
    );

    let admitted = harness.driver.writer().admitted();
    assert_eq!(admitted[1].sequence.get(), 1);
    assert!(matches!(admitted[1].message, ProtocolMessage::Data(_)));
    assert_eq!(admitted[2].sequence.get(), 2);
    assert!(matches!(admitted[2].message, ProtocolMessage::Control(_)));
    assert!(send.try_recv().is_err());
}

/// Confirms Send maps all writer outcomes and an attributable Reject without
/// duplicate completion when the retained unique outcome arrives later.
#[test]
fn send_outcomes_and_peer_reject_complete_exactly_once() {
    let cases = [
        (
            WriteOutcome::NotWritten(transport_fault()),
            OperationError::ConnectionLost,
        ),
        (
            WriteOutcome::Indeterminate(transport_fault()),
            OperationError::DeliveryIndeterminate,
        ),
    ];
    for (outcome, expected) in cases {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        enter_selected(&mut harness);
        let receiver = harness.accept_send("send", primary(1, 1, None));
        assert!(harness.drive_one(HarnessInput::AcceptedCommand));
        let (write_id, _) = admitted_data(&harness, 0);
        assert!(harness.drive_one(HarnessInput::WriteOutcome { write_id, outcome }));
        assert_eq!(send_result(&receiver), Err(expected));
        assert_eq!(
            harness.driver.close_reason(),
            Some(GenerationCloseReason::TransportLost)
        );
    }

    let mut rejected = DriverHarness::new();
    assert!(rejected.drive_one(HarnessInput::Connected));
    enter_selected(&mut rejected);
    let receiver = rejected.accept_send("send", primary(1, 1, None));
    assert!(rejected.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, data) = admitted_data(&rejected, 0);
    let header = data.header();
    drive_message(
        &mut rejected,
        ProtocolMessage::Control(ControlMessage::RejectRequest {
            session_id: header.session_id().get(),
            header_byte_2: 0,
            reason: RejectReason::TRANSACTION_NOT_OPEN,
            system_bytes: header.system_bytes(),
        }),
    );
    assert_eq!(
        send_result(&receiver),
        Err(OperationError::PeerRejected {
            reason: RejectReason::TRANSACTION_NOT_OPEN,
        })
    );
    assert!(rejected.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed,
    }));
    assert!(receiver.try_recv().is_err());
    assert_eq!(rejected.driver.close_reason(), None);
}

/// Confirms a committed-first Request remains pending without a B2 timeout and
/// later materializes the matched Secondary including its typed-empty body.
#[test]
fn request_committed_first_waits_and_materializes_secondary() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let receiver = harness.accept_request("request", primary(4, 3, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, data) = admitted_data(&harness, 0);
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed,
    }));
    harness
        .clock
        .set(MonoTime::from_elapsed(Duration::from_secs(86_400)));
    assert!(harness.drive_one(HarnessInput::AdvanceTime));
    assert!(receiver.try_recv().is_err());
    assert_eq!(harness.driver.next_deadline(), None);
    let header = data.header();
    let body = Some(SecsItem::List(Vec::new()));
    drive_message(
        &mut harness,
        inbound_data(
            7,
            header.stream().get(),
            4,
            false,
            header.system_bytes().get(),
            body.clone(),
        ),
    );
    let secondary = request_result(&receiver).expect("matching Secondary must succeed");
    assert_eq!(secondary.stream(), header.stream());
    assert_eq!(secondary.function(), Function::new(4));
    assert_eq!(secondary.body(), body.as_ref());
}

/// Confirms a fast Secondary completes before the writer outcome, while a
/// later writer fault does not overwrite success but still closes transport.
#[test]
fn fast_secondary_then_writer_fault_preserves_success_and_closes() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let receiver = harness.accept_request("request", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, data) = admitted_data(&harness, 0);
    let header = data.header();
    drive_message(
        &mut harness,
        inbound_data(7, 1, 2, false, header.system_bytes().get(), None),
    );
    assert!(request_result(&receiver).is_ok());
    assert_eq!(harness.driver.pending_write_count(), 1);

    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Indeterminate(transport_fault()),
    }));

    assert!(receiver.try_recv().is_err());
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::TransportLost)
    );
}

/// Confirms every normal-Secondary header field participates in matching and
/// mismatches leave the Request available for a later exact response.
#[test]
fn request_matcher_rejects_each_wrong_field_then_accepts_exact_secondary() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let receiver = harness.accept_request("request", primary(3, 5, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (_, request) = admitted_data(&harness, 0);
    let system_bytes = request.header().system_bytes().get();
    let mismatches = [
        inbound_data(8, 3, 6, false, system_bytes, None),
        inbound_data(7, 3, 6, false, system_bytes + 1, None),
        inbound_data(7, 4, 6, false, system_bytes, None),
        inbound_data(7, 3, 6, true, system_bytes, None),
        inbound_data(7, 3, 8, false, system_bytes, None),
    ];
    for mismatch in mismatches {
        drive_message(&mut harness, mismatch);
        assert!(receiver.try_recv().is_err());
        assert_eq!(harness.driver.pending_data_transaction_count(), 1);
    }

    drive_message(
        &mut harness,
        inbound_data(7, 3, 6, false, system_bytes, None),
    );
    assert!(request_result(&receiver).is_ok());
    assert_eq!(harness.driver.pending_data_transaction_count(), 0);
}

/// Confirms concurrent Requests can receive Secondary responses and writer
/// outcomes in independent, deliberately reversed orders.
#[test]
fn concurrent_requests_complete_out_of_order_with_outcomes_out_of_order() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let first = harness.accept_request("first", primary(1, 1, None));
    let second = harness.accept_request("second", primary(2, 3, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (first_write, first_data) = admitted_data(&harness, 0);
    let (second_write, second_data) = admitted_data(&harness, 1);

    drive_message(
        &mut harness,
        inbound_data(
            7,
            2,
            4,
            false,
            second_data.header().system_bytes().get(),
            None,
        ),
    );
    assert!(request_result(&second).is_ok());
    assert!(first.try_recv().is_err());
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: second_write,
        outcome: WriteOutcome::Committed,
    }));
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: first_write,
        outcome: WriteOutcome::Committed,
    }));
    drive_message(
        &mut harness,
        inbound_data(
            7,
            1,
            2,
            false,
            first_data.header().system_bytes().get(),
            None,
        ),
    );
    assert!(request_result(&first).is_ok());
}

/// Confirms F255 Request avoids F+1 overflow and terminates only on its exact
/// header-only F0 transaction-abort contract.
#[test]
fn function_255_request_matches_only_header_only_f0_abort() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let receiver = harness.accept_request("request", primary(7, 255, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (_, data) = admitted_data(&harness, 0);
    let system_bytes = data.header().system_bytes().get();
    drive_message(
        &mut harness,
        inbound_data(
            7,
            7,
            0,
            false,
            system_bytes,
            Some(SecsItem::List(Vec::new())),
        ),
    );
    assert!(receiver.try_recv().is_err());
    drive_message(
        &mut harness,
        inbound_data(7, 7, 0, false, system_bytes, None),
    );
    assert_eq!(
        request_result(&receiver),
        Err(OperationError::Protocol(ProtocolError::TransactionAborted))
    );
    assert_eq!(harness.driver.tombstone_count(), 1);
}

/// Confirms request transaction capacity rejects before frame creation,
/// releases its already reserved permit, and is reusable after completion.
#[test]
fn transaction_capacity_releases_permit_and_recovers_after_response() {
    let mut harness = DriverHarness::with_limits(1, 4, 8, 2);
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let first = harness.accept_request("first", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (_, first_data) = admitted_data(&harness, 0);
    let rejected = harness.accept_request("rejected", primary(2, 1, None));
    harness.trace.clear();
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert_eq!(request_result(&rejected), Err(OperationError::Backpressure));
    assert_eq!(harness.driver.pending_data_transaction_count(), 1);
    assert!(harness
        .trace
        .events()
        .iter()
        .any(|event| matches!(event, TraceEvent::DataPermitReleased { .. })));

    drive_message(
        &mut harness,
        inbound_data(
            7,
            1,
            2,
            false,
            first_data.header().system_bytes().get(),
            None,
        ),
    );
    assert!(request_result(&first).is_ok());
    let third = harness.accept_request("third", primary(3, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert_eq!(harness.driver.pending_data_transaction_count(), 1);
    assert!(third.try_recv().is_err());
}

/// Confirms bounded tombstones isolate duplicate and late Secondary traffic
/// without completing another command or creating an outbound response.
#[test]
fn tombstones_are_bounded_and_late_secondary_is_isolated() {
    let mut harness = DriverHarness::with_limits(4, 1, 8, 4);
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let first = harness.accept_request("first", primary(1, 1, None));
    let second = harness.accept_request("second", primary(2, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (_, first_data) = admitted_data(&harness, 0);
    let (_, second_data) = admitted_data(&harness, 1);
    let first_response = inbound_data(
        7,
        1,
        2,
        false,
        first_data.header().system_bytes().get(),
        None,
    );
    let second_response = inbound_data(
        7,
        2,
        2,
        false,
        second_data.header().system_bytes().get(),
        None,
    );
    drive_message(&mut harness, first_response.clone());
    assert!(request_result(&first).is_ok());
    drive_message(&mut harness, second_response.clone());
    assert!(request_result(&second).is_ok());
    assert_eq!(harness.driver.tombstone_count(), 1);
    let admitted_count = harness.driver.writer().admitted().len();

    drive_message(&mut harness, second_response);
    drive_message(&mut harness, first_response);

    assert_eq!(harness.driver.tombstone_count(), 1);
    assert_eq!(harness.driver.writer().admitted().len(), admitted_count);
    assert_eq!(harness.driver.open_core_command_count(), 0);
}

/// Confirms peer Separate completes open Data commands as deselected while
/// retained accepted writes still accept their sole late committed outcomes.
#[test]
fn peer_separate_completes_data_but_retains_late_write_outcomes() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let send = harness.accept_send("send", primary(1, 1, None));
    let request = harness.accept_request("request", primary(2, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (send_write, _) = admitted_data(&harness, 0);
    let (request_write, _) = admitted_data(&harness, 1);
    drive_message(
        &mut harness,
        ProtocolMessage::Control(ControlMessage::SeparateRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(999),
        }),
    );
    assert_eq!(send_result(&send), Err(OperationError::SessionDeselected));
    assert_eq!(
        request_result(&request),
        Err(OperationError::SessionDeselected)
    );
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: request_write,
        outcome: WriteOutcome::Committed,
    }));
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: send_write,
        outcome: WriteOutcome::Committed,
    }));
    assert!(send.try_recv().is_err());
    assert!(request.try_recv().is_err());
}

/// Confirms shutdown drains table-owned and queued Control, Send, and Request
/// endpoints with their own result variants in deterministic exactly-once order.
#[test]
fn shutdown_mixed_drain_preserves_typed_exactly_once_results() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let open_request = harness.accept_request("open-request", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let queued_send = harness.accept_send("queued-send", primary(2, 1, None));
    let queued_control = harness.accept("queued-control", ControlIntent::Linktest);
    harness.trace.clear();

    assert!(harness.drive_one(HarnessInput::Shutdown(
        GenerationCloseReason::LocalDisconnect,
    )));

    assert_eq!(
        request_result(&open_request),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(
        send_result(&queued_send),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(
        control_result(&queued_control),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(harness.driver.pending_completion_count(), 0);
    assert_eq!(harness.driver.queued_command_count(), 0);
    let events = harness.trace.events();
    let typed: Vec<_> = events
        .iter()
        .filter(|event| {
            matches!(
                event,
                TraceEvent::CommandCompleted { .. }
                    | TraceEvent::SendCommandCompleted { .. }
                    | TraceEvent::RequestCommandCompleted { .. }
            )
        })
        .collect();
    assert!(matches!(
        typed.as_slice(),
        [
            TraceEvent::RequestCommandCompleted {
                label: "open-request",
                ..
            },
            TraceEvent::SendCommandCompleted {
                label: "queued-send",
                ..
            },
            TraceEvent::CommandCompleted {
                label: "queued-control",
                ..
            }
        ]
    ));
}

/// Confirms dropping a Send receiver is benign and completion still occurs
/// exactly once in the shared typed trace.
#[test]
fn dropped_send_receiver_does_not_disrupt_completion() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let receiver = harness.accept_send("dropped", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, _) = admitted_data(&harness, 0);
    drop(receiver);
    harness.trace.clear();

    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed,
    }));

    assert_eq!(
        harness.trace.events(),
        vec![
            TraceEvent::WriterOutcome {
                write_id,
                outcome: WriteOutcome::Committed,
            },
            TraceEvent::SendCommandCompleted {
                label: "dropped",
                result: Ok(SendReceipt::new(
                    crate::hsms::model::ids::ConnectionGeneration::new(17),
                    harness
                        .driver
                        .writer()
                        .admitted()
                        .iter()
                        .find(|frame| frame.write_id == write_id)
                        .expect("Send frame remains available for trace assertion")
                        .sequence,
                )),
            },
        ]
    );
}

/// Confirms WriteId and System Bytes use their maximum values once and never
/// wrap; the next Data command fails the generation closed with typed drain.
#[test]
fn data_identifier_allocators_use_maximum_once_then_fail_closed() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    harness
        .driver
        .core
        .seed_identifiers(Some(u64::MAX), Some(u32::MAX));
    let maximum = harness.accept_send("maximum", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, data) = admitted_data(&harness, 0);
    assert_eq!(write_id, WriteId::new(u64::MAX));
    assert_eq!(data.header().system_bytes(), SystemBytes::new(u32::MAX));
    let exhausted = harness.accept_request("exhausted", primary(1, 1, None));

    assert!(harness.drive_one(HarnessInput::AcceptedCommand));

    assert_eq!(
        request_result(&exhausted),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(send_result(&maximum), Err(OperationError::ConnectionLost));
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
}

/// Confirms an incorrect result type or unmapped Send commit leaves the
/// completion endpoint available for typed shutdown instead of dropping it or
/// delivering the malformed result; the remaining batch is not applied.
#[test]
fn invalid_completion_results_preserve_endpoints_for_typed_drain() {
    let malformed_results = [
        CoreCommandResult::Control(Ok(())),
        CoreCommandResult::Send(Ok(CommittedWrite::new(WriteId::new(999)))),
    ];
    for malformed_result in malformed_results {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        enter_selected(&mut harness);
        let open_send = harness.accept_send("open-send", primary(1, 1, None));
        assert!(harness.drive_one(HarnessInput::AcceptedCommand));
        let queued_request = harness.accept_request("queued-request", primary(2, 1, None));
        harness.trace.clear();
        let mut actions = CoreActions::new();
        actions.push(CoreAction::CompleteCommand {
            command_id: CommandId::new(0),
            result: malformed_result,
        });
        actions.push(CoreAction::SessionStateChanged(SessionState::NotSelected));

        harness.driver.apply_actions(actions, harness.clock.now());

        assert_eq!(send_result(&open_send), Err(OperationError::ConnectionLost));
        assert_eq!(
            request_result(&queued_request),
            Err(OperationError::ConnectionLost)
        );
        assert!(open_send.try_recv().is_err());
        assert!(queued_request.try_recv().is_err());
        assert_eq!(harness.driver.pending_completion_count(), 0);
        assert_eq!(harness.driver.queued_command_count(), 0);
        assert_eq!(harness.driver.open_core_command_count(), 0);
        assert_eq!(
            harness.driver.close_reason(),
            Some(GenerationCloseReason::RuntimeInvariant)
        );
        assert_eq!(
            harness.trace.events(),
            vec![
                TraceEvent::SendCommandCompleted {
                    label: "open-send",
                    result: Err(OperationError::ConnectionLost),
                },
                TraceEvent::TransportClosed,
                TraceEvent::RequestCommandCompleted {
                    label: "queued-request",
                    result: Err(OperationError::ConnectionLost),
                },
            ]
        );
    }
}

/// Confirms a real Core-produced Data frame without its required permit fails
/// closed before Writer admission and drains both table-owned and queued
/// endpoints once, without relying on an unknown WriteId to detect the error.
#[test]
fn data_frame_without_permit_fails_closed_before_admission() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let open_send = harness.accept_send("open-send", primary(1, 1, None));
    let queued_request = harness.accept_request("queued-request", primary(2, 1, None));
    let mut actions = prepare_command_actions(&mut harness);
    actions.push(CoreAction::SessionStateChanged(SessionState::NotSelected));
    assert_eq!(harness.driver.pending_write_count(), 1);
    harness.trace.clear();

    harness.driver.apply_actions(actions, harness.clock.now());

    assert_eq!(send_result(&open_send), Err(OperationError::ConnectionLost));
    assert_eq!(
        request_result(&queued_request),
        Err(OperationError::ConnectionLost)
    );
    assert!(open_send.try_recv().is_err());
    assert!(queued_request.try_recv().is_err());
    assert_eq!(harness.driver.pending_write_count(), 0);
    assert_eq!(harness.driver.pending_completion_count(), 0);
    assert_eq!(harness.driver.queued_command_count(), 0);
    assert_eq!(harness.driver.writer().admitted().len(), 1);
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert_eq!(harness.driver.closer().count(), 1);
    assert_eq!(
        harness.trace.events(),
        vec![
            TraceEvent::SendCommandCompleted {
                label: "open-send",
                result: Err(OperationError::ConnectionLost),
            },
            TraceEvent::TransportClosed,
            TraceEvent::RequestCommandCompleted {
                label: "queued-request",
                result: Err(OperationError::ConnectionLost),
            },
        ]
    );
}

/// Confirms a malformed batch containing two Data frames consumes its sole
/// permit only for the first frame, rejects the second before admission, and
/// retains the first admitted write for its unique late outcome after shutdown.
#[test]
fn multiple_data_frames_in_one_batch_fail_closed_after_single_permit_consumption() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let first = harness.accept_send("first", primary(1, 1, None));
    let second = harness.accept_request("second", primary(2, 1, None));
    harness.trace.clear();
    let permit = harness
        .driver
        .writer_mut()
        .try_reserve_data()
        .expect("fixture must reserve one Data slot");
    let mut malformed_batch = prepare_command_actions(&mut harness);
    for action in prepare_command_actions(&mut harness) {
        malformed_batch.push(action);
    }
    malformed_batch.push(CoreAction::SessionStateChanged(SessionState::NotSelected));
    assert_eq!(harness.driver.pending_write_count(), 2);

    harness.driver.apply_actions_with_data_permit(
        malformed_batch,
        Some(permit),
        harness.clock.now(),
    );

    assert_eq!(send_result(&first), Err(OperationError::ConnectionLost));
    assert_eq!(request_result(&second), Err(OperationError::ConnectionLost));
    assert_eq!(harness.driver.pending_write_count(), 1);
    assert_eq!(harness.driver.pending_data_transaction_count(), 0);
    assert_eq!(harness.driver.pending_completion_count(), 0);
    assert_eq!(harness.driver.writer().admitted().len(), 2);
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert_eq!(harness.driver.closer().count(), 1);
    let (write_id, _) = admitted_data(&harness, 0);
    let events = harness.trace.events();
    assert!(matches!(
        events.as_slice(),
        [
            TraceEvent::DataPermitReserved { reservation_id: 0 },
            TraceEvent::DataPermitConsumed {
                reservation_id: 0,
                ..
            },
            TraceEvent::WriterAdmitted { .. },
            TraceEvent::RequestCommandCompleted {
                label: "second",
                ..
            },
            TraceEvent::SendCommandCompleted { label: "first", .. },
            TraceEvent::TransportClosed,
        ]
    ));
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(harness.driver.pending_write_count(), 0);
    assert!(first.try_recv().is_err());
    assert!(second.try_recv().is_err());
    assert_eq!(harness.driver.closer().count(), 1);
}

/// Confirms Data CommandId exhaustion releases the already reserved permit,
/// does not produce another frame or wrap the ID, and completes open, current,
/// and queued Data commands through their own result types exactly once.
#[test]
fn data_command_id_exhaustion_releases_permit_and_drains_typed_commands() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    harness.driver.next_command_id = Some(u64::MAX);
    let maximum = harness.accept_send("maximum", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let exhausted = harness.accept_request("exhausted", primary(2, 1, None));
    let queued = harness.accept_send("queued", primary(3, 1, None));
    harness.trace.clear();

    assert!(harness.drive_one(HarnessInput::AcceptedCommand));

    assert_eq!(send_result(&maximum), Err(OperationError::ConnectionLost));
    assert_eq!(
        request_result(&exhausted),
        Err(OperationError::ConnectionLost)
    );
    assert_eq!(send_result(&queued), Err(OperationError::ConnectionLost));
    assert!(maximum.try_recv().is_err());
    assert!(exhausted.try_recv().is_err());
    assert!(queued.try_recv().is_err());
    assert_eq!(harness.driver.next_command_id, None);
    assert_eq!(harness.driver.pending_completion_count(), 0);
    assert_eq!(harness.driver.queued_command_count(), 0);
    assert_eq!(harness.driver.open_core_command_count(), 0);
    assert_eq!(harness.driver.writer().admitted().len(), 2);
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
    assert_eq!(
        harness.trace.events(),
        vec![
            TraceEvent::DataPermitReserved { reservation_id: 1 },
            TraceEvent::DataPermitReleased { reservation_id: 1 },
            TraceEvent::RequestCommandCompleted {
                label: "exhausted",
                result: Err(OperationError::ConnectionLost),
            },
            TraceEvent::SendCommandCompleted {
                label: "maximum",
                result: Err(OperationError::ConnectionLost),
            },
            TraceEvent::TransportClosed,
            TraceEvent::SendCommandCompleted {
                label: "queued",
                result: Err(OperationError::ConnectionLost),
            },
        ]
    );
}
