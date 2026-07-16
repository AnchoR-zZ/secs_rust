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
    pub(crate) command_id: CommandId,
    /// Successful completion payload or stable operation failure.
    pub(crate) result: Result<CompletionValue, OperationError>,
}
