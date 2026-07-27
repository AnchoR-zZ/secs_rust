//! Allocates monotonic, generation-local identifiers for Core resource use cases.
//!
//! This Sans-I/O owner keeps independent sequences for Operation, Write,
//! Delivery, ReplyCapability, and Timer identities. `CommandId` remains owned by
//! application admission, while `WireSequence` remains owned by the scheduler.
//! Successful allocation burns IDs immediately: a later resource-registration
//! failure intentionally leaves a diagnostic gap rather than rolling a cursor
//! back and risking identity reuse.
//!
//! Returned bundles are move-only handoff containers, but their contained ID
//! newtypes remain copyable correlation labels rather than unforgeable
//! authorities. The future `CoreResources` aggregate must consume each bundle
//! inside one private use case and atomically commit the corresponding ledgers.

use crate::hsms::model::ids::{
    ConnectionGeneration, DeliveryId, OperationId, ReplyCapabilityId, TimerId, WriteId,
};

/// Structured exhaustion of one generation-local identifier sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IdAllocationError {
    /// No further Operation ID can be represented without wrapping.
    Operation,
    /// No further Write ID can be represented without wrapping.
    Write,
    /// No further Delivery ID can be represented without wrapping.
    Delivery,
    /// No further ReplyCapability ID can be represented without wrapping.
    ReplyCapability,
    /// No further Timer ID can be represented without wrapping.
    Timer,
}

/// One monotonic, non-zero `u64` cursor that permanently stops after `u64::MAX`.
#[derive(Debug, PartialEq, Eq)]
struct SequenceCursor {
    /// Value issued by the next successful allocation, or `None` after exhaustion.
    next: Option<u64>,
}

impl SequenceCursor {
    /// Creates a fresh sequence whose first issued value is one.
    ///
    /// Returns an allocation-free cursor ready to issue its first identity.
    const fn starting_at_one() -> Self {
        Self { next: Some(1) }
    }

    /// Creates a cursor at an injected next value for boundary-focused tests.
    ///
    /// `next` is the value to issue next; `None` represents permanent
    /// exhaustion. Production construction always uses [`Self::starting_at_one`].
    #[cfg(test)]
    const fn from_next_for_test(next: Option<u64>) -> Self {
        Self { next }
    }

    /// Preflights this cursor without changing it.
    ///
    /// `exhausted` identifies this cursor in the returned error. The return
    /// value is the next numeric ID when allocation remains possible.
    fn preflight(&self, exhausted: IdAllocationError) -> Result<u64, IdAllocationError> {
        self.next.ok_or(exhausted)
    }

    /// Burns the preflighted value and advances without ever wrapping.
    ///
    /// Callers invoke this only after every cursor required by the use case has
    /// passed preflight, which makes multi-ID allocation all-or-nothing.
    fn advance_after_preflight(&mut self) {
        self.next = self.next.and_then(|value| value.checked_add(1));
    }
}

/// Move-only handoff of IDs reserved for an outbound operation and its write.
///
/// The bundle keeps allocation results together until `CoreResources` consumes
/// them. Its contained IDs are correlation labels, not mutation authorities.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "outbound Operation and Write IDs must be committed to their resource owners"]
pub(crate) struct OutboundWriteIds {
    /// TCP generation that owns every identity in this bundle.
    generation: ConnectionGeneration,
    /// Newly burned semantic operation identity.
    operation_id: OperationId,
    /// Newly burned outbound write identity.
    write_id: WriteId,
}

impl OutboundWriteIds {
    /// Consumes the handoff into generation, Operation ID, and Write ID.
    ///
    /// The returned IDs are copyable correlation labels. The caller must be the
    /// single `CoreResources` use case that commits both resource owners.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, OperationId, WriteId) {
        (self.generation, self.operation_id, self.write_id)
    }
}

/// Move-only handoff of an ID reserved for a local no-write operation.
///
/// The contained Operation ID is a correlation label, not mutation authority.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "the local Operation ID must be committed to its resource owner"]
pub(crate) struct LocalOperationIds {
    /// TCP generation that owns this operation.
    generation: ConnectionGeneration,
    /// Newly burned semantic operation identity.
    operation_id: OperationId,
}

impl LocalOperationIds {
    /// Consumes the handoff into its generation and Operation ID.
    ///
    /// The returned ID remains a copyable correlation label whose registration
    /// belongs to one private `CoreResources` use case.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, OperationId) {
        (self.generation, self.operation_id)
    }
}

/// Move-only Delivery-ID handoff for an inbound W=0 Primary publication.
///
/// W=0 grants no reply authority, so this type cannot contain or expose a
/// ReplyCapability ID.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "the W=0 Delivery ID must be committed to the delivery owner"]
pub(crate) struct InboundW0PublicationIds {
    /// TCP generation that owns this publication.
    generation: ConnectionGeneration,
    /// Newly burned reliable application-delivery identity.
    delivery_id: DeliveryId,
}

impl InboundW0PublicationIds {
    /// Consumes the W=0 handoff into its generation and Delivery ID.
    ///
    /// The returned ID is a copyable correlation label. Delivery registration
    /// remains the responsibility of one private `CoreResources` use case.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, DeliveryId) {
        (self.generation, self.delivery_id)
    }
}

/// Move-only Delivery and ReplyCapability handoff for an inbound W=1 Primary.
///
/// Both IDs are always present. The ReplyCapability ID is only a correlation
/// label; the Reply ledger separately owns exact publication and token authority.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "W=1 Delivery and ReplyCapability IDs must be committed atomically"]
pub(crate) struct InboundW1PublicationIds {
    /// TCP generation that owns both publication resources.
    generation: ConnectionGeneration,
    /// Newly burned reliable application-delivery identity.
    delivery_id: DeliveryId,
    /// Newly burned reply-capability correlation identity.
    reply_capability_id: ReplyCapabilityId,
}

impl InboundW1PublicationIds {
    /// Consumes the W=1 handoff into generation, Delivery ID, and ReplyCapability ID.
    ///
    /// The returned IDs are copyable labels. A single private `CoreResources`
    /// use case must use them to commit Delivery and Reply ownership atomically.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, DeliveryId, ReplyCapabilityId) {
        (self.generation, self.delivery_id, self.reply_capability_id)
    }
}

/// Move-only Delivery-ID handoff for a protocol-notice publication.
///
/// Protocol notices are not W=0 Data messages and never allocate reply authority.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "the protocol-notice Delivery ID must be committed to the delivery owner"]
pub(crate) struct ProtocolNoticePublicationIds {
    /// TCP generation that owns this notice publication.
    generation: ConnectionGeneration,
    /// Newly burned reliable application-delivery identity.
    delivery_id: DeliveryId,
}

impl ProtocolNoticePublicationIds {
    /// Consumes the notice handoff into its generation and Delivery ID.
    ///
    /// The returned ID is a copyable correlation label whose registration
    /// belongs to one private `CoreResources` use case.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, DeliveryId) {
        (self.generation, self.delivery_id)
    }
}

/// Move-only handoff of an ID reserved for one timer registration.
///
/// The contained Timer ID is a correlation label, not mutation authority.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "the Timer ID must be committed to the timer owner"]
pub(crate) struct TimerIds {
    /// TCP generation that owns this timer.
    generation: ConnectionGeneration,
    /// Newly burned timer registration identity.
    timer_id: TimerId,
}

impl TimerIds {
    /// Consumes the handoff into its generation and Timer ID.
    ///
    /// The returned ID remains a copyable correlation label whose registration
    /// belongs to one private `CoreResources` use case.
    pub(crate) fn into_parts(self) -> (ConnectionGeneration, TimerId) {
        (self.generation, self.timer_id)
    }
}

/// Single-threaded owner of all Core-allocated ID sequences for one TCP generation.
///
/// Each sequence advances independently. Allocation methods operate at use-case
/// granularity so every required cursor is preflighted before any cursor moves.
/// IDs are burned on success and deliberately never rolled back when a later
/// resource commit fails.
#[derive(Debug, PartialEq, Eq)]
#[must_use = "generation-local ID sequences must be retained for the generation lifetime"]
pub(crate) struct IdSequences {
    /// TCP generation stamped onto every returned allocation bundle.
    generation: ConnectionGeneration,
    /// Next semantic Operation ID.
    operations: SequenceCursor,
    /// Next outbound Write ID.
    writes: SequenceCursor,
    /// Next reliable Delivery ID.
    deliveries: SequenceCursor,
    /// Next inbound ReplyCapability ID.
    reply_capabilities: SequenceCursor,
    /// Next timer registration ID.
    timers: SequenceCursor,
}

impl IdSequences {
    /// Creates allocation-free sequences for `generation`, each starting at one.
    ///
    /// The returned owner performs no capacity-proportional allocation and
    /// contains no socket, task, channel, clock, or other runtime dependency.
    pub(crate) const fn new(generation: ConnectionGeneration) -> Self {
        Self {
            generation,
            operations: SequenceCursor::starting_at_one(),
            writes: SequenceCursor::starting_at_one(),
            deliveries: SequenceCursor::starting_at_one(),
            reply_capabilities: SequenceCursor::starting_at_one(),
            timers: SequenceCursor::starting_at_one(),
        }
    }

    /// Creates sequences with injected cursor positions for boundary tests.
    ///
    /// Each `Option<u64>` is the next value for the correspondingly named
    /// sequence; `None` marks that sequence exhausted. Non-zero values preserve
    /// the production invariant that zero is never issued.
    #[cfg(test)]
    fn with_next_values_for_test(
        generation: ConnectionGeneration,
        operation: Option<u64>,
        write: Option<u64>,
        delivery: Option<u64>,
        reply_capability: Option<u64>,
        timer: Option<u64>,
    ) -> Self {
        debug_assert!([operation, write, delivery, reply_capability, timer]
            .into_iter()
            .all(|next| next != Some(0)));
        Self {
            generation,
            operations: SequenceCursor::from_next_for_test(operation),
            writes: SequenceCursor::from_next_for_test(write),
            deliveries: SequenceCursor::from_next_for_test(delivery),
            reply_capabilities: SequenceCursor::from_next_for_test(reply_capability),
            timers: SequenceCursor::from_next_for_test(timer),
        }
    }

    /// Returns the TCP generation stamped onto every allocation bundle.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Atomically burns one Operation ID and one Write ID.
    ///
    /// Both sequences are preflighted before either advances. If either
    /// sequence is exhausted, the method returns the corresponding
    /// [`IdAllocationError`] and leaves every cursor unchanged.
    ///
    /// A successful bundle is not rolled back if later resource registration
    /// fails; the resulting ID gap preserves generation-local non-reuse.
    pub(crate) fn allocate_outbound_write(
        &mut self,
    ) -> Result<OutboundWriteIds, IdAllocationError> {
        let operation = self.operations.preflight(IdAllocationError::Operation)?;
        let write = self.writes.preflight(IdAllocationError::Write)?;

        self.operations.advance_after_preflight();
        self.writes.advance_after_preflight();

        Ok(OutboundWriteIds {
            generation: self.generation,
            operation_id: OperationId::new(operation),
            write_id: WriteId::new(write),
        })
    }

    /// Atomically burns one Operation ID for a no-write local operation.
    ///
    /// Exhaustion returns [`IdAllocationError::Operation`] without
    /// changing any cursor. A successful ID remains burned if later resource
    /// registration fails.
    pub(crate) fn allocate_local_operation(
        &mut self,
    ) -> Result<LocalOperationIds, IdAllocationError> {
        let operation = self.operations.preflight(IdAllocationError::Operation)?;

        self.operations.advance_after_preflight();

        Ok(LocalOperationIds {
            generation: self.generation,
            operation_id: OperationId::new(operation),
        })
    }

    /// Burns one Delivery ID for an inbound W=0 Primary publication.
    ///
    /// This typed path cannot allocate reply authority. Exhaustion leaves every
    /// cursor unchanged; success burns the Delivery ID even if later delivery
    /// registration fails.
    pub(crate) fn allocate_inbound_w0_primary(
        &mut self,
    ) -> Result<InboundW0PublicationIds, IdAllocationError> {
        let delivery = self.deliveries.preflight(IdAllocationError::Delivery)?;

        self.deliveries.advance_after_preflight();

        Ok(InboundW0PublicationIds {
            generation: self.generation,
            delivery_id: DeliveryId::new(delivery),
        })
    }

    /// Atomically burns Delivery and ReplyCapability IDs for an inbound W=1 Primary.
    ///
    /// Both sequences are preflighted before either advances. If either is
    /// exhausted, all cursors remain unchanged. Success always returns a
    /// non-optional ReplyCapability ID and burns both labels even if a later
    /// cross-ledger commit fails.
    pub(crate) fn allocate_inbound_w1_primary(
        &mut self,
    ) -> Result<InboundW1PublicationIds, IdAllocationError> {
        let delivery = self.deliveries.preflight(IdAllocationError::Delivery)?;
        let reply_capability = self
            .reply_capabilities
            .preflight(IdAllocationError::ReplyCapability)?;

        self.deliveries.advance_after_preflight();
        self.reply_capabilities.advance_after_preflight();

        Ok(InboundW1PublicationIds {
            generation: self.generation,
            delivery_id: DeliveryId::new(delivery),
            reply_capability_id: ReplyCapabilityId::new(reply_capability),
        })
    }

    /// Burns one Delivery ID for a non-data protocol-notice publication.
    ///
    /// This path is distinct from inbound W=0 Data and cannot advance the
    /// ReplyCapability cursor. Delivery exhaustion leaves every cursor unchanged;
    /// success burns the label even if later delivery registration fails.
    pub(crate) fn allocate_protocol_notice(
        &mut self,
    ) -> Result<ProtocolNoticePublicationIds, IdAllocationError> {
        let delivery = self.deliveries.preflight(IdAllocationError::Delivery)?;

        self.deliveries.advance_after_preflight();

        Ok(ProtocolNoticePublicationIds {
            generation: self.generation,
            delivery_id: DeliveryId::new(delivery),
        })
    }

    /// Atomically burns one Timer ID for a timer registration.
    ///
    /// Exhaustion returns [`IdAllocationError::Timer`] without
    /// changing any cursor. A successful ID remains burned if later timer
    /// registration fails.
    pub(crate) fn allocate_timer(&mut self) -> Result<TimerIds, IdAllocationError> {
        let timer = self.timers.preflight(IdAllocationError::Timer)?;

        self.timers.advance_after_preflight();

        Ok(TimerIds {
            generation: self.generation,
            timer_id: TimerId::new(timer),
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::model::ids::{ConnectionGeneration, DeliveryId, ReplyCapabilityId};

    use super::{IdAllocationError, IdSequences};

    /// Returns the deterministic TCP generation used by allocator tests.
    fn generation() -> ConnectionGeneration {
        ConnectionGeneration::new(7)
    }

    /// Confirms every production cursor starts at one and advances independently.
    #[test]
    fn sequences_start_at_one_and_advance_independently() {
        let mut ids = IdSequences::new(generation());
        assert_eq!(ids.generation(), generation());

        let (local_generation, local_operation) = ids
            .allocate_local_operation()
            .expect("first local operation ID")
            .into_parts();
        assert_eq!(local_generation, generation());
        assert_eq!(local_operation.get(), 1);

        let (w0_generation, w0_delivery) = ids
            .allocate_inbound_w0_primary()
            .expect("first W=0 publication ID")
            .into_parts();
        assert_eq!(w0_generation, generation());
        assert_eq!(w0_delivery.get(), 1);

        let (notice_generation, notice_delivery) = ids
            .allocate_protocol_notice()
            .expect("first protocol-notice publication ID")
            .into_parts();
        assert_eq!(notice_generation, generation());
        assert_eq!(notice_delivery.get(), 2);

        let (timer_generation, timer_id) =
            ids.allocate_timer().expect("first timer ID").into_parts();
        assert_eq!(timer_generation, generation());
        assert_eq!(timer_id.get(), 1);

        let (outbound_generation, outbound_operation, outbound_write) = ids
            .allocate_outbound_write()
            .expect("first outbound write bundle")
            .into_parts();
        assert_eq!(outbound_generation, generation());
        assert_eq!(outbound_operation.get(), 2);
        assert_eq!(outbound_write.get(), 1);

        let (w1_generation, w1_delivery, w1_reply): (
            ConnectionGeneration,
            DeliveryId,
            ReplyCapabilityId,
        ) = ids
            .allocate_inbound_w1_primary()
            .expect("first W=1 publication bundle")
            .into_parts();
        assert_eq!(w1_generation, generation());
        assert_eq!(w1_delivery.get(), 3);
        assert_eq!(w1_reply.get(), 1);
    }

    /// Confirms outbound and inbound multi-ID failures do not partially advance
    /// an earlier cursor whose preflight succeeded.
    #[test]
    fn bundle_exhaustion_is_atomic_across_all_required_cursors() {
        let mut outbound = IdSequences::with_next_values_for_test(
            generation(),
            Some(41),
            None,
            Some(1),
            Some(1),
            Some(1),
        );
        assert_eq!(
            outbound.allocate_outbound_write(),
            Err(IdAllocationError::Write)
        );
        let (_, local_operation) = outbound
            .allocate_local_operation()
            .expect("failed bundle must preserve Operation cursor")
            .into_parts();
        assert_eq!(local_operation.get(), 41);

        let mut inbound = IdSequences::with_next_values_for_test(
            generation(),
            Some(1),
            Some(1),
            Some(73),
            None,
            Some(1),
        );
        assert_eq!(
            inbound.allocate_inbound_w1_primary(),
            Err(IdAllocationError::ReplyCapability)
        );
        let (_, delivery_only) = inbound
            .allocate_inbound_w0_primary()
            .expect("failed W=1 bundle must preserve Delivery cursor")
            .into_parts();
        assert_eq!(delivery_only.get(), 73);
    }

    /// Confirms `u64::MAX` is issued exactly once by every sequence and every
    /// subsequent request returns a structured non-wrapping exhaustion error.
    #[test]
    fn final_representable_values_are_issued_once_then_exhausted() {
        let mut ids = IdSequences::with_next_values_for_test(
            generation(),
            Some(u64::MAX),
            Some(u64::MAX),
            Some(u64::MAX),
            Some(u64::MAX),
            Some(u64::MAX),
        );

        let (_, operation_id, write_id) = ids
            .allocate_outbound_write()
            .expect("final Operation and Write IDs")
            .into_parts();
        assert_eq!(operation_id.get(), u64::MAX);
        assert_eq!(write_id.get(), u64::MAX);

        let (_, delivery_id, reply_capability_id) = ids
            .allocate_inbound_w1_primary()
            .expect("final Delivery and ReplyCapability IDs")
            .into_parts();
        assert_eq!(delivery_id.get(), u64::MAX);
        assert_eq!(reply_capability_id.get(), u64::MAX);

        let (_, timer_id) = ids.allocate_timer().expect("final Timer ID").into_parts();
        assert_eq!(timer_id.get(), u64::MAX);

        assert_eq!(
            ids.allocate_local_operation(),
            Err(IdAllocationError::Operation)
        );
        assert_eq!(
            ids.allocate_outbound_write(),
            Err(IdAllocationError::Operation)
        );
        assert_eq!(
            ids.allocate_inbound_w0_primary(),
            Err(IdAllocationError::Delivery)
        );
        assert_eq!(
            ids.allocate_protocol_notice(),
            Err(IdAllocationError::Delivery)
        );
        assert_eq!(ids.allocate_timer(), Err(IdAllocationError::Timer));
    }

    /// Confirms the second cursor in each multi-ID bundle also issues its final
    /// representable value once, then fails without burning the first cursor.
    #[test]
    fn trailing_bundle_sequences_exhaust_without_partial_advancement() {
        let mut outbound = IdSequences::with_next_values_for_test(
            generation(),
            Some(10),
            Some(u64::MAX),
            Some(1),
            Some(1),
            Some(1),
        );
        let (_, final_operation_id, final_write_id) = outbound
            .allocate_outbound_write()
            .expect("final Write ID remains allocatable")
            .into_parts();
        assert_eq!(final_operation_id.get(), 10);
        assert_eq!(final_write_id.get(), u64::MAX);
        assert_eq!(
            outbound.allocate_outbound_write(),
            Err(IdAllocationError::Write)
        );
        let (_, preserved_operation_id) = outbound
            .allocate_local_operation()
            .expect("failed outbound bundle preserves Operation cursor")
            .into_parts();
        assert_eq!(
            preserved_operation_id.get(),
            11,
            "failed outbound bundle must not burn Operation ID"
        );

        let mut inbound = IdSequences::with_next_values_for_test(
            generation(),
            Some(1),
            Some(1),
            Some(20),
            Some(u64::MAX),
            Some(1),
        );
        let (_, final_delivery_id, final_reply_id) = inbound
            .allocate_inbound_w1_primary()
            .expect("final ReplyCapability ID remains allocatable")
            .into_parts();
        assert_eq!(final_delivery_id.get(), 20);
        assert_eq!(final_reply_id.get(), u64::MAX);
        assert_eq!(
            inbound.allocate_inbound_w1_primary(),
            Err(IdAllocationError::ReplyCapability)
        );
        let (_, preserved_delivery_id) = inbound
            .allocate_protocol_notice()
            .expect("failed W=1 bundle preserves Delivery cursor")
            .into_parts();
        assert_eq!(
            preserved_delivery_id.get(),
            21,
            "failed W=1 bundle must not burn Delivery ID"
        );
    }

    /// Guards ownership and linear-handoff boundaries against foreign ID
    /// allocation, boolean publication dispatch, and borrowed bundle getters.
    #[test]
    fn source_surface_preserves_owner_and_linear_boundaries() {
        let source = include_str!("ids.rs");
        let forbidden = [
            concat!("allocate_", "command"),
            concat!("next_", "command"),
            concat!("allocate_", "wire"),
            concat!("next_", "wire"),
            concat!("Command", "Id::new"),
            concat!("Wire", "Sequence::new"),
            concat!("allocate_inbound_", "publication"),
            concat!("fn operation_id", "(&self)"),
            concat!("fn write_id", "(&self)"),
            concat!("fn delivery_id", "(&self)"),
            concat!("fn reply_capability_id", "(&self)"),
            concat!("fn timer_id", "(&self)"),
        ];

        for pattern in forbidden {
            assert!(
                !source.contains(pattern),
                "forbidden IdSequences ownership surface was exposed: {pattern}"
            );
        }
    }
}
