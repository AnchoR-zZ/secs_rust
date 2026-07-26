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
    /// Wraps the exact `Select.rsp` status byte without discarding
    /// subsidiary-standard or vendor-defined values.
    pub(crate) const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the exact encoded status byte.
    pub(crate) const fn get(self) -> u8 {
        self.0
    }

    /// Returns `true` when the E37 success status value is zero.
    pub(crate) const fn is_success(self) -> bool {
        self.0 == 0
    }
}

/// Raw E37 `Deselect.rsp` status byte with convenience success semantics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DeselectStatus(
    /// Exact response status byte received from or sent to the peer.
    u8,
);

impl DeselectStatus {
    /// Wraps the exact `Deselect.rsp` status byte without narrowing the value.
    pub(crate) const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the exact encoded status byte.
    pub(crate) const fn get(self) -> u8 {
        self.0
    }

    /// Returns `true` when the E37 success status value is zero.
    pub(crate) const fn is_success(self) -> bool {
        self.0 == 0
    }
}

/// Non-zero E37 `Reject.req` reason byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RejectReason(
    /// Validated non-zero rejection reason.
    NonZeroU8,
);

impl RejectReason {
    /// Validates `value` as an E37 rejection reason.
    ///
    /// Returns `None` for zero, which E37 reserves and the strict Wire
    /// validator classifies as an invalid control header.
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
        /// Rejected message's raw SType copied into Header Byte 2.
        rejected_type: u8,
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
