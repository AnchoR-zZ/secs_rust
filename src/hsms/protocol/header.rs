//! Pure HSMS Data-header and control-message protocol values.
//!
//! These types model only E37 header semantics and deliberately avoid the
//! SECS-II item model. Wire can therefore validate and encode HSMS headers
//! without acquiring a presentation-profile dependency.

use std::num::NonZeroU8;

use crate::hsms::model::ids::{Function, SessionId, Stream, SystemBytes};

/// Structurally valid HSMS Data-message header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DataHeader {
    /// Validated Data-message Session ID.
    session_id: SessionId,
    /// Seven-bit stream value with the W-bit removed.
    stream: Stream,
    /// Eight-bit SECS function value.
    function: Function,
    /// Whether the Primary requests a Secondary response.
    reply_expected: bool,
    /// Four-byte transaction correlation value.
    system_bytes: SystemBytes,
}

impl DataHeader {
    /// Creates a validated semantic Data header from its typed fields.
    ///
    /// `session_id` and `stream` have already passed their value-object
    /// validation. The return value contains no PType or SType because this
    /// HSMS-SS contract fixes both wire fields to zero.
    pub(crate) const fn new(
        session_id: SessionId,
        stream: Stream,
        function: Function,
        reply_expected: bool,
        system_bytes: SystemBytes,
    ) -> Self {
        Self {
            session_id,
            stream,
            function,
            reply_expected,
            system_bytes,
        }
    }

    /// Returns the validated Data-message Session ID.
    pub(crate) const fn session_id(self) -> SessionId {
        self.session_id
    }

    /// Returns the seven-bit SECS stream number.
    pub(crate) const fn stream(self) -> Stream {
        self.stream
    }

    /// Returns the SECS function number.
    pub(crate) const fn function(self) -> Function {
        self.function
    }

    /// Returns whether the HSMS W-bit is set.
    pub(crate) const fn reply_expected(self) -> bool {
        self.reply_expected
    }

    /// Returns the transaction-correlation System Bytes.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        self.system_bytes
    }
}

/// Raw E37 `Select.rsp` status byte with convenience success semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectStatus(
    /// Exact response status byte received from or sent to the peer.
    u8,
);

impl SelectStatus {
    /// E37 status 0: selection completed successfully.
    pub(crate) const SUCCESS: Self = Self(0);
    /// E37 status 1: the HSMS entity is already active.
    pub(crate) const ALREADY_ACTIVE: Self = Self(1);
    /// E37 status 2: the HSMS entity is not ready for selection.
    pub(crate) const NOT_READY: Self = Self(2);
    /// E37 status 3: the HSMS entity has exhausted its connection resources.
    pub(crate) const EXHAUSTED: Self = Self(3);

    /// Wraps the exact `Select.rsp` status byte without discarding
    /// subsidiary-standard or locally defined values.
    pub(crate) const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the exact encoded status byte.
    pub(crate) const fn get(self) -> u8 {
        self.0
    }

    /// Returns `true` when the E37 success status value is zero.
    pub(crate) const fn is_success(self) -> bool {
        self.0 == Self::SUCCESS.0
    }

    /// Returns `true` for one of E37's four named base-standard statuses.
    ///
    /// A `false` result does not make the raw status invalid: subsidiary
    /// standards and local implementations may define additional values.
    pub(crate) const fn is_base_standard(self) -> bool {
        self.0 <= Self::EXHAUSTED.0
    }
}

/// Raw E37 `Deselect.rsp` status byte with convenience success semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeselectStatus(
    /// Exact response status byte received from or sent to the peer.
    u8,
);

impl DeselectStatus {
    /// E37 status 0: deselection completed successfully.
    pub(crate) const SUCCESS: Self = Self(0);
    /// E37 status 1: no selected communication exists.
    pub(crate) const NOT_SELECTED: Self = Self(1);
    /// E37 status 2: the HSMS entity is busy and cannot deselect.
    pub(crate) const BUSY: Self = Self(2);

    /// Wraps the exact `Deselect.rsp` status byte without discarding
    /// subsidiary-standard or locally defined values.
    pub(crate) const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the exact encoded status byte.
    pub(crate) const fn get(self) -> u8 {
        self.0
    }

    /// Returns `true` when the E37 success status value is zero.
    pub(crate) const fn is_success(self) -> bool {
        self.0 == Self::SUCCESS.0
    }

    /// Returns `true` for one of E37's three named base-standard statuses.
    ///
    /// A `false` result preserves a potentially subsidiary-standard or local
    /// status for higher-level policy and diagnostics.
    pub(crate) const fn is_base_standard(self) -> bool {
        self.0 <= Self::BUSY.0
    }
}

/// Non-zero E37 `Reject.req` reason byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RejectReason(
    /// Validated non-zero rejection reason.
    NonZeroU8,
);

impl RejectReason {
    /// E37 reason 1: the received SType is not supported.
    pub(crate) const UNSUPPORTED_STYPE: Self = Self::from_non_zero_literal(1);
    /// E37 reason 2: the received PType is not supported.
    pub(crate) const UNSUPPORTED_PTYPE: Self = Self::from_non_zero_literal(2);
    /// E37 reason 3: no open transaction matches the received message.
    pub(crate) const TRANSACTION_NOT_OPEN: Self = Self::from_non_zero_literal(3);
    /// E37 reason 4: the HSMS entity is not selected.
    pub(crate) const ENTITY_NOT_SELECTED: Self = Self::from_non_zero_literal(4);

    /// Validates `value` as an E37 rejection reason.
    ///
    /// Returns `None` for zero, which E37 reserves and the strict Wire
    /// validator classifies as an invalid control header. Every non-zero raw
    /// value is preserved so subsidiary standards and local implementations
    /// can define additional reasons.
    pub(crate) const fn new(value: u8) -> Option<Self> {
        match NonZeroU8::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the exact non-zero reason byte.
    pub(crate) const fn get(self) -> u8 {
        self.0.get()
    }

    /// Returns `true` for one of E37's four named base-standard reasons.
    ///
    /// A `false` result denotes a preserved extension value, not an invalid
    /// reason.
    pub(crate) const fn is_base_standard(self) -> bool {
        self.get() <= Self::ENTITY_NOT_SELECTED.get()
    }

    /// Constructs one named non-zero constant from its checked literal.
    const fn from_non_zero_literal(value: u8) -> Self {
        match NonZeroU8::new(value) {
            Some(value) => Self(value),
            None => panic!("RejectReason named constants must be non-zero"),
        }
    }
}

/// A coherent, header-only E37 control message.
///
/// Each variant fixes its SType and exposes only the variable header fields
/// that belong to that control form. This prevents stale raw bytes from
/// disagreeing with a separately stored control classification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlMessage {
    /// `Select.req` selection request.
    SelectRequest {
        /// Two-byte control Session ID retained exactly.
        session_id: u16,
        /// Correlation value for the selection transaction.
        system_bytes: SystemBytes,
    },
    /// `Select.rsp` selection response.
    SelectResponse {
        /// Two-byte control Session ID retained exactly.
        session_id: u16,
        /// Exact response status carried in Header Byte 3.
        status: SelectStatus,
        /// Correlation value copied from the request.
        system_bytes: SystemBytes,
    },
    /// `Deselect.req` deselection request.
    DeselectRequest {
        /// Two-byte control Session ID retained exactly.
        session_id: u16,
        /// Correlation value for the deselection transaction.
        system_bytes: SystemBytes,
    },
    /// `Deselect.rsp` deselection response.
    DeselectResponse {
        /// Two-byte control Session ID retained exactly.
        session_id: u16,
        /// Exact response status carried in Header Byte 3.
        status: DeselectStatus,
        /// Correlation value copied from the request.
        system_bytes: SystemBytes,
    },
    /// `Linktest.req` liveness request with the fixed `0xFFFF` Session ID.
    LinktestRequest {
        /// Correlation value for the linktest transaction.
        system_bytes: SystemBytes,
    },
    /// `Linktest.rsp` liveness response with the fixed `0xFFFF` Session ID.
    LinktestResponse {
        /// Correlation value copied from the request.
        system_bytes: SystemBytes,
    },
    /// `Reject.req` reporting an unsupported or malformed received header.
    RejectRequest {
        /// Two-byte control Session ID retained exactly.
        session_id: u16,
        /// Exact Header Byte 2 copied from the rejected message.
        ///
        /// E37 reason 2 identifies this byte as PType; every other base reason
        /// identifies it as SType.
        header_byte_2: u8,
        /// Validated non-zero E37 rejection reason.
        reason: RejectReason,
        /// System Bytes associated with the rejected message.
        system_bytes: SystemBytes,
    },
    /// `Separate.req` unacknowledged separation request.
    SeparateRequest {
        /// Two-byte control Session ID retained exactly.
        session_id: u16,
        /// System Bytes carried by the separation message.
        system_bytes: SystemBytes,
    },
}

impl ControlMessage {
    /// Returns the System Bytes carried by any control-message variant.
    pub(crate) const fn system_bytes(self) -> SystemBytes {
        match self {
            Self::SelectRequest { system_bytes, .. }
            | Self::SelectResponse { system_bytes, .. }
            | Self::DeselectRequest { system_bytes, .. }
            | Self::DeselectResponse { system_bytes, .. }
            | Self::LinktestRequest { system_bytes }
            | Self::LinktestResponse { system_bytes }
            | Self::RejectRequest { system_bytes, .. }
            | Self::SeparateRequest { system_bytes, .. } => system_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeselectStatus, RejectReason, SelectStatus};

    /// Confirms every named base-standard status maps to its E37 wire value.
    #[test]
    fn named_status_constants_match_e37_values() {
        assert_eq!(SelectStatus::SUCCESS.get(), 0);
        assert_eq!(SelectStatus::ALREADY_ACTIVE.get(), 1);
        assert_eq!(SelectStatus::NOT_READY.get(), 2);
        assert_eq!(SelectStatus::EXHAUSTED.get(), 3);
        assert_eq!(DeselectStatus::SUCCESS.get(), 0);
        assert_eq!(DeselectStatus::NOT_SELECTED.get(), 1);
        assert_eq!(DeselectStatus::BUSY.get(), 2);
    }

    /// Confirms named Reject reasons map to their E37 wire values.
    #[test]
    fn named_reject_reason_constants_match_e37_values() {
        assert_eq!(RejectReason::UNSUPPORTED_STYPE.get(), 1);
        assert_eq!(RejectReason::UNSUPPORTED_PTYPE.get(), 2);
        assert_eq!(RejectReason::TRANSACTION_NOT_OPEN.get(), 3);
        assert_eq!(RejectReason::ENTITY_NOT_SELECTED.get(), 4);
    }

    /// Confirms extension status and reason bytes remain lossless raw values.
    #[test]
    fn subsidiary_and_local_values_remain_lossless() {
        let select = SelectStatus::new(0x80);
        let deselect = DeselectStatus::new(0x81);
        let reject = RejectReason::new(0x82).expect("non-zero extension reason");

        assert_eq!(select.get(), 0x80);
        assert!(!select.is_base_standard());
        assert!(!select.is_success());
        assert_eq!(deselect.get(), 0x81);
        assert!(!deselect.is_base_standard());
        assert!(!deselect.is_success());
        assert_eq!(reject.get(), 0x82);
        assert!(!reject.is_base_standard());
    }
}
