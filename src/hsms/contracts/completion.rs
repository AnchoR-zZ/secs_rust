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
#[must_use = "a command completion must be delivered or explicitly discarded"]
pub(crate) struct CoreCommandCompletion {
    /// Accepted command that owns this result.
    command_id: CommandId,
    /// Successful completion payload or stable operation failure.
    result: Result<CoreCompletionValue, OperationError>,
}

/// Move-only authority to construct exactly one command completion envelope.
///
/// Admission creates this value together with a [`CommandId`], moves it through
/// [`super::CoreCommand`] into `OperationLedger`, and never recreates it from
/// the copyable identifier. Every terminal constructor consumes `self`, making
/// duplicate completion construction impossible without duplicating authority.
#[must_use = "accepted command completion authority must be moved to its sole owner"]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CommandCompletionAuthority {
    /// Accepted command correlated by the completion this authority creates.
    command_id: CommandId,
}

impl CommandCompletionAuthority {
    /// Creates the unique completion authority for one newly accepted command.
    ///
    /// Visibility is restricted to the `contracts` module, where the future
    /// command-admission issuer will live. Production Core modules can move an
    /// accepted authority but cannot reconstruct one from a copyable ID.
    pub(super) const fn new(command_id: CommandId) -> Self {
        Self { command_id }
    }

    /// Creates an isolated authority for crate unit tests only.
    ///
    /// This factory is absent from production builds and therefore cannot
    /// become a raw-ID completion-authority issuer in runtime code.
    #[cfg(test)]
    pub(crate) const fn for_test(command_id: CommandId) -> Self {
        Self::new(command_id)
    }

    /// Returns the copyable identity used only for uniqueness indexes.
    pub(crate) const fn command_id(&self) -> CommandId {
        self.command_id
    }

    /// Consumes this authority to complete a W=0 send after local commitment.
    pub(crate) const fn sent(self, receipt: SendReceipt) -> CoreCommandCompletion {
        self.succeeded(CoreCompletionValue::Sent(receipt))
    }

    /// Consumes this authority to complete a normal Secondary reply.
    pub(crate) const fn replied(self, receipt: SendReceipt) -> CoreCommandCompletion {
        self.succeeded(CoreCompletionValue::Replied(receipt))
    }

    /// Consumes this authority to complete a header-only SxF0 reply.
    pub(crate) const fn reply_aborted(self, receipt: SendReceipt) -> CoreCommandCompletion {
        self.succeeded(CoreCompletionValue::ReplyAborted(receipt))
    }

    /// Consumes this authority after locally abandoning a reply capability.
    pub(crate) const fn reply_abandoned(self) -> CoreCommandCompletion {
        self.succeeded(CoreCompletionValue::ReplyAbandoned)
    }

    /// Consumes this authority with the matching request Secondary.
    pub(crate) const fn secondary(self, secondary: SecondaryMessage) -> CoreCommandCompletion {
        self.succeeded(CoreCompletionValue::Secondary(secondary))
    }

    /// Consumes this authority when a typed control operation succeeds.
    pub(crate) const fn control_completed(self) -> CoreCommandCompletion {
        self.succeeded(CoreCompletionValue::ControlCompleted)
    }

    /// Consumes this authority with one stable terminal operation error.
    pub(crate) const fn failed(self, error: OperationError) -> CoreCommandCompletion {
        CoreCommandCompletion {
            command_id: self.command_id,
            result: Err(error),
        }
    }

    /// Consumes this authority with one successful typed completion value.
    const fn succeeded(self, value: CoreCompletionValue) -> CoreCommandCompletion {
        CoreCommandCompletion {
            command_id: self.command_id,
            result: Ok(value),
        }
    }
}

impl CoreCommandCompletion {
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
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        error::OperationError,
        model::ids::{CommandId, ConnectionGeneration, Function, Stream, WireSequence},
    };

    use super::{CommandCompletionAuthority, CoreCompletionValue, SendReceipt};
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
            CommandCompletionAuthority::new(command_id)
                .sent(receipt())
                .result(),
            Ok(CoreCompletionValue::Sent(value)) if *value == receipt()
        ));
        assert!(matches!(
            CommandCompletionAuthority::new(command_id)
                .replied(receipt())
                .result(),
            Ok(CoreCompletionValue::Replied(value)) if *value == receipt()
        ));
        assert!(matches!(
            CommandCompletionAuthority::new(command_id)
                .reply_aborted(receipt())
                .result(),
            Ok(CoreCompletionValue::ReplyAborted(value)) if *value == receipt()
        ));
        assert!(matches!(
            CommandCompletionAuthority::new(command_id)
                .reply_abandoned()
                .result(),
            Ok(CoreCompletionValue::ReplyAbandoned)
        ));
        assert!(matches!(
            CommandCompletionAuthority::new(command_id)
                .secondary(secondary())
                .result(),
            Ok(CoreCompletionValue::Secondary(value))
                if value.stream().get() == 3 && value.function().get() == 4
        ));
        let completed = CommandCompletionAuthority::new(command_id).control_completed();
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
        let completion = CommandCompletionAuthority::new(command_id)
            .failed(OperationError::ReplyCapabilityUnavailable);

        assert_eq!(completion.command_id(), command_id);
        assert_eq!(
            completion
                .into_result()
                .expect_err("failure completion must contain an error"),
            OperationError::ReplyCapabilityUnavailable
        );
    }
}
