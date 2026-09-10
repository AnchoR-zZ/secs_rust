//! Runtime-neutral values shared by HSMS Core and generation orchestration.
//!
//! These values describe logical monotonic time, terminal Writer outcomes,
//! transport-fault categories, and generation-close intent without retaining
//! a Tokio clock, socket, I/O error, channel, or other runtime-owned object.

use std::time::Duration;

use super::ids::WriteId;

/// Generation-local monotonic logical time measured from an arbitrary epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct MonoTime(
    /// Elapsed duration since the current generation's arbitrary epoch.
    Duration,
);

impl MonoTime {
    /// Logical time at the generation-local epoch.
    pub(crate) const ZERO: Self = Self(Duration::ZERO);

    /// Creates logical time from `value` elapsed since the local epoch.
    pub(crate) const fn from_elapsed(value: Duration) -> Self {
        Self(value)
    }

    /// Returns the elapsed duration represented by this logical time.
    pub(crate) const fn elapsed(self) -> Duration {
        self.0
    }

    /// Adds `duration`, returning `None` instead of wrapping on overflow.
    pub(crate) fn checked_add(self, duration: Duration) -> Option<Self> {
        self.0.checked_add(duration).map(Self)
    }
}

/// Stable runtime-neutral category for one terminal transport failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportFaultKind {
    /// A nonempty write returned zero bytes and cannot make further progress.
    WriteZero,
    /// The peer or network reset the current connection.
    ConnectionReset,
    /// The transport could no longer write to the peer.
    BrokenPipe,
    /// The byte stream ended before the expected operation completed.
    UnexpectedEof,
    /// The runtime reported that an I/O operation timed out.
    TimedOut,
    /// Local cancellation interrupted the transport operation.
    Cancelled,
    /// A terminal transport failure did not fit a more stable category.
    Other,
}

/// Runtime-neutral detail retained for a failed asynchronous write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TransportFault {
    /// Stable runtime-neutral failure category.
    kind: TransportFaultKind,
}

impl TransportFault {
    /// Creates a transport fault with the stable failure `kind`.
    pub(crate) const fn new(kind: TransportFaultKind) -> Self {
        Self { kind }
    }

    /// Returns this fault's stable runtime-neutral category.
    #[cfg(test)]
    pub(crate) const fn kind(&self) -> TransportFaultKind {
        self.kind
    }
}

/// Unique terminal outcome for a frame already accepted by Writer ingress.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteOutcome {
    /// Writer proved that the complete frame was committed to its byte stream.
    Committed,
    /// Writer proved that no byte of the frame entered TCP.
    NotWritten(TransportFault),
    /// Some bytes may have become visible to the peer, or zero visibility
    /// cannot be proven.
    Indeterminate(TransportFault),
}

/// HSMS communications timeout whose expiry closes a generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommunicationsTimeoutKind {
    /// T6 control-transaction timeout.
    T6,
    /// T7 connection-not-selected timeout.
    T7,
    /// T8 inter-character receive timeout.
    T8,
}

/// Stable first-reason-wins cause for closing one connection generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GenerationCloseReason {
    /// The local application requested endpoint stop.
    LocalStop,
    /// The local application requested connection teardown.
    LocalDisconnect,
    /// A locally initiated Separate reached the generation close path.
    LocalSeparate,
    /// The peer sent Separate while the session was Selected.
    SeparateReceived,
    /// A terminal transport condition made the connection unusable.
    TransportLost,
    /// The peer violated an enforced protocol invariant.
    ProtocolViolation,
    /// The reserved synchronous Control admission lane was full.
    ControlBackpressure,
    /// An application-facing bounded resource exhausted its capacity.
    ApplicationBackpressure,
    /// All System Bytes were used; Supervisor should rotate this generation.
    SystemBytesExhausted,
    /// A non-recoverable local runtime or ownership invariant failed.
    RuntimeInvariant,
    /// An HSMS communications timer expired.
    CommunicationsTimeout(CommunicationsTimeoutKind),
}

/// Boundary that must be satisfied before Driver closes the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloseBarrier {
    /// Driver may close the transport after applying the current action batch.
    Immediate,
    /// Driver waits for the unique terminal outcome of this accepted write.
    AfterWrite(
        /// Core-assigned identity of the frame protected by the barrier.
        WriteId,
    ),
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        CloseBarrier, CommunicationsTimeoutKind, GenerationCloseReason, MonoTime, TransportFault,
        TransportFaultKind, WriteOutcome,
    };
    use crate::hsms::model::ids::WriteId;

    /// Confirms elapsed-duration construction, extraction, equality, and
    /// ordering preserve the logical-time value exactly.
    #[test]
    fn monotonic_time_has_duration_value_semantics() {
        let earlier = MonoTime::from_elapsed(Duration::from_millis(9));
        let equal = MonoTime::from_elapsed(Duration::from_millis(9));
        let later = MonoTime::from_elapsed(Duration::from_millis(10));

        assert_eq!(MonoTime::ZERO.elapsed(), Duration::ZERO);
        assert_eq!(earlier, equal);
        assert!(earlier < later);
        assert_eq!(later.elapsed(), Duration::from_millis(10));
    }

    /// Confirms logical-time addition returns the exact sum and rejects
    /// duration overflow without wrapping toward the epoch.
    #[test]
    fn monotonic_time_addition_is_checked() {
        let start = MonoTime::from_elapsed(Duration::from_secs(2));

        assert_eq!(
            start.checked_add(Duration::from_millis(500)),
            Some(MonoTime::from_elapsed(Duration::from_millis(2_500)))
        );
        assert_eq!(start.checked_add(Duration::ZERO), Some(start));
        assert_eq!(
            MonoTime::from_elapsed(Duration::MAX).checked_add(Duration::from_nanos(1)),
            None
        );
    }

    /// Confirms a transport fault retains only its stable category and remains
    /// copyable for terminal outcome propagation.
    #[test]
    fn transport_fault_preserves_stable_kind() {
        let fault = TransportFault::new(TransportFaultKind::BrokenPipe);
        let copied = fault;

        assert_eq!(fault.kind(), TransportFaultKind::BrokenPipe);
        assert_eq!(copied, fault);
    }

    /// Confirms the three write-terminal facts remain distinguishable while
    /// preserving failure categories for the two unsuccessful outcomes.
    #[test]
    fn write_outcome_preserves_terminal_fact() {
        let not_written_fault = TransportFault::new(TransportFaultKind::ConnectionReset);
        let indeterminate_fault = TransportFault::new(TransportFaultKind::UnexpectedEof);

        assert_eq!(WriteOutcome::Committed, WriteOutcome::Committed);
        assert_eq!(
            WriteOutcome::NotWritten(not_written_fault),
            WriteOutcome::NotWritten(not_written_fault)
        );
        assert_eq!(
            WriteOutcome::Indeterminate(indeterminate_fault),
            WriteOutcome::Indeterminate(indeterminate_fault)
        );
        assert_ne!(
            WriteOutcome::NotWritten(not_written_fault),
            WriteOutcome::Indeterminate(not_written_fault)
        );
    }

    /// Confirms close reasons retain their timeout subtype and barriers retain
    /// the exact Core-assigned WriteId they protect.
    #[test]
    fn close_values_preserve_reason_and_barrier_identity() {
        let timeout = GenerationCloseReason::CommunicationsTimeout(CommunicationsTimeoutKind::T6);
        let barrier = CloseBarrier::AfterWrite(WriteId::new(29));

        assert_eq!(
            timeout,
            GenerationCloseReason::CommunicationsTimeout(CommunicationsTimeoutKind::T6)
        );
        assert_eq!(barrier, CloseBarrier::AfterWrite(WriteId::new(29)));
        assert_ne!(barrier, CloseBarrier::Immediate);
    }
}
