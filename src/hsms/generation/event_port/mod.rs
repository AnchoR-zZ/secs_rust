//! Non-blocking reliable application event delivery boundary.

#![allow(dead_code)]

use crate::hsms::EndpointEventEnvelope;

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
pub(crate) trait ApplicationEventPort: Send + Sync {
    /// Attempts to publish `event` without waiting for application capacity.
    ///
    /// Returns `Ok(())` only after the port accepts ownership. On full or
    /// closed delivery, returns [`EventPublishError`] containing the event.
    fn try_publish(&self, event: EndpointEventEnvelope) -> Result<(), EventPublishError>;
}
