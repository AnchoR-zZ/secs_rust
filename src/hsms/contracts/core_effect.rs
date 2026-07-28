//! Defines side-effect requests emitted by Core for SessionDriver execution.
//!
//! Each variant names work outside protocol state. SessionDriver performs the
//! work and returns its observable outcome as a [`super::CoreEvent`].
//! It must execute every effect vector strictly in order. In particular,
//! [`CoreEffect::SetDataGate`] is an infallible synchronous fence: the new gate
//! state must take effect before any later effect or the next Core input is
//! processed. After a publication effect, SessionDriver must submit its
//! [`super::CoreEvent::ApplicationDeliveryFinished`] result before
//! polling any application command.

use std::time::Duration;

use crate::hsms::{
    lifecycle::SessionState,
    model::{
        ids::{ConnectionGeneration, DeliveryId},
        runtime::{GenerationCloseReason, TimerToken},
    },
};

use super::{
    completion::CoreCommandCompletion,
    endpoint_event::ProtocolNotice,
    message::InboundPrimary,
    write::{AbortWriteReceipt, DataGateState, PreparedWrite, ProceedWriteReceipt},
};

/// Side effects requested by the pure Core and executed by SessionDriver.
#[derive(Debug)]
pub(crate) enum CoreEffect {
    /// Reserve an outbound lane position for a write already registered by Core.
    ///
    /// SessionDriver returns exactly one
    /// [`super::CoreEvent::WriteScheduled`] event. Scheduling failure is
    /// terminal: no BeginWrite, visibility, or WriteFinished event follows.
    ScheduleWrite {
        /// Owned descriptor binding the exact message, write and operation IDs,
        /// scheduling class, and derived outbound protocol identity.
        prepared: PreparedWrite,
    },
    /// Authorize the single writer to cross a previously reported write fence.
    ///
    /// Before emitting this effect Core validates the exact write/sequence,
    /// commits any deferred peer-response transition, conservatively advances
    /// a still-live Registry operation to may-be-visible, and commits the
    /// WriteLedger fence resolution. Runtime must write no byte before this
    /// effect and must eventually report one terminal WriteFinished event.
    ProceedWrite {
        /// Branded permission binding generation, write, operation, class, and
        /// wire sequence for the exact fence being released.
        receipt: ProceedWriteReceipt,
    },
    /// Cancel a fenced write before any byte becomes visible.
    ///
    /// Runtime must emit no visibility event and must terminate the exact write
    /// as [`WriteResult::NotWritten`](crate::hsms::model::runtime::WriteResult::NotWritten)
    /// with a cancelled transport fault.
    AbortWrite {
        /// Branded permission binding generation, write, operation, class, and
        /// wire sequence for the exact fence being cancelled.
        receipt: AbortWriteReceipt,
    },
    /// Synchronously change whether Data frames may enter the scheduler.
    ///
    /// This effect is an infallible ordering fence. SessionDriver must finish
    /// applying `state` before executing any later effect in the same vector
    /// and before submitting another Core input; it returns no acknowledgement
    /// event to Core.
    SetDataGate {
        /// Gate state the scheduler must apply before continuing execution.
        state: DataGateState,
    },
    /// Register a runtime deadline for a unique Core timer token.
    ArmTimer {
        /// Identity and semantic kind returned if the timer expires.
        token: TimerToken,
        /// Relative delay to execute outside the Core.
        duration: Duration,
    },
    /// Cancel the exact timer registration represented by `token`.
    CancelTimer {
        /// Identity that prevents cancellation of a later re-armed timer.
        token: TimerToken,
    },
    /// Deliver one terminal result to the accepted command's completion guard.
    CompleteCommand(CoreCommandCompletion),
    /// Reliably publish a classified inbound Primary to the application.
    ///
    /// Core has already atomically registered the DeliveryId and, for W=1, its
    /// reply capability. Runtime attempts publication exactly once, never
    /// retries independently, and returns exactly one delivery completion
    /// before polling any application command.
    PublishInbound {
        /// Core-assigned identity used to correlate publication completion.
        delivery_id: DeliveryId,
        /// Classified inbound Primary to publish exactly once.
        inbound: InboundPrimary,
    },
    /// Reliably publish a non-data protocol diagnostic.
    ///
    /// Runtime attempts this DeliveryId exactly once and returns exactly one
    /// delivery completion; retry and close policy remain Core decisions.
    PublishProtocolNotice {
        /// Core-assigned identity used to correlate publication completion.
        delivery_id: DeliveryId,
        /// Non-data protocol diagnostic to publish exactly once.
        notice: ProtocolNotice,
    },
    /// Commit a new selection-state observation to endpoint lifecycle state.
    SessionStateChanged(SessionState),
    /// Begin the sole transport-close request for this generation.
    ///
    /// Core must emit this effect at most once per generation and must never
    /// create concurrent close requests. Runtime execution is idempotent: a
    /// replayed or duplicate effect must not start another close and must not
    /// produce another completion. The unique request terminates with exactly
    /// one [`super::CoreEvent::TransportCloseCompleted`] input.
    RequestTransportClose {
        /// Stable reason retained for the generation's unique close request.
        reason: GenerationCloseReason,
    },
}

impl CoreEffect {
    /// Returns an intrinsic generation stamp carried by a write effect.
    ///
    /// Effects without their own generation-scoped write descriptor inherit
    /// affinity solely from the surrounding [`CoreEffectBatch`].
    fn intrinsic_generation(&self) -> Option<ConnectionGeneration> {
        match self {
            Self::ScheduleWrite { prepared } => Some(prepared.generation()),
            Self::ProceedWrite { receipt } => Some(receipt.generation()),
            Self::AbortWrite { receipt } => Some(receipt.generation()),
            _ => None,
        }
    }
}

/// Generation-bound collection of effects returned by one Core reduction.
///
/// SessionDriver consumes the batch as one value, so the destination
/// generation cannot be selected independently from the effects it carries.
/// This Phase-A constructor validates only effects that intrinsically carry a
/// write generation. Timers, publications, and other effects are not stamped
/// here; the final `HsmsCore::finish_reduction` boundary will be the sole
/// production constructor once aggregate integration is implemented.
#[must_use = "a Core effect batch must be dispatched to its bound generation"]
#[derive(Debug)]
pub(crate) struct CoreEffectBatch {
    /// Exact connection generation that must execute every contained effect.
    generation: ConnectionGeneration,
    /// Ordered effects emitted by one deterministic Core transition.
    effects: Vec<CoreEffect>,
}

impl CoreEffectBatch {
    /// Validates and binds one ordered effect list to a connection generation.
    ///
    /// Every intrinsically stamped write effect must name `generation`.
    /// Validation has no external side effects and a mismatch returns the
    /// complete original effect list.
    ///
    /// # Errors
    ///
    /// Returns [`CoreEffectBatchRejection`] for the first write effect carrying
    /// another generation.
    pub(crate) fn try_new(
        generation: ConnectionGeneration,
        effects: Vec<CoreEffect>,
    ) -> Result<Self, CoreEffectBatchRejection> {
        for (index, effect) in effects.iter().enumerate() {
            if let Some(actual) = effect.intrinsic_generation() {
                if actual != generation {
                    return Err(CoreEffectBatchRejection {
                        index,
                        expected: generation,
                        actual,
                        effects,
                    });
                }
            }
        }
        Ok(Self {
            generation,
            effects,
        })
    }

    /// Returns the exact destination generation.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Borrows the ordered effects for inspection without separating affinity.
    pub(crate) fn effects(&self) -> &[CoreEffect] {
        &self.effects
    }

    /// Consumes the batch into its inseparable destination and effect list.
    ///
    /// Returns `(generation, effects)` for SessionDriver dispatch.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, Vec<CoreEffect>) {
        (self.generation, self.effects)
    }
}

/// Move-only generation-affinity rejection returning every effect intact.
#[must_use = "a rejected effect batch contains the original effects"]
#[derive(Debug)]
pub(crate) struct CoreEffectBatchRejection {
    /// Index of the first intrinsically mismatched write effect.
    index: usize,
    /// Generation selected for the effect batch.
    expected: ConnectionGeneration,
    /// Generation carried by the mismatching write descriptor or receipt.
    actual: ConnectionGeneration,
    /// Complete ordered effects supplied to batch construction.
    effects: Vec<CoreEffect>,
}

impl CoreEffectBatchRejection {
    /// Returns the index of the first mismatching write effect.
    pub(crate) const fn index(&self) -> usize {
        self.index
    }

    /// Returns the generation selected for the effect batch.
    pub(crate) const fn expected(&self) -> ConnectionGeneration {
        self.expected
    }

    /// Returns the conflicting intrinsic write generation.
    pub(crate) const fn actual(&self) -> ConnectionGeneration {
        self.actual
    }

    /// Consumes the rejection into mismatch facts and original effects.
    ///
    /// Returns `(index, expected, actual, effects)`.
    pub(crate) fn into_parts(
        self,
    ) -> (
        usize,
        ConnectionGeneration,
        ConnectionGeneration,
        Vec<CoreEffect>,
    ) {
        (self.index, self.expected, self.actual, self.effects)
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        contracts::{
            CoreEffect, CoreEffectBatch, DataGateState, PreparedWrite, WriteReceiptIssuer,
            WriteSpec,
        },
        model::ids::{ConnectionGeneration, OperationId, SystemBytes, WriteId},
        protocol::{header::ControlMessage, message::ProtocolMessage},
    };

    /// Creates one generation-stamped critical write descriptor.
    fn prepared(generation: u64, write_id: u64) -> PreparedWrite {
        let generation = ConnectionGeneration::new(generation);
        let issuer = WriteReceiptIssuer::new(generation);
        let spec = WriteSpec::no_hook(ProtocolMessage::Control(ControlMessage::LinktestRequest {
            system_bytes: SystemBytes::new(write_id as u32),
        }))
        .expect("control request is a valid no-hook write");
        let registration = issuer
            .bind(WriteId::new(write_id), OperationId::new(write_id), spec)
            .expect("typed control message has a valid outbound shape");
        let (prepared, _scheduling) = registration.into_scheduling();
        prepared
    }

    /// Rejects a mixed-generation write batch and returns every effect intact.
    #[test]
    fn batch_rejects_mixed_generation_write_effects() {
        let effects = vec![
            CoreEffect::ScheduleWrite {
                prepared: prepared(1, 1),
            },
            CoreEffect::ScheduleWrite {
                prepared: prepared(2, 2),
            },
        ];
        let rejection = CoreEffectBatch::try_new(ConnectionGeneration::new(1), effects)
            .expect_err("second write belongs to another generation");
        assert_eq!(rejection.index(), 1);
        assert_eq!(rejection.expected(), ConnectionGeneration::new(1));
        assert_eq!(rejection.actual(), ConnectionGeneration::new(2));
        let (_, _, _, effects) = rejection.into_parts();
        assert_eq!(effects.len(), 2);
    }

    /// Binds matching stamped and unstamped effects to one immutable generation.
    #[test]
    fn batch_accepts_matching_generation_effects() {
        let batch = CoreEffectBatch::try_new(
            ConnectionGeneration::new(1),
            vec![
                CoreEffect::ScheduleWrite {
                    prepared: prepared(1, 1),
                },
                CoreEffect::SetDataGate {
                    state: DataGateState::Closed,
                },
            ],
        )
        .expect("all stamped effects match the batch generation");
        assert_eq!(batch.generation(), ConnectionGeneration::new(1));
        assert_eq!(batch.effects().len(), 2);
        let (generation, effects) = batch.into_parts();
        assert_eq!(generation, ConnectionGeneration::new(1));
        assert_eq!(effects.len(), 2);
    }
}
