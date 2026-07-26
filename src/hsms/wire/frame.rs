//! Byte-preserving HSMS values at the framing and validation boundary.
//!
//! Raw frames retain the exact ten header bytes. Validated Data frames expose
//! a semantic header while leaving Message Text opaque for the selected
//! presentation profile.

use bytes::Bytes;

use crate::hsms::protocol::header::{ControlMessage, DataHeader};

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

/// Structurally valid Data frame awaiting presentation-profile decoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WireDataFrame {
    /// Validated semantic HSMS Data header.
    pub(crate) header: DataHeader,
    /// Opaque Message Text owned by the selected presentation profile.
    pub(crate) text: Bytes,
}

/// Successful output of strict structural frame validation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ValidatedFrame {
    /// Data frame whose PType and SType are both the HSMS-SS value zero.
    Data(WireDataFrame),
    /// Header-only, structurally valid, typed E37 control message.
    Control(ControlMessage),
}
