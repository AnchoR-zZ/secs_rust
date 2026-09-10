//! SML (SECS Message Language) text adapter.
//!
//! This module converts between the PEER SML text notation and the crate's
//! [`SecsItem`](crate::secs2::SecsItem) trees plus validated
//! [`Stream`](crate::secs2::Stream)/[`Function`](crate::secs2::Function)
//! identifiers. It is a pure text layer: it owns no I/O, system bytes, or
//! session state, and it never judges function parity or send policy — those
//! belong to the HSMS layer.
//!
//! The parser implements the strict dialect first: concrete, directly
//! sendable messages with exact-match counts, no `[W]`-brackets, attached
//! W-Bits (`S1F1W` is rejected; write `S1F1 W`), message names, templates,
//! ranges, or ellipses. Dialect-tolerant entry points, if ever needed, will
//! be added beside the strict ones without changing their behavior. The
//! formatter emits one canonical form per style, so
//! `parse(format(item))` round-trips exactly whenever the reparsing parser's
//! limits admit the tree — with default [`DecodeLimits`](crate::DecodeLimits)
//! that means at most 64 nested Lists, 1,000,000 total nodes, and 1,000,000
//! direct children per List. The formatter itself additionally refuses only
//! what no parser could ever accept — List nesting beyond the hard
//! `DecodeLimits` ceiling of 256 — plus unencodable or unsupported content.
//!
//! # Examples
//!
//! Parsing and re-rendering a complete message:
//!
//! ```
//! use secs_rust::sml::{parse_message, FormatStyle, SmlFormatter};
//!
//! let message = parse_message(r#"S5F1 W <L [2] <B [1] 0x04> <A [6] "LOT001">>."#)
//!     .expect("strict SML parses");
//! assert!(message.wait_bit());
//!
//! let text = SmlFormatter::new(FormatStyle::Compact)
//!     .format_message(&message)
//!     .expect("representable items format");
//! assert_eq!(text, r#"S5F1 W <L[2] <B[1] 0x04> <A[6] "LOT001">>."#);
//! ```
//!
//! Parsing a bare item with explicit resource limits:
//!
//! ```
//! use secs_rust::sml::{parse_item, SmlParser};
//!
//! let item = parse_item("<I4 [3] -1 0 2147483647>").expect("counts match");
//! assert_eq!(format!("{item:?}"), "I4([-1, 0, 2147483647])");
//!
//! let tight = SmlParser::new(
//!     secs_rust::DecodeLimits::new(1, 1, 0x00FF_FFFF, 1).expect("valid limits"),
//! );
//! assert!(tight.parse_item("<L [1] <L [0]>>").is_err()); // nesting exceeds depth 1
//! ```

mod error;
mod formatter;
mod message;
mod parser;
mod scanner;

pub use error::{FormatError, ParseError, SourcePos};
pub use formatter::{FormatStyle, SmlFormatter};
pub use message::SmlMessage;
pub use parser::{parse_item, parse_message, SmlParser};
