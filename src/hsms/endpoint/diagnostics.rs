//! Best-effort endpoint diagnostics, isolated from reliable application delivery.
//! Queue saturation never blocks protocol execution. Every omitted record is
//! reflected by a cumulative counter and successful records keep publication order.

use crate::hsms::{ConnectionCloseReason, ConnectionGeneration, ProtocolNotice};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};
use tokio::sync::mpsc;

/// Non-blocking diagnostic observations emitted by a running endpoint.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum DiagnosticEvent {
    /// A generation finished its bounded cleanup attempt.
    ConnectionClosed {
        /// Exact incarnation that ended, even if another is already current.
        generation: ConnectionGeneration,
        /// Original protocol or lifecycle close reason.
        reason: ConnectionCloseReason,
        /// Whether that cleanup attempt proved resource release.
        clean: bool,
    },
    /// One attempted TCP connect/accept failed before generation construction.
    ConnectionAttemptFailed {
        /// Operating-system failure description or connect timeout explanation.
        message: String,
    },
    /// Protocol observation that does not own a message or reply capability.
    Protocol {
        /// Generation that observed the peer event.
        generation: ConnectionGeneration,
        /// Safe protocol classification without raw mutable header authority.
        notice: ProtocolNotice,
    },
}

/// One successfully delivered diagnostic record with publication sequence.
#[derive(Clone, Debug)]
pub struct DiagnosticRecord {
    /// Monotonic attempt sequence; gaps correspond to omitted observations.
    sequence: u64,
    /// Best-effort diagnostic payload.
    event: DiagnosticEvent,
}
impl DiagnosticRecord {
    /// Returns the publication sequence; it never wraps.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }
    /// Borrows the diagnostic payload.
    pub const fn event(&self) -> &DiagnosticEvent {
        &self.event
    }
    /// Transfers the payload to an application diagnostic consumer.
    pub fn into_event(self) -> DiagnosticEvent {
        self.event
    }
}

/// Unique best-effort diagnostic stream; retaining it does not keep handles alive.
pub struct DiagnosticReceiver {
    /// Bounded record receiver.
    records: mpsc::Receiver<DiagnosticRecord>,
    /// Cumulative saturating loss count shared with publishers.
    dropped: Arc<AtomicU64>,
}
impl DiagnosticReceiver {
    /// Receives the next available record, or None after runtime exit and drain.
    pub async fn recv(&mut self) -> Option<DiagnosticRecord> {
        self.records.recv().await
    }
    /// Returns records omitted because capacity, consumer or sequence was unavailable.
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// Cloneable observation publisher, independent of lifecycle command ownership.
#[derive(Clone)]
pub(super) struct DiagnosticPublisher {
    /// Bounded outgoing records; sends always use non-blocking admission.
    records: mpsc::Sender<DiagnosticRecord>,
    /// Next sequence or exhaustion; lock also serializes publication across clones.
    sequence: Arc<Mutex<Option<u64>>>,
    /// Saturating loss count, observable even when the queue remains full.
    dropped: Arc<AtomicU64>,
}
impl DiagnosticPublisher {
    /// Creates an independently bounded diagnostic stream.
    pub(super) fn new(capacity: usize) -> (Self, DiagnosticReceiver) {
        let (records, receiver) = mpsc::channel(capacity);
        let dropped = Arc::new(AtomicU64::new(0));
        (
            Self {
                records,
                sequence: Arc::new(Mutex::new(Some(0))),
                dropped: dropped.clone(),
            },
            DiagnosticReceiver {
                records: receiver,
                dropped,
            },
        )
    }
    /// Publishes once, recording any omission without blocking protocol progress.
    pub(super) fn publish(&self, event: DiagnosticEvent) {
        let mut next = self
            .sequence
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(sequence) = *next {
            *next = sequence.checked_add(1);
            if self
                .records
                .try_send(DiagnosticRecord { sequence, event })
                .is_ok()
            {
                return;
            }
        }
        let _ = self
            .dropped
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
    }
}

/// Converts every internal termination cause without hiding timer/resource detail.
pub(super) fn close_reason(
    reason: crate::hsms::model::runtime::GenerationCloseReason,
) -> ConnectionCloseReason {
    use crate::hsms::model::runtime::{
        CommunicationsTimeoutKind as Timer, GenerationCloseReason as Reason,
    };
    match reason {
        Reason::LocalStop => ConnectionCloseReason::LocalStop,
        Reason::LocalDisconnect => ConnectionCloseReason::LocalDisconnect,
        Reason::LocalSeparate => ConnectionCloseReason::LocalSeparate,
        Reason::SeparateReceived => ConnectionCloseReason::SeparateReceived,
        Reason::TransportLost => ConnectionCloseReason::TransportLost,
        Reason::ProtocolViolation => ConnectionCloseReason::ProtocolViolation,
        Reason::ControlBackpressure => ConnectionCloseReason::ControlBackpressure,
        Reason::ApplicationBackpressure => ConnectionCloseReason::ApplicationBackpressure,
        Reason::SystemBytesExhausted => ConnectionCloseReason::SystemBytesExhausted,
        Reason::RuntimeInvariant => ConnectionCloseReason::RuntimeInvariant,
        Reason::CommunicationsTimeout(timer) => {
            ConnectionCloseReason::CommunicationsTimeout(match timer {
                Timer::T6 => crate::hsms::TimeoutKind::T6,
                Timer::T7 => crate::hsms::TimeoutKind::T7,
                Timer::T8 => crate::hsms::TimeoutKind::T8,
            })
        }
    }
}
