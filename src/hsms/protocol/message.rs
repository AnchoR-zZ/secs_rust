//! Profile-bearing HSMS messages exchanged across runtime layers.
//!
//! This module is the only protocol-message layer that depends on the SECS-II
//! item model. Pure Data headers and typed control messages live in
//! `protocol::header` so Wire does not inherit that dependency.

use crate::secs2::SecsItem;

use super::header::{ControlMessage, DataHeader};

/// One semantic HSMS Data message after PType=0 Profile conversion.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DataMessage {
    /// Structurally valid HSMS Data header.
    header: DataHeader,
    /// One decoded SECS-II item, or `None` for absent Message Text.
    body: Option<SecsItem>,
}

impl DataMessage {
    /// Creates a semantic Data message from `header` and optional SECS-II
    /// `body`, preserving the distinction between absent and typed-empty text.
    pub(crate) const fn new(header: DataHeader, body: Option<SecsItem>) -> Self {
        Self { header, body }
    }

    /// Returns the message's validated Data header.
    pub(crate) const fn header(&self) -> DataHeader {
        self.header
    }

    /// Borrows the decoded SECS-II body, returning `None` for absent text.
    pub(crate) const fn body(&self) -> Option<&SecsItem> {
        self.body.as_ref()
    }

    /// Splits the message into its Data header and owned optional body.
    pub(crate) fn into_parts(self) -> (DataHeader, Option<SecsItem>) {
        (self.header, self.body)
    }
}

/// Direction-neutral semantic message exchanged by Core and SessionDriver.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ProtocolMessage {
    /// HSMS Data message with decoded or to-be-encoded SECS-II content.
    Data(DataMessage),
    /// Typed E37 control message that requires no presentation profile.
    Control(ControlMessage),
}
