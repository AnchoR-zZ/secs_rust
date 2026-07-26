//! Strict, stateless HSMS-SS structural validation.
//!
//! The validator parses a length-delimited raw frame into semantic Data or
//! typed control values. It never evaluates Selected state, transactions, or
//! application intent; those protocol decisions remain in higher layers.

use crate::hsms::{
    model::ids::SystemBytes,
    protocol::{
        header::{ControlMessage, DataHeader, DeselectStatus, RejectReason, SelectStatus},
        violation::{HeaderSnapshot, HeaderViolation, HeaderViolationKind},
    },
    wire::frame::{RawFrame, RawHeader, ValidatedFrame, WireDataFrame},
    Function, SessionId, Stream,
};

/// Session ID that E37 reserves for control use.
const CONTROL_SESSION_ID: u16 = 0xFFFF;

/// Presentation Type fixed by the built-in HSMS-SS protocol contract.
const HSMS_SS_PRESENTATION_TYPE: u8 = 0;

/// Stateless structural classifier for one framed HSMS message.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StrictFrameValidator;

impl StrictFrameValidator {
    /// Creates a reusable stateless frame validator.
    pub(crate) const fn new() -> Self {
        Self
    }

    /// Validates `raw` in deterministic E37 order.
    ///
    /// SType is identified first, followed by the HSMS-SS PType policy and
    /// then the selected Data/control layout. Returns a semantic frame on
    /// success or one recoverable violation containing the exact header.
    pub(crate) fn validate(&self, raw: RawFrame) -> Result<ValidatedFrame, HeaderViolation> {
        let RawFrame { header, text } = raw;
        let s_type = header.s_type();

        if !matches!(s_type, 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 9) {
            return Err(violation(
                header,
                HeaderViolationKind::UnknownSessionType { s_type },
            ));
        }

        let p_type = header.p_type();
        if p_type != HSMS_SS_PRESENTATION_TYPE {
            return Err(violation(
                header,
                HeaderViolationKind::UnknownPresentationType { p_type },
            ));
        }

        if s_type == 0 {
            return validate_data(header, text);
        }

        if !text.is_empty() {
            return Err(violation(
                header,
                HeaderViolationKind::ControlMessageHasText,
            ));
        }

        validate_control(header, s_type).map(ValidatedFrame::Control)
    }
}

/// Validates and parses an HSMS Data header while preserving opaque text.
fn validate_data(header: RawHeader, text: bytes::Bytes) -> Result<ValidatedFrame, HeaderViolation> {
    let bytes = header.as_bytes();
    let session_value = u16::from_be_bytes([bytes[0], bytes[1]]);
    if session_value == CONTROL_SESSION_ID {
        return Err(violation(header, HeaderViolationKind::InvalidDataSessionId));
    }

    let session_id =
        SessionId::new(session_value).expect("a non-control Session ID is always valid");
    let stream = Stream::new(bytes[2] & 0x7F).expect("a seven-bit stream is always valid");
    let function = Function::new(bytes[3]);
    let reply_expected = bytes[2] & 0x80 != 0;
    let data_header = DataHeader::new(
        session_id,
        stream,
        function,
        reply_expected,
        system_bytes_of(header),
    );

    Ok(ValidatedFrame::Data(WireDataFrame {
        header: data_header,
        text,
    }))
}

/// Validates SType-specific fixed fields and constructs a typed control value.
fn validate_control(header: RawHeader, s_type: u8) -> Result<ControlMessage, HeaderViolation> {
    let bytes = header.as_bytes();
    let session_id = u16::from_be_bytes([bytes[0], bytes[1]]);
    let header_byte_2 = bytes[2];
    let header_byte_3 = bytes[3];
    let system_bytes = system_bytes_of(header);

    let message = match s_type {
        1 => {
            require_zero_header(header, header_byte_2, header_byte_3)?;
            ControlMessage::SelectRequest {
                session_id,
                system_bytes,
            }
        }
        2 => {
            require_zero_byte_2(header, header_byte_2)?;
            ControlMessage::SelectResponse {
                session_id,
                status: SelectStatus::new(header_byte_3),
                system_bytes,
            }
        }
        3 => {
            require_zero_header(header, header_byte_2, header_byte_3)?;
            ControlMessage::DeselectRequest {
                session_id,
                system_bytes,
            }
        }
        4 => {
            require_zero_byte_2(header, header_byte_2)?;
            ControlMessage::DeselectResponse {
                session_id,
                status: DeselectStatus::new(header_byte_3),
                system_bytes,
            }
        }
        5 => {
            require_linktest_session(header, session_id)?;
            require_zero_header(header, header_byte_2, header_byte_3)?;
            ControlMessage::LinktestRequest { system_bytes }
        }
        6 => {
            require_linktest_session(header, session_id)?;
            require_zero_header(header, header_byte_2, header_byte_3)?;
            ControlMessage::LinktestResponse { system_bytes }
        }
        7 => {
            let reason = RejectReason::new(header_byte_3)
                .ok_or_else(|| violation(header, HeaderViolationKind::InvalidControlHeader))?;
            ControlMessage::RejectRequest {
                session_id,
                header_byte_2,
                reason,
                system_bytes,
            }
        }
        9 => {
            require_zero_header(header, header_byte_2, header_byte_3)?;
            ControlMessage::SeparateRequest {
                session_id,
                system_bytes,
            }
        }
        _ => unreachable!("the caller dispatches only standard control STypes"),
    };

    Ok(message)
}

/// Creates a recoverable violation containing all original header bytes.
fn violation(header: RawHeader, kind: HeaderViolationKind) -> HeaderViolation {
    HeaderViolation::new(HeaderSnapshot::new(*header.as_bytes()), kind)
}

/// Reads the four-byte System Bytes value from a raw header.
fn system_bytes_of(header: RawHeader) -> SystemBytes {
    let bytes = header.as_bytes();
    SystemBytes::new(u32::from_be_bytes([bytes[6], bytes[7], bytes[8], bytes[9]]))
}

/// Requires Header Byte 2 to equal zero.
fn require_zero_byte_2(header: RawHeader, header_byte_2: u8) -> Result<(), HeaderViolation> {
    if header_byte_2 == 0 {
        Ok(())
    } else {
        Err(violation(header, HeaderViolationKind::InvalidControlHeader))
    }
}

/// Requires both variable control-header bytes to equal zero.
fn require_zero_header(
    header: RawHeader,
    header_byte_2: u8,
    header_byte_3: u8,
) -> Result<(), HeaderViolation> {
    if header_byte_2 == 0 && header_byte_3 == 0 {
        Ok(())
    } else {
        Err(violation(header, HeaderViolationKind::InvalidControlHeader))
    }
}

/// Requires the fixed Linktest Session ID value `0xFFFF`.
fn require_linktest_session(header: RawHeader, session_id: u16) -> Result<(), HeaderViolation> {
    if session_id == CONTROL_SESSION_ID {
        Ok(())
    } else {
        Err(violation(
            header,
            HeaderViolationKind::InvalidControlSessionId,
        ))
    }
}

#[cfg(test)]
mod tests {
    //! Validator tests cover deterministic error order and typed controls.

    use bytes::Bytes;

    use super::*;

    /// Builds a raw frame from exact header bytes and optional Message Text.
    fn raw(header: [u8; 10], text: &[u8]) -> RawFrame {
        RawFrame {
            header: RawHeader::new(header),
            text: Bytes::copy_from_slice(text),
        }
    }

    /// Builds a standard ten-byte header for validation vectors.
    fn header(
        session_id: u16,
        header_byte_2: u8,
        header_byte_3: u8,
        p_type: u8,
        s_type: u8,
        system_bytes: u32,
    ) -> [u8; 10] {
        let session = session_id.to_be_bytes();
        let system = system_bytes.to_be_bytes();
        [
            session[0],
            session[1],
            header_byte_2,
            header_byte_3,
            p_type,
            s_type,
            system[0],
            system[1],
            system[2],
            system[3],
        ]
    }

    /// Asserts one direct raw-header vector produces exactly `expected_kind`
    /// and preserves every offending header byte.
    fn assert_fixed_violation(
        header_bytes: [u8; 10],
        text: &[u8],
        expected_kind: HeaderViolationKind,
    ) {
        let error = StrictFrameValidator::new()
            .validate(raw(header_bytes, text))
            .expect_err("fixed vector must violate one header rule");
        assert_eq!(error.kind(), expected_kind);
        assert_eq!(error.header().as_bytes(), &header_bytes);
    }

    /// Gives every stable `HeaderViolationKind` an independently transcribed
    /// raw vector, including unknown PType and reserved Data Session ID.
    #[test]
    fn every_header_violation_kind_has_a_direct_fixed_vector() {
        assert_fixed_violation(
            [0xFF, 0xFF, 0, 0, 0, 5, 0, 0, 0, 1],
            &[0xAA],
            HeaderViolationKind::ControlMessageHasText,
        );
        assert_fixed_violation(
            [0xFF, 0xFF, 1, 1, 0, 0, 0, 0, 0, 2],
            &[],
            HeaderViolationKind::InvalidDataSessionId,
        );
        assert_fixed_violation(
            [0, 1, 0, 0, 0, 5, 0, 0, 0, 3],
            &[],
            HeaderViolationKind::InvalidControlSessionId,
        );
        assert_fixed_violation(
            [0, 1, 0, 0, 0, 7, 0, 0, 0, 4],
            &[],
            HeaderViolationKind::InvalidControlHeader,
        );
        assert_fixed_violation(
            [0, 1, 1, 1, 9, 0, 0, 0, 0, 5],
            &[],
            HeaderViolationKind::UnknownPresentationType { p_type: 9 },
        );
        assert_fixed_violation(
            [0, 1, 0, 0, 0, 8, 0, 0, 0, 6],
            &[],
            HeaderViolationKind::UnknownSessionType { s_type: 8 },
        );
    }

    /// Confirms that a Data header is parsed into typed semantic fields.
    #[test]
    fn data_header_is_parsed_without_retaining_ptype_or_stype() {
        let validated = StrictFrameValidator::new()
            .validate(raw(header(7, 0x83, 5, 0, 0, 0x0102_0304), &[0x21, 0x00]))
            .expect("valid Data frame");
        let ValidatedFrame::Data(data) = validated else {
            panic!("expected Data");
        };
        assert_eq!(data.header.session_id().get(), 7);
        assert_eq!(data.header.stream().get(), 3);
        assert_eq!(data.header.function().get(), 5);
        assert!(data.header.reply_expected());
        assert_eq!(data.header.system_bytes().get(), 0x0102_0304);
        assert_eq!(data.text.as_ref(), &[0x21, 0x00]);
    }

    /// Confirms unknown SType wins over an unsupported PType.
    #[test]
    fn unknown_stype_is_classified_before_ptype() {
        let error = StrictFrameValidator::new()
            .validate(raw(header(1, 0, 0, 9, 8, 3), &[]))
            .expect_err("unknown SType");
        assert_eq!(
            error.kind(),
            HeaderViolationKind::UnknownSessionType { s_type: 8 }
        );
        assert_eq!(error.header().p_type(), 9);
    }

    /// Confirms control Message Text is rejected before fixed-field parsing.
    #[test]
    fn control_message_text_has_a_stable_violation() {
        let error = StrictFrameValidator::new()
            .validate(raw(header(4, 7, 9, 0, 1, 3), &[1]))
            .expect_err("control text");
        assert_eq!(error.kind(), HeaderViolationKind::ControlMessageHasText);
    }

    /// Confirms Select response status is preserved in the typed variant.
    #[test]
    fn select_response_preserves_status_and_system_bytes() {
        let frame = StrictFrameValidator::new()
            .validate(raw(header(2, 0, 4, 0, 2, 0x1122_3344), &[]))
            .expect("Select.rsp");
        assert_eq!(
            frame,
            ValidatedFrame::Control(ControlMessage::SelectResponse {
                session_id: 2,
                status: SelectStatus::new(4),
                system_bytes: SystemBytes::new(0x1122_3344),
            })
        );
    }

    /// Confirms Linktest requires the control-reserved Session ID.
    #[test]
    fn linktest_requires_control_session_id() {
        let error = StrictFrameValidator::new()
            .validate(raw(header(1, 0, 0, 0, 5, 7), &[]))
            .expect_err("invalid Linktest session");
        assert_eq!(error.kind(), HeaderViolationKind::InvalidControlSessionId);
    }

    /// Confirms Reject reason zero cannot enter the semantic control model.
    #[test]
    fn reject_reason_zero_is_rejected() {
        let error = StrictFrameValidator::new()
            .validate(raw(header(9, 2, 0, 0, 7, 11), &[]))
            .expect_err("zero reason");
        assert_eq!(error.kind(), HeaderViolationKind::InvalidControlHeader);
    }

    /// Confirms a non-zero Reject reason and Header Byte 2 are preserved.
    #[test]
    fn reject_request_is_fully_typed() {
        let frame = StrictFrameValidator::new()
            .validate(raw(header(9, 2, 3, 0, 7, 11), &[]))
            .expect("Reject.req");
        assert_eq!(
            frame,
            ValidatedFrame::Control(ControlMessage::RejectRequest {
                session_id: 9,
                header_byte_2: 2,
                reason: RejectReason::new(3).expect("non-zero"),
                system_bytes: SystemBytes::new(11),
            })
        );
    }
}
