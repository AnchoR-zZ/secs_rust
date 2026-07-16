//! Reliable endpoint events and their monotonic publication envelope.
//!
//! The endpoint publishes state, inbound Data, close, and diagnostic events
//! through the non-blocking application event port without exposing internal
//! actor or channel representations.

#![allow(dead_code)]

use crate::hsms::{
    model::ids::EventSequence, ConnectionGeneration, EndpointStateSnapshot, InboundPrimary,
    ProtocolError,
};

/// Public reason for an open generation ending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionCloseReason {
    /// The application stopped the logical endpoint.
    LocalStop,
    /// The application requested replacement of the current connection.
    LocalDisconnect,
    /// The peer sent `Separate.req`.
    SeparateReceived,
    /// The TCP transport was lost or became unusable.
    TransportLost,
    /// Continuing the connection would violate HSMS protocol invariants.
    ProtocolViolation,
    /// Reliable inbound delivery could not accept another event.
    ApplicationBackpressure,
}

/// Non-data protocol observation intended for diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolNotice {
    /// Diagnostic description of a protocol violation seen by the Core.
    Violation(ProtocolError),
    /// A late event from an obsolete generation was safely ignored.
    StaleEventIgnored,
}

/// Reliable events published to the endpoint consumer.
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
    /// Non-data protocol diagnostic that does not itself complete a command.
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
    /// Wraps `event` with its endpoint publication `sequence` and optional
    /// originating `generation`; only the runtime event dispatcher constructs
    /// envelopes.
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

    #[must_use]
    /// Returns the endpoint-wide monotonic publication sequence.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    #[must_use]
    /// Returns the event's TCP generation, if it originated inside one.
    pub const fn generation(&self) -> Option<ConnectionGeneration> {
        self.generation
    }

    #[must_use]
    /// Borrows the reliable endpoint event payload.
    pub const fn event(&self) -> &EndpointEvent {
        &self.event
    }

    #[must_use]
    /// Consumes the envelope and returns its event payload.
    pub fn into_event(self) -> EndpointEvent {
        self.event
    }
}
