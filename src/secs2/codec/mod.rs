//! Strict SECS-II binary codec.
//!
//! The primary operations are two public entry points:
//!
//! - [`encode_to_vec`] serialises a [`SecsItem`](crate::secs2::SecsItem)
//!   tree into a freshly allocated byte vector.
//! - [`Secs2Decoder::decode_item`] decodes exactly one complete item and
//!   rejects empty input or trailing bytes.
//!
//! Supporting public modules expose header helpers and structured error types
//! for applications that need wire diagnostics. The encoder first validates
//! and exactly measures the item tree, then allocates the final output buffer
//! once at that capacity and writes it. Its explicit traversal stacks are
//! separate auxiliary vectors and may allocate as nesting grows. The decoder
//! uses an explicit List stack and a hard nesting ceiling, bounding both
//! parsing and recursive decoded-tree cleanup.
//!
//! Resource policy is stored by [`Secs2Decoder`]; encoding only enforces the
//! E5 24-bit length ceiling. Protocol layers such as HSMS remain responsible
//! for representing absent Message Text before calling the strict item
//! decoder.

pub mod decode;
pub mod encode;
pub mod error;
pub mod header;

pub use decode::Secs2Decoder;
pub use encode::encode_to_vec;
pub use error::{DecodeError, EncodeError};
