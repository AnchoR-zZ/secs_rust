//! Transport-independent SECS message identifiers and primary content.
//!
//! SML, the HSMS Core and application APIs share these values. They contain no
//! connection identity, header allocation, reply capability or runtime policy.

use thiserror::Error;

use super::SecsItem;

/// A stream number that cannot be represented by the seven-bit SECS field.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("SECS stream {value} is outside the seven-bit range 0..=127")]
pub struct StreamError {
    /// Supplied value outside the representable range.
    pub value: u8,
}

/// A seven-bit SECS stream, independent of any transport header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Stream(
    /// Validated seven-bit value without a W-bit.
    u8,
);

impl Stream {
    /// Validates `value`, returning its stream or the rejected out-of-range value.
    pub const fn new(value: u8) -> Result<Self, StreamError> {
        if value > 127 {
            Err(StreamError { value })
        } else {
            Ok(Self(value))
        }
    }

    /// Returns the validated stream number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// A SECS function; its primary/secondary role is validated by the protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Function(
    /// Full eight-bit function value, including the F0 abort code.
    u8,
);

impl Function {
    /// Wraps the representable function `value` without imposing a message role.
    #[must_use]
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    /// Returns the original function number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Application-owned primary content, validated for sending by the protocol Core.
#[derive(Clone, Debug, PartialEq)]
pub struct PrimaryMessage {
    /// SECS stream supplied by the application or classified inbound message.
    stream: Stream,
    /// Function checked for primary validity when the protocol accepts it.
    function: Function,
    /// Optional root item; absent text remains distinct from typed-empty items.
    body: Option<SecsItem>,
}

impl PrimaryMessage {
    /// Combines `stream`, `function` and owned optional `body` without transport policy.
    #[must_use]
    pub const fn new(stream: Stream, function: Function, body: Option<SecsItem>) -> Self {
        Self {
            stream,
            function,
            body,
        }
    }

    /// Returns this message's SECS stream.
    #[must_use]
    pub const fn stream(&self) -> Stream {
        self.stream
    }

    /// Returns this message's SECS function.
    #[must_use]
    pub const fn function(&self) -> Function {
        self.function
    }

    /// Borrows the optional body without cloning its item tree.
    #[must_use]
    pub const fn body(&self) -> Option<&SecsItem> {
        self.body.as_ref()
    }

    /// Consumes the message and returns its optional owned body.
    #[must_use]
    pub fn into_body(self) -> Option<SecsItem> {
        self.body
    }

    /// Consumes the content into `(stream, function, body)` without copying.
    #[must_use]
    pub fn into_parts(self) -> (Stream, Function, Option<SecsItem>) {
        (self.stream, self.function, self.body)
    }
}
