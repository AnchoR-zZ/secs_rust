//! Single-consumer bounded delivery of incoming Primaries and protocol errors.
//! Separate reliable queues keep malformed-frame reports independent of Primary
//! capacity. Queued Primaries retain encoded-byte charges until the app receives them.

use crate::hsms::{EndpointConfig, InboundPrimary, InboundProtocolError};
use std::sync::Arc;
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

/// Exclusive application receiver; it does not keep endpoint handles alive.
pub struct HsmsReceiver {
    /// Reliable, ordered Primary queue with byte ownership envelopes.
    primaries: mpsc::Receiver<QueuedPrimary>,
    /// Independent reliable malformed-frame error queue.
    errors: mpsc::Receiver<InboundProtocolError>,
}

impl HsmsReceiver {
    /// Receives the next Primary and releases its queue byte charge to the runtime.
    /// Returns None once the runtime ends and all buffered Primaries are consumed.
    pub async fn recv_primary(&mut self) -> Option<InboundPrimary> {
        self.primaries.recv().await.map(|queued| queued.primary)
    }
    /// Receives the next structured malformed-frame error; None means stream end.
    pub async fn recv_protocol_error(&mut self) -> Option<InboundProtocolError> {
        self.errors.recv().await
    }
    /// Splits independent queues so two application tasks can consume them concurrently.
    pub fn split(self) -> (PrimaryReceiver, ProtocolErrorReceiver) {
        (
            PrimaryReceiver {
                inner: self.primaries,
            },
            ProtocolErrorReceiver { inner: self.errors },
        )
    }
}

/// Exclusive Primary stream extracted from the endpoint receiver.
pub struct PrimaryReceiver {
    /// Ordered message envelopes retaining queue byte reservations.
    inner: mpsc::Receiver<QueuedPrimary>,
}
impl PrimaryReceiver {
    /// Transfers the next incoming Primary, returning None after stream completion.
    pub async fn recv(&mut self) -> Option<InboundPrimary> {
        self.inner.recv().await.map(|queued| queued.primary)
    }
}

/// Exclusive structured-error stream extracted from the endpoint receiver.
pub struct ProtocolErrorReceiver {
    /// Reliable error envelopes independent of Primary queue capacity.
    inner: mpsc::Receiver<InboundProtocolError>,
}
impl ProtocolErrorReceiver {
    /// Transfers the next malformed-frame report, returning None after stream end.
    pub async fn recv(&mut self) -> Option<InboundProtocolError> {
        self.inner.recv().await
    }
}

/// Incoming Primary and its queue-only resource reservation.
struct QueuedPrimary {
    /// Owned decoded Primary and any generation-bound reply capability.
    primary: InboundPrimary,
    /// Encoded-byte charge released on receive or receiver drop.
    _bytes: OwnedSemaphorePermit,
}

/// Runtime-only sender side with independent count and aggregate byte bounds.
pub(super) struct Delivery {
    /// Primary queue sender, never cloned outside the runtime.
    primaries: mpsc::Sender<QueuedPrimary>,
    /// Independent reliable error sender.
    errors: mpsc::Sender<InboundProtocolError>,
    /// Aggregate encoded bytes retained by the public Primary queue.
    bytes: Arc<Semaphore>,
}

impl Delivery {
    /// Creates bounded independent queues from the validated endpoint policy.
    pub(super) fn new(config: &EndpointConfig) -> (Self, HsmsReceiver) {
        let (primaries, primary_receiver) =
            mpsc::channel(config.limits().application_event_capacity());
        let (errors, error_receiver) = mpsc::channel(config.runtime().protocol_error_capacity());
        (
            Self {
                primaries,
                errors,
                bytes: Arc::new(Semaphore::new(
                    config.runtime().primary_queue_bytes() as usize
                )),
            },
            HsmsReceiver {
                primaries: primary_receiver,
                errors: error_receiver,
            },
        )
    }

    /// Returns currently usable slots; closed application streams have zero capacity.
    pub(super) fn capacity(&self) -> (usize, usize) {
        (
            if self.primaries.is_closed() {
                0
            } else {
                self.primaries.capacity()
            },
            if self.errors.is_closed() {
                0
            } else {
                self.errors.capacity()
            },
        )
    }

    /// Measures and transfers one Primary without cloning its content or token.
    pub(super) fn primary(&self, primary: InboundPrimary) -> Result<(), ()> {
        use crate::hsms::profile::secs2::{Secs2Profile, StrictSecs2Profile};
        let profile = StrictSecs2Profile::new(crate::secs2::codec::Secs2Decoder::default());
        let length = profile
            .prepare_body(primary.message().body())
            .map_err(|_| ())?
            .encoded_length()
            .checked_add(14)
            .and_then(|length| u32::try_from(length).ok())
            .ok_or(())?;
        let bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(length)
            .map_err(|_| ())?;
        self.primaries
            .try_send(QueuedPrimary {
                primary,
                _bytes: bytes,
            })
            .map_err(|_| ())
    }

    /// Transfers a protocol error without consuming Primary count or byte capacity.
    pub(super) fn error(&self, error: InboundProtocolError) -> Result<(), ()> {
        self.errors.try_send(error).map_err(|_| ())
    }
}
