//! Complete runtime-neutral input vocabulary for one generation-scoped Core.
//!
//! SessionDriver serializes these events into `HsmsCore`; the Core never reads
//! sockets, clocks, tasks, or application channels directly.

use crate::hsms::{
    api::ProtocolCommand,
    model::{
        ids::{OperationId, WireSequence},
        runtime::{TimerToken, TransportFault, WriteResult},
    },
    profile::secs2::InboundProtocolFrame,
};

/// Every input that may advance one generation-scoped `HsmsCore`.
#[derive(Debug)]
pub(crate) enum CoreEvent {
    /// An application command passed all-or-nothing admission.
    Command(ProtocolCommand),
    /// Wire validation and presentation-profile decoding produced an inbound frame.
    FrameReceived(InboundProtocolFrame),
    /// The single writer reached the pre-write fence for one scheduled operation.
    BeginWrite {
        /// Core operation that owns the scheduled frame.
        operation_id: OperationId,
        /// Generation-local position assigned by `WireScheduler`.
        wire_sequence: WireSequence,
    },
    /// At least one frame byte may now be visible to the peer.
    WriteMayBeVisible {
        /// Wire position whose delivery classification became uncertain.
        wire_sequence: WireSequence,
    },
    /// The writer reached a terminal outcome for one wire position.
    WriteFinished {
        /// Wire position whose write ended.
        wire_sequence: WireSequence,
        /// Committed, definitely-not-written, or indeterminate result.
        result: WriteResult,
    },
    /// TimerDriver delivered the still-identifiable expired registration.
    TimerExpired(TimerToken),
    /// Reader, writer, or transport ownership reported terminal failure.
    TransportClosed(TransportFault),
    /// Reliable application publication failed without blocking the Core loop.
    ApplicationDeliveryFailed,
}
