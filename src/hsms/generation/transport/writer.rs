//! Runtime-neutral synchronous admission boundary for outbound HSMS frames.
//!
//! The generation Driver reserves Data capacity before entering the Core, then
//! transfers at most one Core-assigned Data frame with that permit. Control
//! frames use their independent lane directly. Both lanes receive positions
//! from the Writer's single generation-local wire order; actual I/O and later
//! asynchronous write outcomes remain outside this seam.

#![allow(dead_code)]

use crate::hsms::{
    model::ids::{WireSequence, WriteId},
    protocol::message::ProtocolMessage,
};

/// One complete semantic frame offered to the generation Writer.
#[derive(Debug, PartialEq)]
pub(crate) struct OutboundFrame {
    /// Core-assigned identity used to correlate the later write outcome.
    write_id: WriteId,
    /// Complete semantic message whose variant determines the Writer lane.
    message: ProtocolMessage,
}

impl OutboundFrame {
    /// Creates an outbound frame from its Core-assigned identity and message.
    pub(crate) const fn new(write_id: WriteId, message: ProtocolMessage) -> Self {
        Self { write_id, message }
    }

    /// Returns the Core-assigned write identity for outcome correlation.
    pub(crate) const fn write_id(&self) -> WriteId {
        self.write_id
    }

    /// Borrows the complete semantic message without duplicating ownership.
    pub(crate) const fn message(&self) -> &ProtocolMessage {
        &self.message
    }

    /// Splits the frame into its write identity and owned semantic message.
    pub(crate) fn into_parts(self) -> (WriteId, ProtocolMessage) {
        (self.write_id, self.message)
    }
}

/// Immediate reason a Control frame was not admitted by the Writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteAdmissionError {
    /// The independently reserved Control lane has no available capacity.
    Full,
    /// The Writer ingress is closed and cannot accept additional frames.
    Closed,
    /// The caller violated the Control-only admission contract.
    Invariant,
}

/// Immediate reason the Writer could not reserve one Data-lane slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataReserveError {
    /// The Data lane has no capacity available for another reservation.
    Full,
    /// The Writer ingress is closed and cannot create a reservation.
    Closed,
}

/// Reason an unused Data permit could not be released safely.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataPermitError {
    /// The permit is unknown, belongs to another Writer, or was already retired.
    Invariant,
}

/// Reason a reserved Data frame was not admitted by the Writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReservedDataAdmissionError {
    /// The Writer closed after the Data permit was reserved.
    Closed,
    /// The permit or frame violated the reserved Data admission contract.
    Invariant,
}

/// Synchronous ownership-transfer boundary implemented by a generation Writer.
pub(crate) trait WriterIngress {
    /// Implementation-owned, non-duplicable proof of one reserved Data slot.
    ///
    /// A permit remains local to one synchronous Driver-to-Core action batch
    /// and must be consumed by [`Self::admit_reserved_data`] or retired by
    /// [`Self::release_data`] exactly once.
    type DataPermit;

    /// Attempts to reserve one Data-lane slot before the Driver enters the Core.
    ///
    /// Returns an opaque permit on success, [`DataReserveError::Full`] when the
    /// Data lane is saturated, or [`DataReserveError::Closed`] when no further
    /// admission is possible. This operation never consumes Control capacity.
    fn try_reserve_data(&mut self) -> Result<Self::DataPermit, DataReserveError>;

    /// Retires an unused `permit` and returns its Data capacity exactly once.
    ///
    /// Returns [`DataPermitError::Invariant`] if the permit is unknown, belongs
    /// to a different Writer, or has already been consumed or released.
    fn release_data(&mut self, permit: Self::DataPermit) -> Result<(), DataPermitError>;

    /// Transfers one Data `frame` using a previously reserved Data `permit`.
    ///
    /// On success, the returned [`WireSequence`] is allocated from the same
    /// total order as Control admission, the Writer owns the frame, and the
    /// permit is consumed. A close after reservation returns
    /// [`ReservedDataAdmissionError::Closed`]. An invalid permit, a non-Data
    /// frame, or any impossible post-reservation state returns
    /// [`ReservedDataAdmissionError::Invariant`]. Every return retires a valid
    /// permit; no later reuse is allowed.
    fn admit_reserved_data(
        &mut self,
        permit: Self::DataPermit,
        frame: OutboundFrame,
    ) -> Result<WireSequence, ReservedDataAdmissionError>;

    /// Attempts to transfer one Control `frame` and assign total wire order.
    ///
    /// On success, the returned [`WireSequence`] is allocated before this call
    /// returns and cannot be overtaken by a frame admitted later. The Writer
    /// owns the frame and must eventually report exactly one write outcome.
    /// On error, ownership is rejected: no sequence or later outcome may be
    /// allocated. Passing a Data frame is a contract violation reported as
    /// [`WriteAdmissionError::Invariant`]; Data must use a reserved permit.
    fn try_admit(&mut self, frame: OutboundFrame) -> Result<WireSequence, WriteAdmissionError>;
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, rc::Rc};

    use crate::hsms::{
        model::ids::{Function, SessionId, Stream, SystemBytes, WireSequence, WriteId},
        protocol::{
            header::{ControlMessage, DataHeader},
            message::{DataMessage, ProtocolMessage},
        },
    };

    use super::{
        DataPermitError, DataReserveError, OutboundFrame, ReservedDataAdmissionError,
        WriteAdmissionError, WriterIngress,
    };

    /// One frame retained by the fake after successful synchronous admission.
    #[derive(Debug, PartialEq)]
    struct AdmittedFrame {
        /// Total generation-local position assigned by the fake Writer.
        sequence: WireSequence,
        /// Owned frame retained until a test injects its terminal outcome.
        frame: OutboundFrame,
    }

    /// Opaque proof that one slot belongs to one fake Writer reservation.
    #[derive(Debug)]
    struct FakeDataPermit {
        /// Unforgeable, instance-local identity of the fake Writer owner.
        owner: Rc<()>,
        /// Monotonic identity of the reservation within its Writer.
        reservation_id: u64,
    }

    impl FakeDataPermit {
        /// Forges the same identity solely for duplicate-use invariant tests.
        fn duplicate_for_invariant_test(&self) -> Self {
            Self {
                owner: Rc::clone(&self.owner),
                reservation_id: self.reservation_id,
            }
        }

        /// Forges an identity owned by another Writer for unknown-permit tests.
        fn foreign_for_invariant_test(reservation_id: u64) -> Self {
            Self {
                owner: Rc::new(()),
                reservation_id,
            }
        }
    }

    /// Lane-aware Writer fake used to verify the B2 admission boundary.
    #[derive(Debug)]
    struct FakeWriterIngress {
        /// Unforgeable identity used to reject permits from every other instance.
        owner: Rc<()>,
        /// Next numeric wire sequence, or `None` after sequence exhaustion.
        next_sequence: Option<u64>,
        /// Next numeric reservation identity to allocate.
        next_reservation_id: u64,
        /// Remaining slots in the independently reserved Control lane.
        available_control: usize,
        /// Remaining unreserved slots in the Data lane.
        available_data: usize,
        /// Data reservation identities that remain live and unretired.
        active_data_reservations: HashSet<u64>,
        /// Frames owned by the fake in their assigned total wire order.
        admitted: Vec<AdmittedFrame>,
        /// Number of admitted frames routed through the Control capacity lane.
        control_admissions: usize,
        /// Number of admitted frames routed through the Data capacity lane.
        data_admissions: usize,
        /// Whether this ingress rejects all new reservations and admissions.
        closed: bool,
        /// Write identities for which the fake emitted a terminal outcome.
        outcomes: Vec<WriteId>,
    }

    impl FakeWriterIngress {
        /// Creates an open fake with independent lane capacities and wire order.
        fn with_capacities(
            start_sequence: u64,
            control_capacity: usize,
            data_capacity: usize,
        ) -> Self {
            Self {
                owner: Rc::new(()),
                next_sequence: Some(start_sequence),
                next_reservation_id: 0,
                available_control: control_capacity,
                available_data: data_capacity,
                active_data_reservations: HashSet::new(),
                admitted: Vec::new(),
                control_admissions: 0,
                data_admissions: 0,
                closed: false,
                outcomes: Vec::new(),
            }
        }

        /// Closes the fake while retaining any already-issued Data permits.
        fn close(&mut self) {
            self.closed = true;
        }

        /// Allocates the next sequence without wrapping at the numeric boundary.
        fn allocate_sequence(&mut self) -> Option<WireSequence> {
            let raw = self.next_sequence?;
            self.next_sequence = raw.checked_add(1);
            Some(WireSequence::new(raw))
        }

        /// Removes a live permit after validating its Writer and reservation.
        fn take_data_reservation(
            &mut self,
            permit: &FakeDataPermit,
        ) -> Result<(), DataPermitError> {
            if !Rc::ptr_eq(&permit.owner, &self.owner)
                || !self.active_data_reservations.remove(&permit.reservation_id)
            {
                return Err(DataPermitError::Invariant);
            }
            Ok(())
        }

        /// Returns one retired reservation to the Data lane without overflow.
        fn restore_data_capacity(&mut self) -> Result<(), DataPermitError> {
            self.available_data = self
                .available_data
                .checked_add(1)
                .ok_or(DataPermitError::Invariant)?;
            Ok(())
        }

        /// Records the unique terminal outcome that a successful write may emit.
        fn emit_outcome(&mut self, write_id: WriteId) {
            assert!(
                self.admitted
                    .iter()
                    .any(|admitted| admitted.frame.write_id() == write_id),
                "only admitted frames may produce a write outcome"
            );
            assert!(
                !self.outcomes.contains(&write_id),
                "an admitted frame may produce only one write outcome"
            );
            self.outcomes.push(write_id);
        }
    }

    impl WriterIngress for FakeWriterIngress {
        type DataPermit = FakeDataPermit;

        /// Reserves one Data slot without affecting Control capacity.
        fn try_reserve_data(&mut self) -> Result<Self::DataPermit, DataReserveError> {
            if self.closed {
                return Err(DataReserveError::Closed);
            }
            if self.available_data == 0 {
                return Err(DataReserveError::Full);
            }

            let reservation_id = self.next_reservation_id;
            self.next_reservation_id = self
                .next_reservation_id
                .checked_add(1)
                .expect("test reservation identity must not overflow");
            self.available_data -= 1;
            assert!(self.active_data_reservations.insert(reservation_id));
            Ok(FakeDataPermit {
                owner: Rc::clone(&self.owner),
                reservation_id,
            })
        }

        /// Releases one live permit and restores its Data slot exactly once.
        fn release_data(&mut self, permit: Self::DataPermit) -> Result<(), DataPermitError> {
            self.take_data_reservation(&permit)?;
            self.restore_data_capacity()
        }

        /// Admits one Data frame by consuming a live permit exactly once.
        fn admit_reserved_data(
            &mut self,
            permit: Self::DataPermit,
            frame: OutboundFrame,
        ) -> Result<WireSequence, ReservedDataAdmissionError> {
            self.take_data_reservation(&permit)
                .map_err(|_| ReservedDataAdmissionError::Invariant)?;

            if !matches!(frame.message(), ProtocolMessage::Data(_)) {
                self.restore_data_capacity()
                    .map_err(|_| ReservedDataAdmissionError::Invariant)?;
                return Err(ReservedDataAdmissionError::Invariant);
            }
            if self.closed {
                self.restore_data_capacity()
                    .map_err(|_| ReservedDataAdmissionError::Invariant)?;
                return Err(ReservedDataAdmissionError::Closed);
            }

            let Some(sequence) = self.allocate_sequence() else {
                self.restore_data_capacity()
                    .map_err(|_| ReservedDataAdmissionError::Invariant)?;
                return Err(ReservedDataAdmissionError::Invariant);
            };
            self.data_admissions += 1;
            self.admitted.push(AdmittedFrame { sequence, frame });
            Ok(sequence)
        }

        /// Admits only Control frames through the independent Control lane.
        fn try_admit(&mut self, frame: OutboundFrame) -> Result<WireSequence, WriteAdmissionError> {
            if !matches!(frame.message(), ProtocolMessage::Control(_)) {
                return Err(WriteAdmissionError::Invariant);
            }
            if self.closed {
                return Err(WriteAdmissionError::Closed);
            }
            if self.available_control == 0 {
                return Err(WriteAdmissionError::Full);
            }

            let sequence = self
                .allocate_sequence()
                .ok_or(WriteAdmissionError::Invariant)?;
            self.available_control -= 1;
            self.control_admissions += 1;
            self.admitted.push(AdmittedFrame { sequence, frame });
            Ok(sequence)
        }
    }

    /// Builds one Control frame whose fields make ownership checks unambiguous.
    fn control_frame(write_id: u64, system_bytes: u32) -> OutboundFrame {
        OutboundFrame::new(
            WriteId::new(write_id),
            ProtocolMessage::Control(ControlMessage::LinktestRequest {
                system_bytes: SystemBytes::new(system_bytes),
            }),
        )
    }

    /// Builds one body-less Data frame for direct variant-based lane checks.
    fn data_frame(write_id: u64, system_bytes: u32) -> OutboundFrame {
        let session_id = SessionId::new(3).expect("fixture Session ID is valid");
        let header = DataHeader::new(
            session_id,
            Stream::new(1).expect("fixture stream is valid"),
            Function::new(1),
            false,
            SystemBytes::new(system_bytes),
        );
        OutboundFrame::new(
            WriteId::new(write_id),
            ProtocolMessage::Data(DataMessage::new(header, None)),
        )
    }

    /// Confirms reserve reports Data-lane saturation and ingress closure.
    #[test]
    fn reserve_data_reports_full_and_closed_without_touching_control() {
        let mut writer = FakeWriterIngress::with_capacities(10, 1, 1);

        let permit = writer
            .try_reserve_data()
            .expect("the only Data slot should be reservable");
        assert!(matches!(
            writer.try_reserve_data(),
            Err(DataReserveError::Full)
        ));
        assert_eq!(writer.available_control, 1);

        writer.close();
        assert!(matches!(
            writer.try_reserve_data(),
            Err(DataReserveError::Closed)
        ));
        writer
            .release_data(permit)
            .expect("an outstanding permit remains releasable after close");
        assert_eq!(writer.available_data, 1);
    }

    /// Confirms a successful Data admission consumes its permit only once.
    #[test]
    fn reserved_data_admission_consumes_permit_exactly_once() {
        let mut writer = FakeWriterIngress::with_capacities(20, 1, 1);
        let permit = writer
            .try_reserve_data()
            .expect("Data slot should be reservable");
        let duplicate = permit.duplicate_for_invariant_test();

        assert_eq!(
            writer.admit_reserved_data(permit, data_frame(1, 11)),
            Ok(WireSequence::new(20))
        );
        assert_eq!(
            writer.admit_reserved_data(duplicate, data_frame(2, 12)),
            Err(ReservedDataAdmissionError::Invariant)
        );
        assert_eq!(writer.data_admissions, 1);
        assert_eq!(writer.admitted.len(), 1);
        assert_eq!(writer.available_data, 0);
    }

    /// Confirms releasing a Data permit restores capacity exactly once.
    #[test]
    fn release_data_retires_permit_exactly_once() {
        let mut writer = FakeWriterIngress::with_capacities(30, 1, 1);
        let permit = writer
            .try_reserve_data()
            .expect("Data slot should be reservable");
        let duplicate = permit.duplicate_for_invariant_test();

        assert_eq!(writer.release_data(permit), Ok(()));
        assert_eq!(writer.available_data, 1);
        assert_eq!(
            writer.release_data(duplicate),
            Err(DataPermitError::Invariant)
        );
        assert_eq!(writer.available_data, 1);
    }

    /// Confirms permits from another Writer cannot mutate local reservations.
    #[test]
    fn unknown_data_permit_is_an_invariant() {
        let mut writer = FakeWriterIngress::with_capacities(40, 1, 1);
        let permit = writer
            .try_reserve_data()
            .expect("Data slot should be reservable");
        let foreign_admission = FakeDataPermit::foreign_for_invariant_test(permit.reservation_id);
        let foreign_release = FakeDataPermit::foreign_for_invariant_test(permit.reservation_id);

        assert_eq!(
            writer.admit_reserved_data(foreign_admission, data_frame(14, 114)),
            Err(ReservedDataAdmissionError::Invariant)
        );
        assert_eq!(
            writer.release_data(foreign_release),
            Err(DataPermitError::Invariant)
        );
        assert_eq!(writer.available_data, 0);
        writer
            .release_data(permit)
            .expect("the original reservation must remain live");
        assert_eq!(writer.available_data, 1);
    }

    /// Confirms identical Writer configuration cannot make permits interchangeable.
    #[test]
    fn same_configuration_writers_reject_cross_instance_permits() {
        let mut first_writer = FakeWriterIngress::with_capacities(45, 1, 1);
        let mut second_writer = FakeWriterIngress::with_capacities(45, 1, 1);
        let first_permit = first_writer
            .try_reserve_data()
            .expect("first Writer Data slot should be reservable");
        let cross_release = first_permit.duplicate_for_invariant_test();
        let first_local_release = first_permit.duplicate_for_invariant_test();
        let second_permit = second_writer
            .try_reserve_data()
            .expect("second Writer Data slot should be reservable");

        assert_eq!(
            second_writer.admit_reserved_data(first_permit, data_frame(15, 115)),
            Err(ReservedDataAdmissionError::Invariant)
        );
        assert_eq!(
            second_writer.release_data(cross_release),
            Err(DataPermitError::Invariant)
        );
        assert_eq!(second_writer.available_data, 0);
        assert!(second_writer.admitted.is_empty());

        second_writer
            .release_data(second_permit)
            .expect("second Writer's own reservation must remain live");
        first_writer
            .release_data(first_local_release)
            .expect("first Writer's rejected reservation must remain live");
        assert_eq!(first_writer.available_data, 1);
        assert_eq!(second_writer.available_data, 1);
    }

    /// Confirms each admission path rejects the other protocol-message lane.
    #[test]
    fn control_and_reserved_data_paths_validate_their_lanes() {
        let mut writer = FakeWriterIngress::with_capacities(50, 1, 1);

        assert_eq!(
            writer.try_admit(data_frame(1, 21)),
            Err(WriteAdmissionError::Invariant)
        );
        let permit = writer
            .try_reserve_data()
            .expect("Data slot should be reservable");
        assert_eq!(
            writer.admit_reserved_data(permit, control_frame(2, 22)),
            Err(ReservedDataAdmissionError::Invariant)
        );
        assert_eq!(writer.available_control, 1);
        assert_eq!(writer.available_data, 1);
        assert!(writer.admitted.is_empty());
    }

    /// Confirms a reservation survives later Data saturation without `Full`.
    #[test]
    fn reserved_data_slot_survives_post_reservation_full_state() {
        let mut writer = FakeWriterIngress::with_capacities(60, 1, 1);
        let permit = writer
            .try_reserve_data()
            .expect("Data slot should be reservable");

        assert!(matches!(
            writer.try_reserve_data(),
            Err(DataReserveError::Full)
        ));
        assert_eq!(
            writer.admit_reserved_data(permit, data_frame(3, 23)),
            Ok(WireSequence::new(60))
        );
        assert_eq!(writer.data_admissions, 1);
    }

    /// Confirms impossible post-permit sequence exhaustion is an invariant.
    #[test]
    fn sequence_exhaustion_after_data_reservation_is_an_invariant() {
        let mut writer = FakeWriterIngress::with_capacities(u64::MAX, 1, 1);
        let permit = writer
            .try_reserve_data()
            .expect("Data slot should be reservable");

        assert_eq!(
            writer.try_admit(control_frame(31, 231)),
            Ok(WireSequence::new(u64::MAX))
        );
        assert_eq!(
            writer.admit_reserved_data(permit, data_frame(32, 232)),
            Err(ReservedDataAdmissionError::Invariant)
        );
        assert_eq!(writer.available_data, 1);
        assert_eq!(writer.data_admissions, 0);
    }

    /// Confirms closure after reservation retires the permit without admission.
    #[test]
    fn close_after_reservation_returns_closed_and_retires_permit() {
        let mut writer = FakeWriterIngress::with_capacities(70, 1, 1);
        let permit = writer
            .try_reserve_data()
            .expect("Data slot should be reservable");
        let duplicate = permit.duplicate_for_invariant_test();

        writer.close();
        assert_eq!(
            writer.admit_reserved_data(permit, data_frame(4, 24)),
            Err(ReservedDataAdmissionError::Closed)
        );
        assert_eq!(writer.available_data, 1);
        assert_eq!(
            writer.admit_reserved_data(duplicate, data_frame(5, 25)),
            Err(ReservedDataAdmissionError::Invariant)
        );
        assert!(writer.admitted.is_empty());
    }

    /// Confirms Data reservation cannot consume the reserved Control slot.
    #[test]
    fn data_reservation_does_not_consume_control_capacity() {
        let mut writer = FakeWriterIngress::with_capacities(80, 1, 1);
        let permit = writer
            .try_reserve_data()
            .expect("Data slot should be reservable");

        assert_eq!(
            writer.try_admit(control_frame(6, 26)),
            Ok(WireSequence::new(80))
        );
        assert_eq!(
            writer.admit_reserved_data(permit, data_frame(7, 27)),
            Ok(WireSequence::new(81))
        );
        assert_eq!(writer.control_admissions, 1);
        assert_eq!(writer.data_admissions, 1);
    }

    /// Confirms both lanes share one strictly increasing total wire sequence.
    #[test]
    fn control_and_data_admissions_share_one_total_wire_order() {
        let mut writer = FakeWriterIngress::with_capacities(91, 2, 2);
        let first_data_permit = writer
            .try_reserve_data()
            .expect("first Data slot should be reservable");
        let second_data_permit = writer
            .try_reserve_data()
            .expect("second Data slot should be reservable");

        let first = writer
            .admit_reserved_data(first_data_permit, data_frame(8, 28))
            .expect("reserved Data frame should be admitted");
        let second = writer
            .try_admit(control_frame(9, 29))
            .expect("Control frame should be admitted");
        let third = writer
            .admit_reserved_data(second_data_permit, data_frame(10, 30))
            .expect("reserved Data frame should be admitted");
        let fourth = writer
            .try_admit(control_frame(11, 31))
            .expect("Control frame should be admitted");

        assert_eq!(
            [first, second, third, fourth],
            [
                WireSequence::new(91),
                WireSequence::new(92),
                WireSequence::new(93),
                WireSequence::new(94),
            ]
        );
        assert_eq!(
            writer
                .admitted
                .iter()
                .map(|admitted| admitted.sequence)
                .collect::<Vec<_>>(),
            vec![first, second, third, fourth]
        );

        for write_id in [8, 9, 10, 11] {
            writer.emit_outcome(WriteId::new(write_id));
        }
        assert_eq!(writer.outcomes.len(), 4);
    }

    /// Confirms Control Full and Closed preserve the B1 admission behavior.
    #[test]
    fn control_admission_preserves_full_and_closed_behavior() {
        let mut full_writer = FakeWriterIngress::with_capacities(100, 0, 1);
        assert_eq!(
            full_writer.try_admit(control_frame(12, 32)),
            Err(WriteAdmissionError::Full)
        );
        assert_eq!(full_writer.next_sequence, Some(100));
        assert!(full_writer.admitted.is_empty());

        let mut closed_writer = FakeWriterIngress::with_capacities(110, 1, 1);
        closed_writer.close();
        assert_eq!(
            closed_writer.try_admit(control_frame(13, 33)),
            Err(WriteAdmissionError::Closed)
        );
        assert_eq!(closed_writer.next_sequence, Some(110));
        assert!(closed_writer.admitted.is_empty());
    }

    /// Confirms accessors preserve identity and owned-message round trips.
    #[test]
    fn outbound_frame_accessors_preserve_both_parts() {
        let frame = control_frame(17, 19);

        assert_eq!(frame.write_id(), WriteId::new(17));
        assert!(matches!(frame.message(), ProtocolMessage::Control(_)));
        let (write_id, message) = frame.into_parts();

        assert_eq!(write_id, WriteId::new(17));
        assert!(matches!(
            message,
            ProtocolMessage::Control(ControlMessage::LinktestRequest { system_bytes })
                if system_bytes == SystemBytes::new(19)
        ));
    }
}
