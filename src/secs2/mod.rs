//! SEMI E5 value types.
//!
//! This module owns the immutable item model, construction validation,
//! bounded decode policy, and the strict binary codec implemented by
//! [`codec`]. It intentionally does not own HSMS framing or session state.

pub mod codec;
mod error;
mod item;
mod limits;

pub use error::SecsItemError;
pub use item::{AsciiString, LocalizedEncodingCode, LocalizedString, SecsItem};
pub use limits::{DecodeLimits, MAX_DECODE_NESTING_DEPTH, MAX_ENCODED_ITEM_LENGTH};
