//! Public endpoint ownership and asynchronous lifecycle requests.
//! Building allocates bounded channels only. A caller owns the runtime future;
//! cloned handles submit requests while watch subscribers retain the latest state.

mod data;
mod reply;
pub use reply::ReplyError;
mod diagnostics;
mod receiver;
mod runtime;
pub use data::MessageError;
pub use diagnostics::{DiagnosticEvent, DiagnosticReceiver, DiagnosticRecord};
pub use receiver::{HsmsReceiver, PrimaryReceiver, ProtocolErrorReceiver};
pub use runtime::HsmsRuntime;

use crate::hsms::{
    ConfigError, ConnectionGeneration, EndpointConfig, EndpointStateSnapshot,
    GenerationSlotSnapshot,
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};

/// Public failure of endpoint startup, lifecycle admission or resource cleanup.
#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum EndpointError {
    /// The endpoint currently has no connection to receive this protocol command.
    #[error("HSMS endpoint has no current connection")]
    NotConnected,
    /// A routed protocol operation completed with its precise protocol failure.
    #[error(transparent)]
    Operation(#[from] crate::hsms::OperationError),
    /// The bounded endpoint command budget is currently exhausted.
    #[error("HSMS endpoint command capacity exhausted")]
    Backpressure,
    /// The caller-owned runtime future ended or was dropped.
    #[error("HSMS endpoint runtime has stopped")]
    RuntimeStopped,
    /// An existing lifecycle close must finish before this operation can start.
    #[error("HSMS endpoint is draining")]
    Draining,
    /// A local failure requires explicit Stop/recovery before another Start.
    #[error("HSMS endpoint is faulted")]
    Faulted,
    /// The original connection was replaced before Disconnect could be applied.
    #[error("HSMS request belongs to a stale connection generation")]
    StaleConnectionGeneration,
    /// Resource cleanup could not establish that all old tasks were released.
    #[error("HSMS cleanup could not prove resource release")]
    CleanupUnproven,
    /// Socket creation or inspection failed with a stable I/O category.
    #[error("HSMS endpoint I/O error ({kind:?}): {message}")]
    Io {
        /// Portable error category supplied by the operating system.
        kind: std::io::ErrorKind,
        /// Original descriptive error text for diagnostics.
        message: String,
    },
    /// A monotonic lifecycle or generation counter was exhausted.
    #[error("HSMS endpoint identity space exhausted")]
    IdentifierExhausted,
}

/// Acknowledgement that the endpoint's running intent was established.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StartReceipt {
    /// Actual Passive bind address, including the assigned port; None for Active.
    local_address: Option<SocketAddr>,
}

impl StartReceipt {
    /// Returns the listener address; Active startup does not promise connection yet.
    pub const fn local_address(self) -> Option<SocketAddr> {
        self.local_address
    }
}

/// Factory for one stopped, reusable logical HSMS endpoint.
pub struct HsmsEndpoint;

impl HsmsEndpoint {
    /// Builds a handle and an explicitly owned runtime without network I/O or tasks.
    /// Run `runtime.run()` in Tokio before awaiting handle lifecycle operations.
    pub fn build(config: EndpointConfig) -> Result<(HsmsHandle, HsmsRuntime), ConfigError> {
        config.validate()?;
        let capacity = config.limits().command_capacity();
        // Validated sum reserves distinct reply and close admission capacities.
        let (commands, receiver) = mpsc::channel(capacity + config.runtime().reply_capacity() + 1);
        let (state, snapshot) = watch::channel(EndpointStateSnapshot::stopped_clean());
        let (delivery, incoming) = receiver::Delivery::new(&config);
        let (diagnostics, diagnostic_receiver) =
            diagnostics::DiagnosticPublisher::new(config.runtime().diagnostic_capacity());
        let handle = HsmsHandle {
            commands,
            slots: Arc::new(Semaphore::new(capacity)),
            close_slots: Arc::new(Semaphore::new(1)),
            reply_slots: Arc::new(Semaphore::new(config.runtime().reply_capacity())),
            reply_bytes: Arc::new(Semaphore::new(config.runtime().reply_bytes() as usize)),
            snapshot,
            bytes: Arc::new(Semaphore::new(config.runtime().command_bytes() as usize)),
            maximum_message_length: config.limits().max_message_length(),
            receiver: Arc::new(Mutex::new(Some(incoming))),
            diagnostics: Arc::new(Mutex::new(Some(diagnostic_receiver))),
        };
        Ok((
            handle,
            HsmsRuntime::new(config, receiver, state, delivery, diagnostics)?,
        ))
    }
}

/// Cloneable application control side; dropping the final handle ends the runtime.
#[derive(Clone)]
pub struct HsmsHandle {
    /// Sole application senders; the runtime retains only the receiving side.
    commands: mpsc::Sender<LifecycleCommand>,
    /// Ordinary command count budget retained through completion.
    slots: Arc<Semaphore>,
    /// One independently reserved Stop/Disconnect admission through cleanup.
    close_slots: Arc<Semaphore>,
    /// Reply/abort/abandon slots cannot be consumed by outbound Primary requests.
    reply_slots: Arc<Semaphore>,
    /// Encoded reply bytes retained through completion, independent of Primaries.
    reply_bytes: Arc<Semaphore>,
    /// Latest coherent endpoint state, independent of event-consumer speed.
    snapshot: watch::Receiver<EndpointStateSnapshot>,
    /// Aggregate encoded-byte budget retained through each Data completion.
    bytes: Arc<Semaphore>,
    /// Maximum configured HSMS Message Length for pre-queue body validation.
    maximum_message_length: usize,
    /// One receiver shared only for unique extraction across cloned handles.
    receiver: Arc<Mutex<Option<HsmsReceiver>>>,
    /// Unique diagnostic receiver, independent of reliable inbound consumption.
    diagnostics: Arc<Mutex<Option<DiagnosticReceiver>>>,
}

impl HsmsHandle {
    /// Takes the non-blocking diagnostic stream once across all handle clones.
    pub fn take_diagnostics(&self) -> Option<DiagnosticReceiver> {
        self.diagnostics
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }
    /// Takes the endpoint's single receiver once; clones cannot duplicate consumption.
    pub fn take_receiver(&self) -> Option<HsmsReceiver> {
        self.receiver
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
    }
    /// Runs one control procedure on the connection visible when first polled.
    /// Accepted operations are never replayed on a replacement connection.
    pub async fn control(&self, intent: crate::hsms::ControlIntent) -> Result<(), EndpointError> {
        let generation = match self.snapshot().generation() {
            GenerationSlotSnapshot::Open(id) | GenerationSlotSnapshot::Draining(id) => id,
            GenerationSlotSnapshot::None => {
                return Err(if self.commands.is_closed() {
                    EndpointError::RuntimeStopped
                } else {
                    EndpointError::NotConnected
                })
            }
        };
        self.submit(LifecycleIntent::Control(generation, intent))
            .await
            .map(|_| ())
    }

    /// Starts supervision; Passive success means the listener is already bound.
    pub async fn start(&self) -> Result<StartReceipt, EndpointError> {
        match self.submit(LifecycleIntent::Start).await? {
            LifecycleReceipt::Started(receipt) => Ok(receipt),
            _ => unreachable!("Start has a typed internal receipt"),
        }
    }

    /// Stops supervision and waits for graceful close plus proven resource cleanup.
    /// After successful Stop this same handle may start the endpoint again.
    /// Shares one reserved admission slot with Disconnect, independent of ordinary
    /// command pressure; a concurrent close may still return Backpressure.
    pub async fn stop(&self) -> Result<(), EndpointError> {
        self.submit(LifecycleIntent::Stop).await.map(|_| ())
    }

    /// Closes only the generation visible at invocation; running intent persists.
    /// With no visible connection this is a no-op, even if one connects later.
    pub async fn disconnect(&self) -> Result<(), EndpointError> {
        let generation = match self.snapshot().generation() {
            GenerationSlotSnapshot::Open(id) | GenerationSlotSnapshot::Draining(id) => Some(id),
            GenerationSlotSnapshot::None => None,
        };
        self.submit(LifecycleIntent::Disconnect(generation))
            .await
            .map(|_| ())
    }

    /// Returns the latest consistent lifecycle snapshot without blocking.
    pub fn snapshot(&self) -> EndpointStateSnapshot {
        *self.snapshot.borrow()
    }

    /// Creates an independent latest-value subscription; closure means runtime exit.
    pub fn subscribe(&self) -> watch::Receiver<EndpointStateSnapshot> {
        self.snapshot.clone()
    }

    /// Reserves bounded ownership, submits one request, and awaits its unique result.
    /// Dropping the awaiting future never cancels a request already enqueued.
    async fn submit(&self, intent: LifecycleIntent) -> Result<LifecycleReceipt, EndpointError> {
        if self.commands.is_closed() {
            return Err(EndpointError::RuntimeStopped);
        }
        let slots = if matches!(
            intent,
            LifecycleIntent::Stop | LifecycleIntent::Disconnect(_)
        ) {
            &self.close_slots
        } else {
            &self.slots
        };
        let permit = slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| EndpointError::Backpressure)?;
        let (completion, receiver) = oneshot::channel();
        let command = LifecycleCommand {
            intent,
            completion,
            _permit: permit,
            _bytes: None,
        };
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => EndpointError::Backpressure,
                mpsc::error::TrySendError::Closed(_) => EndpointError::RuntimeStopped,
            })?;
        receiver.await.map_err(|_| EndpointError::RuntimeStopped)?
    }
}

/// Endpoint-owned lifecycle intent, with Disconnect's captured route.
enum LifecycleIntent {
    /// Single-use response authority routed without consuming it during admission.
    Reply {
        /// Connection captured when the application submits the reply.
        generation: ConnectionGeneration,
        /// Normal Secondary, Abort or local abandonment.
        intent: crate::hsms::ReplyIntent,
        /// Exclusive peer response capability.
        token: crate::hsms::ReplyToken,
        /// Optional normal-Secondary Message Text.
        body: Option<crate::secs2::SecsItem>,
    },
    /// Owned outbound Primary fixed to its captured connection.
    Data {
        /// Only this TCP incarnation may receive the message.
        generation: ConnectionGeneration,
        /// Application-owned Primary, moved into Driver after route validation.
        message: crate::hsms::PrimaryMessage,
        /// True requests a matched Secondary; false completes at local write.
        reply_expected: bool,
    },
    /// A protocol control operation permanently bound to its captured generation.
    Control(ConnectionGeneration, crate::hsms::ControlIntent),
    /// Establish running intent and bind if Passive.
    Start,
    /// Stop all supervision and recover resources if a previous close was poisoned.
    Stop,
    /// Replace only this captured incarnation, or perform a no-op for None.
    Disconnect(Option<ConnectionGeneration>),
}

/// Typed internal acknowledgement for a lifecycle command.
enum LifecycleReceipt {
    /// Pre-Core reply rejection retaining every moved application input.
    RejectedReply {
        /// Attempted reply operation.
        intent: crate::hsms::ReplyIntent,
        /// Unconsumed response capability.
        token: crate::hsms::ReplyToken,
        /// Original response body.
        body: Option<crate::secs2::SecsItem>,
        /// Exact endpoint or protocol admission failure.
        error: EndpointError,
    },
    /// Local full-write proof for an accepted Send.
    Sent(crate::hsms::SendReceipt),
    /// Matched Secondary for an accepted Request.
    Secondary(crate::hsms::SecondaryMessage),
    /// Rejection before Core ownership, retaining the original Primary.
    RejectedPrimary {
        /// Unconsumed Primary returned to the caller.
        message: crate::hsms::PrimaryMessage,
        /// Precise route, capacity or validation failure.
        error: EndpointError,
    },
    /// Startup is established with its actual listener information.
    Started(StartReceipt),
    /// Stop/Disconnect completed its applicable cleanup barrier.
    Closed,
}

/// One counted lifecycle request retained until its terminal acknowledgement.
struct LifecycleCommand {
    /// Operation and any captured connection identity.
    intent: LifecycleIntent,
    /// Unique response receiver owned by the caller's waiting future.
    completion: oneshot::Sender<Result<LifecycleReceipt, EndpointError>>,
    /// Count charge, including time waiting in a drain-completion collection.
    _permit: OwnedSemaphorePermit,
    /// Data byte reservation; lifecycle/control envelopes carry no body charge.
    _bytes: Option<OwnedSemaphorePermit>,
}

impl LifecycleCommand {
    /// Consumes the unique result endpoint and releases its count reservation.
    fn complete(self, result: Result<LifecycleReceipt, EndpointError>) {
        let _ = self.completion.send(result);
    }
}
