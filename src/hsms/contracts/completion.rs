//! Defines exactly-once generation-command completion values emitted by Core.
//!
//! Public receipts expose deterministic local write commitment. Internal
//! completion envelopes correlate one result to the command accepted by
//! admission without leaking concrete oneshot or channel implementations.

#![allow(dead_code)]

use crate::hsms::{
    error::OperationError,
    model::ids::{CommandId, ConnectionGeneration, WireSequence},
};

use super::message::SecondaryMessage;

/// Proof that one complete frame reached the local writer commit point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SendReceipt {
    /// TCP incarnation whose writer committed the frame.
    generation: ConnectionGeneration,
    /// Generation-local ordered position of the committed frame.
    wire_sequence: u64,
}

impl SendReceipt {
    /// Creates a receipt for `wire_sequence` committed on `generation`; only
    /// the generation runtime may construct this proof.
    pub(crate) const fn new(generation: ConnectionGeneration, wire_sequence: WireSequence) -> Self {
        Self {
            generation,
            wire_sequence: wire_sequence.get(),
        }
    }

    #[must_use]
    /// Returns the TCP generation that committed the frame.
    pub const fn generation(self) -> ConnectionGeneration {
        self.generation
    }

    #[must_use]
    /// Returns the frame's generation-local wire sequence.
    pub const fn wire_sequence(self) -> u64 {
        self.wire_sequence
    }
}

/// Successful result payload for one generation-scoped Core command.
#[derive(Debug)]
pub(crate) enum CoreCompletionValue {
    /// A W=0 Primary frame committed locally.
    Sent(SendReceipt),
    /// A Secondary reply frame committed locally.
    Replied(SendReceipt),
    /// A header-only SxF0 transaction abort committed locally.
    ReplyAborted(SendReceipt),
    /// A reply capability was released locally without writing a frame.
    ReplyAbandoned,
    /// A request received and validated its matching Secondary.
    Secondary(SecondaryMessage),
    /// A typed control operation reached its defined successful outcome.
    ControlCompleted,
}

/// Result routed back to one generation command's exactly-once completion guard.
#[derive(Debug)]
pub(crate) struct CoreCommandCompletion {
    /// Accepted command that owns this result.
    command_id: CommandId,
    /// Successful completion payload or stable operation failure.
    result: Result<CoreCompletionValue, OperationError>,
}

impl CoreCommandCompletion {
    /// Completes W=0 send command `command_id` with local commit `receipt`.
    pub(crate) const fn sent(command_id: CommandId, receipt: SendReceipt) -> Self {
        Self::succeeded(command_id, CoreCompletionValue::Sent(receipt))
    }

    /// Completes reply command `command_id` with local commit `receipt`.
    pub(crate) const fn replied(command_id: CommandId, receipt: SendReceipt) -> Self {
        Self::succeeded(command_id, CoreCompletionValue::Replied(receipt))
    }

    /// Completes abort command `command_id` with local SxF0 commit `receipt`.
    pub(crate) const fn reply_aborted(command_id: CommandId, receipt: SendReceipt) -> Self {
        Self::succeeded(command_id, CoreCompletionValue::ReplyAborted(receipt))
    }

    /// Completes command `command_id` after locally abandoning its capability.
    pub(crate) const fn reply_abandoned(command_id: CommandId) -> Self {
        Self::succeeded(command_id, CoreCompletionValue::ReplyAbandoned)
    }

    /// Completes request command `command_id` with its matched `secondary`.
    pub(crate) const fn secondary(command_id: CommandId, secondary: SecondaryMessage) -> Self {
        Self::succeeded(command_id, CoreCompletionValue::Secondary(secondary))
    }

    /// Completes typed control command `command_id` after its success point.
    pub(crate) const fn control_completed(command_id: CommandId) -> Self {
        Self::succeeded(command_id, CoreCompletionValue::ControlCompleted)
    }

    /// Completes command `command_id` with stable operation `error`.
    pub(crate) const fn failed(command_id: CommandId, error: OperationError) -> Self {
        Self {
            command_id,
            result: Err(error),
        }
    }

    /// Returns the accepted application command that owns this completion.
    pub(crate) const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Borrows the successful completion value or operation failure.
    pub(crate) const fn result(&self) -> &Result<CoreCompletionValue, OperationError> {
        &self.result
    }

    /// Consumes the envelope and returns its successful value or failure.
    pub(crate) fn into_result(self) -> Result<CoreCompletionValue, OperationError> {
        self.result
    }

    /// Builds one successful completion for `command_id` and `value`.
    const fn succeeded(command_id: CommandId, value: CoreCompletionValue) -> Self {
        Self {
            command_id,
            result: Ok(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        error::OperationError,
        model::ids::{CommandId, ConnectionGeneration, Function, Stream, WireSequence},
    };

    use super::{CoreCommandCompletion, CoreCompletionValue, SendReceipt};
    use crate::hsms::contracts::message::SecondaryMessage;

    /// Creates a deterministic local commit receipt for completion tests.
    fn receipt() -> SendReceipt {
        SendReceipt::new(ConnectionGeneration::new(7), WireSequence::new(11))
    }

    /// Creates a deterministic validated Secondary for completion tests.
    fn secondary() -> SecondaryMessage {
        SecondaryMessage::new(
            Stream::new(3).expect("valid stream"),
            Function::new(4),
            None,
        )
    }

    /// Confirms every successful completion constructor preserves the command
    /// identity and selects the intended typed value.
    #[test]
    fn successful_completion_constructors_are_typed() {
        let command_id = CommandId::new(5);

        assert!(matches!(
            CoreCommandCompletion::sent(command_id, receipt()).result(),
            Ok(CoreCompletionValue::Sent(value)) if *value == receipt()
        ));
        assert!(matches!(
            CoreCommandCompletion::replied(command_id, receipt()).result(),
            Ok(CoreCompletionValue::Replied(value)) if *value == receipt()
        ));
        assert!(matches!(
            CoreCommandCompletion::reply_aborted(command_id, receipt()).result(),
            Ok(CoreCompletionValue::ReplyAborted(value)) if *value == receipt()
        ));
        assert!(matches!(
            CoreCommandCompletion::reply_abandoned(command_id).result(),
            Ok(CoreCompletionValue::ReplyAbandoned)
        ));
        assert!(matches!(
            CoreCommandCompletion::secondary(command_id, secondary()).result(),
            Ok(CoreCompletionValue::Secondary(value))
                if value.stream().get() == 3 && value.function().get() == 4
        ));
        let completed = CoreCommandCompletion::control_completed(command_id);
        assert_eq!(completed.command_id(), command_id);
        assert!(matches!(
            completed.into_result(),
            Ok(CoreCompletionValue::ControlCompleted)
        ));
    }

    /// Confirms the failure constructor retains the exact stable error.
    #[test]
    fn failure_completion_preserves_operation_error() {
        let command_id = CommandId::new(6);
        let completion =
            CoreCommandCompletion::failed(command_id, OperationError::ReplyCapabilityUnavailable);

        assert_eq!(completion.command_id(), command_id);
        assert_eq!(
            completion
                .into_result()
                .expect_err("failure completion must contain an error"),
            OperationError::ReplyCapabilityUnavailable
        );
    }
}
