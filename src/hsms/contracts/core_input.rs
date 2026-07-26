//! Defines the complete runtime-neutral input vocabulary consumed by Core.
//!
//! SessionDriver serializes these events into `HsmsCore`; the Core never reads
//! sockets, clocks, tasks, or application channels directly.
//! Before submitting the next [`CoreInput`], SessionDriver must finish the
//! prior effect vector in order, including synchronously applying any
//! [`super::CoreEffect::SetDataGate`] fence. A publication completion
//! must also return to Core before SessionDriver polls any application command.

use crate::hsms::{
    model::{
        ids::{ConnectionGeneration, DeliveryId, WireSequence, WriteId},
        runtime::{TimerToken, TransportFault, WriteResult},
    },
    protocol::{message::ProtocolMessage, violation::InboundViolation},
};

use super::{command::CoreCommand, write::ScheduleFailure};

/// Generation-stamped input submitted to the single-threaded Core loop.
#[derive(Debug)]
pub(crate) struct CoreInput {
    /// TCP incarnation that produced this event.
    generation: ConnectionGeneration,
    /// Runtime-neutral event to apply if the generation is still current.
    event: CoreEvent,
}

impl CoreInput {
    /// Stamps `event` with the `generation` that produced it.
    pub(crate) const fn new(generation: ConnectionGeneration, event: CoreEvent) -> Self {
        Self { generation, event }
    }

    /// Returns the TCP incarnation that produced this input.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Borrows the runtime-neutral Core event.
    pub(crate) const fn event(&self) -> &CoreEvent {
        &self.event
    }

    /// Consumes the input and returns its generation stamp and event.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, CoreEvent) {
        (self.generation, self.event)
    }
}

/// Endpoint lifecycle requests that begin generation shutdown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownKind {
    /// The logical endpoint is stopping and will not retain this generation.
    EndpointStopping,
    /// The application requested replacement of the current connection.
    DisconnectRequested,
}

/// Runtime-neutral outcome of one reliable application delivery attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ApplicationDeliveryResult {
    /// The application event port accepted and transferred event ownership.
    Delivered,
    /// The bounded queue was full and runtime retained ownership of the event.
    Full,
    /// The receiver was closed and runtime retained ownership of the event.
    Closed,
}

/// Every input that may advance one generation-scoped `HsmsCore`.
#[derive(Debug)]
pub(crate) enum CoreEvent {
    /// An application command passed all-or-nothing admission.
    Command(CoreCommand),
    /// Wire validation and presentation-profile decoding produced a message.
    MessageReceived(ProtocolMessage),
    /// A recoverable header or Message Text failure requires a Core decision.
    InboundViolation(InboundViolation),
    /// The scheduler completed the sole attempt to reserve a wire position.
    ///
    /// Every ScheduleWrite effect produces exactly one such event. `Err` is a
    /// terminal result and no BeginWrite, visibility, or WriteFinished event
    /// follows. `Ok` establishes a generation-unique WriteId/WireSequence pair.
    WriteScheduled {
        /// Core-assigned identity of the requested outbound write.
        write_id: WriteId,
        /// Reserved wire position or stable scheduling failure.
        result: Result<WireSequence, ScheduleFailure>,
    },
    /// The single writer reached the pre-write fence for one scheduled write.
    ///
    /// This can follow only a successful exact scheduling pair. The writer is
    /// stopped and no byte may be written until Core resolves the first valid
    /// fence with exactly one ProceedWrite or AbortWrite effect. A duplicate
    /// must never recommit a deferred peer-response transition.
    BeginWrite {
        /// Core-assigned identity of the scheduled write.
        write_id: WriteId,
        /// Generation-local position assigned by `WireScheduler`.
        wire_sequence: WireSequence,
    },
    /// At least one frame byte may now be visible to the peer.
    ///
    /// This runtime observation occurs at most once after ProceedWrite and
    /// before Committed or Indeterminate. It never follows AbortWrite. Core has
    /// already advanced conservative Registry visibility before proceeding.
    WriteMayBeVisible {
        /// Core-assigned identity of the write whose visibility changed.
        write_id: WriteId,
        /// Wire position whose delivery classification became uncertain.
        wire_sequence: WireSequence,
    },
    /// The writer reached the sole terminal outcome for one wire position.
    ///
    /// Proceed resolves to exactly one Committed, NotWritten, or Indeterminate
    /// result. Abort resolves only to NotWritten with cancellation and produces
    /// no visibility event. Scheduling failure and WriteFinished are mutually
    /// exclusive; duplicates cannot repeat protocol mutation.
    WriteFinished {
        /// Core-assigned identity of the write that ended.
        write_id: WriteId,
        /// Wire position whose write ended.
        wire_sequence: WireSequence,
        /// Committed, definitely-not-written, or indeterminate result.
        result: WriteResult,
    },
    /// TimerDriver delivered the still-identifiable expired registration.
    TimerExpired(TimerToken),
    /// Endpoint lifecycle requested an orderly generation shutdown.
    ShutdownRequested(ShutdownKind),
    /// Reader, writer, or transport ownership reported a terminal failure.
    TransportFailed(TransportFault),
    /// The generation's unique transport-close request reached a terminal outcome.
    ///
    /// SessionDriver emits exactly one such completion for the sole
    /// [`super::CoreEffect::RequestTransportClose`] request in a
    /// generation. Replayed or duplicate effect execution is idempotent and
    /// must not produce another completion.
    TransportCloseCompleted {
        /// Successful close confirmation or runtime-neutral close failure.
        result: Result<(), TransportFault>,
    },
    /// One reliable application publication attempt reached its sole outcome.
    ///
    /// Delivered transfers ownership; Full and Closed do not. Stale or
    /// duplicate DeliveryId values cannot change a newer publication or reply
    /// capability. SessionDriver submits this event before polling any
    /// application command.
    ApplicationDeliveryFinished {
        /// Identity assigned by Core to the publication attempt.
        delivery_id: DeliveryId,
        /// Whether delivery succeeded, found a full queue, or found it closed.
        result: ApplicationDeliveryResult,
    },
}
