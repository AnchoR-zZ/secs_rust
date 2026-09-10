//! Public connection-close causes and safe protocol diagnostic classifications.
//! Primary delivery, latest-state subscriptions and diagnostic records use their
//! separate endpoint channels rather than a shared generic event envelope.

use crate::hsms::{error::ProtocolError, protocol::header::RejectReason};

/// Public reason for an open connection generation ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionCloseReason {
    /// A protocol communications timer expired and ended the connection.
    CommunicationsTimeout(crate::hsms::TimeoutKind),
    /// Reserved control-write capacity could not admit a required response.
    ControlBackpressure,
    /// The generation consumed its System Bytes namespace and must rotate.
    SystemBytesExhausted,
    /// A runtime invariant failed and automatic recovery was stopped.
    RuntimeInvariant,
    /// The application stopped the logical endpoint.
    LocalStop,
    /// The application requested replacement of the current connection.
    LocalDisconnect,
    /// The application sent `Separate.req` and then closed the connection.
    LocalSeparate,
    /// The peer sent `Separate.req`.
    SeparateReceived,
    /// The TCP transport was lost or became unusable.
    TransportLost,
    /// Continuing the connection would violate HSMS protocol invariants.
    ProtocolViolation,
    /// Reliable inbound event delivery could not accept more work.
    ApplicationBackpressure,
}

/// How the Core classified one peer `Reject.req`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PeerRejectDisposition {
    /// A uniquely attributed command-backed operation was failed. This includes
    /// runtime-initiated Select, which uses the same command path as public control.
    OperationRejected,
    /// A uniquely attributed Core-owned operation without a command completion
    /// was retired, such as the idle Linktest probe.
    AutonomousRejected,
    /// No retained outbound candidate matched the peer reference.
    Unknown,
    /// More than one candidate matched, so no operation was changed.
    Ambiguous,
    /// The reference matched work already completed by another outcome.
    Late,
    /// The same rejection had already been processed.
    Duplicate,
    /// A retained operation had already received a different rejection.
    Conflicting,
    /// An extension reason had no configured attribution semantics.
    UnsupportedExtension,
}

/// Header-safe diagnostic for one peer `Reject.req`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PeerRejectNotice {
    /// Exact non-zero base or extension reason supplied by the peer.
    reason: RejectReason,
    /// Safe attribution result selected by the Core.
    disposition: PeerRejectDisposition,
}

impl PeerRejectNotice {
    /// Creates a peer-rejection notice from its reason and safe attribution.
    #[cfg(any(feature = "runtime-tokio", test))]
    pub(crate) const fn new(reason: RejectReason, disposition: PeerRejectDisposition) -> Self {
        Self {
            reason,
            disposition,
        }
    }

    /// Returns the exact non-zero peer rejection reason.
    #[must_use]
    pub const fn reason(self) -> RejectReason {
        self.reason
    }

    /// Returns how the Core attributed the rejected work.
    #[must_use]
    pub const fn disposition(self) -> PeerRejectDisposition {
        self.disposition
    }
}

/// Non-data protocol observation intended for application diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProtocolNotice {
    /// A live transaction was found but the Secondary violated its response contract.
    SecondaryMismatch,
    /// No live or retained transaction matched an incoming Secondary.
    UnmatchedSecondary,
    /// Diagnostic description of a protocol violation seen by the Core.
    Violation(ProtocolError),
    /// Structured result of attributing one peer `Reject.req`.
    PeerReject(PeerRejectNotice),
    /// A late event from an obsolete or terminal transaction was ignored.
    StaleEventIgnored,
}

#[cfg(test)]
mod tests {
    use crate::hsms::protocol::header::RejectReason;

    use super::{PeerRejectDisposition, PeerRejectNotice};

    /// Confirms peer-rejection diagnostics retain safe structured information.
    #[test]
    fn peer_reject_notice_is_header_safe() {
        let notice = PeerRejectNotice::new(
            RejectReason::UNSUPPORTED_PTYPE,
            PeerRejectDisposition::Ambiguous,
        );

        assert_eq!(notice.reason(), RejectReason::UNSUPPORTED_PTYPE);
        assert_eq!(notice.disposition(), PeerRejectDisposition::Ambiguous);
    }
}
