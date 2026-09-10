//! Public Send/Request admission and ownership-preserving Primary failures.
//! Bodies are measured before queueing and share a bounded byte budget. Every
//! accepted message retains its captured connection and is never retried implicitly.

use super::{EndpointError, HsmsHandle, LifecycleCommand, LifecycleIntent, LifecycleReceipt};
use crate::hsms::{
    GenerationSlotSnapshot, OperationError, PrimaryMessage, SecondaryMessage, SendReceipt,
};
use tokio::sync::{mpsc, oneshot};

/// A Primary operation failure, optionally retaining its pre-Core message.
#[derive(Debug)]
pub struct MessageError {
    /// Precise endpoint admission or protocol completion failure.
    error: EndpointError,
    /// Original message when rejected before Core ownership; otherwise None.
    message: Option<PrimaryMessage>,
}

impl MessageError {
    /// Borrows the precise operation failure.
    pub fn error(&self) -> &EndpointError {
        &self.error
    }
    /// Borrows a returned Primary, available only for a pre-Core rejection.
    pub fn message(&self) -> Option<&PrimaryMessage> {
        self.message.as_ref()
    }
    /// Returns the failure and any unconsumed message for application retry policy.
    pub fn into_parts(self) -> (EndpointError, Option<PrimaryMessage>) {
        (self.error, self.message)
    }
    /// Builds an ownership-preserving rejection before entering the command queue.
    fn rejected(error: EndpointError, message: PrimaryMessage) -> Self {
        Self {
            error,
            message: Some(message),
        }
    }
}
impl std::fmt::Display for MessageError {
    /// Formats the precise failure without traversing application content.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}
impl std::error::Error for MessageError {
    /// Exposes the underlying typed endpoint or protocol error.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl HsmsHandle {
    /// Sends a W=0 Primary on the connection captured when this future is polled.
    /// Success proves local full-write completion, not peer processing.
    pub async fn send(&self, message: PrimaryMessage) -> Result<SendReceipt, MessageError> {
        match self.submit_primary(message, false).await? {
            LifecycleReceipt::Sent(receipt) => Ok(receipt),
            _ => unreachable!("Send completion remains typed"),
        }
    }

    /// Sends a W=1 Primary and awaits its matched Secondary under T3.
    /// Dropping this future after queue admission does not cancel protocol work.
    pub async fn request(&self, message: PrimaryMessage) -> Result<SecondaryMessage, MessageError> {
        match self.submit_primary(message, true).await? {
            LifecycleReceipt::Secondary(message) => Ok(message),
            _ => unreachable!("Request completion remains typed"),
        }
    }

    /// Captures routing, validates wire size, reserves count/bytes and transfers once.
    async fn submit_primary(
        &self,
        message: PrimaryMessage,
        reply_expected: bool,
    ) -> Result<LifecycleReceipt, MessageError> {
        let reject = |error, message| MessageError::rejected(error, message);
        if self.commands.is_closed() {
            return Err(reject(EndpointError::RuntimeStopped, message));
        }
        let generation = match self.snapshot().generation() {
            GenerationSlotSnapshot::Open(id) => id,
            GenerationSlotSnapshot::Draining(_) => {
                return Err(reject(EndpointError::Draining, message))
            }
            GenerationSlotSnapshot::None => {
                return Err(reject(EndpointError::NotConnected, message))
            }
        };
        let permit = match self.slots.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return Err(reject(EndpointError::Backpressure, message)),
        };
        let charge = match self.primary_charge(&message) {
            Ok(charge) => charge,
            Err(error) => return Err(reject(error, message)),
        };
        let bytes = match self.bytes.clone().try_acquire_many_owned(charge) {
            Ok(bytes) => bytes,
            Err(_) => return Err(reject(EndpointError::Backpressure, message)),
        };
        let (completion, receiver) = oneshot::channel();
        let command = LifecycleCommand {
            intent: LifecycleIntent::Data {
                generation,
                message,
                reply_expected,
            },
            completion,
            _permit: permit,
            _bytes: Some(bytes),
        };
        if let Err(error) = self.commands.try_send(command) {
            let (command, error) = match error {
                mpsc::error::TrySendError::Full(command) => (command, EndpointError::Backpressure),
                mpsc::error::TrySendError::Closed(command) => {
                    (command, EndpointError::RuntimeStopped)
                }
            };
            let LifecycleIntent::Data { message, .. } = command.intent else {
                unreachable!()
            };
            return Err(reject(error, message));
        }
        match receiver.await {
            Ok(Ok(LifecycleReceipt::RejectedPrimary { message, error })) => {
                Err(reject(error, message))
            }
            Ok(Ok(receipt)) => Ok(receipt),
            Ok(Err(error)) => Err(MessageError {
                error,
                message: None,
            }),
            Err(_) => Err(MessageError {
                error: EndpointError::RuntimeStopped,
                message: None,
            }),
        }
    }

    /// Measures the immutable body without allocation and returns complete wire bytes.
    fn primary_charge(&self, message: &PrimaryMessage) -> Result<u32, EndpointError> {
        use crate::hsms::profile::secs2::{Secs2Profile, StrictSecs2Profile};
        let profile = StrictSecs2Profile::new(crate::secs2::codec::Secs2Decoder::default());
        let text_length = profile
            .prepare_body(message.body())
            .map_err(OperationError::Encode)?
            .encoded_length();
        let message_length = text_length
            .checked_add(10)
            .filter(|length| *length <= self.maximum_message_length)
            .ok_or(OperationError::OutboundFrameTooLarge {
                text_length,
                maximum_message_length: self.maximum_message_length,
            })?;
        message_length
            .checked_add(4)
            .and_then(|length| u32::try_from(length).ok())
            .ok_or(EndpointError::Backpressure)
    }
}
