//! Ordered effects emitted by the deterministic Session Core.
//!
//! Core commits its internal state before returning one owned `CoreActions`
//! batch. The Driver then consumes that batch in order to admit frames, publish
//! state, complete commands, and close the current connection generation.

use crate::hsms::{
    lifecycle::SessionState,
    model::{
        ids::{CommandId, ReplyCapabilityId, WriteId},
        runtime::{CloseBarrier, GenerationCloseReason},
    },
    protocol::message::{DataMessage, ProtocolMessage},
};

use super::command::CoreCommandResult;

/// One ordered action emitted by Session Core for Driver application.
#[derive(Debug, PartialEq)]
pub(crate) enum CoreAction {
    /// Delivers a classified Primary and optional single-use reply authority.
    DeliverPrimary {
        /// Validated header and owned decoded application content.
        message: DataMessage,
        /// Registered capability for W=1; absent for W=0.
        capability: Option<ReplyCapabilityId>,
    },
    /// Synchronously offers one complete semantic frame to Writer ingress.
    SendFrame {
        /// Core-assigned identity used to correlate the later write outcome.
        write_id: WriteId,
        /// Complete semantic message whose variant selects the Writer lane.
        message: ProtocolMessage,
    },
    /// Publishes a committed HSMS session-state transition.
    SessionStateChanged(SessionState),
    /// Delivers the unique terminal result for one accepted command.
    CompleteCommand {
        /// Driver-assigned identity whose completion sender must be consumed.
        command_id: CommandId,
        /// Typed terminal result produced by Core.
        result: CoreCommandResult,
    },
    /// Requests closure of the current connection generation.
    CloseGeneration {
        /// Stable first-reason-wins cause for generation shutdown.
        reason: GenerationCloseReason,
        /// Write boundary that must be satisfied before transport closure.
        barrier: CloseBarrier,
    },
}

/// One owned, ordered, single-application batch of Core actions.
///
/// The batch intentionally does not implement `Clone`: Driver must consume it
/// rather than treating Core output as a replayable effect log.
#[derive(Debug, PartialEq)]
pub(crate) struct CoreActions(
    /// Actions stored in the exact order in which Driver must apply them.
    Vec<CoreAction>,
);

impl CoreActions {
    /// Creates an empty ordered action batch.
    pub(crate) const fn new() -> Self {
        Self(Vec::new())
    }

    /// Appends `action` to the end of this batch's application order.
    pub(crate) fn push(&mut self, action: CoreAction) {
        self.0.push(action);
    }

    /// Consumes this batch and returns its ordered action storage.
    #[cfg(test)]
    pub(crate) fn into_actions(self) -> Vec<CoreAction> {
        self.0
    }
}

impl IntoIterator for CoreActions {
    type Item = CoreAction;
    type IntoIter = std::vec::IntoIter<CoreAction>;

    /// Consumes the batch and yields actions in their original application order.
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        error::OperationError,
        lifecycle::SessionState,
        model::{
            ids::{CommandId, SystemBytes, WriteId},
            runtime::{CloseBarrier, GenerationCloseReason},
        },
        protocol::{header::ControlMessage, message::ProtocolMessage},
    };

    use super::{CoreAction, CoreActions, CoreCommandResult};

    /// Builds one deterministic Control message for action value tests.
    fn linktest_request(system_bytes: u32) -> ProtocolMessage {
        ProtocolMessage::Control(ControlMessage::LinktestRequest {
            system_bytes: SystemBytes::new(system_bytes),
        })
    }

    /// Confirms an empty batch consumes to an empty action vector.
    #[test]
    fn new_batch_is_empty_when_consumed() {
        assert!(CoreActions::new().into_actions().is_empty());
    }

    /// Confirms every action variant retains its typed correlation, payload,
    /// completion, state, close reason, and barrier values.
    #[test]
    fn action_variants_preserve_values() {
        let write_id = WriteId::new(7);
        let message = linktest_request(11);
        let send = CoreAction::SendFrame {
            write_id,
            message: message.clone(),
        };
        let complete = CoreAction::CompleteCommand {
            command_id: CommandId::new(9),
            result: CoreCommandResult::Control(Err(OperationError::Backpressure)),
        };
        let close = CoreAction::CloseGeneration {
            reason: GenerationCloseReason::LocalSeparate,
            barrier: CloseBarrier::AfterWrite(write_id),
        };

        assert_eq!(send, CoreAction::SendFrame { write_id, message });
        assert_eq!(
            CoreAction::SessionStateChanged(SessionState::Selected),
            CoreAction::SessionStateChanged(SessionState::Selected)
        );
        assert_eq!(
            complete,
            CoreAction::CompleteCommand {
                command_id: CommandId::new(9),
                result: CoreCommandResult::Control(Err(OperationError::Backpressure)),
            }
        );
        assert_eq!(
            close,
            CoreAction::CloseGeneration {
                reason: GenerationCloseReason::LocalSeparate,
                barrier: CloseBarrier::AfterWrite(write_id),
            }
        );
    }

    /// Confirms vector extraction preserves insertion order across action kinds.
    #[test]
    fn into_actions_preserves_application_order() {
        let command_id = CommandId::new(13);
        let mut actions = CoreActions::new();
        actions.push(CoreAction::SessionStateChanged(SessionState::Selected));
        actions.push(CoreAction::CompleteCommand {
            command_id,
            result: CoreCommandResult::Control(Ok(())),
        });

        assert_eq!(
            actions.into_actions(),
            vec![
                CoreAction::SessionStateChanged(SessionState::Selected),
                CoreAction::CompleteCommand {
                    command_id,
                    result: CoreCommandResult::Control(Ok(())),
                },
            ]
        );
    }

    /// Confirms consuming iteration yields each action exactly once and in the
    /// same order in which Core appended it.
    #[test]
    fn consuming_iteration_preserves_order() {
        let mut actions = CoreActions::new();
        actions.push(CoreAction::SessionStateChanged(SessionState::NotSelected));
        actions.push(CoreAction::SessionStateChanged(SessionState::Selected));

        let observed: Vec<_> = actions.into_iter().collect();

        assert_eq!(
            observed,
            vec![
                CoreAction::SessionStateChanged(SessionState::NotSelected),
                CoreAction::SessionStateChanged(SessionState::Selected),
            ]
        );
    }
}
