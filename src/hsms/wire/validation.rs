//! Structural HSMS validation outcomes independent of protocol session state.
//!
//! Validation preserves any available raw header so the Core can make the
//! ordered E37 Reject/close decision without the Wire layer deciding Selected.

use super::frame::RawHeader;

/// Structural violation identified without losing the original header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FrameViolation {
    /// Parsed raw header when at least ten message bytes were available.
    pub(crate) raw_header: Option<RawHeader>,
    /// Stable structural reason assigned by the strict validator.
    pub(crate) kind: FrameViolationKind,
}

/// Total structural violation vocabulary produced before Core processing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameViolationKind {
    /// E37 Message Length was smaller than the mandatory ten-byte header.
    MessageLengthBelowHeader,
    /// E37 Message Length exceeded the configured bounded frame budget.
    MessageLengthAboveLimit,
    /// A header-only control message incorrectly carried Message Text.
    ControlMessageHasText,
    /// A Data message used the control-only Session ID value.
    InvalidDataSessionId,
    /// A control message used a Session ID forbidden for its control type.
    InvalidControlSessionId,
    /// Reserved control-header fields or response/status bytes were malformed.
    InvalidControlHeader,
    /// Presentation Type is not supported by the configured profile.
    UnknownPresentationType(u8),
    /// Session Type is not a known E37 Data or control type.
    UnknownSessionType(u8),
}
