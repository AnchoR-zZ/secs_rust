//! Non-blocking reliable application event delivery boundary.
//!
//! The port only transfers an already-classified event once. It never
//! allocates, validates, consumes, or retries a reply capability and never
//! decides whether a generation closes. SessionDriver reports the exact result
//! of every Delivered, Full, or Closed attempt to Core before polling any
//! application command.

#![allow(dead_code)]

use crate::hsms::contracts::EndpointEventEnvelope;

/// A publish failure always returns ownership of the event. Nothing is
/// silently dropped, and the driver can feed the failure back to the Core.
#[derive(Debug)]
pub(crate) enum EventPublishError {
    /// Bounded queue had no capacity; contains the unpublished event.
    Full(EndpointEventEnvelope),
    /// Application receiver was closed; contains the unpublished event.
    Closed(EndpointEventEnvelope),
}

impl EventPublishError {
    /// Consumes the failure and returns the event that was not published.
    pub(crate) fn into_event(self) -> EndpointEventEnvelope {
        match self {
            Self::Full(event) | Self::Closed(event) => event,
        }
    }
}

/// Runtime-neutral port used by SessionDriver. Implementations must return
/// immediately; application backpressure must never block the Core loop.
/// `Ok(())` transfers event ownership to the application side. Full and Closed
/// return the original event, leaving close policy exclusively to Core. The
/// driver must return every outcome to Core before polling any application
/// command, regardless of whether publication succeeded.
pub(crate) trait ApplicationEventPort: Send + Sync {
    /// Attempts to publish `event` without waiting for application capacity.
    ///
    /// Returns `Ok(())` only after the port accepts ownership. On full or
    /// closed delivery, returns [`EventPublishError`] containing the event.
    /// Implementations make exactly one attempt and never retry internally.
    fn try_publish(&self, event: EndpointEventEnvelope) -> Result<(), EventPublishError>;
}
