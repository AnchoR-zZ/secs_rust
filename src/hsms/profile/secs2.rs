//! PType=0 mapping between HSMS Data text and `Option<SecsItem>`.
//!
//! Implementation follows the strict SECS-II codec in Wave 1.

#![allow(dead_code)]

use crate::{
    hsms::wire::{
        frame::{ControlFrame, DataFrame, DataHeader},
        validation::FrameViolation,
    },
    secs2::SecsItem,
};

/// A structurally valid HSMS Data frame whose Message Text has already been
/// decoded by the SECS-II profile boundary.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Secs2DataFrame {
    /// Structurally validated HSMS Data header.
    pub(crate) header: DataHeader,
    /// Exactly one decoded E5 item, or `None` for absent Message Text.
    pub(crate) body: Option<SecsItem>,
}

/// Failed conversion of an otherwise structurally valid Data Message Text.
/// The exact codec error remains owned by `secs2`; Core only needs a stable
/// diagnostic and the original Data header for its E37 decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DataTextViolation {
    /// Data header retained so Core can construct the correct E37 response.
    pub(crate) header: DataHeader,
    /// Stable diagnostic derived from the private SECS-II codec error.
    pub(crate) description: String,
}

/// Semantic input accepted by the pure Core after Wire and Profile handling.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum InboundProtocolFrame {
    /// Validated Data message with fully decoded SECS-II content.
    Data(Secs2DataFrame),
    /// Structurally valid known E37 control message.
    Control(ControlFrame),
    /// Structural Wire failure awaiting the Core's ordered protocol decision.
    WireViolation(FrameViolation),
    /// Malformed SECS-II Message Text paired with its valid Data header.
    DataTextViolation(DataTextViolation),
}

/// Semantic output emitted by the Core before Profile/Wire encoding.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OutboundProtocolFrame {
    /// Semantic Data message that Profile must encode before Wire scheduling.
    Data(Secs2DataFrame),
    /// Header-only control frame that bypasses SECS-II encoding.
    Control(ControlFrame),
}

/// Pure adapter seam executed by SessionDriver, never by `HsmsCore` itself.
/// Empty Message Text maps to `body = None`; non-empty text must decode to
/// exactly one complete item with no trailing bytes.
pub(crate) trait Secs2Profile {
    /// Concrete SECS-II encoding or decoding failure retained inside runtime.
    type Error;

    /// Decodes `frame.text` as absent text or exactly one complete E5 item.
    ///
    /// Returns the semantic frame on success or `Self::Error` for malformed,
    /// trailing, or resource-limit-exceeding Message Text.
    fn decode_data(&self, frame: DataFrame) -> Result<Secs2DataFrame, Self::Error>;

    /// Encodes the optional body of semantic `frame` into wire-ready Data text.
    ///
    /// Returns the byte-preserving Data frame or `Self::Error` if the body
    /// cannot be represented within the configured E5 limits.
    fn encode_data(&self, frame: Secs2DataFrame) -> Result<DataFrame, Self::Error>;
}
