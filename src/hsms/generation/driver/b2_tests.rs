//! B2 vertical-slice tests for Data permits and typed Driver completions.
//!
//! Scenarios in this module use the real Session Core with deterministic fake
//! ports to verify end-to-end Send and Request behavior without a runtime.

use std::{sync::mpsc::Receiver, time::Duration};

use crate::{
    hsms::{
        api::{ControlIntent, PrimaryMessage, SecondaryMessage, SendReceipt},
        core::{CommittedWrite, CoreAction, CoreActions, CoreCommand, CoreCommandResult},
        error::{OperationError, ProtocolError, TimeoutKind},
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
    match receiver
        .try_recv()
        .expect("Send command must have one completion available")
    {
        DriverCommandResult::Send(result) => result,
        DriverCommandResult::PrimaryRejected {
            reply_expected: false,
            error,
            ..
        } => Err(error),
        _ => panic!("Send command must retain its typed Driver result"),
    }
}

/// Extracts the sole available Request result without blocking.
fn request_result(
    receiver: &Receiver<DriverCommandResult>,
) -> Result<SecondaryMessage, OperationError> {
    match receiver
        .try_recv()
        .expect("Request command must have one completion available")
    {
        DriverCommandResult::Request(result) => result,
        DriverCommandResult::PrimaryRejected {
            reply_expected: true,
            error,
            ..
        } => Err(error),
        _ => panic!("Request command must retain its typed Driver result"),
    }
}

/// Pre-Core Writer rejection returns the original allocation for both Primary operations.
#[test]
fn writer_reservation_rejection_returns_original_primary() {
    for reply_expected in [false, true] {
        for failure in [DataReserveError::Full, DataReserveError::Closed] {
            let mut harness = DriverHarness::new();
            assert!(harness.drive_one(HarnessInput::Connected));
            enter_selected(&mut harness);
            harness
                .driver
                .writer_mut()
                .fail_next_data_reserve_with(failure);
            let bytes = vec![3_u8, 7, 19];
            let allocation = bytes.as_ptr();
            let original = primary(3, 1, Some(SecsItem::U1(bytes)));
            let receiver = if reply_expected {
                harness.accept_request("rejected", original)
            } else {
                harness.accept_send("rejected", original)
            };
            assert!(harness.drive_one(HarnessInput::AcceptedCommand));
            let DriverCommandResult::PrimaryRejected {
                message,
                reply_expected: returned_kind,
                error,
            } = receiver.try_recv().unwrap()
            else {
                panic!("pre-Core failure must return original Primary");
            };
            assert_eq!(returned_kind, reply_expected);
            assert_eq!(
                error,
                match failure {
                    DataReserveError::Full => OperationError::Backpressure,
                    DataReserveError::Closed => OperationError::ConnectionLost,
                }
            );
            let Some(SecsItem::U1(returned)) = message.body() else {
                panic!("original body retained")
            };
            assert_eq!(returned, &[3, 7, 19]);
            assert_eq!(returned.as_ptr(), allocation);
            assert_eq!(harness.driver.pending_data_transaction_count(), 0);
        }
    }
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
/// retain frames in their shared FIFO admission order.
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

    assert!(matches!(admitted[1].message, ProtocolMessage::Data(_)));

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

/// Confirms a committed-first Request remains pending before T3 expires and
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
        .set(MonoTime::from_elapsed(Duration::from_secs(10)));
    assert!(harness.drive_one(HarnessInput::AdvanceTime));
    assert!(receiver.try_recv().is_err());
    assert_eq!(
        harness.driver.next_deadline(),
        Some(MonoTime::from_elapsed(Duration::from_secs(30)))
    );
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
    assert_eq!(
        secondary.context().header(),
        &[0, 7, 4, 4, 0, 0, 0, 0, 0, 0]
    );
    assert_eq!(secondary.context().generation().get(), 17);
}

/// Uses actual commit time through Driver and gives T3 priority at equality.
#[test]
fn delayed_commit_and_response_boundary_do_not_extend_t3() {
    for processing_seconds in [46, 47] {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        enter_selected(&mut harness);
        let receiver = harness.accept_request("request", primary(1, 1, None));
        assert!(harness.drive_one(HarnessInput::AcceptedCommand));
        let (write_id, data) = admitted_data(&harness, 0);
        harness.driver.on_write_outcome_at(
            write_id,
            WriteOutcome::Committed,
            MonoTime::from_elapsed(Duration::from_secs(2)),
            MonoTime::from_elapsed(Duration::from_secs(10)),
        );
        assert!(receiver.try_recv().is_err());
        harness
            .clock
            .set(MonoTime::from_elapsed(Duration::from_secs(
                processing_seconds,
            )));
        drive_message(
            &mut harness,
            inbound_data(7, 1, 2, false, data.header().system_bytes().get(), None),
        );
        let result = request_result(&receiver);
        if processing_seconds == 46 {
            assert!(result.is_ok());
        } else {
            assert_eq!(
                result,
                Err(OperationError::RequestTimeout {
                    context: crate::hsms::MessageContext::from_data(
                        harness.driver.generation,
                        data.header()
                    )
                })
            );
        }
        assert!(receiver.try_recv().is_err());
        assert_eq!(harness.driver.pending_data_transaction_count(), 0);
        assert_eq!(harness.driver.close_reason(), None);
    }
}

/// A callback delayed past T3 settles timeout in its own Driver action batch.
#[test]
fn overdue_commit_callback_times_out_without_an_extra_clock_turn() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let receiver = harness.accept_request("request", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, data) = admitted_data(&harness, 0);
    harness.driver.on_write_outcome_at(
        write_id,
        WriteOutcome::Committed,
        MonoTime::from_elapsed(Duration::from_secs(2)),
        MonoTime::from_elapsed(Duration::from_secs(47)),
    );
    assert_eq!(
        request_result(&receiver),
        Err(OperationError::RequestTimeout {
            context: crate::hsms::MessageContext::from_data(
                harness.driver.generation,
                data.header()
            )
        })
    );
    assert!(receiver.try_recv().is_err());
    assert!(!harness.driver.has_admitted_write(write_id));
    assert_eq!(harness.driver.close_reason(), None);
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
    assert!(send.try_recv().is_err());
    assert!(request.try_recv().is_err());
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: request_write,
        outcome: WriteOutcome::Committed,
    }));
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: send_write,
        outcome: WriteOutcome::Committed,
    }));
    assert!(send_result(&send).is_ok());
    assert_eq!(
        request_result(&request),
        Err(OperationError::SessionDeselected)
    );
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

    assert!(open_request.try_recv().is_err());
    harness.driver.on_writer_stopped(harness.clock.now());
    assert_eq!(
        request_result(&open_request),
        Err(OperationError::DeliveryIndeterminate)
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
            TraceEvent::SendCommandCompleted {
                label: "queued-send",
                ..
            },
            TraceEvent::CommandCompleted {
                label: "queued-control",
                ..
            },
            TraceEvent::RequestCommandCompleted {
                label: "open-request",
                ..
            }
        ]
    ));
}

/// Shutdown preserves all three visibility facts for pending Sends and Requests.
#[test]
fn shutdown_waits_for_actual_write_visibility_before_typed_settlement() {
    for request in [false, true] {
        for outcome in [
            WriteOutcome::Committed,
            WriteOutcome::NotWritten(transport_fault()),
            WriteOutcome::Indeterminate(transport_fault()),
        ] {
            let mut harness = DriverHarness::new();
            assert!(harness.drive_one(HarnessInput::Connected));
            enter_selected(&mut harness);
            let receiver = if request {
                harness.accept_request("pending", primary(1, 1, None))
            } else {
                harness.accept_send("pending", primary(1, 1, None))
            };
            assert!(harness.drive_one(HarnessInput::AcceptedCommand));
            let (write_id, _) = admitted_data(&harness, 0);
            assert!(harness.drive_one(HarnessInput::Shutdown(
                GenerationCloseReason::LocalDisconnect
            )));
            assert!(receiver.try_recv().is_err());
            assert_eq!(harness.driver.pending_completion_count(), 1);
            assert_eq!(harness.driver.closer().count(), 1);
            assert!(harness.driver.has_admitted_write(write_id));
            assert!(harness.drive_one(HarnessInput::WriteOutcome { write_id, outcome }));
            let expected_error = match outcome {
                WriteOutcome::Indeterminate(_) => OperationError::DeliveryIndeterminate,
                _ => OperationError::ConnectionLost,
            };
            if request {
                assert_eq!(request_result(&receiver), Err(expected_error));
            } else if outcome == WriteOutcome::Committed {
                assert!(send_result(&receiver).is_ok());
            } else {
                assert_eq!(send_result(&receiver), Err(expected_error));
            }
            harness.driver.on_writer_stopped(harness.clock.now());
            harness.driver.on_writer_stopped(harness.clock.now());
            assert!(receiver.try_recv().is_err());
            assert_eq!(harness.driver.open_core_command_count(), 0);
            assert_eq!(harness.driver.pending_completion_count(), 0);
            assert_eq!(harness.driver.pending_write_count(), 0);
            assert_eq!(harness.driver.closer().count(), 1);
            assert_eq!(
                harness.driver.close_reason(),
                Some(GenerationCloseReason::LocalDisconnect)
            );
        }
    }
}

/// One failed write cannot falsely settle an independent unresolved write.
#[test]
fn writer_fault_keeps_other_write_pending_until_its_own_outcome() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let first = harness.accept_send("first", primary(1, 1, None));
    let second = harness.accept_send("second", primary(2, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (first_write, _) = admitted_data(&harness, 0);
    let (second_write, _) = admitted_data(&harness, 1);
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: first_write,
        outcome: WriteOutcome::Indeterminate(transport_fault()),
    }));
    assert_eq!(
        send_result(&first),
        Err(OperationError::DeliveryIndeterminate)
    );
    assert!(second.try_recv().is_err());
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: second_write,
        outcome: WriteOutcome::NotWritten(transport_fault()),
    }));
    assert_eq!(send_result(&second), Err(OperationError::ConnectionLost));
    assert!(first.try_recv().is_err());
    assert!(second.try_recv().is_err());
    assert_eq!(harness.driver.pending_completion_count(), 0);
}

/// A due control timeout closes transport without erasing a delayed Send commit.
#[test]
fn delayed_send_commit_survives_t6_priority_and_physical_close() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let probe = harness.accept("probe", ControlIntent::Linktest);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let probe_write = harness.driver.writer().admitted().last().unwrap().write_id;
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id: probe_write,
        outcome: WriteOutcome::Committed
    }));
    let send = harness.accept_send("send", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, _) = admitted_data(&harness, 0);
    harness.driver.on_write_outcome_at(
        write_id,
        WriteOutcome::Committed,
        MonoTime::from_elapsed(Duration::from_secs(1)),
        MonoTime::from_elapsed(Duration::from_secs(5)),
    );
    assert_eq!(
        control_result(&probe),
        Err(OperationError::Timeout(TimeoutKind::T6))
    );
    assert!(send_result(&send).is_ok());
    assert_eq!(harness.driver.closer().count(), 1);
    assert_eq!(harness.driver.pending_completion_count(), 0);
}

/// Writer finalization does not overwrite a Secondary received before shutdown.
#[test]
fn finalizing_missing_write_outcome_preserves_fast_secondary_success() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let request = harness.accept_request("request", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (_, message) = admitted_data(&harness, 0);
    drive_message(
        &mut harness,
        inbound_data(7, 1, 2, false, message.header().system_bytes().get(), None),
    );
    assert!(request_result(&request).is_ok());
    assert!(harness.drive_one(HarnessInput::Shutdown(GenerationCloseReason::TransportLost)));
    harness.driver.on_writer_stopped(harness.clock.now());
    assert!(request.try_recv().is_err());
    assert_eq!(harness.driver.pending_write_count(), 0);
    assert_eq!(harness.driver.pending_completion_count(), 0);
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
    assert!(maximum.try_recv().is_err());
    harness.driver.on_writer_stopped(harness.clock.now());
    assert_eq!(
        send_result(&maximum),
        Err(OperationError::DeliveryIndeterminate)
    );
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::RuntimeInvariant)
    );
}

/// Exhausting System Bytes requests rotation and preserves the final write's fact.
#[test]
fn system_bytes_retirement_keeps_last_send_until_actual_commit() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    harness
        .driver
        .core
        .seed_identifiers(Some(10), Some(u32::MAX));
    let last = harness.accept_send("last", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, message) = admitted_data(&harness, 0);
    assert_eq!(message.header().system_bytes().get(), u32::MAX);
    let exhausted = harness.accept_request("exhausted", primary(1, 1, None));
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert_eq!(
        request_result(&exhausted),
        Err(OperationError::ConnectionLost)
    );
    assert!(last.try_recv().is_err());
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::SystemBytesExhausted)
    );
    assert_eq!(harness.driver.writer().admitted().len(), 2);
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed
    }));
    assert!(send_result(&last).is_ok());
    harness.driver.on_writer_stopped(harness.clock.now());
    assert_eq!(harness.driver.pending_completion_count(), 0);
    assert_eq!(harness.driver.pending_write_count(), 0);
    assert_eq!(harness.driver.closer().count(), 1);
    assert!(last.try_recv().is_err());
    assert!(exhausted.try_recv().is_err());
}

/// One endpoint configuration supplies Driver capacities and Core drain deadlines.
#[test]
fn configured_driver_uses_endpoint_capacities_and_drain_policy() {
    let harness = DriverHarness::new();
    let policy = crate::hsms::RuntimePolicy::default()
        .with_protocol_error_capacity(3)
        .with_deadlines(
            Duration::from_secs(2),
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_secs(5),
            Duration::from_secs(20),
        );
    let config = crate::hsms::EndpointConfig::active(
        "127.0.0.1:5000".parse().unwrap(),
        SessionId::new(7).unwrap(),
    )
    .with_limits(crate::hsms::EndpointLimits::new(32, 2, 2, 2, 1, 1, 2, 1).unwrap())
    .with_runtime(policy);
    let mut driver = super::SessionDriver::from_config(
        harness.driver.generation,
        &config,
        harness.driver.writer,
        harness.driver.observer,
        harness.driver.closer,
    )
    .unwrap();
    assert_eq!(driver.command_capacity, 2);
    assert_eq!(driver.inbound_capacity, 1);
    assert_eq!(driver.protocol_error_capacity, 3);
    driver.on_connected(MonoTime::ZERO);
    driver.on_message(
        ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(99),
        }),
        MonoTime::ZERO,
    );
    driver.on_message(inbound_data(7, 1, 1, true, 42, None), MonoTime::ZERO);
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("deselect", harness.trace.clone());
    assert!(driver
        .try_accept_control(ControlIntent::Deselect, completion)
        .is_ok());
    assert!(driver.drive_next_command(MonoTime::ZERO));
    assert_eq!(
        driver.next_deadline(),
        Some(MonoTime::from_elapsed(Duration::from_secs(2)))
    );
    driver.advance_time(MonoTime::from_elapsed(Duration::from_secs(2)));
    assert_eq!(
        control_result(&receiver),
        Err(OperationError::Timeout(TimeoutKind::Drain))
    );
    assert_eq!(driver.close_reason(), None);
}

/// Decodes an independent wire fixture through the strict production codec.
fn decode_fixture(
    bytes: &[u8],
    decoder: crate::secs2::codec::Secs2Decoder,
) -> crate::hsms::codec::HsmsSsDecodeStep {
    let mut codec =
        crate::hsms::codec::HsmsSsCodec::new(crate::hsms::EndpointLimits::default(), decoder);
    codec.decode(&mut bytes::BytesMut::from(bytes)).unwrap()
}

/// Unsupported SType wins over PType and Reject copies the exact received facts.
#[test]
fn malformed_header_priority_and_reject_preserve_original_context() {
    use crate::hsms::{HeaderViolationKind, InboundViolationKind};
    for (p_type, s_type, reason, reference) in [
        (3, 99, RejectReason::UNSUPPORTED_STYPE, 99),
        (3, 5, RejectReason::UNSUPPORTED_PTYPE, 3),
    ] {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        let frame = [0, 0, 0, 10, 0x12, 0x34, 0, 0, p_type, s_type, 1, 2, 3, 4];
        let decoded = decode_fixture(&frame, crate::secs2::codec::Secs2Decoder::default());
        assert!(harness.driver.on_decode_step(decoded, MonoTime::ZERO));
        let error = harness.driver.take_protocol_error().unwrap();
        assert_eq!(
            error.context().header(),
            <&[u8; 10]>::try_from(&frame[4..]).unwrap()
        );
        let expected = if s_type == 99 {
            HeaderViolationKind::UnknownSessionType { s_type }
        } else {
            HeaderViolationKind::UnknownPresentationType { p_type }
        };
        assert_eq!(error.kind(), InboundViolationKind::Header(expected));
        assert!(error.decode_error().is_none());
        let ProtocolMessage::Control(ControlMessage::RejectRequest {
            session_id,
            header_byte_2,
            reason: actual,
            system_bytes,
        }) = harness.driver.writer().admitted().last().unwrap().message
        else {
            panic!("expected Reject");
        };
        assert_eq!(session_id, 0x1234);
        assert_eq!(header_byte_2, reference);
        assert_eq!(actual, reason);
        assert_eq!(system_bytes.get(), 0x01020304);
        assert_eq!(harness.driver.close_reason(), None);
    }
}

/// Malformed/over-limit Secondary bodies cannot consume the pending request.
#[test]
fn invalid_secondary_retains_transaction_and_detailed_decoder_error() {
    use crate::hsms::{InboundViolationKind, PayloadViolationKind};
    use crate::secs2::{codec::Secs2Decoder, DecodeLimits};
    for limited in [false, true] {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        enter_selected(&mut harness);
        let receiver = harness.accept_request("request", primary(1, 1, None));
        assert!(harness.drive_one(HarnessInput::AcceptedCommand));
        let bytes: &[u8] = if limited {
            &[0, 0, 0, 14, 0, 7, 1, 2, 0, 0, 0, 0, 0, 0, 0x21, 2, 1, 2]
        } else {
            &[0, 0, 0, 11, 0, 7, 1, 2, 0, 0, 0, 0, 0, 0, 0xff]
        };
        let decoder = if limited {
            Secs2Decoder::new(DecodeLimits::new(4, 4, 1, 4).unwrap())
        } else {
            Secs2Decoder::default()
        };
        assert!(harness
            .driver
            .on_decode_step(decode_fixture(bytes, decoder), MonoTime::ZERO));
        let error = harness.driver.take_protocol_error().unwrap();
        assert_eq!(
            error.kind(),
            InboundViolationKind::Payload(if limited {
                PayloadViolationKind::ResourceLimitExceeded
            } else {
                PayloadViolationKind::MalformedSecs2
            })
        );
        assert!(error.decode_error().is_some());
        assert_eq!(harness.driver.pending_data_transaction_count(), 1);
        assert!(receiver.try_recv().is_err());
        assert!(harness.driver.take_inbound().is_none());
        drive_message(&mut harness, inbound_data(7, 1, 2, false, 0, None));
        assert!(request_result(&receiver).is_ok());
        assert_eq!(harness.driver.close_reason(), None);
    }
}

/// Error pressure never overwrites earlier reports or consumes Primary capacity.
#[test]
fn reliable_protocol_error_overflow_closes_without_overwriting_prior_report() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    harness.driver.protocol_error_capacity = 1;
    drive_message(&mut harness, inbound_data(7, 1, 1, false, 42, None));
    let bytes = [0, 0, 0, 10, 255, 255, 1, 0, 0, 5, 0, 0, 0, 1];
    assert!(harness.driver.on_decode_step(
        decode_fixture(&bytes, crate::secs2::codec::Secs2Decoder::default()),
        MonoTime::ZERO
    ));
    assert!(!harness.driver.on_decode_step(
        decode_fixture(&bytes, crate::secs2::codec::Secs2Decoder::default()),
        MonoTime::ZERO
    ));
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::ApplicationBackpressure)
    );
    assert!(harness.driver.take_protocol_error().is_some());
    assert!(harness.driver.take_protocol_error().is_none());
    assert!(harness.driver.take_inbound().is_some());
}

/// Delivers a peer W=1 Primary and extracts its exclusive public reply token.
fn peer_token(harness: &mut DriverHarness, function: u8) -> crate::hsms::ReplyToken {
    drive_message(harness, inbound_data(7, 3, function, true, 42, None));
    let event = harness.driver.take_inbound().unwrap();
    assert_eq!(
        event.context().header(),
        &[0, 7, 0x83, function, 0, 0, 0, 0, 0, 42]
    );
    assert_eq!(event.context().generation().get(), 17);
    let (_, crate::hsms::InboundToken::Reply(token)) = event.into_parts() else {
        panic!("W=1 must provide reply capability");
    };
    token
}

/// Driver materializes an exclusive token and completes reply from its actual write.
#[test]
fn inbound_token_reply_runs_through_common_command_and_writer_paths() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let token = peer_token(&mut harness, 3);
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("reply", harness.trace.clone());
    assert!(harness
        .driver
        .try_accept_reply(
            crate::hsms::ReplyIntent::Secondary,
            token,
            Some(SecsItem::Binary(vec![4])),
            completion
        )
        .is_ok());
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert!(receiver.try_recv().is_err());
    let (write_id, message) = admitted_data(&harness, 0);
    assert_eq!(message.header().function().get(), 4);
    assert_eq!(message.header().system_bytes().get(), 42);
    assert!(!message.header().reply_expected());
    assert_eq!(harness.driver.core.reply_capability_count(), 0);
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed
    }));
    assert!(send_result(&receiver).is_ok());
    assert_eq!(harness.driver.close_reason(), None);
}

/// F255 preflight returns the original token, allowing explicit abort or abandon.
#[test]
fn f255_reply_rejection_returns_token_for_abort_or_abandon() {
    for intent in [
        crate::hsms::ReplyIntent::Abort,
        crate::hsms::ReplyIntent::Abandon,
    ] {
        let mut harness = DriverHarness::new();
        assert!(harness.drive_one(HarnessInput::Connected));
        enter_selected(&mut harness);
        let token = peer_token(&mut harness, 255);
        let (completion, receiver) =
            super::test_support::FakeCompletion::channel("reply", harness.trace.clone());
        let Err((completion, DriverCommandResult::ReplyRejected { token, error, .. })) = harness
            .driver
            .try_accept_reply(crate::hsms::ReplyIntent::Secondary, token, None, completion)
        else {
            panic!("normal F255 reply must be rejected");
        };
        assert_eq!(error, OperationError::ReplyRequiresAbort);
        assert!(receiver.try_recv().is_err());
        assert_eq!(harness.driver.core.reply_capability_count(), 1);
        assert!(harness
            .driver
            .try_accept_reply(intent, token, None, completion)
            .is_ok());
        assert!(harness.drive_one(HarnessInput::AcceptedCommand));
        if intent == crate::hsms::ReplyIntent::Abandon {
            assert_eq!(control_result(&receiver), Ok(()));
        } else {
            let (write_id, reply) = admitted_data(&harness, 0);
            assert_eq!(reply.header().function().get(), 0);
            assert!(reply.body().is_none());
            assert!(harness.drive_one(HarnessInput::WriteOutcome {
                write_id,
                outcome: WriteOutcome::Committed
            }));
            assert!(send_result(&receiver).is_ok());
        }
        assert_eq!(harness.driver.core.reply_capability_count(), 0);
    }
}

/// Writer saturation returns reply inputs after queue acceptance, before Core use.
#[test]
fn reply_writer_full_retains_capability_and_original_body() {
    let mut harness = DriverHarness::with_limits(4, 4, 4, 0);
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let token = peer_token(&mut harness, 1);
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("reply", harness.trace.clone());
    let body = Some(SecsItem::Binary(vec![7]));
    assert!(harness
        .driver
        .try_accept_reply(
            crate::hsms::ReplyIntent::Secondary,
            token,
            body.clone(),
            completion
        )
        .is_ok());
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let DriverCommandResult::ReplyRejected {
        token,
        body: returned,
        error,
        ..
    } = receiver.try_recv().unwrap()
    else {
        panic!("pre-Core failure must return token");
    };
    assert_eq!(returned, body);
    assert_eq!(error, OperationError::Backpressure);
    assert_eq!(harness.driver.core.reply_capability_count(), 1);
    assert_eq!(harness.driver.close_reason(), None);
    let (completion, abandoned) =
        super::test_support::FakeCompletion::channel("abandon", harness.trace.clone());
    assert!(harness
        .driver
        .try_accept_reply(crate::hsms::ReplyIntent::Abandon, token, None, completion)
        .is_ok());
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert_eq!(control_result(&abandoned), Ok(()));
}

/// Foreign ownership is rejected without consuming a token; queued close returns it.
#[test]
fn foreign_reply_and_queued_shutdown_preserve_exclusive_token() {
    let mut origin = DriverHarness::new();
    assert!(origin.drive_one(HarnessInput::Connected));
    enter_selected(&mut origin);
    let token = peer_token(&mut origin, 1);
    let mut foreign = DriverHarness::new();
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("reply", origin.trace.clone());
    let Err((completion, DriverCommandResult::ReplyRejected { token, error, .. })) = foreign
        .driver
        .try_accept_reply(crate::hsms::ReplyIntent::Secondary, token, None, completion)
    else {
        panic!("foreign token must be rejected");
    };
    assert_eq!(error, OperationError::ReplyCapabilityUnavailable);
    assert!(origin
        .driver
        .try_accept_reply(crate::hsms::ReplyIntent::Secondary, token, None, completion)
        .is_ok());
    assert!(origin.drive_one(HarnessInput::Shutdown(GenerationCloseReason::LocalStop)));
    assert!(matches!(
        receiver.try_recv().unwrap(),
        DriverCommandResult::ReplyRejected {
            error: OperationError::ConnectionLost,
            ..
        }
    ));
    assert_eq!(origin.driver.core.reply_capability_count(), 0);
    assert!(receiver.try_recv().is_err());
}

/// Deselect's sent-request barrier returns reply ownership until peer rejection.
#[test]
fn deselect_barrier_returns_reply_token_and_rejection_reopens_admission() {
    use crate::hsms::protocol::header::DeselectStatus;
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    let deselect = harness.accept("deselect", ControlIntent::Deselect);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let admitted = harness.driver.writer().admitted().last().unwrap();
    let ProtocolMessage::Control(ControlMessage::DeselectRequest { system_bytes, .. }) =
        admitted.message
    else {
        panic!("Deselect request");
    };
    let token = peer_token(&mut harness, 1);
    let (completion, reply_result) =
        super::test_support::FakeCompletion::channel("reply", harness.trace.clone());
    let Err((completion, DriverCommandResult::ReplyRejected { token, error, .. })) = harness
        .driver
        .try_accept_reply(crate::hsms::ReplyIntent::Secondary, token, None, completion)
    else {
        panic!("Data must not follow Deselect.req");
    };
    assert_eq!(error, OperationError::Draining);
    assert_eq!(harness.driver.core.reply_capability_count(), 1);
    drive_message(
        &mut harness,
        ProtocolMessage::Control(ControlMessage::DeselectResponse {
            session_id: u16::MAX,
            system_bytes,
            status: DeselectStatus::BUSY,
        }),
    );
    assert!(matches!(
        control_result(&deselect),
        Err(OperationError::DeselectRejected { .. })
    ));
    assert!(harness
        .driver
        .try_accept_reply(crate::hsms::ReplyIntent::Secondary, token, None, completion)
        .is_ok());
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, _) = admitted_data(&harness, 0);
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed
    }));
    assert!(send_result(&reply_result).is_ok());
    assert_eq!(harness.driver.close_reason(), None);
}

/// Reliable delivery pressure closes the generation and revokes minted tokens.
#[test]
fn full_inbound_delivery_revokes_new_and_previous_capabilities() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    harness.driver.set_delivery_capacity(1, 1);
    drive_message(&mut harness, inbound_data(7, 1, 1, true, 10, None));
    drive_message(&mut harness, inbound_data(7, 1, 1, true, 11, None));
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::ApplicationBackpressure)
    );
    assert_eq!(harness.driver.core.reply_capability_count(), 0);
    assert!(harness.driver.take_inbound().is_some());
    assert!(harness.driver.take_inbound().is_none());
}

/// Graceful close waits for Separate commit and retains its cause over late T8.
#[test]
fn pure_driver_shutdown_drain_preserves_barrier_and_first_reason() {
    let mut harness = DriverHarness::new();
    harness.drive_one(HarnessInput::Connected);
    enter_selected(&mut harness);
    harness.driver.begin_shutdown_drain();
    assert!(harness.driver.shutdown_drain_ready());
    harness
        .driver
        .finish_shutdown_drain(GenerationCloseReason::LocalStop, harness.clock.now());
    assert!(!harness.driver.transport_closed());
    let write_id = harness.driver.writer().admitted().last().unwrap().write_id;
    harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed,
    });
    assert!(harness.driver.transport_closed());
    harness.driver.on_shutdown(
        GenerationCloseReason::CommunicationsTimeout(
            crate::hsms::model::runtime::CommunicationsTimeoutKind::T8,
        ),
        None,
        harness.clock.now(),
    );
    assert_eq!(
        harness.driver.close_reason(),
        Some(GenerationCloseReason::LocalStop)
    );
}

/// Delivery certainty depends on byte visibility, independent of I/O failure category.
#[test]
fn transport_fault_categories_preserve_zero_and_partial_write_distinction() {
    for kind in [
        TransportFaultKind::WriteZero,
        TransportFaultKind::TimedOut,
        TransportFaultKind::Cancelled,
        TransportFaultKind::Other,
    ] {
        for partial in [false, true] {
            let mut harness = DriverHarness::new();
            harness.drive_one(HarnessInput::Connected);
            enter_selected(&mut harness);
            let receiver = harness.accept_send("send", primary(1, 1, None));
            harness.drive_one(HarnessInput::AcceptedCommand);
            let (write_id, _) = admitted_data(&harness, 0);
            let fault = TransportFault::new(kind);
            let outcome = if partial {
                WriteOutcome::Indeterminate(fault)
            } else {
                WriteOutcome::NotWritten(fault)
            };
            harness.drive_one(HarnessInput::WriteOutcome { write_id, outcome });
            assert_eq!(
                send_result(&receiver),
                Err(if partial {
                    OperationError::DeliveryIndeterminate
                } else {
                    OperationError::ConnectionLost
                })
            );
        }
    }
}

/// Byte pressure preserves ownership and releases charges on dequeue and close.
#[test]
fn command_byte_budget_preserves_ownership_and_releases_on_every_exit() {
    let mut harness = DriverHarness::new();
    harness.drive_one(HarnessInput::Connected);
    enter_selected(&mut harness);
    harness.driver.command_byte_capacity = 19;
    let first = harness.accept_send(
        "first",
        primary(1, 1, Some(SecsItem::Binary(vec![1, 2, 3]))),
    );
    assert_eq!(harness.driver.command_bytes, 19);
    let next_id = harness.driver.next_command_id;
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("retry", harness.trace.clone());
    let rejected = match harness
        .driver
        .try_accept_request(primary(1, 1, None), completion)
    {
        Err(error) => error,
        Ok(()) => panic!("encoded byte budget must be enforced"),
    };
    assert_eq!(rejected.kind(), super::DataAdmissionErrorKind::Full);
    assert_eq!(harness.driver.next_command_id, next_id);
    assert!(receiver.try_recv().is_err());
    let _control = harness.accept("linktest", ControlIntent::Linktest);
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert_eq!(harness.driver.command_bytes, 0);
    let (message, completion) = rejected.into_parts();
    assert!(harness
        .driver
        .try_accept_request(message, completion)
        .is_ok());
    assert_eq!(harness.driver.command_bytes, 14);
    harness
        .driver
        .on_shutdown(GenerationCloseReason::LocalStop, None, MonoTime::ZERO);
    assert_eq!(harness.driver.command_bytes, 0);
    let DriverCommandResult::PrimaryRejected {
        message,
        reply_expected: true,
        error: OperationError::ConnectionLost,
    } = receiver.try_recv().unwrap()
    else {
        panic!("shutdown must return the queued, unconsumed Primary");
    };
    assert_eq!(message, primary(1, 1, None));
    assert!(first.try_recv().is_err());
}

/// A too-large body is returned before queuing or consuming a protocol identity.
#[test]
fn command_wire_limit_rejection_returns_original_body() {
    let mut harness = DriverHarness::new();
    harness.driver.maximum_message_length = 10;
    let message = primary(1, 1, Some(SecsItem::Binary(vec![3])));
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("large", harness.trace.clone());
    let error = match harness.driver.try_accept_send(message.clone(), completion) {
        Err(error) => error,
        Ok(()) => panic!("oversized body must be rejected before admission"),
    };
    assert_eq!(
        error.kind(),
        super::DataAdmissionErrorKind::Invalid(OperationError::OutboundFrameTooLarge {
            text_length: 3,
            maximum_message_length: 10,
        })
    );
    assert_eq!(error.message(), &message);
    assert_eq!(harness.driver.command_bytes, 0);
    assert_eq!(harness.driver.next_command_id, Some(0));
    assert!(receiver.try_recv().is_err());
}

/// Reply budget rejection returns the capability for a subsequent accepted retry.
#[test]
fn reply_byte_budget_returns_token_for_retry_after_queue_drain() {
    let mut harness = DriverHarness::new();
    harness.drive_one(HarnessInput::Connected);
    enter_selected(&mut harness);
    drive_message(&mut harness, inbound_data(7, 1, 1, true, 42, None));
    let (_, crate::hsms::InboundToken::Reply(token)) =
        harness.driver.take_inbound().unwrap().into_parts()
    else {
        panic!("reply capability expected")
    };
    harness.driver.command_byte_capacity = 19;
    let _first = harness.accept_send("first", primary(1, 1, None));
    let body = Some(SecsItem::Binary(vec![1, 2, 3]));
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("reply", harness.trace.clone());
    let (completion, rejected) = harness
        .driver
        .try_accept_reply(
            crate::hsms::ReplyIntent::Secondary,
            token,
            body.clone(),
            completion,
        )
        .unwrap_err();
    let DriverCommandResult::ReplyRejected {
        intent,
        token,
        body: returned,
        error,
    } = rejected
    else {
        panic!("reply inputs must be returned")
    };
    assert_eq!(error, OperationError::Backpressure);
    assert_eq!(returned, body);
    assert!(receiver.try_recv().is_err());
    assert_eq!(harness.driver.core.reply_capability_count(), 1);
    harness.drive_one(HarnessInput::AcceptedCommand);
    assert!(harness
        .driver
        .try_accept_reply(intent, token, returned, completion)
        .is_ok());
    assert_eq!(harness.driver.command_bytes, 19);
    harness.drive_one(HarnessInput::AcceptedCommand);
    assert_eq!(harness.driver.command_bytes, 0);
    assert_eq!(harness.driver.core.reply_capability_count(), 0);
}

/// A full command queue returns ownership and allows retry after one dequeue.
#[test]
fn command_fifo_bound_preserves_rejected_message_and_completion() {
    let mut harness = DriverHarness::new();
    assert!(harness.drive_one(HarnessInput::Connected));
    enter_selected(&mut harness);
    harness.driver.command_capacity = 1;
    let probe = harness.accept("probe", ControlIntent::Linktest);
    let message = primary(1, 1, Some(SecsItem::Binary(vec![1, 2, 3])));
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("send", harness.trace.clone());
    let rejected = match harness.driver.try_accept_send(message.clone(), completion) {
        Err(rejected) => rejected,
        Ok(()) => panic!("full command FIFO must reject"),
    };
    assert_eq!(rejected.kind(), super::DataAdmissionErrorKind::Full);
    assert_eq!(rejected.message(), &message);
    assert!(receiver.try_recv().is_err());
    let (message, completion) = rejected.into_parts();
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    assert!(harness.driver.try_accept_send(message, completion).is_ok());
    assert!(harness.drive_one(HarnessInput::AcceptedCommand));
    let (write_id, _) = admitted_data(&harness, 0);
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed
    }));
    assert!(send_result(&receiver).is_ok());
    assert!(probe.try_recv().is_err());
    assert_eq!(harness.driver.close_reason(), None);
}

/// Control and Data share one finite command FIFO and closing takes precedence.
#[test]
fn control_admission_bound_returns_original_unconsumed_endpoint() {
    let mut harness = DriverHarness::new();
    harness.driver.command_capacity = 1;
    let _first = harness.accept_send("send", primary(1, 1, None));
    let (completion, receiver) =
        super::test_support::FakeCompletion::channel("control", harness.trace.clone());
    let rejected = harness
        .driver
        .try_accept_control(ControlIntent::Select, completion)
        .unwrap_err();
    assert_eq!(rejected.kind(), super::ControlAdmissionErrorKind::Full);
    let (intent, completion) = rejected.into_parts();
    assert_eq!(intent, ControlIntent::Select);
    assert!(receiver.try_recv().is_err());
    harness
        .driver
        .on_shutdown(GenerationCloseReason::LocalStop, None, MonoTime::ZERO);
    let rejected = harness
        .driver
        .try_accept_control(intent, completion)
        .unwrap_err();
    assert_eq!(rejected.kind(), super::ControlAdmissionErrorKind::Closing);
    assert!(receiver.try_recv().is_err());
}

/// Real Writer rejects oversize Data before Core IDs and later sends valid bytes.
#[cfg(feature = "runtime-tokio")]
#[tokio::test]
async fn bounded_writer_preflight_preserves_session_and_real_send_receipt() {
    use crate::hsms::{
        generation::transport::bounded_writer::{BoundedWriter, WriterPolicy},
        EndpointLimits,
    };
    use tokio::{io::AsyncReadExt, sync::watch, time::Instant};
    let harness = DriverHarness::new();
    let epoch = Instant::now();
    let (writer, worker, mut reports) = BoundedWriter::new(
        EndpointLimits::new(32, 4, 1, 2, 4, 4, 4, 4).unwrap(),
        WriterPolicy {
            data_bytes: 64,
            residence: Duration::from_secs(3),
            active_write: Duration::from_secs(1),
        },
        epoch,
    )
    .unwrap();
    let mut driver = super::SessionDriver::new(
        harness.driver.generation,
        harness.driver.core,
        writer,
        harness.driver.observer,
        harness.driver.closer,
    );
    let (write_half, mut peer) = tokio::io::duplex(128);
    let (cancel, cancellation) = watch::channel(false);
    let task = tokio::spawn(worker.run(write_half, cancellation));
    driver.on_connected(MonoTime::ZERO);
    driver.on_message(
        ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(91),
        }),
        MonoTime::ZERO,
    );
    let report = reports.recv().await.unwrap();
    driver.on_write_outcome_at(
        report.write_id,
        report.outcome,
        report.occurred_at,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    drop(report);
    let mut selection = [0; 14];
    peer.read_exact(&mut selection).await.unwrap();
    assert_eq!(selection, [0, 0, 0, 10, 255, 255, 0, 0, 0, 2, 0, 0, 0, 91]);

    let (completion, rejected) =
        super::test_support::FakeCompletion::channel("large", harness.trace.clone());
    assert!(driver
        .try_accept_send(
            primary(1, 1, Some(SecsItem::Binary(vec![0; 33]))),
            completion
        )
        .is_ok());
    driver.drive_next_command(MonoTime::from_elapsed(epoch.elapsed()));
    assert!(matches!(
        send_result(&rejected),
        Err(OperationError::OutboundFrameTooLarge { .. })
    ));
    assert_eq!(driver.close_reason(), None);
    assert_eq!(driver.open_core_command_count(), 0);

    let (completion, sent) =
        super::test_support::FakeCompletion::channel("valid", harness.trace.clone());
    assert!(driver
        .try_accept_send(
            primary(1, 1, Some(SecsItem::Binary(vec![1, 2, 3]))),
            completion
        )
        .is_ok());
    driver.drive_next_command(MonoTime::from_elapsed(epoch.elapsed()));
    let report = reports.recv().await.unwrap();
    driver.on_write_outcome_at(
        report.write_id,
        report.outcome,
        report.occurred_at,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    drop(report);
    assert!(send_result(&sent).is_ok());
    let mut bytes = [0; 19];
    peer.read_exact(&mut bytes).await.unwrap();
    assert_eq!(
        bytes,
        [0, 0, 0, 15, 0, 7, 1, 1, 0, 0, 0, 0, 0, 0, 0x21, 3, 1, 2, 3]
    );
    driver.on_shutdown(
        GenerationCloseReason::LocalStop,
        None,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    cancel.send(true).unwrap();
    task.await.unwrap();
    assert!(reports.recv().await.is_none());
    driver.on_writer_stopped(MonoTime::from_elapsed(epoch.elapsed()));
    assert_eq!(driver.pending_completion_count(), 0);
    assert_eq!(driver.pending_write_count(), 0);
}

/// Real TCP carries independent Select and Secondary bytes through both workers.
#[cfg(feature = "runtime-tokio")]
#[tokio::test]
async fn tcp_reader_writer_driver_exchange_and_join_without_lost_completions() {
    use crate::hsms::{
        codec::HsmsSsDecodeStep,
        generation::transport::{bounded_reader::ReaderWorker, bounded_writer::BoundedWriter},
        EndpointLimits,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::watch,
        time::Instant,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (connected, accepted) = tokio::join!(
        TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    let (read_half, write_half) = connected.unwrap().into_split();
    let (mut peer, _) = accepted.unwrap();
    let epoch = Instant::now();
    let limits = EndpointLimits::new(32, 4, 2, 2, 4, 4, 4, 4).unwrap();
    let config =
        crate::hsms::EndpointConfig::active(peer.local_addr().unwrap(), SessionId::new(7).unwrap())
            .with_limits(limits)
            .with_runtime(
                crate::hsms::RuntimePolicy::default()
                    .with_byte_budgets(64, 72, 64)
                    .with_protocol_error_capacity(2)
                    .with_deadlines(
                        Duration::from_secs(5),
                        Duration::from_secs(3),
                        Duration::from_secs(1),
                        Duration::from_secs(5),
                        Duration::from_secs(20),
                    ),
            );
    let (reader, mut inbound) = ReaderWorker::from_config(read_half, &config, epoch).unwrap();
    let (writer, worker, mut outcomes) = BoundedWriter::from_config(&config, epoch).unwrap();
    let harness = DriverHarness::new();
    let mut driver = super::SessionDriver::from_config(
        harness.driver.generation,
        &config,
        writer,
        harness.driver.observer,
        harness.driver.closer,
    )
    .unwrap();
    let (cancel, cancellation) = watch::channel(false);
    let reader_task = tokio::spawn(reader.run(cancellation.clone()));
    let writer_task = tokio::spawn(worker.run(write_half, cancellation));
    driver.on_connected(MonoTime::from_elapsed(epoch.elapsed()));
    peer.write_all(&[0, 0, 0, 10, 0x12, 0x34, 0, 0, 3, 99, 1, 2, 3, 4])
        .await
        .unwrap();
    let report = inbound.recv().await.unwrap();
    driver.on_decode_step(
        report.frame.decoded.clone(),
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    drop(report);
    let mut reject = [0; 14];
    peer.read_exact(&mut reject).await.unwrap();
    assert_eq!(reject, [0, 0, 0, 10, 0x12, 0x34, 99, 1, 0, 7, 1, 2, 3, 4]);
    let error = driver.take_protocol_error().unwrap();
    assert_eq!(
        error.context().header(),
        &[0x12, 0x34, 0, 0, 3, 99, 1, 2, 3, 4]
    );
    let report = outcomes.recv().await.unwrap();
    driver.on_write_outcome_at(
        report.write_id,
        report.outcome,
        report.occurred_at,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    drop(report);
    peer.write_all(&[0, 0, 0, 10, 255, 255, 0, 0, 0, 1, 0, 0, 0, 91])
        .await
        .unwrap();
    let report = inbound.recv().await.unwrap();
    let HsmsSsDecodeStep::Message(message) = report.frame.decoded.clone() else {
        panic!("valid Select fixture");
    };
    driver.on_message(message, MonoTime::from_elapsed(epoch.elapsed()));
    drop(report);
    let mut response = [0; 14];
    peer.read_exact(&mut response).await.unwrap();
    assert_eq!(response, [0, 0, 0, 10, 255, 255, 0, 0, 0, 2, 0, 0, 0, 91]);
    let report = outcomes.recv().await.unwrap();
    driver.on_write_outcome_at(
        report.write_id,
        report.outcome,
        report.occurred_at,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    drop(report);
    let (completion, result) =
        super::test_support::FakeCompletion::channel("request", harness.trace.clone());
    assert!(driver
        .try_accept_request(primary(1, 1, None), completion)
        .is_ok());
    driver.drive_next_command(MonoTime::from_elapsed(epoch.elapsed()));
    let mut request = [0; 14];
    peer.read_exact(&mut request).await.unwrap();
    assert_eq!(request, [0, 0, 0, 10, 0, 7, 0x81, 1, 0, 0, 0, 0, 0, 0]);
    // Process the peer response first to verify a fast reply before commit callback.
    peer.write_all(&[0, 0, 0, 10, 0, 7, 1, 2, 0, 0, 0, 0, 0, 0])
        .await
        .unwrap();
    let report = inbound.recv().await.unwrap();
    let HsmsSsDecodeStep::Message(message) = report.frame.decoded.clone() else {
        panic!("valid Secondary fixture");
    };
    driver.on_message(message, MonoTime::from_elapsed(epoch.elapsed()));
    drop(report);
    assert!(request_result(&result).is_ok());
    let report = outcomes.recv().await.unwrap();
    driver.on_write_outcome_at(
        report.write_id,
        report.outcome,
        report.occurred_at,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    drop(report);
    assert!(result.try_recv().is_err());
    assert_eq!(driver.pending_data_transaction_count(), 0);
    peer.write_all(&[0, 0, 0, 10, 0, 7, 0x83, 5, 0, 0, 0, 0, 0, 42])
        .await
        .unwrap();
    let report = inbound.recv().await.unwrap();
    let HsmsSsDecodeStep::Message(message) = report.frame.decoded.clone() else {
        panic!("valid peer Primary");
    };
    driver.on_message(message, MonoTime::from_elapsed(epoch.elapsed()));
    drop(report);
    let event = driver.take_inbound().unwrap();
    assert_eq!(
        event.context().header(),
        &[0, 7, 0x83, 5, 0, 0, 0, 0, 0, 42]
    );
    let (_, crate::hsms::InboundToken::Reply(token)) = event.into_parts() else {
        panic!("reply token");
    };
    let (completion, reply_result) =
        super::test_support::FakeCompletion::channel("peer-reply", harness.trace.clone());
    assert!(driver
        .try_accept_reply(
            crate::hsms::ReplyIntent::Secondary,
            token,
            Some(SecsItem::Binary(vec![9])),
            completion
        )
        .is_ok());
    driver.drive_next_command(MonoTime::from_elapsed(epoch.elapsed()));
    let mut reply = [0; 17];
    peer.read_exact(&mut reply).await.unwrap();
    assert_eq!(
        reply,
        [0, 0, 0, 13, 0, 7, 3, 6, 0, 0, 0, 0, 0, 42, 0x21, 1, 9]
    );
    let report = outcomes.recv().await.unwrap();
    driver.on_write_outcome_at(
        report.write_id,
        report.outcome,
        report.occurred_at,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    drop(report);
    assert!(send_result(&reply_result).is_ok());
    assert_eq!(driver.core.reply_capability_count(), 0);
    let (completion, deselected) =
        super::test_support::FakeCompletion::channel("deselect", harness.trace.clone());
    assert!(driver
        .try_accept_control(ControlIntent::Deselect, completion)
        .is_ok());
    driver.drive_next_command(MonoTime::from_elapsed(epoch.elapsed()));
    let mut deselect_request = [0; 14];
    peer.read_exact(&mut deselect_request).await.unwrap();
    assert_eq!(
        deselect_request,
        [0, 0, 0, 10, 255, 255, 0, 0, 0, 3, 0, 0, 0, 1]
    );
    peer.write_all(&[0, 0, 0, 10, 255, 255, 0, 0, 0, 4, 0, 0, 0, 1])
        .await
        .unwrap();
    let report = inbound.recv().await.unwrap();
    let HsmsSsDecodeStep::Message(message) = report.frame.decoded.clone() else {
        panic!("valid Deselect response");
    };
    driver.on_message(message, MonoTime::from_elapsed(epoch.elapsed()));
    drop(report);
    assert_eq!(control_result(&deselected), Ok(()));
    assert_eq!(driver.state(), Some(SessionState::NotSelected));
    assert!(!driver.transport_closed());
    let report = outcomes.recv().await.unwrap();
    driver.on_write_outcome_at(
        report.write_id,
        report.outcome,
        report.occurred_at,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    drop(report);
    driver.on_shutdown(
        GenerationCloseReason::LocalStop,
        None,
        MonoTime::from_elapsed(epoch.elapsed()),
    );
    cancel.send(true).unwrap();
    writer_task.await.unwrap();
    assert!(reader_task.await.unwrap().is_err());
    assert!(outcomes.recv().await.is_none());
    assert!(inbound.recv().await.is_none());
    driver.on_writer_stopped(MonoTime::from_elapsed(epoch.elapsed()));
    assert_eq!(driver.pending_completion_count(), 0);
    assert_eq!(driver.pending_write_count(), 0);
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

        assert!(open_send.try_recv().is_err());
        harness.driver.on_writer_stopped(harness.clock.now());
        assert_eq!(
            send_result(&open_send),
            Err(OperationError::DeliveryIndeterminate)
        );
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
                TraceEvent::TransportClosed,
                TraceEvent::RequestCommandCompleted {
                    label: "queued-request",
                    result: Err(OperationError::ConnectionLost),
                },
                TraceEvent::SendCommandCompleted {
                    label: "open-send",
                    result: Err(OperationError::DeliveryIndeterminate),
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

    assert!(first.try_recv().is_err());
    assert_eq!(request_result(&second), Err(OperationError::ConnectionLost));
    assert_eq!(harness.driver.pending_write_count(), 1);
    assert_eq!(harness.driver.pending_data_transaction_count(), 0);
    assert_eq!(harness.driver.pending_completion_count(), 1);
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
            TraceEvent::TransportClosed,
        ]
    ));
    assert!(harness.drive_one(HarnessInput::WriteOutcome {
        write_id,
        outcome: WriteOutcome::Committed,
    }));
    assert_eq!(harness.driver.pending_write_count(), 0);
    assert!(send_result(&first).is_ok());
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

    assert!(maximum.try_recv().is_err());
    harness.driver.on_writer_stopped(harness.clock.now());
    assert_eq!(
        send_result(&maximum),
        Err(OperationError::DeliveryIndeterminate)
    );
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
            TraceEvent::TransportClosed,
            TraceEvent::SendCommandCompleted {
                label: "queued",
                result: Err(OperationError::ConnectionLost),
            },
            TraceEvent::SendCommandCompleted {
                label: "maximum",
                result: Err(OperationError::DeliveryIndeterminate),
            },
        ]
    );
}
