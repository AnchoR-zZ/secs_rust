//! Public single-use reply operations and ownership-preserving error conversion.
//! Reply admission retains original tokens until Core accepts the operation; the
//! same bounded command queue and byte budget apply to outbound reply content.

use super::{EndpointError, HsmsHandle, LifecycleCommand, LifecycleIntent, LifecycleReceipt};
use crate::hsms::{
    GenerationSlotSnapshot, OperationError, ReplyAdmissionError, ReplyAdmissionReason, ReplyIntent,
    ReplyToken, SendReceipt,
};
use crate::secs2::SecsItem;
use tokio::sync::{mpsc, oneshot};

/// Exact reply failure with original inputs when rejected before Core ownership.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct ReplyError {
    /// Precise endpoint, encoding or protocol failure.
    #[source]
    error: EndpointError,
    /// Original reply token/body, present for a known pre-Core rejection.
    admission: Option<ReplyAdmissionError>,
}
impl ReplyError {
    /// Borrows the precise failure independently of retained reply ownership.
    pub fn error(&self) -> &EndpointError {
        &self.error
    }
    /// Borrows the original inputs when the capability was not consumed by Core.
    pub fn admission(&self) -> Option<&ReplyAdmissionError> {
        self.admission.as_ref()
    }
    /// Returns the exact failure and any reusable token/body admission envelope.
    pub fn into_parts(self) -> (EndpointError, Option<ReplyAdmissionError>) {
        (self.error, self.admission)
    }
    /// Preserves original input ownership and maps a stable public admission category.
    fn rejected(
        error: EndpointError,
        intent: ReplyIntent,
        token: ReplyToken,
        body: Option<SecsItem>,
    ) -> Self {
        let reason = match &error {
            EndpointError::RuntimeStopped => ReplyAdmissionReason::RuntimeStopped,
            EndpointError::NotConnected => ReplyAdmissionReason::NotConnected,
            EndpointError::Draining | EndpointError::Operation(OperationError::Draining) => {
                ReplyAdmissionReason::Draining
            }
            EndpointError::Backpressure
            | EndpointError::Operation(OperationError::Backpressure) => {
                ReplyAdmissionReason::Backpressure
            }
            EndpointError::StaleConnectionGeneration => {
                ReplyAdmissionReason::StaleConnectionGeneration
            }
            EndpointError::Faulted => ReplyAdmissionReason::Faulted,
            EndpointError::Operation(OperationError::ReplyRequiresAbort) => {
                ReplyAdmissionReason::ReplyRequiresAbort
            }
            EndpointError::Operation(
                OperationError::Encode(_) | OperationError::OutboundFrameTooLarge { .. },
            ) => ReplyAdmissionReason::InvalidMessage,
            _ => ReplyAdmissionReason::CapabilityUnavailable,
        };
        let admission = match intent {
            ReplyIntent::Secondary => ReplyAdmissionError::secondary(reason, token, body),
            ReplyIntent::Abort => ReplyAdmissionError::abort(reason, token),
            ReplyIntent::Abandon => ReplyAdmissionError::abandon(reason, token),
        };
        Self {
            error,
            admission: Some(admission),
        }
    }
}

impl HsmsHandle {
    /// Replies once with the normal F+1 Secondary, preserving the peer's correlation.
    pub async fn reply(
        &self,
        token: ReplyToken,
        body: Option<SecsItem>,
    ) -> Result<SendReceipt, ReplyError> {
        match self
            .submit_reply(ReplyIntent::Secondary, token, body)
            .await?
        {
            LifecycleReceipt::Sent(receipt) => Ok(receipt),
            _ => unreachable!("typed reply completion"),
        }
    }
    /// Sends a header-only SxF0 abort using the original peer transaction identity.
    pub async fn abort_reply(&self, token: ReplyToken) -> Result<SendReceipt, ReplyError> {
        match self.submit_reply(ReplyIntent::Abort, token, None).await? {
            LifecycleReceipt::Sent(receipt) => Ok(receipt),
            _ => unreachable!("typed abort completion"),
        }
    }
    /// Releases the capability locally without writing a response.
    pub async fn abandon_reply(&self, token: ReplyToken) -> Result<(), ReplyError> {
        self.submit_reply(ReplyIntent::Abandon, token, None)
            .await
            .map(|_| ())
    }

    /// Reserves resources and transfers an intact capability to its captured route.
    async fn submit_reply(
        &self,
        intent: ReplyIntent,
        token: ReplyToken,
        body: Option<SecsItem>,
    ) -> Result<LifecycleReceipt, ReplyError> {
        let reserve = || -> Result<_, EndpointError> {
            if self.commands.is_closed() {
                return Err(EndpointError::RuntimeStopped);
            }
            let generation = match self.snapshot().generation() {
                GenerationSlotSnapshot::Open(id) | GenerationSlotSnapshot::Draining(id) => id,
                GenerationSlotSnapshot::None => return Err(EndpointError::NotConnected),
            };
            if intent == ReplyIntent::Secondary && !token.normal_secondary_available() {
                return Err(OperationError::ReplyRequiresAbort.into());
            }
            let count = self
                .reply_slots
                .clone()
                .try_acquire_owned()
                .map_err(|_| EndpointError::Backpressure)?;
            use crate::hsms::profile::secs2::{Secs2Profile, StrictSecs2Profile};
            let profile = StrictSecs2Profile::new(crate::secs2::codec::Secs2Decoder::default());
            let text_length = profile
                .prepare_body(body.as_ref())
                .map_err(OperationError::Encode)?
                .encoded_length();
            let length = text_length
                .checked_add(10)
                .filter(|length| *length <= self.maximum_message_length)
                .ok_or(OperationError::OutboundFrameTooLarge {
                    text_length,
                    maximum_message_length: self.maximum_message_length,
                })?;
            let bytes = if intent == ReplyIntent::Abandon {
                None
            } else {
                let length = u32::try_from(length + 4).map_err(|_| EndpointError::Backpressure)?;
                Some(
                    self.reply_bytes
                        .clone()
                        .try_acquire_many_owned(length)
                        .map_err(|_| EndpointError::Backpressure)?,
                )
            };
            Ok((generation, count, bytes))
        };
        let (generation, count, bytes) = match reserve() {
            Ok(reserved) => reserved,
            Err(error) => return Err(ReplyError::rejected(error, intent, token, body)),
        };
        let (completion, receiver) = oneshot::channel();
        let command = LifecycleCommand {
            intent: LifecycleIntent::Reply {
                generation,
                intent,
                token,
                body,
            },
            completion,
            _permit: count,
            _bytes: bytes,
        };
        if let Err(error) = self.commands.try_send(command) {
            let (command, error) = match error {
                mpsc::error::TrySendError::Full(command) => (command, EndpointError::Backpressure),
                mpsc::error::TrySendError::Closed(command) => {
                    (command, EndpointError::RuntimeStopped)
                }
            };
            let LifecycleIntent::Reply {
                intent,
                token,
                body,
                ..
            } = command.intent
            else {
                unreachable!()
            };
            return Err(ReplyError::rejected(error, intent, token, body));
        }
        match receiver.await {
            Ok(Ok(LifecycleReceipt::RejectedReply {
                intent,
                token,
                body,
                error,
            })) => Err(ReplyError::rejected(error, intent, token, body)),
            Ok(Ok(receipt)) => Ok(receipt),
            Ok(Err(error)) => Err(ReplyError {
                error,
                admission: None,
            }),
            Err(_) => Err(ReplyError {
                error: EndpointError::RuntimeStopped,
                admission: None,
            }),
        }
    }
}
