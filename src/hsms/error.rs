//! Stable error vocabulary shared by the public API and protocol boundaries.
//!
//! These errors describe invalid configuration, identifiers, protocol
//! decisions, and operation outcomes without exposing socket or runtime types.

use thiserror::Error;

use crate::{hsms::model::ids::Function, secs2::SecsItemError};

/// Invalid protocol identifier values.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum IdentifierError {
    /// A Data-message Session ID used the control-only `0xFFFF` value.
    #[error("HSMS session id 0xFFFF is reserved for control messages")]
    ReservedControlSession,

    /// A stream value exceeded the seven bits available beside the W-bit.
    #[error("SECS stream {value} is outside the seven-bit range 0..=127")]
    StreamOutOfRange {
        /// Supplied stream value.
        value: u8,
    },
}

/// Invalid endpoint configuration.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// A mandatory or enabled timer was configured with zero duration.
    #[error("duration `{field}` must be greater than zero")]
    ZeroDuration {
        /// Name of the invalid timer field.
        field: &'static str,
    },

    /// A bounded queue or registry was configured with zero capacity.
    #[error("capacity `{field}` must be greater than zero")]
    ZeroCapacity {
        /// Name of the invalid capacity field.
        field: &'static str,
    },

    /// The configured Message Length cannot contain the mandatory HSMS header.
    #[error("maximum HSMS message length {value} is smaller than the 10-byte header")]
    MessageLengthTooSmall {
        /// Supplied maximum Message Length.
        value: usize,
    },

    /// The configured Message Length cannot fit in E37's four-byte prefix.
    #[error("maximum HSMS message length {value} exceeds the four-byte length field")]
    MessageLengthTooLarge {
        /// Supplied maximum Message Length.
        value: usize,
    },

    /// The nested SECS-II decoding policy was invalid.
    #[error(transparent)]
    InvalidSecs2Limits(#[from] SecsItemError),
}

/// Timer kinds visible at the protocol and endpoint boundaries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TimeoutKind {
    /// TCP connection-attempt timeout.
    Connect,
    /// Data-message reply timeout.
    T3,
    /// Active reconnect-delay timer.
    T5,
    /// Control-transaction reply timeout.
    T6,
    /// Selection timeout after TCP establishment.
    T7,
    /// Inter-byte timeout while receiving one HSMS message.
    T8,
    /// Library-configured idle Linktest interval.
    Linktest,
}

/// A protocol-level failure that is independent of transport implementation.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    /// An application attempted to use an even or zero SECS function as a
    /// primary message.
    #[error("SECS function {function:?} is not a valid primary function")]
    InvalidPrimaryFunction {
        /// Function value rejected by primary-message validation.
        function: Function,
    },

    /// An inbound message violated an HSMS protocol rule.
    #[error("HSMS protocol violation: {description}")]
    Violation {
        /// Stable diagnostic suitable for application logging.
        description: String,
    },

    /// A candidate Secondary failed the pending transaction's response matcher.
    #[error("received response does not match the pending transaction")]
    ResponseMismatch,

    /// Protocol state changed before a transaction could complete normally.
    #[error("HSMS transaction was aborted")]
    TransactionAborted,
}

/// Failure returned by a typed endpoint operation.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OperationError {
    /// The logical endpoint has not been started.
    #[error("HSMS endpoint is not running")]
    NotRunning,

    /// The endpoint is running but has no usable TCP generation.
    #[error("HSMS endpoint has no open TCP generation")]
    NotConnected,

    /// A Data operation was attempted outside the Selected state.
    #[error("HSMS session is not selected")]
    NotSelected,

    /// A previously accepted operation was aborted because the session left
    /// the Selected state.
    #[error("HSMS session was deselected before the operation completed")]
    SessionDeselected,

    /// Shutdown has closed admission for this operation class.
    #[error("HSMS endpoint is draining and no longer accepts this operation")]
    Draining,

    /// Cleanup could not prove that the previous generation released resources.
    #[error("HSMS endpoint is faulted and must be stopped before restart")]
    Faulted,

    /// The operation targets a TCP incarnation that is no longer current.
    #[error("operation belongs to a stale TCP generation")]
    StaleConnectionGeneration,

    /// Admission could not reserve all bounded resources atomically.
    #[error("operation was rejected by bounded admission")]
    Backpressure,

    /// The operation exceeded the named protocol or runtime deadline.
    #[error("HSMS {0:?} timeout")]
    Timeout(TimeoutKind),

    /// The TCP generation ended before a deterministic completion was available.
    #[error("TCP generation closed before the operation completed")]
    ConnectionLost,

    /// A failed partial write may already be visible to the peer.
    #[error("frame visibility is indeterminate because the write failed after it may be visible")]
    DeliveryIndeterminate,

    /// The owning endpoint runtime is no longer executing.
    #[error("HSMS runtime has stopped")]
    RuntimeStopped,

    /// A protocol decision prevented successful completion.
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}
