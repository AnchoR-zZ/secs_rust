//! Construction and resource-limit errors for SECS-II value objects.
//!
//! Binary codec failures are added by the codec module; this file only owns
//! errors that can be detected while building Wave 0 values and limits.

use thiserror::Error;

/// Errors raised while constructing SECS-II values or limits.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SecsItemError {
    /// An ASCII item contained a byte outside the seven-bit ASCII range.
    #[error("SECS-II ASCII contains non-ASCII byte 0x{byte:02X} at byte index {index}")]
    NonAscii {
        /// Zero-based byte offset of the first non-ASCII byte.
        index: usize,
        /// Offending byte value.
        byte: u8,
    },

    /// A decode limit was configured as zero.
    #[error("SECS-II decode limit `{field}` must be greater than zero")]
    ZeroLimit {
        /// Name of the limit whose value was zero.
        field: &'static str,
    },

    /// A single item limit exceeded the three-byte E5 length field.
    #[error("SECS-II item byte limit {value} exceeds the maximum encodable length {maximum}")]
    ItemLengthTooLarge {
        /// Configured item-byte limit.
        value: usize,
        /// Largest value representable by the E5 item length field.
        maximum: usize,
    },

    /// Encoding code zero is reserved by E5 for localized strings.
    #[error("SECS-II localized string encoding code 0 is reserved")]
    ReservedLocalizedEncoding,
}
