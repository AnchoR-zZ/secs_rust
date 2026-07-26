//! Side-effect requests emitted by the pure Core for SessionDriver execution.
//!
//! Each variant names work outside protocol state. SessionDriver performs the
//! work and returns its observable outcome as a [`super::event::CoreEvent`].
//! It must execute every effect vector strictly in order. In particular,
//! [`CoreEffect::SetDataGate`] is an infallible synchronous fence: the new gate
//! state must take effect before any later effect or the next Core input is
//! processed.

use std::time::Duration;

use crate::hsms::{
    api::CommandCompletion,
    model::{
        ids::{DeliveryId, WireSequence, WriteId},
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

/// Scheduler gate state applied synchronously to outbound Data scheduling.
///
/// Executing [`CoreEffect::SetDataGate`] is infallible and produces no
/// completion event. SessionDriver must apply the state before continuing its
/// ordered effect vector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataGateState {
    /// Allow newly scheduled Data frames to enter the bounded Data lane.
    Open,
    /// Reject newly scheduled Data frames while preserving critical traffic.
    Closed,
}

/// Side effects requested by the pure Core and executed by SessionDriver.
#[derive(Debug)]
pub(crate) enum CoreEffect {
    /// Reserve an outbound lane position for a semantic frame.
    ScheduleWrite {
        /// Core-assigned identity used to correlate all scheduler and writer events.
        write_id: WriteId,
        /// Critical or Data scheduler lane required by the frame.
        class: WriteClass,
        /// Protocol frame to encode and enqueue without application mutation.
        frame: ProtocolMessage,
    },
    /// Authorize the single writer to cross a previously reported write fence.
    ProceedWrite {
        /// Core-assigned identity of the write being authorized.
        write_id: WriteId,
        /// Wire position authorized to begin transport I/O.
        wire_sequence: WireSequence,
    },
    /// Cancel a fenced write before any byte becomes visible.
    AbortWrite {
        /// Core-assigned identity of the write being aborted.
        write_id: WriteId,
        /// Wire position that must terminate as definitely not written.
        wire_sequence: WireSequence,
    },
    /// Synchronously change whether Data frames may enter the scheduler.
    ///
    /// This effect is an infallible ordering fence. SessionDriver must finish
    /// applying `state` before executing any later effect in the same vector
    /// and before submitting another Core input; it returns no acknowledgement
    /// event to Core.
    SetDataGate {
        /// Gate state the scheduler must apply before continuing execution.
        state: DataGateState,
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
    PublishInbound {
        /// Core-assigned identity used to correlate publication completion.
        delivery_id: DeliveryId,
        /// Classified inbound Primary to publish exactly once.
        inbound: InboundPrimary,
    },
    /// Reliably publish a non-data protocol diagnostic.
    PublishProtocolNotice {
        /// Core-assigned identity used to correlate publication completion.
        delivery_id: DeliveryId,
        /// Non-data protocol diagnostic to publish exactly once.
        notice: ProtocolNotice,
    },
    /// Commit a new selection-state observation to endpoint lifecycle state.
    SessionStateChanged(SessionState),
    /// Begin the sole transport-close request for this generation.
    ///
    /// Core must emit this effect at most once per generation and must never
    /// create concurrent close requests. Runtime execution is idempotent: a
    /// replayed or duplicate effect must not start another close and must not
    /// produce another completion. The unique request terminates with exactly
    /// one [`super::event::CoreEvent::TransportCloseCompleted`] input.
    RequestTransportClose {
        /// Stable reason retained for the generation's unique close request.
        reason: GenerationCloseReason,
    },
}
