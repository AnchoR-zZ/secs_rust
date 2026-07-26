//! Strongly typed identifiers used to correlate asynchronous HSMS work.
//!
//! Public identifiers expose read-only values, while allocation remains inside
//! the owning endpoint, supervisor, Core, or generation runtime.

use std::fmt;

use crate::hsms::IdentifierError;

/// One concrete TCP connection incarnation owned by a logical endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnectionGeneration(
    /// Monotonic TCP-incarnation number allocated by `ConnectionSupervisor`.
    u64,
);

impl ConnectionGeneration {
    /// Creates an internally allocated generation identifier from `value`.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    /// Returns the monotonic generation number for diagnostics and correlation.
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ConnectionGeneration {
    /// Writes the numeric generation identifier to `formatter`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A data-message session id. `0xFFFF` is reserved for control messages.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionId(
    /// Data-message Session ID, guaranteed not to equal `0xFFFF`.
    u16,
);

impl SessionId {
    /// Validates and returns a Data-message Session ID created from `value`.
    ///
    /// Returns [`IdentifierError::ReservedControlSession`] for `0xFFFF`, which
    /// E37 reserves for most control-message headers.
    pub fn new(value: u16) -> Result<Self, IdentifierError> {
        if value == u16::MAX {
            return Err(IdentifierError::ReservedControlSession);
        }
        Ok(Self(value))
    }

    #[must_use]
    /// Returns the validated two-byte Session ID.
    pub const fn get(self) -> u16 {
        self.0
    }
}

/// A seven-bit SECS stream number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Stream(
    /// Seven-bit stream number with no W-bit mixed into the value.
    u8,
);

impl Stream {
    /// Validates and returns a stream number created from `value`.
    ///
    /// Returns [`IdentifierError::StreamOutOfRange`] when `value` cannot fit in
    /// the seven stream bits of an HSMS Data header.
    pub fn new(value: u8) -> Result<Self, IdentifierError> {
        if value > 0x7F {
            return Err(IdentifierError::StreamOutOfRange { value });
        }
        Ok(Self(value))
    }

    #[must_use]
    /// Returns the validated seven-bit stream number.
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// A SECS function number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Function(
    /// Full eight-bit SECS function number.
    u8,
);

impl Function {
    #[must_use]
    /// Wraps the eight-bit SECS function `value` without further restrictions.
    pub const fn new(value: u8) -> Self {
        Self(value)
    }

    #[must_use]
    /// Returns the SECS function number.
    pub const fn get(self) -> u8 {
        self.0
    }
}

/// Defines a crate-private monotonic `u64` identifier with controlled
/// construction and read-only numeric access.
macro_rules! internal_id {
    ($name:ident, $description:literal) => {
        #[doc = $description]
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub(crate) struct $name(
            /// Opaque monotonic value allocated by the identifier's owner.
            u64,
        );

        impl $name {
            #[doc = concat!("Creates an internal `", stringify!($name), "` from `value`.")]
            pub(crate) const fn new(value: u64) -> Self {
                Self(value)
            }

            #[doc = concat!("Returns the numeric value of this `", stringify!($name), "`.")]
            pub(crate) const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

internal_id!(
    CommandId,
    "Identifies one application command and its completion."
);
internal_id!(
    OperationId,
    "Identifies one Core operation across scheduling and completion."
);
internal_id!(
    WriteId,
    "Identifies one outbound write independently of its owning Core operation."
);
internal_id!(
    DeliveryId,
    "Identifies one reliable application delivery attempt and its completion."
);
internal_id!(
    ReplyCapabilityId,
    "Identifies one single-use authority to reply to an inbound primary."
);
internal_id!(
    TimerId,
    "Identifies one timer registration independently of its timeout kind."
);
internal_id!(
    WireSequence,
    "Identifies one frame position in generation-local wire order."
);
internal_id!(
    LifecycleSequence,
    "Identifies one linearized endpoint lifecycle revision."
);
internal_id!(
    EventSequence,
    "Identifies one event in endpoint publication order."
);

/// HSMS System Bytes. This value is allocated internally and never accepted
/// from the application API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct SystemBytes(
    /// Four-byte transaction correlation value allocated by the Core registry.
    u32,
);

impl SystemBytes {
    /// Wraps an internally allocated System Bytes `value`.
    pub(crate) const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the four-byte transaction correlation value.
    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}
