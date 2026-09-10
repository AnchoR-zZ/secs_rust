//! Incoming Primary classification and single-use response contracts.
//! Peer reply authority is separate from locally allocated transaction IDs and
//! is revoked on generation close. Applications never choose response headers.

use super::*;
use crate::{
    hsms::{api::ReplyIntent, model::ids::Function},
    secs2::SecsItem,
};

impl SessionCore {
    /// Handles validated violation categories without touching Data transactions.
    /// Only unsupported SType/PType generates Reject; other complete invalid
    /// frames remain application diagnostics. Fatal framing never enters here.
    pub(crate) fn on_inbound_violation(
        &mut self,
        violation: crate::hsms::protocol::violation::InboundViolation,
        now: MonoTime,
    ) -> CoreActions {
        use crate::hsms::protocol::violation::{HeaderViolationKind, InboundViolation};
        let mut actions = CoreActions::new();
        if !self.accept_time(now, &mut actions) || self.state.is_none() || self.closing {
            return actions;
        }
        if let InboundViolation::Header(violation) = violation {
            let header = violation.header();
            let rejection = match violation.kind() {
                HeaderViolationKind::UnknownSessionType { .. } => {
                    Some((RejectReason::UNSUPPORTED_STYPE, header.s_type()))
                }
                HeaderViolationKind::UnknownPresentationType { .. } => {
                    Some((RejectReason::UNSUPPORTED_PTYPE, header.p_type()))
                }
                _ => None,
            };
            if let Some((reason, header_byte_2)) = rejection {
                self.send_response(
                    ControlMessage::RejectRequest {
                        session_id: header.session_id(),
                        header_byte_2,
                        reason,
                        system_bytes: SystemBytes::new(header.system_bytes()),
                    },
                    &mut actions,
                );
            }
        }
        actions
    }
    /// Routes a valid Data frame by session, response role and Primary parity.
    pub(super) fn receive_inbound_data(&mut self, message: DataMessage, actions: &mut CoreActions) {
        let header = message.header();
        if self.state != Some(SessionState::Selected)
            || header.session_id() != self.config.session_id()
        {
            self.send_response(
                ControlMessage::RejectRequest {
                    session_id: header.session_id().get(),
                    header_byte_2: 0,
                    reason: RejectReason::ENTITY_NOT_SELECTED,
                    system_bytes: header.system_bytes(),
                },
                actions,
            );
            return;
        }
        // Exact Secondary/F0 contracts can only match even functions. Odd
        // functions must remain eligible as peer Primaries even when System
        // Bytes overlap a local request or its retained tombstone.
        if header.function().get() & 1 == 0 {
            self.notice = match self.receive_data(message, actions) {
                DataInputClassification::LiveMismatch => {
                    Some(crate::hsms::ProtocolNotice::SecondaryMismatch)
                }
                DataInputClassification::RetainedTombstone => {
                    Some(crate::hsms::ProtocolNotice::StaleEventIgnored)
                }
                DataInputClassification::Unmatched => {
                    Some(crate::hsms::ProtocolNotice::UnmatchedSecondary)
                }
                _ => None,
            };
            return;
        }
        if self
            .reply_contracts
            .values()
            .any(|live| live.system_bytes() == header.system_bytes())
        {
            self.complete_all(OperationError::ConnectionLost, actions);
            self.request_close(
                GenerationCloseReason::ProtocolViolation,
                CloseBarrier::Immediate,
                actions,
            );
            return;
        }
        let capability = if header.reply_expected() {
            if self.reply_contracts.len() >= self.config.reply_capacity {
                self.complete_all(OperationError::ConnectionLost, actions);
                self.request_close(
                    GenerationCloseReason::ApplicationBackpressure,
                    CloseBarrier::Immediate,
                    actions,
                );
                return;
            }
            let Some(raw) = self.next_reply_id else {
                self.fail_runtime_invariant(actions);
                return;
            };
            self.next_reply_id = raw.checked_add(1);
            let id = ReplyCapabilityId::new(raw);
            self.reply_contracts.insert(id, header);
            Some(id)
        } else {
            None
        };
        actions.push(CoreAction::DeliverPrimary {
            message,
            capability,
        });
    }

    /// Returns the number of live inbound reply capabilities for drain decisions.
    pub(crate) fn reply_capability_count(&self) -> usize {
        self.reply_contracts.len()
    }

    /// Consumes one Core-admitted reply intent after Driver validates its owner.
    /// Normal/abort replies use Send completion and a reserved Writer Data permit;
    /// abandonment uses Control completion and performs no network operation.
    pub(crate) fn on_reply(
        &mut self,
        command_id: CommandId,
        capability: ReplyCapabilityId,
        intent: ReplyIntent,
        body: Option<SecsItem>,
        now: MonoTime,
    ) -> CoreActions {
        let mut actions = CoreActions::new();
        if self.open_commands.contains_key(&command_id) {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        }
        self.open_commands.insert(
            command_id,
            if intent == ReplyIntent::Abandon {
                OpenCommandKind::Control
            } else {
                OpenCommandKind::Send
            },
        );
        if !self.accept_time(now, &mut actions) {
            return actions;
        }
        if self.closing || self.state != Some(SessionState::Selected) {
            self.complete_error(
                command_id,
                OperationError::ReplyCapabilityUnavailable,
                &mut actions,
            );
            return actions;
        }
        if intent != ReplyIntent::Abandon && self.replies_blocked() {
            self.complete_error(command_id, OperationError::Draining, &mut actions);
            return actions;
        }
        let Some(primary) = self.reply_contracts.remove(&capability) else {
            self.complete_error(
                command_id,
                OperationError::ReplyCapabilityUnavailable,
                &mut actions,
            );
            return actions;
        };
        if intent == ReplyIntent::Abandon {
            self.complete_ok(command_id, &mut actions);
            self.progress_deselect(now, &mut actions);
            return actions;
        }
        let (function, body) = if intent == ReplyIntent::Abort {
            (0, None)
        } else {
            let Some(function) = primary.function().get().checked_add(1) else {
                self.complete_error(command_id, OperationError::ReplyRequiresAbort, &mut actions);
                return actions;
            };
            (function, body)
        };
        let Some(write_id) = self.allocate_write_id() else {
            self.fail_runtime_invariant(&mut actions);
            return actions;
        };
        self.pending_writes.insert(
            write_id,
            PendingWrite {
                kind: PendingWriteKind::DataSend {
                    correlation: DataWriteCorrelation {
                        session_id: primary.session_id(),
                        system_bytes: primary.system_bytes(),
                    },
                },
                command_id: Some(command_id),
            },
        );
        actions.push(CoreAction::SendFrame {
            write_id,
            message: ProtocolMessage::Data(DataMessage::new(
                DataHeader::new(
                    primary.session_id(),
                    primary.stream(),
                    Function::new(function),
                    false,
                    primary.system_bytes(),
                ),
                body,
            )),
        });
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::Stream;

    /// Creates a Selected Core with a configurable live peer-contract bound.
    fn selected(capacity: usize) -> SessionCore {
        let mut core = SessionCore::new(
            SessionCoreConfig::new(SessionId::new(7).unwrap()).with_reply_capacity(capacity),
        );
        core.on_connected(MonoTime::ZERO);
        core.on_message(
            ProtocolMessage::Control(ControlMessage::SelectRequest {
                session_id: u16::MAX,
                system_bytes: SystemBytes::new(99),
            }),
            MonoTime::ZERO,
        );
        core.on_write_outcome(WriteId::new(0), WriteOutcome::Committed, MonoTime::ZERO);
        core
    }

    /// Creates a semantic S3 Primary/Secondary with explicit peer correlation.
    fn data(function: u8, w: bool, system: u32) -> ProtocolMessage {
        ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                SessionId::new(7).unwrap(),
                Stream::new(3).unwrap(),
                Function::new(function),
                w,
                SystemBytes::new(system),
            ),
            None,
        ))
    }

    /// Delivers a W=1 Primary and extracts the registered response capability.
    fn receive(core: &mut SessionCore, function: u8, system: u32) -> ReplyCapabilityId {
        let actions = core
            .on_message(data(function, true, system), MonoTime::ZERO)
            .into_actions();
        let [CoreAction::DeliverPrimary {
            capability: Some(id),
            ..
        }] = actions.as_slice()
        else {
            panic!("one Primary delivery");
        };
        *id
    }

    /// Normal reply preserves peer header fields and completes only at commit.
    #[test]
    fn reply_uses_peer_contract_once_without_allocating_system_bytes() {
        let mut core = selected(2);
        let capability = receive(&mut core, 3, 42);
        let actions = core
            .on_reply(
                CommandId::new(1),
                capability,
                ReplyIntent::Secondary,
                Some(SecsItem::Binary(vec![9])),
                MonoTime::ZERO,
            )
            .into_actions();
        let [CoreAction::SendFrame {
            write_id,
            message: ProtocolMessage::Data(reply),
        }] = actions.as_slice()
        else {
            panic!("one reply frame");
        };
        assert_eq!(reply.header().session_id().get(), 7);
        assert_eq!(reply.header().stream().get(), 3);
        assert_eq!(reply.header().function().get(), 4);
        assert_eq!(reply.header().system_bytes().get(), 42);
        assert!(!reply.header().reply_expected());
        assert_eq!(reply.body(), Some(&SecsItem::Binary(vec![9])));
        assert_eq!(core.reply_capability_count(), 0);
        assert_eq!(core.next_system_bytes, Some(0));
        assert_eq!(
            core.on_write_outcome(*write_id, WriteOutcome::Committed, MonoTime::ZERO)
                .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(1),
                result: CoreCommandResult::Send(Ok(CommittedWrite::new(*write_id)))
            }]
        );
        assert_eq!(
            core.on_reply(
                CommandId::new(2),
                capability,
                ReplyIntent::Secondary,
                None,
                MonoTime::ZERO
            )
            .into_actions(),
            vec![CoreAction::CompleteCommand {
                command_id: CommandId::new(2),
                result: CoreCommandResult::Send(Err(OperationError::ReplyCapabilityUnavailable))
            }]
        );
    }

    /// F255 can abort or abandon, while normal reply cannot wrap to F0.
    #[test]
    fn f255_abort_and_abandon_are_explicit_and_normal_reply_cannot_wrap() {
        for intent in [
            ReplyIntent::Secondary,
            ReplyIntent::Abort,
            ReplyIntent::Abandon,
        ] {
            let mut core = selected(1);
            let capability = receive(&mut core, 255, 20);
            let actions = core
                .on_reply(
                    CommandId::new(1),
                    capability,
                    intent,
                    Some(SecsItem::Binary(vec![9])),
                    MonoTime::ZERO,
                )
                .into_actions();
            match intent {
                ReplyIntent::Abort => {
                    let [CoreAction::SendFrame {
                        message: ProtocolMessage::Data(reply),
                        ..
                    }] = actions.as_slice()
                    else {
                        panic!("one abort frame");
                    };
                    assert_eq!(reply.header().function().get(), 0);
                    assert_eq!(reply.header().system_bytes().get(), 20);
                    assert!(reply.body().is_none());
                }
                ReplyIntent::Abandon => assert_eq!(
                    actions,
                    vec![CoreAction::CompleteCommand {
                        command_id: CommandId::new(1),
                        result: CoreCommandResult::Control(Ok(()))
                    }]
                ),
                ReplyIntent::Secondary => assert_eq!(
                    actions,
                    vec![CoreAction::CompleteCommand {
                        command_id: CommandId::new(1),
                        result: CoreCommandResult::Send(Err(OperationError::ReplyRequiresAbort))
                    }]
                ),
            }
            assert_eq!(core.reply_capability_count(), 0);
        }
    }

    /// Opposite-direction transaction IDs may overlap without losing either role.
    #[test]
    fn peer_primary_with_local_request_system_bytes_is_delivered() {
        let mut core = selected(2);
        core.on_command(
            CoreCommand::new(
                CommandId::new(1),
                CoreCommandKind::Request(OutboundPrimary::new(
                    Stream::new(3).unwrap(),
                    Function::new(1),
                    None,
                )),
            ),
            MonoTime::ZERO,
        );
        let _capability = receive(&mut core, 1, 0);
        assert_eq!(core.pending_data_transaction_count(), 1);
        assert_eq!(core.reply_capability_count(), 1);
        let actions = core
            .on_message(data(2, false, 0), MonoTime::ZERO)
            .into_actions();
        assert!(matches!(
            actions.as_slice(),
            [CoreAction::CompleteCommand {
                result: CoreCommandResult::Request(Ok(_)),
                ..
            }]
        ));
        assert_eq!(core.reply_capability_count(), 1);
    }

    /// Reusing a live peer transaction closes and revokes every capability.
    #[test]
    fn duplicate_peer_primary_rejects_both_w_bit_forms() {
        for w in [false, true] {
            let mut core = selected(2);
            receive(&mut core, 1, 42);
            let actions = core
                .on_message(data(3, w, 42), MonoTime::ZERO)
                .into_actions();
            assert_eq!(
                actions,
                vec![CoreAction::CloseGeneration {
                    reason: GenerationCloseReason::ProtocolViolation,
                    barrier: CloseBarrier::Immediate
                }]
            );
            assert_eq!(core.reply_capability_count(), 0);
        }
    }

    /// W=0 delivery needs no capability; W=1 exhaustion closes without leakage.
    #[test]
    fn capability_pressure_does_not_charge_w0_events() {
        let mut core = selected(0);
        assert!(matches!(
            core.on_message(data(1, false, 1), MonoTime::ZERO)
                .into_actions()
                .as_slice(),
            [CoreAction::DeliverPrimary {
                capability: None,
                ..
            }]
        ));
        assert_eq!(
            core.on_message(data(1, true, 2), MonoTime::ZERO)
                .into_actions(),
            vec![CoreAction::CloseGeneration {
                reason: GenerationCloseReason::ApplicationBackpressure,
                barrier: CloseBarrier::Immediate
            }]
        );
        assert_eq!(core.reply_capability_count(), 0);
    }

    /// Even functions and F0 cannot become application Primaries, regardless of W.
    #[test]
    fn unmatched_even_frames_never_mint_reply_authority() {
        let mut core = selected(2);
        for function in [0, 2, 254] {
            for w in [false, true] {
                assert!(core
                    .on_message(data(function, w, 7), MonoTime::ZERO)
                    .into_actions()
                    .is_empty());
            }
        }
        assert_eq!(core.reply_capability_count(), 0);
    }
}
