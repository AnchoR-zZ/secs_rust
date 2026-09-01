//! Runtime-neutral synchronous admission boundary for outbound HSMS frames.
//!
//! The generation Driver uses this seam to transfer one Core-assigned frame
//! to the single Writer and obtain its position in wire order immediately.
//! Actual I/O and asynchronous write outcomes remain the Writer runtime's
//! responsibility and are intentionally outside this B0 contract.

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

/// Immediate reason an outbound frame was not admitted by the Writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteAdmissionError {
    /// The lane selected from the message variant has no reserved capacity.
    Full,
    /// The Writer ingress is closed and cannot accept additional frames.
    Closed,
}

/// Synchronous ownership-transfer boundary implemented by a generation Writer.
pub(crate) trait WriterIngress {
    /// Attempts to transfer `frame` to the Writer and assign total wire order.
    ///
    /// On success, the returned [`WireSequence`] is allocated before this call
    /// returns and cannot be overtaken by a frame admitted later. The Writer
    /// owns the frame and must eventually report exactly one write outcome.
    /// On error, ownership is rejected: no sequence or later outcome may be
    /// allocated. Implementations select their capacity lane directly from
    /// [`ProtocolMessage::Control`] or [`ProtocolMessage::Data`].
    fn try_admit(&mut self, frame: OutboundFrame) -> Result<WireSequence, WriteAdmissionError>;
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use crate::hsms::{
        model::ids::{Function, SessionId, Stream, SystemBytes, WireSequence, WriteId},
        protocol::{
            header::{ControlMessage, DataHeader},
            message::{DataMessage, ProtocolMessage},
        },
    };

    use super::{OutboundFrame, WriteAdmissionError, WriterIngress};

    /// One frame retained by the fake after successful synchronous admission.
    #[derive(Debug, PartialEq)]
    struct AdmittedFrame {
        /// Total generation-local position assigned by the fake Writer.
        sequence: WireSequence,
        /// Owned frame retained until a test injects its terminal outcome.
        frame: OutboundFrame,
    }

    /// Minimal Writer fake used to verify the B0 admission boundary.
    #[derive(Debug, Default)]
    struct FakeWriterIngress {
        /// Next numeric wire sequence to allocate after successful admission.
        next_sequence: u64,
        /// Frames owned by the fake in their assigned wire order.
        admitted: Vec<AdmittedFrame>,
        /// Number of admitted frames routed through the Control capacity lane.
        control_admissions: usize,
        /// Number of admitted frames routed through the Data capacity lane.
        data_admissions: usize,
        /// Admission failures consumed before accepting another frame.
        failures: VecDeque<WriteAdmissionError>,
        /// Write identities for which the fake emitted a terminal outcome.
        outcomes: Vec<WriteId>,
    }

    impl FakeWriterIngress {
        /// Creates an empty fake whose first admitted frame receives `start`.
        fn starting_at(start: u64) -> Self {
            Self {
                next_sequence: start,
                ..Self::default()
            }
        }

        /// Queues one immediate failure for the next admission attempt.
        fn fail_next_with(&mut self, error: WriteAdmissionError) {
            self.failures.push_back(error);
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
        /// Admits one frame or rejects it without allocating any Writer state.
        fn try_admit(&mut self, frame: OutboundFrame) -> Result<WireSequence, WriteAdmissionError> {
            if let Some(error) = self.failures.pop_front() {
                return Err(error);
            }

            let sequence = WireSequence::new(self.next_sequence);
            self.next_sequence = self
                .next_sequence
                .checked_add(1)
                .expect("test wire sequence must not overflow");
            match frame.message() {
                ProtocolMessage::Control(_) => self.control_admissions += 1,
                ProtocolMessage::Data(_) => self.data_admissions += 1,
            }
            self.admitted.push(AdmittedFrame { sequence, frame });
            Ok(sequence)
        }
    }

    /// Builds one control frame whose fields make ownership checks unambiguous.
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

    /// Confirms success allocates wire order and transfers the complete frame.
    #[test]
    fn successful_admission_assigns_sequence_and_retains_frame() {
        let mut writer = FakeWriterIngress::starting_at(41);

        let sequence = writer
            .try_admit(control_frame(7, 11))
            .expect("fake Writer should accept the frame");
        let data_sequence = writer
            .try_admit(data_frame(8, 12))
            .expect("fake Writer should accept the Data frame");

        assert_eq!(sequence, WireSequence::new(41));
        assert_eq!(data_sequence, WireSequence::new(42));
        assert_eq!(writer.next_sequence, 43);
        assert_eq!(writer.admitted.len(), 2);
        assert_eq!(writer.control_admissions, 1);
        assert_eq!(writer.data_admissions, 1);
        assert_eq!(writer.admitted[0].sequence, sequence);
        assert_eq!(writer.admitted[0].frame.write_id(), WriteId::new(7));
        assert!(matches!(
            writer.admitted[0].frame.message(),
            ProtocolMessage::Control(ControlMessage::LinktestRequest { system_bytes })
                if *system_bytes == SystemBytes::new(11)
        ));

        writer.emit_outcome(WriteId::new(7));
        writer.emit_outcome(WriteId::new(8));
        assert_eq!(writer.outcomes, vec![WriteId::new(7), WriteId::new(8)]);
    }

    /// Confirms Full and Closed reject ownership without sequence or outcome.
    #[test]
    fn failed_admission_allocates_no_sequence_record_or_outcome() {
        let mut writer = FakeWriterIngress::starting_at(73);

        for error in [WriteAdmissionError::Full, WriteAdmissionError::Closed] {
            writer.fail_next_with(error);
            assert_eq!(writer.try_admit(control_frame(9, 13)), Err(error));
            assert_eq!(writer.next_sequence, 73);
            assert!(writer.admitted.is_empty());
            assert!(writer.outcomes.is_empty());
        }
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
