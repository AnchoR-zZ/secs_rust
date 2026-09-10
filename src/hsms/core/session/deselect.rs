//! Finite local drain and symmetric HSMS Deselect control procedures.
//! Successful deselection retains TCP and starts a fresh NotSelected tenure;
//! unsuccessful local replies never undo a peer-initiated state transition.

use super::*;
use crate::hsms::{error::TimeoutKind, protocol::header::DeselectStatus};

/// A local Deselect command waiting for existing Data obligations to finish.
#[derive(Clone, Copy, Debug)]
pub(super) struct DeselectDrain {
    /// Unique completion identity of the pending local Deselect operation.
    pub(super) command_id: CommandId,
    /// Absolute deadline that new peer activity cannot extend.
    pub(super) deadline: MonoTime,
}

impl SessionCore {
    /// Reports the post-request barrier that prevents Data following Deselect.req.
    pub(crate) fn replies_blocked(&self) -> bool {
        self.active_transaction
            .is_some_and(|transaction| transaction.kind == TransactionKind::Deselect)
    }

    /// Uses authoritative live state to decide whether Data obligations remain.
    fn data_in_use(&self) -> bool {
        !self.data_transactions.is_empty()
            || !self.reply_contracts.is_empty()
            || self.pending_writes.values().any(|write| {
                matches!(
                    write.kind,
                    PendingWriteKind::DataSend { .. } | PendingWriteKind::DataRequest { .. }
                )
            })
    }

    /// Begins a finite drain while leaving Selected and existing replies usable.
    pub(super) fn start_deselect(
        &mut self,
        command_id: CommandId,
        now: MonoTime,
        actions: &mut CoreActions,
    ) {
        if self.state != Some(SessionState::Selected) {
            self.complete_error(command_id, OperationError::NotSelected, actions);
        } else if self.active_transaction.is_some() || self.deselect_drain.is_some() {
            self.complete_error(command_id, OperationError::ControlBusy, actions);
        } else if let Some(deadline) = now.checked_add(self.config.drain_timeout) {
            self.deselect_drain = Some(DeselectDrain {
                command_id,
                deadline,
            });
            self.progress_deselect(now, actions);
        } else {
            self.fail_runtime_invariant(actions);
        }
    }

    /// Expires drain first at equality, otherwise starts the drained transaction.
    pub(super) fn progress_deselect(&mut self, now: MonoTime, actions: &mut CoreActions) {
        if self.closing {
            return;
        }
        let Some(drain) = self.deselect_drain else {
            return;
        };
        if drain.deadline <= now {
            self.deselect_drain = None;
            self.complete_error(
                drain.command_id,
                OperationError::Timeout(TimeoutKind::Drain),
                actions,
            );
        } else if self.state == Some(SessionState::NotSelected) {
            self.deselect_drain = None;
            self.complete_ok(drain.command_id, actions);
        } else if !self.data_in_use() {
            self.deselect_drain = None;
            self.start_transaction(TransactionKind::Deselect, Some(drain.command_id), actions);
        }
    }

    /// Commits one transition to NotSelected and starts its uninterrupted T7.
    fn commit_deselection(&mut self, now: MonoTime, actions: &mut CoreActions) {
        if self.state != Some(SessionState::NotSelected) {
            self.state = Some(SessionState::NotSelected);
            self.reply_contracts.clear();
            self.arm_selection_timeout(now, actions);
            actions.push(CoreAction::SessionStateChanged(SessionState::NotSelected));
        }
    }

    /// Responds to a peer Deselect using session identity and live Data occupancy.
    pub(super) fn receive_deselect_request(
        &mut self,
        session_id: u16,
        system_bytes: SystemBytes,
        now: MonoTime,
        actions: &mut CoreActions,
    ) {
        let status =
            if session_id != CONTROL_SESSION_ID || self.state != Some(SessionState::Selected) {
                DeselectStatus::NOT_SELECTED
            } else if self.data_in_use() {
                DeselectStatus::BUSY
            } else {
                DeselectStatus::SUCCESS
            };
        // Driver must assign the response its wire order before publishing state.
        self.send_response(
            ControlMessage::DeselectResponse {
                session_id,
                status,
                system_bytes,
            },
            actions,
        );
        if status.is_success() && !self.closing {
            self.commit_deselection(now, actions);
        }
    }

    /// Matches the complete local response tuple, preserving extension statuses.
    pub(super) fn receive_deselect_response(
        &mut self,
        session_id: u16,
        status: DeselectStatus,
        system_bytes: SystemBytes,
        now: MonoTime,
        actions: &mut CoreActions,
    ) {
        let transaction = self.active_transaction.filter(|transaction| {
            transaction.kind == TransactionKind::Deselect
                && transaction.session_id == session_id
                && transaction.system_bytes == system_bytes
        });
        let Some(transaction) = transaction else {
            self.send_transaction_not_open_reject(session_id, 4, system_bytes, actions);
            return;
        };
        self.active_transaction = None;
        if status.is_success() {
            self.commit_deselection(now, actions);
            self.complete_control_transaction(transaction, Ok(()), actions);
        } else {
            self.complete_control_transaction(
                transaction,
                Err(OperationError::DeselectRejected {
                    status: NonZeroU8::new(status.get()).expect("non-success status is nonzero"),
                }),
                actions,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::{api::ReplyIntent, Function, Stream};
    use std::time::Duration;

    /// Maps exact test seconds into the runtime-neutral generation clock.
    fn time(seconds: u64) -> MonoTime {
        MonoTime::from_elapsed(Duration::from_secs(seconds))
    }

    /// Creates a Selected Core after committing the passive Select response.
    fn selected() -> SessionCore {
        let mut core = SessionCore::new(SessionCoreConfig::new(SessionId::new(7).unwrap()));
        core.on_connected(time(0));
        core.on_message(
            ProtocolMessage::Control(ControlMessage::SelectRequest {
                session_id: u16::MAX,
                system_bytes: SystemBytes::new(99),
            }),
            time(0),
        );
        core.on_write_outcome(WriteId::new(0), WriteOutcome::Committed, time(0));
        core
    }

    /// Begins an idle local Deselect and extracts its exact request correlation.
    fn start(core: &mut SessionCore) -> (WriteId, SystemBytes) {
        let actions = core
            .on_command(
                CoreCommand::new(CommandId::new(10), CoreCommandKind::Deselect),
                time(0),
            )
            .into_actions();
        let [CoreAction::SendFrame {
            write_id,
            message: ProtocolMessage::Control(ControlMessage::DeselectRequest { system_bytes, .. }),
        }] = actions.as_slice()
        else {
            panic!("idle deselect must send one request");
        };
        (*write_id, *system_bytes)
    }

    /// Builds one matched Deselect response preserving arbitrary peer status.
    fn response(system_bytes: SystemBytes, status: u8) -> ProtocolMessage {
        ProtocolMessage::Control(ControlMessage::DeselectResponse {
            session_id: u16::MAX,
            system_bytes,
            status: DeselectStatus::new(status),
        })
    }

    /// Creates a peer Primary or Secondary for authoritative drain occupancy.
    fn data(function: u8, w: bool, system: u32) -> ProtocolMessage {
        ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                SessionId::new(7).unwrap(),
                Stream::new(1).unwrap(),
                Function::new(function),
                w,
                SystemBytes::new(system),
            ),
            None,
        ))
    }

    /// Deselect T6 starts at commit and successful response retains TCP under T7.
    #[test]
    fn successful_deselect_keeps_connection_and_arms_t7_from_transition() {
        let mut core = selected();
        let (write, system) = start(&mut core);
        assert!(core.replies_blocked());
        assert_eq!(core.state(), Some(SessionState::Selected));
        core.on_write_outcome(write, WriteOutcome::Committed, time(2));
        assert_eq!(core.next_deadline(), Some(time(7)));
        let actions = core.on_message(response(system, 0), time(3)).into_actions();
        assert_eq!(
            actions,
            vec![
                CoreAction::SessionStateChanged(SessionState::NotSelected),
                CoreAction::CompleteCommand {
                    command_id: CommandId::new(10),
                    result: CoreCommandResult::Control(Ok(()))
                }
            ]
        );
        assert!(!core.closing);
        assert_eq!(core.next_deadline(), Some(time(13)));
        assert!(!core.replies_blocked());
        assert!(matches!(
            core.advance_time(time(13)).into_actions().as_slice(),
            [CoreAction::CloseGeneration { .. }]
        ));
    }

    /// Nonzero base/extension statuses leave Selected and reopen Data admission.
    #[test]
    fn rejected_deselect_preserves_status_and_selected_state() {
        for status in [1, 2, 0x81] {
            let mut core = selected();
            let (_, system) = start(&mut core);
            let actions = core
                .on_message(response(system, status), time(1))
                .into_actions();
            assert_eq!(
                actions,
                vec![CoreAction::CompleteCommand {
                    command_id: CommandId::new(10),
                    result: CoreCommandResult::Control(Err(OperationError::DeselectRejected {
                        status: NonZeroU8::new(status).unwrap()
                    }))
                }]
            );
            assert_eq!(core.state(), Some(SessionState::Selected));
            assert!(!core.replies_blocked());
        }
    }

    /// A peer's simultaneous success cannot be undone by our later failed response.
    #[test]
    fn simultaneous_deselect_failure_does_not_restore_selected_or_reset_t7() {
        let mut core = selected();
        let (write, system) = start(&mut core);
        let actions = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::DeselectRequest {
                    session_id: u16::MAX,
                    system_bytes: SystemBytes::new(42),
                }),
                time(1),
            )
            .into_actions();
        assert!(
            matches!(actions.as_slice(), [CoreAction::SendFrame { message: ProtocolMessage::Control(ControlMessage::DeselectResponse { status, .. }), .. },
            CoreAction::SessionStateChanged(SessionState::NotSelected)] if status.is_success())
        );
        assert!(core.replies_blocked());
        core.on_message(response(system, 2), time(2));
        core.on_write_outcome(write, WriteOutcome::Committed, time(3));
        assert_eq!(core.state(), Some(SessionState::NotSelected));
        assert_eq!(core.next_deadline(), Some(time(11)));
        assert!(!core.closing);
    }

    /// Fast Secondary does not finish drain before the associated write outcome.
    #[test]
    fn drain_waits_for_both_transaction_and_data_write_then_sends_request() {
        let mut core = selected();
        let actions = core
            .on_command(
                CoreCommand::new(
                    CommandId::new(1),
                    CoreCommandKind::Request(OutboundPrimary::new(
                        Stream::new(1).unwrap(),
                        Function::new(1),
                        None,
                    )),
                ),
                time(0),
            )
            .into_actions();
        let CoreAction::SendFrame { write_id, .. } = actions[0] else {
            panic!("Data request write");
        };
        assert!(core
            .on_command(
                CoreCommand::new(CommandId::new(10), CoreCommandKind::Deselect),
                time(0)
            )
            .into_actions()
            .is_empty());
        let actions = core.on_message(data(2, false, 0), time(1)).into_actions();
        assert!(matches!(
            actions.as_slice(),
            [CoreAction::CompleteCommand {
                result: CoreCommandResult::Request(Ok(_)),
                ..
            }]
        ));
        assert!(core.deselect_drain.is_some());
        let actions = core
            .on_write_outcome(write_id, WriteOutcome::Committed, time(2))
            .into_actions();
        assert!(matches!(
            actions.as_slice(),
            [CoreAction::SendFrame {
                message: ProtocolMessage::Control(ControlMessage::DeselectRequest { .. }),
                ..
            }]
        ));
        assert!(core.deselect_drain.is_none());
        assert!(core.replies_blocked());
    }

    /// New peer obligations never extend the absolute drain deadline or revoke tokens.
    #[test]
    fn drain_timeout_restores_admission_without_abandoning_peer_capabilities() {
        let mut core = selected();
        core.on_message(data(1, true, 42), time(0));
        core.on_command(
            CoreCommand::new(CommandId::new(10), CoreCommandKind::Deselect),
            time(0),
        );
        core.on_message(data(3, true, 43), time(4));
        assert_eq!(core.next_deadline(), Some(time(5)));
        assert_eq!(
            core.advance_time(time(5)).into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(10),
                result: CoreCommandResult::Control(Err(OperationError::Timeout(
                    TimeoutKind::Drain
                )))
            }]
        );
        assert_eq!(core.reply_capability_count(), 2);
        assert_eq!(core.state(), Some(SessionState::Selected));
        let actions = core
            .on_command(
                CoreCommand::new(
                    CommandId::new(11),
                    CoreCommandKind::Send(OutboundPrimary::new(
                        Stream::new(1).unwrap(),
                        Function::new(1),
                        None,
                    )),
                ),
                time(5),
            )
            .into_actions();
        assert!(matches!(actions.as_slice(), [CoreAction::SendFrame { .. }]));
    }

    /// Explicit abandonment can finish pre-request drain without a Data write.
    #[test]
    fn abandon_finishes_drain_and_new_send_is_blocked_during_drain() {
        let mut core = selected();
        core.on_message(data(1, true, 42), time(0));
        core.on_command(
            CoreCommand::new(CommandId::new(10), CoreCommandKind::Deselect),
            time(0),
        );
        let actions = core
            .on_command(
                CoreCommand::new(
                    CommandId::new(11),
                    CoreCommandKind::Send(OutboundPrimary::new(
                        Stream::new(1).unwrap(),
                        Function::new(1),
                        None,
                    )),
                ),
                time(1),
            )
            .into_actions();
        assert_eq!(
            actions,
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(11),
                result: CoreCommandResult::Send(Err(OperationError::Draining))
            }]
        );
        let actions = core
            .on_reply(
                CommandId::new(12),
                ReplyCapabilityId::new(0),
                ReplyIntent::Abandon,
                None,
                time(2),
            )
            .into_actions();
        assert!(matches!(
            actions.as_slice(),
            [
                CoreAction::CompleteCommand {
                    result: CoreCommandResult::Control(Ok(())),
                    ..
                },
                CoreAction::SendFrame {
                    message: ProtocolMessage::Control(ControlMessage::DeselectRequest { .. }),
                    ..
                }
            ]
        ));
    }

    /// Busy peer requests are refused until Data obligations finish; no T7 starts.
    #[test]
    fn peer_deselect_busy_preserves_reply_authority() {
        let mut core = selected();
        core.on_message(data(1, true, 42), time(0));
        let actions = core
            .on_message(
                ProtocolMessage::Control(ControlMessage::DeselectRequest {
                    session_id: u16::MAX,
                    system_bytes: SystemBytes::new(44),
                }),
                time(1),
            )
            .into_actions();
        assert!(
            matches!(actions.as_slice(), [CoreAction::SendFrame { message: ProtocolMessage::Control(ControlMessage::DeselectResponse { status, .. }), .. }] if *status == DeselectStatus::BUSY)
        );
        assert_eq!(core.reply_capability_count(), 1);
        assert_eq!(core.state(), Some(SessionState::Selected));
        assert!(core.selection_deadline.is_none());
    }

    /// A mismatched response cannot finish Deselect; its actual T6 still closes.
    #[test]
    fn mismatched_response_keeps_deselect_transaction_until_t6() {
        let mut core = selected();
        let (write, system) = start(&mut core);
        core.on_write_outcome(write, WriteOutcome::Committed, time(0));
        let actions = core
            .on_message(response(SystemBytes::new(system.get() + 1), 0), time(1))
            .into_actions();
        assert!(matches!(
            actions.as_slice(),
            [CoreAction::SendFrame {
                message: ProtocolMessage::Control(ControlMessage::RejectRequest {
                    header_byte_2: 4,
                    ..
                }),
                ..
            }]
        ));
        assert!(core.replies_blocked());
        let actions = core.advance_time(time(5)).into_actions();
        assert!(matches!(
            actions.first(),
            Some(CoreAction::CompleteCommand {
                result: CoreCommandResult::Control(Err(OperationError::Timeout(TimeoutKind::T6))),
                ..
            })
        ));
        assert!(core.closing);
    }
}
