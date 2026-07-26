//! Stable protocol-violation contracts delivered to the pure HSMS Core.
//!
//! Fatal length framing faults remain Wire/runtime concerns. This module only
//! describes recoverable header and Message Text violations for which Core may
//! choose an ordered E37 response without depending on codec error internals.

use super::header::DataHeader;

/// Exact ten-byte header snapshot associated with a recoverable violation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeaderSnapshot(
    /// Header bytes in E37 wire order, excluding the four-byte length prefix.
    [u8; 10],
);

impl HeaderSnapshot {
    /// Preserves all ten unmodified `bytes` for Core diagnostics and Reject
    /// construction.
    pub(crate) const fn new(bytes: [u8; 10]) -> Self {
        Self(bytes)
    }

    /// Borrows all ten captured header bytes in E37 wire order.
    pub(crate) const fn as_bytes(&self) -> &[u8; 10] {
        &self.0
    }

    /// Returns the raw two-byte Session ID.
    pub(crate) const fn session_id(self) -> u16 {
        u16::from_be_bytes([self.0[0], self.0[1]])
    }

    /// Returns raw Header Byte 2.
    pub(crate) const fn header_byte_2(self) -> u8 {
        self.0[2]
    }

    /// Returns raw Header Byte 3.
    pub(crate) const fn header_byte_3(self) -> u8 {
        self.0[3]
    }

    /// Returns the raw Presentation Type byte.
    pub(crate) const fn p_type(self) -> u8 {
        self.0[4]
    }

    /// Returns the raw Session Type byte.
    pub(crate) const fn s_type(self) -> u8 {
        self.0[5]
    }

    /// Returns the raw four-byte System Bytes value.
    pub(crate) const fn system_bytes(self) -> u32 {
        u32::from_be_bytes([self.0[6], self.0[7], self.0[8], self.0[9]])
    }
}

/// Recoverable structural header failure found after length framing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct HeaderViolation {
    /// Exact header snapshot needed for ordered protocol handling.
    header: HeaderSnapshot,
    /// Stable semantic reason independent of Wire implementation details.
    kind: HeaderViolationKind,
}

impl HeaderViolation {
    /// Creates a recoverable header violation from the exact `header` and its
    /// classified semantic `kind`.
    pub(crate) const fn new(header: HeaderSnapshot, kind: HeaderViolationKind) -> Self {
        Self { header, kind }
    }

    /// Returns the exact offending header snapshot.
    pub(crate) const fn header(self) -> HeaderSnapshot {
        self.header
    }

    /// Returns the stable structural violation category.
    pub(crate) const fn kind(self) -> HeaderViolationKind {
        self.kind
    }
}

/// Stable recoverable header-violation categories understood by Core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderViolationKind {
    /// A header-only control message incorrectly carried Message Text.
    ControlMessageHasText,
    /// A Data message used the control-reserved `0xFFFF` Session ID.
    InvalidDataSessionId,
    /// A control message violated an SType-specific Session ID rule.
    InvalidControlSessionId,
    /// Fixed control header bytes, status, or reason were invalid.
    InvalidControlHeader,
    /// No installed HSMS-SS presentation profile supports the supplied PType.
    UnknownPresentationType {
        /// Unsupported raw Presentation Type byte.
        p_type: u8,
    },
    /// The Session Type byte does not identify a standard supported message.
    UnknownSessionType {
        /// Unsupported raw Session Type byte.
        s_type: u8,
    },
}

/// Stable classification of malformed SECS-II Message Text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PayloadViolation {
    /// Validated Data header associated with the malformed Message Text.
    header: DataHeader,
    /// Stable category that does not expose concrete codec error variants.
    kind: PayloadViolationKind,
}

impl PayloadViolation {
    /// Creates a payload violation for `header` with the classified `kind`.
    pub(crate) const fn new(header: DataHeader, kind: PayloadViolationKind) -> Self {
        Self { header, kind }
    }

    /// Returns the Data header associated with the malformed text.
    pub(crate) const fn header(self) -> DataHeader {
        self.header
    }

    /// Returns the stable payload-failure category.
    pub(crate) const fn kind(self) -> PayloadViolationKind {
        self.kind
    }
}

/// Coarse Message Text failure classes used for protocol decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PayloadViolationKind {
    /// Message Text is not exactly one well-formed SECS-II item.
    MalformedSecs2,
    /// Decoding was refused by a configured resource bound.
    ResourceLimitExceeded,
}

/// Any recoverable inbound violation that may require a Core response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InboundViolation {
    /// Structurally invalid ten-byte HSMS header.
    Header(HeaderViolation),
    /// Structurally valid Data header with invalid or over-limit Message Text.
    Payload(PayloadViolation),
}
