//! Exactly-once command completion values produced by Core and endpoint runtime.
//!
//! Public receipts expose deterministic local write commitment. Internal
//! completion envelopes correlate one result to the command accepted by
//! admission without leaking concrete oneshot or channel implementations.

#![allow(dead_code)]

use crate::hsms::{
    model::ids::{CommandId, WireSequence},
    ConnectionGeneration, OperationError, SecondaryMessage,
};

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

/// Successful result payload for one accepted endpoint command.
#[derive(Debug)]
pub(crate) enum CompletionValue {
    /// Endpoint startup reached its defined completion point.
    Started,
    /// Endpoint shutdown completed with clean resources.
    Stopped,
    /// The requested connection generation was disconnected.
    Disconnected,
    /// A W=0 Primary frame committed locally.
    Sent(SendReceipt),
    /// A Secondary reply frame committed locally.
    Replied(SendReceipt),
    /// A request received and validated its matching Secondary.
    Secondary(SecondaryMessage),
    /// A typed control operation reached its defined successful outcome.
    ControlCompleted,
}

/// Result routed back to the exactly-once completion guard for one command.
#[derive(Debug)]
pub(crate) struct CommandCompletion {
    /// Accepted command that owns this result.
    command_id: CommandId,
    /// Successful completion payload or stable operation failure.
    result: Result<CompletionValue, OperationError>,
}

impl CommandCompletion {
    /// Completes startup command `command_id` after the endpoint reaches its
    /// defined running state.
    pub(crate) const fn started(command_id: CommandId) -> Self {
        Self::succeeded(command_id, CompletionValue::Started)
    }

    /// Completes stop command `command_id` after endpoint cleanup succeeds.
    pub(crate) const fn stopped(command_id: CommandId) -> Self {
        Self::succeeded(command_id, CompletionValue::Stopped)
    }

    /// Completes disconnect command `command_id` after its generation ends.
    pub(crate) const fn disconnected(command_id: CommandId) -> Self {
        Self::succeeded(command_id, CompletionValue::Disconnected)
    }

    /// Completes W=0 send command `command_id` with local commit `receipt`.
    pub(crate) const fn sent(command_id: CommandId, receipt: SendReceipt) -> Self {
        Self::succeeded(command_id, CompletionValue::Sent(receipt))
    }

    /// Completes reply command `command_id` with local commit `receipt`.
    pub(crate) const fn replied(command_id: CommandId, receipt: SendReceipt) -> Self {
        Self::succeeded(command_id, CompletionValue::Replied(receipt))
    }

    /// Completes request command `command_id` with its matched `secondary`.
    pub(crate) const fn secondary(command_id: CommandId, secondary: SecondaryMessage) -> Self {
        Self::succeeded(command_id, CompletionValue::Secondary(secondary))
    }

    /// Completes typed control command `command_id` after its success point.
    pub(crate) const fn control_completed(command_id: CommandId) -> Self {
        Self::succeeded(command_id, CompletionValue::ControlCompleted)
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
    pub(crate) const fn result(&self) -> &Result<CompletionValue, OperationError> {
        &self.result
    }

    /// Consumes the envelope and returns its successful value or failure.
    pub(crate) fn into_result(self) -> Result<CompletionValue, OperationError> {
        self.result
    }

    /// Builds one successful completion for `command_id` and `value`.
    const fn succeeded(command_id: CommandId, value: CompletionValue) -> Self {
        Self {
            command_id,
            result: Ok(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        model::ids::{CommandId, ConnectionGeneration, WireSequence},
        Function, OperationError, Stream,
    };

    use super::{CommandCompletion, CompletionValue, SecondaryMessage, SendReceipt};

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
            CommandCompletion::started(command_id).result(),
            Ok(CompletionValue::Started)
        ));
        assert!(matches!(
            CommandCompletion::stopped(command_id).result(),
            Ok(CompletionValue::Stopped)
        ));
        assert!(matches!(
            CommandCompletion::disconnected(command_id).result(),
            Ok(CompletionValue::Disconnected)
        ));
        assert!(matches!(
            CommandCompletion::sent(command_id, receipt()).result(),
            Ok(CompletionValue::Sent(value)) if *value == receipt()
        ));
        assert!(matches!(
            CommandCompletion::replied(command_id, receipt()).result(),
            Ok(CompletionValue::Replied(value)) if *value == receipt()
        ));
        assert!(matches!(
            CommandCompletion::secondary(command_id, secondary()).result(),
            Ok(CompletionValue::Secondary(value))
                if value.stream().get() == 3 && value.function().get() == 4
        ));
        let completed = CommandCompletion::control_completed(command_id);
        assert_eq!(completed.command_id(), command_id);
        assert!(matches!(
            completed.into_result(),
            Ok(CompletionValue::ControlCompleted)
        ));
    }

    /// Confirms the failure constructor retains the exact stable error.
    #[test]
    fn failure_completion_preserves_operation_error() {
        let command_id = CommandId::new(6);
        let completion =
            CommandCompletion::failed(command_id, OperationError::ReplyCapabilityUnavailable);

        assert_eq!(completion.command_id(), command_id);
        assert_eq!(
            completion
                .into_result()
                .expect_err("failure completion must contain an error"),
            OperationError::ReplyCapabilityUnavailable
        );
    }
}
