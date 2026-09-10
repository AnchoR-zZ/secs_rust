//! Deterministic deadline and callback-latency regressions for the real Core.

use std::time::Duration;

use crate::hsms::{
    config::HsmsTimeouts,
    core::{
        CoreAction, CoreCommand, CoreCommandKind, CoreCommandResult, OutboundPrimary, SessionCore,
        SessionCoreConfig,
    },
    error::{OperationError, TimeoutKind},
    lifecycle::SessionState,
    model::{
        ids::{CommandId, Function, SessionId, Stream, SystemBytes, WriteId},
        runtime::{CommunicationsTimeoutKind, GenerationCloseReason, MonoTime, WriteOutcome},
    },
    protocol::{
        header::ControlMessage,
        message::{DataMessage, ProtocolMessage},
    },
};

/// Converts fixture milliseconds to generation-local logical time.
fn time(milliseconds: u64) -> MonoTime {
    MonoTime::from_elapsed(Duration::from_millis(milliseconds))
}

/// Builds a connected Core with short deterministic timers and optional probes.
fn connected(idle: Option<Duration>) -> SessionCore {
    let timeouts = HsmsTimeouts::new(
        Duration::from_secs(1),
        Duration::from_millis(40),
        Duration::from_secs(1),
        Duration::from_millis(5),
        Duration::from_secs(1),
        Duration::from_secs(1),
        idle,
    )
    .unwrap();
    let mut core = SessionCore::new(
        SessionCoreConfig::new(SessionId::new(7).unwrap()).with_timeouts(timeouts),
    );
    core.on_connected(time(0));
    core
}

/// Selects a fixture through the actual passive protocol path and commits its response.
fn selected(idle: Option<Duration>) -> SessionCore {
    let mut core = connected(idle);
    let actions = core.on_message(
        ProtocolMessage::Control(ControlMessage::SelectRequest {
            session_id: u16::MAX,
            system_bytes: SystemBytes::new(99),
        }),
        time(0),
    );
    let (write, _) = frame(actions.into_actions());
    core.on_write_outcome(write, WriteOutcome::Committed, time(0));
    core
}

/// Takes the unique outbound frame from a protocol transition.
fn frame(actions: Vec<CoreAction>) -> (WriteId, ProtocolMessage) {
    let mut frames = actions.into_iter().filter_map(|action| match action {
        CoreAction::SendFrame { write_id, message } => Some((write_id, message)),
        _ => None,
    });
    let result = frames.next().expect("one frame");
    assert!(frames.next().is_none());
    result
}

/// Starts a W=1 request at `now` and returns its frame and identity.
fn request(core: &mut SessionCore, command: u64, now: u64) -> (WriteId, DataMessage) {
    let actions = core.on_command(
        CoreCommand::new(
            CommandId::new(command),
            CoreCommandKind::Request(OutboundPrimary::new(
                Stream::new(1).unwrap(),
                Function::new(1),
                None,
            )),
        ),
        time(now),
    );
    let (write, message) = frame(actions.into_actions());
    let ProtocolMessage::Data(message) = message else {
        panic!("Data expected")
    };
    (write, message)
}

/// Returns whether the action batch closes with the expected communications timer.
fn closes_with(actions: &[CoreAction], timer: CommunicationsTimeoutKind) -> bool {
    actions.iter().any(|action| {
        matches!(action, CoreAction::CloseGeneration { reason, .. }
        if *reason == GenerationCloseReason::CommunicationsTimeout(timer))
    })
}

/// T3 starts at actual commit and expires at equality, retiring only its request.
#[test]
fn t3_boundary_releases_transaction_and_retains_tombstone() {
    let mut core = selected(None);
    let (write, data) = request(&mut core, 1, 1);
    assert_eq!(core.next_deadline(), None);
    core.on_write_outcome_at(write, WriteOutcome::Committed, time(2), time(10));
    assert_eq!(core.next_deadline(), Some(time(42)));
    assert!(core.advance_time(time(41)).into_actions().is_empty());
    assert_eq!(
        core.advance_time(time(42)).into_actions(),
        vec![CoreAction::CompleteCommand {
            command_id: CommandId::new(1),
            result: CoreCommandResult::RequestTimedOut(data.header()),
        }]
    );
    assert_eq!(core.state(), Some(SessionState::Selected));
    assert_eq!(core.pending_data_transaction_count(), 0);
    assert_eq!(core.tombstone_count(), 1);
    assert!(core.advance_time(time(100)).into_actions().is_empty());
}

/// Autonomous idle-probe Reject is distinct from an application-command rejection.
#[test]
fn rejected_autonomous_probe_retains_its_diagnostic_classification() {
    let mut core = selected(Some(Duration::from_millis(10)));
    let (write, message) = frame(core.advance_time(time(10)).into_actions());
    let ProtocolMessage::Control(ControlMessage::LinktestRequest { system_bytes }) = message else {
        panic!("idle probe")
    };
    core.on_write_outcome(write, WriteOutcome::Committed, time(10));
    core.on_message(
        ProtocolMessage::Control(ControlMessage::RejectRequest {
            session_id: u16::MAX,
            header_byte_2: 5,
            reason: crate::hsms::RejectReason::TRANSACTION_NOT_OPEN,
            system_bytes,
        }),
        time(11),
    );
    assert!(
        matches!(core.take_notice(), Some(crate::hsms::ProtocolNotice::PeerReject(notice)) if notice.disposition() == crate::hsms::PeerRejectDisposition::AutonomousRejected)
    );
    assert_eq!(core.state(), Some(SessionState::Selected));
    assert_eq!(core.open_command_count(), 0);
}

/// A callback processed after its commit-derived deadline expires immediately.
#[test]
fn delayed_commit_does_not_extend_t3() {
    let mut core = selected(None);
    let (write, data) = request(&mut core, 1, 1);
    let actions = core
        .on_write_outcome_at(write, WriteOutcome::Committed, time(2), time(100))
        .into_actions();
    assert!(matches!(
        actions.as_slice(),
        [CoreAction::CompleteCommand {
            result: CoreCommandResult::RequestTimedOut(header),
            ..
        }] if *header == data.header()
    ));
    assert_eq!(core.next_deadline(), None);
}

/// Peer proof can precede the callback and never resurrects T3 later.
#[test]
fn fast_secondary_then_late_commit_has_no_timer() {
    let mut core = selected(None);
    let (write, request) = request(&mut core, 1, 1);
    let header = request.header();
    let reply = DataMessage::new(
        crate::hsms::protocol::header::DataHeader::new(
            header.session_id(),
            header.stream(),
            Function::new(2),
            false,
            header.system_bytes(),
        ),
        None,
    );
    assert_eq!(
        core.on_message(ProtocolMessage::Data(reply), time(3))
            .into_actions()
            .len(),
        1
    );
    assert!(core
        .on_write_outcome_at(write, WriteOutcome::Committed, time(2), time(100))
        .into_actions()
        .is_empty());
    assert_eq!(core.next_deadline(), None);
    assert_eq!(core.open_command_count(), 0);
}

/// Equal T3 deadlines are deterministic even though transactions use a HashMap.
#[test]
fn t3_expirations_follow_command_order() {
    let mut core = selected(None);
    let (first, _) = request(&mut core, 20, 1);
    let (second, _) = request(&mut core, 10, 1);
    core.on_write_outcome(first, WriteOutcome::Committed, time(2));
    core.on_write_outcome(second, WriteOutcome::Committed, time(2));
    let ids: Vec<_> = core
        .advance_time(time(42))
        .into_actions()
        .into_iter()
        .filter_map(|action| match action {
            CoreAction::CompleteCommand { command_id, .. } => Some(command_id),
            _ => None,
        })
        .collect();
    assert_eq!(ids, vec![CommandId::new(10), CommandId::new(20)]);
}

/// Other control traffic cannot reset a continuous NotSelected tenure.
#[test]
fn t7_is_not_refreshed_by_linktest_requests() {
    let mut core = connected(None);
    core.on_message(
        ProtocolMessage::Control(ControlMessage::LinktestRequest {
            system_bytes: SystemBytes::new(1),
        }),
        time(900),
    );
    assert_eq!(core.next_deadline(), Some(time(1000)));
    assert!(core.advance_time(time(999)).into_actions().is_empty());
    assert!(closes_with(
        &core.advance_time(time(1000)).into_actions(),
        CommunicationsTimeoutKind::T7
    ));
    assert_eq!(core.next_deadline(), None);
}

/// Select's committed request starts T6; expiry completes once and closes.
#[test]
fn select_t6_uses_actual_commit_time() {
    let mut core = connected(None);
    let (write, _) = frame(
        core.on_command(
            CoreCommand::new(CommandId::new(1), CoreCommandKind::Select),
            time(1),
        )
        .into_actions(),
    );
    let actions = core
        .on_write_outcome_at(write, WriteOutcome::Committed, time(2), time(7))
        .into_actions();
    assert!(closes_with(&actions, CommunicationsTimeoutKind::T6));
    assert!(matches!(
        &actions[0],
        CoreAction::CompleteCommand {
            result: CoreCommandResult::Control(Err(OperationError::Timeout(TimeoutKind::T6))),
            ..
        }
    ));
    assert!(core.advance_time(time(100)).into_actions().is_empty());
}

/// Autonomous probes need no application command or completion endpoint.
#[test]
fn idle_probe_uses_control_slot_and_times_out() {
    let mut core = selected(Some(Duration::from_millis(20)));
    let (write, message) = frame(core.advance_time(time(20)).into_actions());
    assert!(matches!(
        message,
        ProtocolMessage::Control(ControlMessage::LinktestRequest { .. })
    ));
    assert_eq!(core.open_command_count(), 0);
    assert_eq!(core.next_deadline(), None);
    core.on_write_outcome(write, WriteOutcome::Committed, time(21));
    assert_eq!(core.next_deadline(), Some(time(26)));
    let actions = core.advance_time(time(26)).into_actions();
    assert!(closes_with(&actions, CommunicationsTimeoutKind::T6));
    assert!(!actions
        .iter()
        .any(|action| matches!(action, CoreAction::CompleteCommand { .. })));
}

/// A busy control slot suppresses idle wakes, then real activity re-arms probing.
#[test]
fn busy_control_does_not_spin_and_delayed_commit_cannot_move_idle_back() {
    let mut core = selected(Some(Duration::from_millis(20)));
    let (write, message) = frame(
        core.on_command(
            CoreCommand::new(CommandId::new(1), CoreCommandKind::Linktest),
            time(1),
        )
        .into_actions(),
    );
    let ProtocolMessage::Control(ControlMessage::LinktestRequest { system_bytes }) = message else {
        panic!("Linktest")
    };
    assert_eq!(core.next_deadline(), None);
    assert!(core.advance_time(time(100)).into_actions().is_empty());
    core.on_message(
        ProtocolMessage::Control(ControlMessage::LinktestResponse { system_bytes }),
        time(100),
    );
    assert!(core
        .on_write_outcome_at(write, WriteOutcome::Committed, time(2), time(101))
        .into_actions()
        .is_empty());
    assert_eq!(core.next_deadline(), Some(time(120)));
}

/// Future occurrence timestamps are rejected instead of extending transactions.
#[test]
fn future_writer_timestamp_fails_closed() {
    let mut core = selected(None);
    let (write, _) = request(&mut core, 1, 1);
    let actions = core
        .on_write_outcome_at(write, WriteOutcome::Committed, time(4), time(3))
        .into_actions();
    assert!(actions.iter().any(|action| matches!(
        action,
        CoreAction::CloseGeneration {
            reason: GenerationCloseReason::RuntimeInvariant,
            ..
        }
    )));
}

/// Unrepresentable deadlines cannot wrap into a short or absent timer.
#[test]
fn selection_deadline_overflow_fails_closed() {
    let mut core = SessionCore::new(SessionCoreConfig::new(SessionId::new(7).unwrap()));
    let actions = core
        .on_connected(MonoTime::from_elapsed(Duration::MAX))
        .into_actions();
    assert!(actions.iter().any(|action| matches!(
        action,
        CoreAction::CloseGeneration {
            reason: GenerationCloseReason::RuntimeInvariant,
            ..
        }
    )));
    assert_eq!(core.next_deadline(), None);
}
