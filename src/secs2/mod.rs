//! SEMI E5 value types.
//!
//! Wave 0 defines the data model and decode limits. Binary encoding and
//! decoding are deliberately left to the Wave 1 SECS-II work package.

pub mod codec;
mod error;
mod item;
mod limits;

pub use error::SecsItemError;
pub use item::{AsciiString, LocalizedEncodingCode, LocalizedString, SecsItem};
pub use limits::{DecodeLimits, MAX_ENCODED_ITEM_LENGTH};
