//! Side-effect requests emitted by the pure Core for SessionDriver execution.
//!
//! Each variant names work outside protocol state. SessionDriver performs the
//! work and returns its observable outcome as a [`super::event::CoreEvent`].

use std::time::Duration;

use crate::hsms::{
    api::CommandCompletion,
    model::{
        ids::{OperationId, WireSequence},
        runtime::{GenerationCloseReason, TimerToken},
    },
    protocol::message::ProtocolMessage,
    InboundPrimary, ProtocolNotice, SessionState,
};

/// Scheduler lane selected for an outbound frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteClass {
    /// Reserved lane for control and termination-critical traffic.
    Critical,
    /// Bounded lane gated by the Core's Selected state.
    Data,
}

/// Side effects requested by the pure Core and executed by SessionDriver.
#[derive(Debug)]
pub(crate) enum CoreEffect {
    /// Reserve an outbound lane position for a semantic frame.
    ScheduleWrite {
        /// Core operation that owns completion of the frame.
        operation_id: OperationId,
        /// Critical or Data scheduler lane required by the frame.
        class: WriteClass,
        /// Protocol frame to encode and enqueue without application mutation.
        frame: ProtocolMessage,
    },
    /// Authorize the single writer to cross a previously reported write fence.
    ProceedWrite {
        /// Wire position authorized to begin transport I/O.
        wire_sequence: WireSequence,
    },
    /// Cancel a fenced write before any byte becomes visible.
    AbortWrite {
        /// Wire position that must terminate as definitely not written.
        wire_sequence: WireSequence,
    },
    /// Register a runtime deadline for a unique Core timer token.
    ArmTimer {
        /// Identity and semantic kind returned if the timer expires.
        token: TimerToken,
        /// Relative delay to execute outside the Core.
        duration: Duration,
    },
    /// Cancel the exact timer registration represented by `token`.
    CancelTimer {
        /// Identity that prevents cancellation of a later re-armed timer.
        token: TimerToken,
    },
    /// Deliver one terminal result to the accepted command's completion guard.
    CompleteCommand(CommandCompletion),
    /// Reliably publish a classified inbound Primary to the application.
    PublishInbound(InboundPrimary),
    /// Reliably publish a non-data protocol diagnostic.
    PublishProtocolNotice(ProtocolNotice),
    /// Commit a new selection-state observation to endpoint lifecycle state.
    SessionStateChanged(SessionState),
    /// Terminate this generation for the supplied protocol-level reason.
    CloseGeneration(GenerationCloseReason),
}
