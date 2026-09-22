//! Strongly typed identifiers used to correlate asynchronous HSMS work.
//!
//! Public identifiers expose read-only values, while allocation remains inside
//! the owning endpoint, supervisor, Core, or generation runtime. Generation-
//! local allocators advance monotonically, permit diagnostic gaps after failed
//! admission, never wrap, and permanently stop allocating a kind at `u64`
//! exhaustion.

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
    #[cfg(any(feature = "runtime-tokio", test))]
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

pub use crate::secs2::{Function, Stream};

/// Defines a crate-private monotonic `u64` identifier with controlled
/// construction and read-only numeric access.
#[cfg(any(feature = "runtime-tokio", test))]
macro_rules! internal_id {
    ($name:ident, $description:literal $(, $getter_gate:meta)?) => {
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
            $(#[$getter_gate])?
            pub(crate) const fn get(self) -> u64 {
                self.0
            }
        }
    };
}

/// Identifies a single-use reply authority without exposing its numeric contents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg(any(feature = "runtime-tokio", test))]
pub(crate) struct ReplyCapabilityId(
    /// Opaque value allocated only by the owning generation's Core.
    u64,
);
#[cfg(any(feature = "runtime-tokio", test))]
impl ReplyCapabilityId {
    /// Creates an internally allocated reply identity from `value`.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}
#[cfg(any(feature = "runtime-tokio", test))]
internal_id!(
    CommandId,
    "Identifies one accepted generation-local command.",
    cfg(test)
);
#[cfg(any(feature = "runtime-tokio", test))]
internal_id!(
    WriteId,
    "Identifies one Core-produced generation-local frame.",
    cfg(test)
);
#[cfg(feature = "runtime-tokio")]
internal_id!(
    LifecycleSequence,
    "Identifies one linearized endpoint lifecycle revision."
);

/// HSMS System Bytes. This value is allocated internally and never accepted
/// from the application API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg(any(feature = "runtime-tokio", test))]
pub(crate) struct SystemBytes(
    /// Four-byte transaction correlation value allocated by the protocol core.
    u32,
);

#[cfg(any(feature = "runtime-tokio", test))]
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

#[cfg(test)]
mod tests {
    use std::{any::TypeId, collections::HashSet};

    use super::{CommandId, WriteId};

    /// Confirms internal identifiers retain their values and total ordering.
    #[test]
    fn internal_identifiers_have_value_semantics() {
        let lower = CommandId::new(17);
        let equal = CommandId::new(17);
        let higher = CommandId::new(18);

        assert_eq!(lower, equal);
        assert!(lower < higher);
        assert_eq!(lower.get(), 17);

        let mut identities = HashSet::new();
        assert!(identities.insert(lower));
        assert!(!identities.insert(equal));
        assert!(identities.insert(higher));
    }

    /// Confirms command and Core-write identifiers remain
    /// distinct type-level facts even when their numeric values coincide.
    #[test]
    fn correlation_identifiers_are_distinct_types() {
        assert_ne!(TypeId::of::<CommandId>(), TypeId::of::<WriteId>());

        assert_eq!(CommandId::new(23).get(), 23);
        assert_eq!(WriteId::new(23).get(), 23);
    }

    /// Confirms the frozen correlation identifiers are inexpensive copyable
    /// values suitable for map keys without sharing allocator ownership.
    #[test]
    fn correlation_identifiers_are_copy_values() {
        /// Requires `T` to implement `Copy` and returns both copied values.
        fn copy_twice<T: Copy>(value: T) -> (T, T) {
            (value, value)
        }

        assert_eq!(
            copy_twice(CommandId::new(u64::MAX)),
            (CommandId::new(u64::MAX), CommandId::new(u64::MAX))
        );
        assert_eq!(
            copy_twice(WriteId::new(u64::MAX)),
            (WriteId::new(u64::MAX), WriteId::new(u64::MAX))
        );
    }
}
