//! Parsed SML message value object.
//!
//! `SmlMessage` is the frozen result of parsing one complete `SxFy W? item? .`
//! text. It is a plain value description: it carries no system bytes, session
//! identity, or send policy, and it deliberately never reaches into the HSMS
//! session layer. Converting a message into an HSMS primary/secondary
//! operation stays the responsibility of the future HSMS send API.

use crate::secs2::SecsItem;
use crate::secs2::{Function, Stream};

/// Complete parsed form of one SML message: `SxFy`, optional W-Bit, and an
/// optional single root item as the Message Text.
///
/// The stream and function numbers are already validated newtypes, so a
/// constructed `SmlMessage` is always in range. A body of `None` means the
/// message has no Message Text at all, which is distinct from `Some` holding
/// an empty item such as an empty list.
#[derive(Clone, Debug, PartialEq)]
pub struct SmlMessage {
    /// Seven-bit stream number of the message header.
    stream: Stream,
    /// Eight-bit function number of the message header.
    function: Function,
    /// Whether the parsed text set the W-Bit (reply expected).
    wait_bit: bool,
    /// Root Message Text item; `None` when the message has no body.
    body: Option<SecsItem>,
}

impl SmlMessage {
    /// Assembles a message from already-validated parts.
    ///
    /// `stream` and `function` are the header identifiers, `wait_bit` records
    /// the W-Bit, and `body` is the optional root item. Returns the assembled
    /// message; construction cannot fail because the identifiers are
    /// validated newtypes.
    #[must_use]
    pub const fn new(
        stream: Stream,
        function: Function,
        wait_bit: bool,
        body: Option<SecsItem>,
    ) -> Self {
        Self {
            stream,
            function,
            wait_bit,
            body,
        }
    }

    /// Returns the seven-bit stream number of the message header.
    #[must_use]
    pub const fn stream(&self) -> Stream {
        self.stream
    }

    /// Returns the eight-bit function number of the message header.
    #[must_use]
    pub const fn function(&self) -> Function {
        self.function
    }

    /// Returns whether the W-Bit is set on this message.
    #[must_use]
    pub const fn wait_bit(&self) -> bool {
        self.wait_bit
    }

    /// Returns the root Message Text item, or `None` when the message has no
    /// body.
    #[must_use]
    pub const fn body(&self) -> Option<&SecsItem> {
        self.body.as_ref()
    }

    /// Consumes the message and returns its parts.
    ///
    /// Returns `(stream, function, wait_bit, body)` with the body as the owned
    /// optional root item, ready for conversion into protocol-layer types.
    #[must_use]
    pub fn into_parts(self) -> (Stream, Function, bool, Option<SecsItem>) {
        (self.stream, self.function, self.wait_bit, self.body)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the parsed message value object.

    use super::*;

    /// Builds a message with S1F13, W-Bit set, and no body for shared
    /// assertions across the accessors.
    fn sample() -> SmlMessage {
        SmlMessage::new(
            Stream::new(1).expect("stream 1"),
            Function::new(13),
            true,
            None,
        )
    }

    /// Confirms the accessors return exactly the parts passed to `new`.
    #[test]
    fn accessors_return_constructed_parts() {
        let message = sample();
        assert_eq!(message.stream(), Stream::new(1).expect("stream 1"));
        assert_eq!(message.function().get(), 13);
        assert!(message.wait_bit());
        assert!(message.body().is_none());
    }

    /// Confirms `into_parts` yields the same components in declared order.
    #[test]
    fn into_parts_returns_components_in_order() {
        let (stream, function, wait_bit, body) = sample().into_parts();
        assert_eq!(stream.get(), 1);
        assert_eq!(function.get(), 13);
        assert!(wait_bit);
        assert!(body.is_none());
    }

    /// Confirms equality is structural over all four fields.
    #[test]
    fn messages_with_same_parts_are_equal() {
        assert_eq!(sample(), sample());
    }
}
