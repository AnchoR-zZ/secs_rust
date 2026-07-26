//! Runtime-neutral outcomes returned to the pure Core by generation services.
//!
//! These values carry timer, transport, write, and close results without
//! embedding Tokio errors, sockets, tasks, or channels in protocol contracts.

use crate::hsms::{model::ids::TimerId, TimeoutKind};

/// Unique token for one armed Core timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TimerToken {
    /// Identity that distinguishes re-armed timers of the same kind.
    id: TimerId,
    /// Protocol or runtime deadline represented by this timer.
    kind: TimeoutKind,
}

impl TimerToken {
    /// Combines timer identity `id` with semantic timeout `kind`.
    pub(crate) const fn new(id: TimerId, kind: TimeoutKind) -> Self {
        Self { id, kind }
    }

    /// Returns the unique registration identity of this timer.
    pub(crate) const fn id(self) -> TimerId {
        self.id
    }

    /// Returns the semantic timeout represented by this token.
    pub(crate) const fn kind(self) -> TimeoutKind {
        self.kind
    }
}

/// Transport failure category stable across concrete I/O implementations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportFaultKind {
    /// The peer or network reset the connection.
    ConnectionReset,
    /// A write targeted a transport whose peer side was already closed.
    BrokenPipe,
    /// EOF arrived before the required bytes or protocol shutdown completed.
    UnexpectedEof,
    /// A runtime transport operation exceeded its deadline.
    TimedOut,
    /// Local generation cancellation interrupted the transport operation.
    Cancelled,
    /// A transport error did not fit a more specific stable category.
    Other,
}

/// Runtime-neutral transport failure with a static operation context.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransportFault {
    /// Stable category used by Core completion and close decisions.
    pub(crate) kind: TransportFaultKind,
    /// Static name of the transport operation that failed.
    pub(crate) context: &'static str,
}

/// Visibility classification produced when one scheduled frame write ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WriteResult {
    /// The complete frame reached the local ordered writer commit point.
    Committed,
    /// No frame byte became visible on the transport.
    NotWritten(TransportFault),
    /// Some bytes may be visible, so peer delivery cannot be determined.
    Indeterminate(TransportFault),
}

/// Communications timeout that makes an open generation unusable.
///
/// This close classification deliberately excludes application-operation
/// timers such as T3 and endpoint-supervision timers such as Connect or T5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommunicationsTimeoutKind {
    /// T6 expired while waiting for a response to a committed control request.
    T6,
    /// T7 expired during one contiguous `NotSelected` tenure.
    T7,
    /// T8 expired between bytes of an incomplete inbound HSMS message.
    T8,
}

/// Why the Core asks SessionDriver to terminate the current generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GenerationCloseReason {
    /// The logical endpoint is stopping.
    LocalStop,
    /// The application explicitly requested connection replacement.
    LocalDisconnect,
    /// The application sent `Separate.req` and then ended the generation.
    LocalSeparate,
    /// The peer sent `Separate.req`.
    SeparateReceived,
    /// The underlying transport can no longer carry protocol traffic.
    TransportLost,
    /// Continuing the HSMS session would violate protocol invariants.
    ProtocolViolation,
    /// Reliable application delivery could not accept an inbound event.
    ApplicationBackpressure,
    /// A communications timeout made the current generation unusable.
    CommunicationsTimeout(CommunicationsTimeoutKind),
}

#[cfg(test)]
mod tests {
    use super::{CommunicationsTimeoutKind, GenerationCloseReason};

    /// Converts every generation-closing communications timeout to its stable
    /// diagnostic timer name, making the internal classification exhaustive.
    fn timeout_name(kind: CommunicationsTimeoutKind) -> &'static str {
        match kind {
            CommunicationsTimeoutKind::T6 => "T6",
            CommunicationsTimeoutKind::T7 => "T7",
            CommunicationsTimeoutKind::T8 => "T8",
        }
    }

    /// Confirms the generation close reason retains each allowed
    /// communications-timeout classification without admitting other timers.
    #[test]
    fn communications_timeout_close_reason_is_exhaustive() {
        for (kind, expected_name) in [
            (CommunicationsTimeoutKind::T6, "T6"),
            (CommunicationsTimeoutKind::T7, "T7"),
            (CommunicationsTimeoutKind::T8, "T8"),
        ] {
            let GenerationCloseReason::CommunicationsTimeout(actual) =
                GenerationCloseReason::CommunicationsTimeout(kind)
            else {
                panic!("constructed communications timeout changed variant");
            };

            assert_eq!(timeout_name(actual), expected_name);
        }
    }
}
