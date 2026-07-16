//! Byte-preserving HSMS frame types shared by framing, validation, and Profile.
//!
//! Raw values retain the exact ten header bytes. Validated values expose typed
//! Data/control structure while keeping Message Text as opaque bytes so the
//! Wire layer never depends on SECS-II.

use bytes::Bytes;

use crate::hsms::{
    model::ids::SystemBytes, wire::validation::FrameViolation, Function, SessionId, Stream,
};

/// Number of bytes in every E37 HSMS message header.
pub(crate) const HSMS_HEADER_LENGTH: usize = 10;

/// Exact ten-byte HSMS header before semantic validation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RawHeader(
    /// Exact header bytes in E37 wire order.
    [u8; HSMS_HEADER_LENGTH],
);

impl RawHeader {
    /// Preserves the supplied ten `bytes` as an unvalidated HSMS header.
    pub(crate) const fn new(bytes: [u8; HSMS_HEADER_LENGTH]) -> Self {
        Self(bytes)
    }

    /// Borrows all ten raw header bytes in wire order.
    pub(crate) const fn as_bytes(&self) -> &[u8; HSMS_HEADER_LENGTH] {
        &self.0
    }

    /// Returns the raw Presentation Type byte at header offset four.
    pub(crate) const fn p_type(self) -> u8 {
        self.0[4]
    }

    /// Returns the raw Session Type byte at header offset five.
    pub(crate) const fn s_type(self) -> u8 {
        self.0[5]
    }
}

/// One length-delimited frame whose header bytes are preserved exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RawFrame {
    /// Exact unvalidated ten-byte header.
    pub(crate) header: RawHeader,
    /// Message Text bytes following the header.
    pub(crate) text: Bytes,
}

/// Structurally valid HSMS Data header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DataHeader {
    /// Validated Data-message Session ID.
    pub(crate) session_id: SessionId,
    /// Seven-bit stream value with the W-bit removed.
    pub(crate) stream: Stream,
    /// Eight-bit SECS function value.
    pub(crate) function: Function,
    /// W-bit indicating whether the Primary expects a Secondary.
    pub(crate) reply_expected: bool,
    /// Presentation Type retained for profile selection; HSMS-SS uses zero.
    pub(crate) p_type: u8,
    /// Four-byte transaction correlation value.
    pub(crate) system_bytes: SystemBytes,
}

/// Structurally valid Data frame whose Message Text remains opaque to Wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DataFrame {
    /// Validated Data-message header fields.
    pub(crate) header: DataHeader,
    /// Undecoded presentation-profile payload.
    pub(crate) text: Bytes,
}

/// Known E37 control message types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ControlType {
    /// `Select.req` selection request.
    SelectRequest,
    /// `Select.rsp` selection response.
    SelectResponse,
    /// `Deselect.req` deselection request.
    DeselectRequest,
    /// `Deselect.rsp` deselection response.
    DeselectResponse,
    /// `Linktest.req` liveness probe.
    LinktestRequest,
    /// `Linktest.rsp` liveness response.
    LinktestResponse,
    /// `Reject.req` protocol rejection.
    RejectRequest,
    /// `Separate.req` unacknowledged separation request.
    SeparateRequest,
}

/// Structurally valid control frame with its exact header preserved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ControlFrame {
    /// Original header used for status, reason, and rejection diagnostics.
    pub(crate) raw_header: RawHeader,
    /// Known E37 control-message classification.
    pub(crate) control_type: ControlType,
    /// Correlation value used by request/response control transactions.
    pub(crate) system_bytes: SystemBytes,
}

/// Total output of structural frame classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InboundFrame {
    /// Structurally valid Data frame awaiting presentation-profile decoding.
    Data(DataFrame),
    /// Structurally valid known control frame.
    Control(ControlFrame),
    /// Total structural-validation result retaining available raw context.
    Violation(FrameViolation),
}

/// Wire-ready frame produced after any presentation-profile encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum OutboundFrame {
    /// Data header and encoded Message Text.
    Data(DataFrame),
    /// Header-only E37 control message.
    Control(ControlFrame),
}
