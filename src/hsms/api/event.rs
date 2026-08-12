//! Public endpoint events and their monotonically ordered envelope.
//!
//! Events expose stable application semantics while keeping raw correlation
//! headers and runtime implementation details inside the endpoint.

// Internal constructors become production-reachable with SessionDriver/EventPort.
#![allow(dead_code)]

use crate::hsms::{
    error::ProtocolError,
    lifecycle::EndpointStateSnapshot,
    model::ids::{ConnectionGeneration, EventSequence},
    protocol::header::RejectReason,
};

use super::InboundPrimary;

/// Public reason for an open connection generation ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionCloseReason {
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
    /// A uniquely attributed live application operation was failed.
    OperationRejected,
    /// A uniquely attributed autonomous protocol operation was retired.
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
    /// Diagnostic description of a protocol violation seen by the Core.
    Violation(ProtocolError),
    /// Structured result of attributing one peer `Reject.req`.
    PeerReject(PeerRejectNotice),
    /// A late event from an obsolete or terminal transaction was ignored.
    StaleEventIgnored,
}

/// Reliable event published to the endpoint consumer.
#[derive(Debug)]
pub enum EndpointEvent {
    /// The externally observable lifecycle snapshot changed.
    StateChanged(EndpointStateSnapshot),
    /// The Core classified and decoded an inbound Primary Data message.
    Primary(InboundPrimary),
    /// One concrete TCP generation ended.
    ConnectionClosed {
        /// Generation that ended; it may already have been replaced.
        generation: ConnectionGeneration,
        /// Stable public reason for the close.
        reason: ConnectionCloseReason,
    },
    /// Non-data protocol diagnostic that does not complete a command.
    ProtocolNotice(ProtocolNotice),
}

/// Monotonic endpoint event envelope.
#[derive(Debug)]
pub struct EndpointEventEnvelope {
    /// Endpoint-wide monotonic publication sequence.
    sequence: u64,
    /// Originating TCP incarnation, or `None` for endpoint-only events.
    generation: Option<ConnectionGeneration>,
    /// Reliable application event payload.
    event: EndpointEvent,
}

impl EndpointEventEnvelope {
    /// Wraps an event with its publication order and optional generation.
    pub(crate) const fn new(
        sequence: EventSequence,
        generation: Option<ConnectionGeneration>,
        event: EndpointEvent,
    ) -> Self {
        Self {
            sequence: sequence.get(),
            generation,
            event,
        }
    }

    /// Returns the endpoint-wide publication sequence.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the originating TCP generation, when applicable.
    #[must_use]
    pub const fn generation(&self) -> Option<ConnectionGeneration> {
        self.generation
    }

    /// Borrows the reliable endpoint event payload.
    #[must_use]
    pub const fn event(&self) -> &EndpointEvent {
        &self.event
    }

    /// Consumes the envelope and returns its event payload.
    pub fn into_event(self) -> EndpointEvent {
        self.event
    }
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
