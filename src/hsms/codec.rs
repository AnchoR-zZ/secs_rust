//! Sans-I/O composition of HSMS framing, validation, and the SECS-II profile.
//!
//! `HsmsSsCodec` is the runtime-facing boundary for one TCP generation. It
//! keeps fatal framing separate from recoverable protocol violations and
//! encodes semantic Data messages into one exactly sized contiguous buffer.

#![allow(dead_code)]

use bytes::{Bytes, BytesMut};
use thiserror::Error;

use crate::{
    hsms::{
        profile::secs2::{Secs2Profile, StrictSecs2Profile},
        protocol::{
            header::DataHeader,
            message::{DataMessage, ProtocolMessage},
            violation::{
                HeaderViolation, InboundViolation, PayloadViolation, PayloadViolationKind,
            },
        },
        wire::{
            codec::{FrameDecodeStep, FrameReadProgress, HsmsFrameDecoder, HsmsWireEncoder},
            frame::ValidatedFrame,
            validation::{FrameEncodeError, FramingFault},
            validator::StrictFrameValidator,
        },
        EndpointLimits,
    },
    secs2::codec::{DecodeError, EncodeError, Secs2Decoder},
};

/// Non-fatal result of one inbound composition-codec step.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HsmsSsDecodeStep {
    /// The current TCP buffer does not yet contain a complete frame.
    NeedMore(FrameReadProgress),
    /// A complete semantic Data or control message is ready for Core.
    Message(ProtocolMessage),
    /// A recoverable HSMS header violation is ready for Core classification.
    HeaderViolation {
        /// Stable violation contract containing the exact raw header.
        violation: HeaderViolation,
    },
    /// A recoverable Data Message Text violation is ready for Core while the
    /// concrete codec source remains available to SessionDriver diagnostics.
    PayloadViolation {
        /// Stable protocol category passed to the pure Core.
        violation: PayloadViolation,
        /// Complete SECS-II decoder failure retained outside Core.
        source: DecodeError,
    },
}

impl HsmsSsDecodeStep {
    /// Converts either recoverable failure variant to the Core contract.
    ///
    /// Returns `None` for partial input and valid messages. Detailed SECS-II
    /// diagnostic data intentionally remains in the composition step.
    pub(crate) const fn inbound_violation(&self) -> Option<InboundViolation> {
        match self {
            Self::HeaderViolation { violation } => Some(InboundViolation::Header(*violation)),
            Self::PayloadViolation { violation, .. } => Some(InboundViolation::Payload(*violation)),
            Self::NeedMore(_) | Self::Message(_) => None,
        }
    }
}

/// Outbound semantic-message encoding failure.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum HsmsSsEncodeError {
    /// The optional SECS-II body cannot be represented by the E5 codec.
    #[error("failed to encode SECS-II Message Text for Data header {header:?}: {source}")]
    Payload {
        /// Data header whose semantic body failed to encode.
        header: DataHeader,
        /// Complete structured SECS-II encoding failure.
        source: EncodeError,
    },
    /// The measured Message Text cannot fit the configured HSMS frame bound.
    #[error("failed to frame Data header {header:?}: {source}")]
    Frame {
        /// Data header whose final frame could not be represented.
        header: DataHeader,
        /// Structured Wire size or arithmetic failure.
        source: FrameEncodeError,
    },
}

/// Generation-scoped HSMS-SS codec composed from pure lower-layer algorithms.
#[derive(Debug)]
pub(crate) struct HsmsSsCodec {
    /// Incremental length framer whose terminal state belongs to one TCP
    /// generation.
    frame_decoder: HsmsFrameDecoder,
    /// Stateless strict HSMS-SS header classifier.
    frame_validator: StrictFrameValidator,
    /// PType=0 absent-text and strict SECS-II mapping.
    profile: StrictSecs2Profile,
    /// Checked semantic-header encoder sharing the endpoint frame bound.
    wire_encoder: HsmsWireEncoder,
}

impl HsmsSsCodec {
    /// Creates one codec for a TCP generation from frame and SECS-II limits.
    pub(crate) fn new(limits: EndpointLimits, secs2_decoder: Secs2Decoder) -> Self {
        Self {
            frame_decoder: HsmsFrameDecoder::new(limits),
            frame_validator: StrictFrameValidator::new(),
            profile: StrictSecs2Profile::new(secs2_decoder),
            wire_encoder: HsmsWireEncoder::new(limits),
        }
    }

    /// Consumes at most one inbound frame from `input`.
    ///
    /// Fatal Message Length faults are returned as `Err` and permanently
    /// terminate this codec instance. Partial input, valid messages, and
    /// recoverable header/payload violations are returned as explicit steps.
    pub(crate) fn decode(
        &mut self,
        input: &mut BytesMut,
    ) -> Result<HsmsSsDecodeStep, FramingFault> {
        let raw = match self.frame_decoder.decode(input)? {
            FrameDecodeStep::NeedMore(progress) => {
                return Ok(HsmsSsDecodeStep::NeedMore(progress));
            }
            FrameDecodeStep::Frame(raw) => raw,
        };

        let validated = match self.frame_validator.validate(raw) {
            Ok(frame) => frame,
            Err(violation) => {
                return Ok(HsmsSsDecodeStep::HeaderViolation { violation });
            }
        };

        match validated {
            ValidatedFrame::Control(message) => {
                Ok(HsmsSsDecodeStep::Message(ProtocolMessage::Control(message)))
            }
            ValidatedFrame::Data(frame) => match self.profile.decode_text(&frame.text) {
                Ok(body) => Ok(HsmsSsDecodeStep::Message(ProtocolMessage::Data(
                    DataMessage::new(frame.header, body),
                ))),
                Err(source) => {
                    let kind = classify_payload_error(&source);
                    Ok(HsmsSsDecodeStep::PayloadViolation {
                        violation: PayloadViolation::new(frame.header, kind),
                        source,
                    })
                }
            },
        }
    }

    /// Encodes one semantic message into one exactly sized contiguous buffer.
    ///
    /// Control messages write their unique typed header directly. Data
    /// messages measure SECS-II once, validate the complete HSMS length,
    /// reserve the final frame once, and append Message Text into that buffer.
    ///
    /// # Errors
    ///
    /// Returns [`HsmsSsEncodeError::Payload`] for SECS-II representation
    /// failures or [`HsmsSsEncodeError::Frame`] when the measured final frame
    /// exceeds the configured HSMS bound.
    pub(crate) fn encode(&self, message: &ProtocolMessage) -> Result<Bytes, HsmsSsEncodeError> {
        match message {
            ProtocolMessage::Control(message) => Ok(self.wire_encoder.encode_control(*message)),
            ProtocolMessage::Data(message) => self.encode_data(message),
        }
    }

    /// Encodes one semantic Data message using a single final allocation.
    fn encode_data(&self, message: &DataMessage) -> Result<Bytes, HsmsSsEncodeError> {
        let header = message.header();
        let body_plan = self
            .profile
            .prepare_body(message.body())
            .map_err(|source| HsmsSsEncodeError::Payload { header, source })?;
        let frame_plan = self
            .wire_encoder
            .plan_data(body_plan.encoded_length())
            .map_err(|source| HsmsSsEncodeError::Frame { header, source })?;

        let mut output = Vec::with_capacity(frame_plan.encoded_length());
        frame_plan.write_prefix_and_header(&mut output, header);
        body_plan
            .write_into(&mut output)
            .map_err(|source| HsmsSsEncodeError::Payload { header, source })?;
        debug_assert_eq!(
            output.len(),
            frame_plan.encoded_length(),
            "composed HSMS frame must match its checked allocation plan"
        );
        Ok(Bytes::from(output))
    }
}

/// Maps concrete strict-decoder errors to the stable Core-facing category.
fn classify_payload_error(error: &DecodeError) -> PayloadViolationKind {
    if matches!(
        error,
        DecodeError::DepthExceeded { .. }
            | DecodeError::TotalItemsExceeded { .. }
            | DecodeError::ItemBytesExceeded { .. }
            | DecodeError::ListItemsExceeded { .. }
            | DecodeError::ArithmeticOverflow { .. }
    ) {
        PayloadViolationKind::ResourceLimitExceeded
    } else {
        PayloadViolationKind::MalformedSecs2
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end Sans-I/O tests for Wire + validation + SECS-II composition.

    use crate::{
        hsms::{
            model::ids::SystemBytes,
            protocol::{
                header::{ControlMessage, RejectReason},
                violation::HeaderViolationKind,
            },
            Function, SessionId, Stream,
        },
        secs2::{AsciiString, DecodeLimits, SecsItem},
    };

    use super::*;

    /// Independently transcribed HSMS Data S1F1 frame carrying ASCII `HELLO`.
    const FIXED_HSMS_DATA_S1F1_ASCII_HELLO: &[u8] = &[
        0x00, 0x00, 0x00, 0x11, // Message Length: 10-byte header + 7-byte text.
        0x00, 0x00, // Session ID 0.
        0x01, // W=0, Stream=1.
        0x01, // Function=1.
        0x00, // PType=0.
        0x00, // SType=Data.
        0x00, 0x00, 0x00, 0x01, // System Bytes 1.
        0x41, 0x05, b'H', b'E', b'L', b'L', b'O', // ASCII item.
    ];

    /// Independently transcribed header-only E37 `Reject.req` frame.
    const FIXED_HSMS_REJECT_REQUEST: &[u8] = &[
        0x00, 0x00, 0x00, 0x0A, // Message Length: header only.
        0x00, 0x00, // Rejected message Session ID.
        0x00, // Rejected SType=Data.
        0x04, // Reason: Entity Not Selected.
        0x00, // PType=0.
        0x07, // SType=Reject.req.
        0x00, 0x00, 0x00, 0x11, // Associated System Bytes.
    ];

    /// Builds valid endpoint limits with a custom Message Length maximum.
    fn limits_with_max(maximum: usize) -> EndpointLimits {
        let defaults = EndpointLimits::default();
        EndpointLimits::new(
            maximum,
            defaults.command_capacity(),
            defaults.critical_lane_capacity(),
            defaults.data_lane_capacity(),
            defaults.application_event_capacity(),
            defaults.transaction_capacity(),
            defaults.tombstone_capacity(),
        )
        .expect("valid test limits")
    }

    /// Builds a representative semantic Data header.
    fn data_header() -> DataHeader {
        DataHeader::new(
            SessionId::new(3).expect("session"),
            Stream::new(1).expect("stream"),
            Function::new(2),
            true,
            SystemBytes::new(0x0102_0304),
        )
    }

    /// Creates a default generation codec.
    fn codec() -> HsmsSsCodec {
        HsmsSsCodec::new(EndpointLimits::default(), Secs2Decoder::default())
    }

    /// Confirms a manually fixed S1F1 ASCII frame decodes to the expected
    /// semantics and re-encodes byte-for-byte without encoder-built input.
    #[test]
    fn fixed_s1f1_ascii_hello_decodes_and_reencodes_exactly() {
        let expected = ProtocolMessage::Data(DataMessage::new(
            DataHeader::new(
                SessionId::new(0).expect("session"),
                Stream::new(1).expect("stream"),
                Function::new(1),
                false,
                SystemBytes::new(1),
            ),
            Some(SecsItem::Ascii(
                AsciiString::new("HELLO").expect("fixed ASCII"),
            )),
        ));
        let mut decoder = codec();
        let mut input = BytesMut::from(FIXED_HSMS_DATA_S1F1_ASCII_HELLO);

        assert_eq!(
            decoder.decode(&mut input).expect("fixed frame decodes"),
            HsmsSsDecodeStep::Message(expected.clone())
        );
        assert!(input.is_empty());

        let encoded = codec().encode(&expected).expect("semantic re-encode");
        assert_eq!(encoded.as_ref(), FIXED_HSMS_DATA_S1F1_ASCII_HELLO);
    }

    /// Confirms a manually fixed `Reject.req` frame becomes the exact typed
    /// control value and returns to its original bytes.
    #[test]
    fn fixed_reject_request_decodes_and_reencodes_exactly() {
        let expected = ProtocolMessage::Control(ControlMessage::RejectRequest {
            session_id: 0,
            rejected_type: 0,
            reason: RejectReason::new(4).expect("non-zero fixed reason"),
            system_bytes: SystemBytes::new(0x11),
        });
        let mut decoder = codec();
        let mut input = BytesMut::from(FIXED_HSMS_REJECT_REQUEST);

        assert_eq!(
            decoder.decode(&mut input).expect("fixed frame decodes"),
            HsmsSsDecodeStep::Message(expected.clone())
        );
        assert!(input.is_empty());

        let encoded = codec().encode(&expected).expect("semantic re-encode");
        assert_eq!(encoded.as_ref(), FIXED_HSMS_REJECT_REQUEST);
    }

    /// Confirms a semantic Data message round-trips through one final buffer.
    #[test]
    fn data_round_trip_preserves_header_and_secs2_body() {
        let expected = ProtocolMessage::Data(DataMessage::new(
            data_header(),
            Some(SecsItem::U1(vec![1, 2, 3])),
        ));
        let encoded = codec().encode(&expected).expect("encode");
        assert_eq!(encoded.len(), 19);
        assert_eq!(&encoded[..4], &15_u32.to_be_bytes());

        let mut input = BytesMut::from(encoded.as_ref());
        assert_eq!(
            codec().decode(&mut input).expect("decode"),
            HsmsSsDecodeStep::Message(expected)
        );
        assert!(input.is_empty());
    }

    /// Confirms absent Message Text round-trips distinctly from typed-empty.
    #[test]
    fn absent_text_round_trips_as_none() {
        let expected = ProtocolMessage::Data(DataMessage::new(data_header(), None));
        let encoded = codec().encode(&expected).expect("encode");
        assert_eq!(encoded.len(), 14);
        let mut input = BytesMut::from(encoded.as_ref());
        assert_eq!(
            codec().decode(&mut input).expect("decode"),
            HsmsSsDecodeStep::Message(expected)
        );
    }

    /// Confirms typed controls bypass SECS-II and remain coherent.
    #[test]
    fn control_round_trip_uses_typed_header() {
        let expected = ProtocolMessage::Control(ControlMessage::LinktestRequest {
            system_bytes: SystemBytes::new(7),
        });
        let encoded = codec().encode(&expected).expect("encode");
        let mut input = BytesMut::from(encoded.as_ref());
        assert_eq!(
            codec().decode(&mut input).expect("decode"),
            HsmsSsDecodeStep::Message(expected)
        );
    }

    /// Confirms malformed Message Text retains both stable and detailed forms.
    #[test]
    fn malformed_payload_retains_structured_source_for_runtime() {
        let mut bytes = vec![0, 0, 0, 12, 0, 3, 0x81, 2, 0, 0, 0, 0, 0, 1];
        bytes.extend_from_slice(&[0x21, 3]);
        let mut input = BytesMut::from(bytes.as_slice());
        let step = codec().decode(&mut input).expect("recoverable");
        let HsmsSsDecodeStep::PayloadViolation {
            violation,
            ref source,
        } = step
        else {
            panic!("expected payload violation");
        };
        assert_eq!(violation.kind(), PayloadViolationKind::MalformedSecs2);
        assert!(matches!(source, DecodeError::TruncatedBody { .. }));
    }

    /// Confirms resource-limit failures receive a stable distinct category.
    #[test]
    fn payload_resource_limit_is_distinct_from_malformed_text() {
        let limits = DecodeLimits::new(4, 8, 2, 4).expect("limits");
        let mut codec = HsmsSsCodec::new(EndpointLimits::default(), Secs2Decoder::new(limits));
        let mut input = BytesMut::from(
            &[
                0, 0, 0, 15, 0, 3, 0x81, 2, 0, 0, 0, 0, 0, 1, 0x21, 3, 1, 2, 3,
            ][..],
        );
        let step = codec.decode(&mut input).expect("recoverable");
        let HsmsSsDecodeStep::PayloadViolation {
            violation,
            ref source,
        } = step
        else {
            panic!("expected payload violation");
        };
        assert_eq!(
            violation.kind(),
            PayloadViolationKind::ResourceLimitExceeded
        );
        assert!(matches!(source, DecodeError::ItemBytesExceeded { .. }));
        assert_eq!(
            step.inbound_violation(),
            Some(InboundViolation::Payload(violation))
        );
    }

    /// Confirms header violations remain independent of profile errors.
    #[test]
    fn control_text_produces_header_violation_with_snapshot() {
        let mut input =
            BytesMut::from(&[0, 0, 0, 11, 0xFF, 0xFF, 0, 0, 0, 5, 0, 0, 0, 7, 0xAA][..]);
        let step = codec().decode(&mut input).expect("recoverable");
        let HsmsSsDecodeStep::HeaderViolation { violation } = step else {
            panic!("expected header violation");
        };
        assert_eq!(violation.kind(), HeaderViolationKind::ControlMessageHasText);
        assert_eq!(violation.header().system_bytes(), 7);
    }

    /// Confirms a fatal framing fault is not converted to a Core violation.
    #[test]
    fn aggressive_length_remains_a_fatal_wire_result() {
        let mut codec = codec();
        let mut input = BytesMut::from(&9_u32.to_be_bytes()[..]);
        assert_eq!(
            codec.decode(&mut input),
            Err(FramingFault::MessageLengthBelowHeader {
                declared_length: 9,
                header_length: 10,
            })
        );
    }

    /// Confirms the full HSMS bound is checked after exact SECS-II measuring.
    #[test]
    fn measured_payload_is_rejected_before_final_allocation() {
        let codec = HsmsSsCodec::new(limits_with_max(10), Secs2Decoder::default());
        let message = ProtocolMessage::Data(DataMessage::new(
            data_header(),
            Some(SecsItem::List(Vec::new())),
        ));
        assert_eq!(
            codec.encode(&message),
            Err(HsmsSsEncodeError::Frame {
                header: data_header(),
                source: FrameEncodeError::MessageLengthAboveLimit {
                    message_length: 12,
                    maximum_length: 10,
                },
            })
        );
    }
}
