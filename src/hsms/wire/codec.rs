//! Bounded incremental framing and low-level HSMS header encoding.
//!
//! The decoder is Sans-I/O and generation-scoped: it consumes caller-owned
//! bytes, reports partial progress for T8 management, and becomes terminal
//! after an aggressive Message Length. The encoder produces checked plans so
//! a higher composition layer can allocate one final contiguous frame.

use bytes::{Buf, Bytes, BytesMut};

use crate::hsms::{
    protocol::header::{ControlMessage, DataHeader},
    wire::{
        frame::{RawFrame, RawHeader, HSMS_HEADER_LENGTH},
        validation::{FrameEncodeError, FramingFault},
    },
    EndpointLimits,
};

/// Number of bytes in the big-endian E37 Message Length prefix.
const LENGTH_PREFIX_BYTES: usize = 4;

/// Outcome of one non-fatal incremental framing attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FrameDecodeStep {
    /// Exactly one complete frame was consumed and is ready for validation.
    Frame(RawFrame),
    /// More bytes are needed; the buffer remains unchanged.
    NeedMore(FrameReadProgress),
}

/// Partial-frame progress used by SessionDriver to manage E37 T8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameReadProgress {
    /// Fewer than four Message Length prefix bytes are buffered.
    AwaitingLength {
        /// Number of available prefix bytes in the range `0..=3`.
        bytes_seen: u8,
    },
    /// A legal target length is known but its complete body is not buffered.
    PartialBody {
        /// Validated header-plus-text Message Length.
        target_length: usize,
        /// Body bytes currently buffered after the four-byte prefix.
        bytes_seen: usize,
    },
}

/// Incremental length decoder owned by exactly one TCP generation.
#[derive(Debug)]
pub(crate) struct HsmsFrameDecoder {
    /// Largest accepted header-plus-text E37 Message Length.
    max_message_length: usize,
    /// Exact fatal fault already observed for this generation, if any.
    terminal_fault: Option<FramingFault>,
}

impl HsmsFrameDecoder {
    /// Creates a fresh generation-scoped decoder using `limits`.
    pub(crate) fn new(limits: EndpointLimits) -> Self {
        Self {
            max_message_length: limits.max_message_length(),
            terminal_fault: None,
        }
    }

    /// Attempts to consume one complete frame from `input`.
    ///
    /// Returns partial progress without consuming bytes, a complete raw frame,
    /// or a fatal length fault. Once a fault occurs, subsequent calls return
    /// that exact same fault and never attempt to resynchronize.
    pub(crate) fn decode(&mut self, input: &mut BytesMut) -> Result<FrameDecodeStep, FramingFault> {
        if let Some(fault) = self.terminal_fault {
            return Err(fault);
        }

        if input.len() < LENGTH_PREFIX_BYTES {
            return Ok(FrameDecodeStep::NeedMore(
                FrameReadProgress::AwaitingLength {
                    bytes_seen: input.len() as u8,
                },
            ));
        }

        let declared_u32 = u32::from_be_bytes([input[0], input[1], input[2], input[3]]);
        if declared_u32 < HSMS_HEADER_LENGTH as u32 {
            return Err(self.terminate(FramingFault::MessageLengthBelowHeader {
                declared_length: declared_u32,
                header_length: HSMS_HEADER_LENGTH,
            }));
        }
        if u64::from(declared_u32) > self.max_message_length as u64 {
            return Err(self.terminate(FramingFault::MessageLengthAboveLimit {
                declared_length: declared_u32,
                maximum_length: self.max_message_length,
            }));
        }
        // This cast is safe because the comparison above proved the peer's
        // u32 value is no larger than a bound already representable in usize.
        let declared_length = declared_u32 as usize;

        let available_body = input.len() - LENGTH_PREFIX_BYTES;
        if available_body < declared_length {
            return Ok(FrameDecodeStep::NeedMore(FrameReadProgress::PartialBody {
                target_length: declared_length,
                bytes_seen: available_body,
            }));
        }

        input.advance(LENGTH_PREFIX_BYTES);
        let body = input.split_to(declared_length).freeze();
        let mut header_bytes = [0_u8; HSMS_HEADER_LENGTH];
        header_bytes.copy_from_slice(&body[..HSMS_HEADER_LENGTH]);

        Ok(FrameDecodeStep::Frame(RawFrame {
            header: RawHeader::new(header_bytes),
            text: body.slice(HSMS_HEADER_LENGTH..),
        }))
    }

    /// Stores `fault` as the generation's permanent terminal result.
    fn terminate(&mut self, fault: FramingFault) -> FramingFault {
        self.terminal_fault = Some(fault);
        fault
    }
}

/// Checked metadata for encoding one HSMS Data frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DataFramePlan {
    /// Header-plus-text length encoded in the four-byte prefix.
    message_length: u32,
    /// Complete prefix-plus-message allocation length.
    encoded_length: usize,
}

impl DataFramePlan {
    /// Returns the exact final frame size including the four-byte prefix.
    pub(crate) const fn encoded_length(self) -> usize {
        self.encoded_length
    }

    /// Appends the checked prefix and semantic Data header to `output`.
    ///
    /// The caller must subsequently append exactly the Message Text length
    /// used to create this plan.
    pub(crate) fn write_prefix_and_header(self, output: &mut Vec<u8>, header: DataHeader) {
        output.extend_from_slice(&self.message_length.to_be_bytes());
        output.extend_from_slice(&data_header_bytes(header));
    }
}

/// Low-level HSMS encoder that checks lengths before final allocation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HsmsWireEncoder {
    /// Largest permitted header-plus-text E37 Message Length.
    max_message_length: usize,
}

impl HsmsWireEncoder {
    /// Creates an encoder enforcing the Message Length bound in `limits`.
    pub(crate) fn new(limits: EndpointLimits) -> Self {
        Self {
            max_message_length: limits.max_message_length(),
        }
    }

    /// Checks `text_length` and returns exact Data-frame allocation metadata.
    ///
    /// No output is allocated or written before all arithmetic and configured
    /// bounds succeed.
    pub(crate) fn plan_data(self, text_length: usize) -> Result<DataFramePlan, FrameEncodeError> {
        let message_length = HSMS_HEADER_LENGTH
            .checked_add(text_length)
            .ok_or(FrameEncodeError::MessageLengthOverflow { text_length })?;
        if message_length > self.max_message_length {
            return Err(FrameEncodeError::MessageLengthAboveLimit {
                message_length,
                maximum_length: self.max_message_length,
            });
        }
        let encoded_length = LENGTH_PREFIX_BYTES
            .checked_add(message_length)
            .ok_or(FrameEncodeError::EncodedFrameLengthOverflow { message_length })?;
        let message_length =
            u32::try_from(message_length).expect("EndpointLimits guarantees a u32 maximum");

        Ok(DataFramePlan {
            message_length,
            encoded_length,
        })
    }

    /// Encodes one typed header-only control message into a single allocation.
    pub(crate) fn encode_control(self, message: ControlMessage) -> Bytes {
        let encoded_length = LENGTH_PREFIX_BYTES + HSMS_HEADER_LENGTH;
        let mut output = Vec::with_capacity(encoded_length);
        output.extend_from_slice(&(HSMS_HEADER_LENGTH as u32).to_be_bytes());
        output.extend_from_slice(&control_header_bytes(message));
        debug_assert_eq!(output.len(), encoded_length);
        Bytes::from(output)
    }
}

/// Converts a semantic Data header to its fixed HSMS-SS wire representation.
fn data_header_bytes(header: DataHeader) -> [u8; HSMS_HEADER_LENGTH] {
    let session = header.session_id().get().to_be_bytes();
    let system = header.system_bytes().get().to_be_bytes();
    let stream_and_w = header.stream().get() | if header.reply_expected() { 0x80 } else { 0 };
    [
        session[0],
        session[1],
        stream_and_w,
        header.function().get(),
        0,
        0,
        system[0],
        system[1],
        system[2],
        system[3],
    ]
}

/// Converts a typed control message to its unique E37 wire header.
fn control_header_bytes(message: ControlMessage) -> [u8; HSMS_HEADER_LENGTH] {
    let (session_id, header_byte_2, header_byte_3, s_type, system_bytes) = match message {
        ControlMessage::SelectRequest {
            session_id,
            system_bytes,
        } => (session_id, 0, 0, 1, system_bytes),
        ControlMessage::SelectResponse {
            session_id,
            status,
            system_bytes,
        } => (session_id, 0, status.get(), 2, system_bytes),
        ControlMessage::DeselectRequest {
            session_id,
            system_bytes,
        } => (session_id, 0, 0, 3, system_bytes),
        ControlMessage::DeselectResponse {
            session_id,
            status,
            system_bytes,
        } => (session_id, 0, status.get(), 4, system_bytes),
        ControlMessage::LinktestRequest { system_bytes } => (u16::MAX, 0, 0, 5, system_bytes),
        ControlMessage::LinktestResponse { system_bytes } => (u16::MAX, 0, 0, 6, system_bytes),
        ControlMessage::RejectRequest {
            session_id,
            rejected_type,
            reason,
            system_bytes,
        } => (session_id, rejected_type, reason.get(), 7, system_bytes),
        ControlMessage::SeparateRequest {
            session_id,
            system_bytes,
        } => (session_id, 0, 0, 9, system_bytes),
    };
    let session = session_id.to_be_bytes();
    let system = system_bytes.get().to_be_bytes();
    [
        session[0],
        session[1],
        header_byte_2,
        header_byte_3,
        0,
        s_type,
        system[0],
        system[1],
        system[2],
        system[3],
    ]
}

#[cfg(test)]
mod tests {
    //! Framing tests cover partial input, sticky packets, terminal faults, and
    //! symmetric semantic header encoding.

    use crate::hsms::{
        model::ids::SystemBytes,
        protocol::header::{DeselectStatus, RejectReason, SelectStatus},
        Function, SessionId, Stream,
    };

    use super::*;

    /// Independently transcribed complete HSMS Data S1F1 ASCII `HELLO` frame.
    const FIXED_HSMS_DATA_S1F1_ASCII_HELLO: &[u8] = &[
        0x00, 0x00, 0x00, 0x11, // Message Length: 10-byte header + 7-byte text.
        0x00, 0x00, 0x01, 0x01, 0x00, 0x00, // Data header fields.
        0x00, 0x00, 0x00, 0x01, // System Bytes 1.
        0x41, 0x05, b'H', b'E', b'L', b'L', b'O', // ASCII item.
    ];

    /// Creates valid endpoint limits with a custom Message Length maximum.
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

    /// Builds a complete raw header-only frame for decoder tests.
    fn header_only_frame(system_bytes: u32) -> Vec<u8> {
        let mut frame = Vec::from((HSMS_HEADER_LENGTH as u32).to_be_bytes());
        frame.extend_from_slice(&[
            0xFF,
            0xFF,
            0,
            0,
            0,
            5,
            (system_bytes >> 24) as u8,
            (system_bytes >> 16) as u8,
            (system_bytes >> 8) as u8,
            system_bytes as u8,
        ]);
        frame
    }

    /// Confirms the fixed valid frame succeeds after every possible two-chunk
    /// split while the incomplete first chunk remains unconsumed.
    #[test]
    fn fixed_valid_frame_decodes_at_every_split_position() {
        for split in 1..FIXED_HSMS_DATA_S1F1_ASCII_HELLO.len() {
            let mut decoder = HsmsFrameDecoder::new(EndpointLimits::default());
            let mut input = BytesMut::from(&FIXED_HSMS_DATA_S1F1_ASCII_HELLO[..split]);

            let expected_progress = if split < LENGTH_PREFIX_BYTES {
                FrameReadProgress::AwaitingLength {
                    bytes_seen: split as u8,
                }
            } else {
                FrameReadProgress::PartialBody {
                    target_length: 17,
                    bytes_seen: split - LENGTH_PREFIX_BYTES,
                }
            };
            assert_eq!(
                decoder.decode(&mut input),
                Ok(FrameDecodeStep::NeedMore(expected_progress)),
                "split position {split}"
            );
            assert_eq!(
                input.as_ref(),
                &FIXED_HSMS_DATA_S1F1_ASCII_HELLO[..split],
                "split position {split} must not consume partial input"
            );

            input.extend_from_slice(&FIXED_HSMS_DATA_S1F1_ASCII_HELLO[split..]);
            let FrameDecodeStep::Frame(frame) =
                decoder.decode(&mut input).expect("complete fixed frame")
            else {
                panic!("split position {split} did not yield a frame");
            };
            assert_eq!(
                frame.header.as_bytes(),
                &[0, 0, 1, 1, 0, 0, 0, 0, 0, 1],
                "split position {split}"
            );
            assert_eq!(
                frame.text.as_ref(),
                &[0x41, 0x05, b'H', b'E', b'L', b'L', b'O'],
                "split position {split}"
            );
            assert!(input.is_empty(), "split position {split}");
        }
    }

    /// Confirms the minimum legal Message Length of ten accepts a complete
    /// independently transcribed header-only frame.
    #[test]
    fn length_ten_accepts_fixed_header_only_frame() {
        let fixed_linktest_request = [
            0x00, 0x00, 0x00, 0x0A, // Message Length 10.
            0xFF, 0xFF, 0x00, 0x00, 0x00, 0x05, // Linktest.req fields.
            0x01, 0x02, 0x03, 0x04, // System Bytes.
        ];
        let mut decoder = HsmsFrameDecoder::new(limits_with_max(10));
        let mut input = BytesMut::from(fixed_linktest_request.as_slice());

        let FrameDecodeStep::Frame(frame) = decoder.decode(&mut input).expect("minimum frame")
        else {
            panic!("complete minimum frame must decode");
        };
        assert_eq!(
            frame.header.as_bytes(),
            &[0xFF, 0xFF, 0, 0, 0, 5, 1, 2, 3, 4]
        );
        assert!(frame.text.is_empty());
        assert!(input.is_empty());
    }

    /// Confirms a complete frame whose declared length equals the configured
    /// maximum is accepted without narrowing the inclusive bound.
    #[test]
    fn length_equal_to_configured_maximum_is_accepted() {
        let mut decoder = HsmsFrameDecoder::new(limits_with_max(17));
        let mut input = BytesMut::from(FIXED_HSMS_DATA_S1F1_ASCII_HELLO);

        let FrameDecodeStep::Frame(frame) = decoder.decode(&mut input).expect("length=max frame")
        else {
            panic!("complete length=max frame must decode");
        };
        assert_eq!(frame.header.as_bytes(), &[0, 0, 1, 1, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            frame.text.as_ref(),
            &[0x41, 0x05, b'H', b'E', b'L', b'L', b'O']
        );
        assert!(input.is_empty());
    }

    /// Confirms `max + 1` is rejected from its four fixed prefix bytes before
    /// waiting for, consuming, or allocating the declared body.
    #[test]
    fn length_one_above_configured_maximum_is_immediately_fatal() {
        let mut decoder = HsmsFrameDecoder::new(limits_with_max(17));
        let mut input = BytesMut::from(&[0x00, 0x00, 0x00, 0x12][..]);
        let original_capacity = input.capacity();

        assert_eq!(
            decoder.decode(&mut input),
            Err(FramingFault::MessageLengthAboveLimit {
                declared_length: 18,
                maximum_length: 17,
            })
        );
        assert_eq!(input.as_ref(), &[0x00, 0x00, 0x00, 0x12]);
        assert_eq!(input.capacity(), original_capacity);
    }

    /// Confirms the largest possible wire declaration is rejected directly
    /// from a fixed prefix and cannot drive body allocation.
    #[test]
    fn u32_maximum_length_prefix_is_immediately_fatal() {
        let maximum = EndpointLimits::default().max_message_length();
        let mut decoder = HsmsFrameDecoder::new(EndpointLimits::default());
        let mut input = BytesMut::from(&[0xFF, 0xFF, 0xFF, 0xFF][..]);
        let original_capacity = input.capacity();

        assert_eq!(
            decoder.decode(&mut input),
            Err(FramingFault::MessageLengthAboveLimit {
                declared_length: u32::MAX,
                maximum_length: maximum,
            })
        );
        assert_eq!(input.as_ref(), &[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(input.capacity(), original_capacity);
    }

    /// Confirms partial prefix and body progress do not consume input.
    #[test]
    fn partial_input_reports_t8_progress_without_consumption() {
        let mut decoder = HsmsFrameDecoder::new(EndpointLimits::default());
        let mut input = BytesMut::from(&[0, 0, 0][..]);
        assert_eq!(
            decoder.decode(&mut input),
            Ok(FrameDecodeStep::NeedMore(
                FrameReadProgress::AwaitingLength { bytes_seen: 3 }
            ))
        );
        assert_eq!(input.as_ref(), &[0, 0, 0]);

        input.extend_from_slice(&[12, 0, 1, 2]);
        assert_eq!(
            decoder.decode(&mut input),
            Ok(FrameDecodeStep::NeedMore(FrameReadProgress::PartialBody {
                target_length: 12,
                bytes_seen: 3,
            }))
        );
        assert_eq!(input.len(), 7);
    }

    /// Confirms one call consumes one frame and preserves a sticky tail.
    #[test]
    fn sticky_frames_are_consumed_one_at_a_time() {
        let first = header_only_frame(1);
        let second = header_only_frame(2);
        let mut input = BytesMut::from([first.clone(), second.clone()].concat().as_slice());
        let mut decoder = HsmsFrameDecoder::new(EndpointLimits::default());

        let FrameDecodeStep::Frame(frame) = decoder.decode(&mut input).expect("first frame") else {
            panic!("expected frame");
        };
        assert_eq!(&frame.header.as_bytes()[6..], &[0, 0, 0, 1]);
        assert_eq!(input.as_ref(), second.as_slice());
    }

    /// Confirms a below-header fault remains exact on every later call.
    #[test]
    fn terminal_decoder_replays_original_below_header_fault() {
        let mut decoder = HsmsFrameDecoder::new(EndpointLimits::default());
        let mut input = BytesMut::from(&9_u32.to_be_bytes()[..]);
        let expected = FramingFault::MessageLengthBelowHeader {
            declared_length: 9,
            header_length: HSMS_HEADER_LENGTH,
        };
        assert_eq!(decoder.decode(&mut input), Err(expected));

        input.clear();
        input.extend_from_slice(&header_only_frame(1));
        assert_eq!(decoder.decode(&mut input), Err(expected));
        assert_eq!(input.as_ref(), header_only_frame(1).as_slice());
    }

    /// Confirms an above-limit declaration terminates before body allocation.
    #[test]
    fn above_limit_length_is_fatal_without_consumption() {
        let limits = limits_with_max(20);
        let mut decoder = HsmsFrameDecoder::new(limits);
        let mut input = BytesMut::from(&21_u32.to_be_bytes()[..]);
        assert_eq!(
            decoder.decode(&mut input),
            Err(FramingFault::MessageLengthAboveLimit {
                declared_length: 21,
                maximum_length: 20,
            })
        );
        assert_eq!(input.as_ref(), &21_u32.to_be_bytes());
    }

    /// Confirms Data planning emits one exact prefix and semantic header.
    #[test]
    fn data_plan_writes_the_checked_hsms_ss_envelope() {
        let encoder = HsmsWireEncoder::new(EndpointLimits::default());
        let plan = encoder.plan_data(3).expect("plan");
        let header = DataHeader::new(
            SessionId::new(0x0102).expect("session"),
            Stream::new(3).expect("stream"),
            Function::new(4),
            true,
            SystemBytes::new(0x1122_3344),
        );
        let mut output = Vec::with_capacity(plan.encoded_length());
        plan.write_prefix_and_header(&mut output, header);
        output.extend_from_slice(&[1, 2, 3]);
        assert_eq!(
            output,
            [0, 0, 0, 13, 0x01, 0x02, 0x83, 4, 0, 0, 0x11, 0x22, 0x33, 0x44, 1, 2, 3,]
        );
        assert_eq!(output.len(), plan.encoded_length());
    }

    /// Confirms every typed control variant has one deterministic header.
    #[test]
    fn typed_controls_encode_without_raw_header_state() {
        let system_bytes = SystemBytes::new(0x0102_0304);
        let cases = [
            (
                ControlMessage::SelectRequest {
                    session_id: 3,
                    system_bytes,
                },
                [0, 3, 0, 0, 0, 1],
            ),
            (
                ControlMessage::SelectResponse {
                    session_id: 3,
                    status: SelectStatus::new(2),
                    system_bytes,
                },
                [0, 3, 0, 2, 0, 2],
            ),
            (
                ControlMessage::DeselectRequest {
                    session_id: 3,
                    system_bytes,
                },
                [0, 3, 0, 0, 0, 3],
            ),
            (
                ControlMessage::DeselectResponse {
                    session_id: 3,
                    status: DeselectStatus::new(4),
                    system_bytes,
                },
                [0, 3, 0, 4, 0, 4],
            ),
            (
                ControlMessage::LinktestRequest { system_bytes },
                [0xFF, 0xFF, 0, 0, 0, 5],
            ),
            (
                ControlMessage::LinktestResponse { system_bytes },
                [0xFF, 0xFF, 0, 0, 0, 6],
            ),
            (
                ControlMessage::RejectRequest {
                    session_id: 3,
                    rejected_type: 8,
                    reason: RejectReason::new(1).expect("reason"),
                    system_bytes,
                },
                [0, 3, 8, 1, 0, 7],
            ),
            (
                ControlMessage::SeparateRequest {
                    session_id: 3,
                    system_bytes,
                },
                [0, 3, 0, 0, 0, 9],
            ),
        ];

        let encoder = HsmsWireEncoder::new(EndpointLimits::default());
        for (message, expected_header_start) in cases {
            let encoded = encoder.encode_control(message);
            assert_eq!(&encoded[..4], &10_u32.to_be_bytes());
            assert_eq!(&encoded[4..10], &expected_header_start);
            assert_eq!(&encoded[10..14], &0x0102_0304_u32.to_be_bytes());
        }
    }

    /// Confirms an outbound payload larger than the configured bound fails
    /// before any frame buffer exists.
    #[test]
    fn oversized_data_is_rejected_during_planning() {
        let encoder = HsmsWireEncoder::new(limits_with_max(12));
        assert_eq!(
            encoder.plan_data(3),
            Err(FrameEncodeError::MessageLengthAboveLimit {
                message_length: 13,
                maximum_length: 12,
            })
        );
    }

    /// Confirms a 32-bit target reports prefix-plus-message overflow instead
    /// of panicking at the E37 `u32::MAX` Message Length boundary.
    #[cfg(target_pointer_width = "32")]
    #[test]
    fn maximum_u32_message_length_reports_complete_frame_overflow_on_32_bit() {
        let encoder = HsmsWireEncoder::new(limits_with_max(usize::MAX));
        let text_length = usize::MAX - HSMS_HEADER_LENGTH;
        assert_eq!(
            encoder.plan_data(text_length),
            Err(FrameEncodeError::EncodedFrameLengthOverflow {
                message_length: usize::MAX,
            })
        );
    }
}
