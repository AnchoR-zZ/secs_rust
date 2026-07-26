//! Fatal framing and outbound-size errors owned by the HSMS Wire boundary.
//!
//! Recoverable ten-byte header failures are protocol contracts in
//! `hsms::protocol`; this file deliberately contains only conditions that
//! terminate framing or prevent a local frame from being represented.

use thiserror::Error;

/// Fatal inbound E37 Message Length fault for one TCP generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum FramingFault {
    /// The declared Message Length cannot contain the mandatory ten-byte
    /// header.
    #[error(
        "HSMS Message Length {declared_length} is below the mandatory {header_length}-byte header"
    )]
    MessageLengthBelowHeader {
        /// Exact unsigned length value read from the peer.
        declared_length: u32,
        /// Mandatory HSMS header length used by the decoder.
        header_length: usize,
    },
    /// The declared Message Length exceeds the configured generation bound.
    #[error("HSMS Message Length {declared_length} exceeds configured maximum {maximum_length}")]
    MessageLengthAboveLimit {
        /// Exact unsigned length value read from the peer.
        declared_length: u32,
        /// Maximum accepted E37 Message Length.
        maximum_length: usize,
    },
}

/// Failure to represent a local semantic Data message as one HSMS frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub(crate) enum FrameEncodeError {
    /// Adding the ten-byte header to Message Text overflowed `usize`.
    #[error("HSMS Data Message Length overflow for {text_length} Message Text bytes")]
    MessageLengthOverflow {
        /// Number of Message Text bytes supplied by the presentation profile.
        text_length: usize,
    },
    /// Adding the four-byte prefix to a representable Message Length overflowed
    /// the platform's `usize`.
    #[error("complete HSMS frame length overflows usize for Message Length {message_length}")]
    EncodedFrameLengthOverflow {
        /// Representable E37 header-plus-text Message Length.
        message_length: usize,
    },
    /// The computed E37 Message Length exceeds the configured generation
    /// bound.
    #[error(
        "HSMS Data Message Length {message_length} exceeds configured maximum {maximum_length}"
    )]
    MessageLengthAboveLimit {
        /// Computed header-plus-text Message Length.
        message_length: usize,
        /// Maximum configured E37 Message Length.
        maximum_length: usize,
    },
}
