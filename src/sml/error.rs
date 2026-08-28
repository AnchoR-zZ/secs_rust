//! Error types for the SML text adapter.
//!
//! `ParseError` carries a [`SourcePos`] with byte offset plus one-based line
//! and column for every strict-dialect syntax or semantic failure, mirroring
//! how the binary codec's `DecodeError` carries wire offsets. `FormatError`
//! reports canonical-formatting refusals (unsupported item types, non-finite
//! floats, List nesting no parser could accept, E5-unencodable trees) before
//! any partial text is produced.

use std::fmt;

use thiserror::Error;

use crate::secs2::codec::EncodeError;

/// Location of an SML diagnostic inside the source text.
///
/// Positions are computed by the scanner in one pass: `offset` is the
/// zero-based byte index, `line` is one-based, and `column` is one-based in
/// characters from the start of the line, which keeps columns meaningful for
/// editors even though the input is scanned bytewise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourcePos {
    /// Zero-based byte offset of the offending token or character.
    pub offset: usize,
    /// One-based line number within the source text.
    pub line: usize,
    /// One-based column number in characters within the line.
    pub column: usize,
}

impl SourcePos {
    /// Builds a source position from its byte offset and one-based line and
    /// column.
    ///
    /// The `offset` is the byte index of the first character of the token;
    /// `line` and `column` must both be at least one. Returns the assembled
    /// position unchanged (callers derive the components while scanning).
    #[must_use]
    pub const fn new(offset: usize, line: usize, column: usize) -> Self {
        Self {
            offset,
            line,
            column,
        }
    }
}

impl fmt::Display for SourcePos {
    /// Renders the position as `line L, column C (byte N)` for diagnostics.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "line {}, column {} (byte {})",
            self.line, self.column, self.offset
        )
    }
}

/// Failure while parsing SML text in the strict dialect.
///
/// Every variant names the position of the offending token. The enum is
/// `non_exhaustive` so new strict-dialect checks can be added without a
/// breaking release, matching the crate's other error types.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum ParseError {
    /// A character that cannot begin any SML token was encountered.
    #[error("invalid character {found:?} at {pos}")]
    InvalidCharacter {
        /// Position of the offending character.
        pos: SourcePos,
        /// The character itself, for display in diagnostics.
        found: char,
    },

    /// A quoted ASCII fragment reached end of input before its closing quote.
    #[error("unterminated ASCII string starting at {pos}")]
    UnterminatedString {
        /// Position of the opening double quote.
        pos: SourcePos,
    },

    /// A quoted ASCII fragment held a byte that strict SML requires to be
    /// written as `0xHH` instead (controls, `"`, and `\`).
    #[error("ASCII string at {pos} contains byte 0x{byte:02X} that must be written as 0xHH")]
    InvalidAsciiFragment {
        /// Position of the offending byte within the fragment.
        pos: SourcePos,
        /// Raw byte value found inside the quotes.
        byte: u8,
    },

    /// A `0x` literal had no hexadecimal digits after the prefix, which is
    /// neither a byte nor an integer in any SML context.
    #[error("invalid hexadecimal literal {text:?} at {pos}")]
    InvalidHexLiteral {
        /// Position where the literal starts.
        pos: SourcePos,
        /// The literal text as scanned, for diagnostics.
        text: String,
    },

    /// An integer literal exceeds every SECS integer range (it does not even
    /// fit the scanner's widest intermediate type).
    #[error("numeric literal {text:?} at {pos} is too large for any SECS integer type")]
    NumberTooLarge {
        /// Position where the literal starts.
        pos: SourcePos,
        /// The literal text as scanned, for diagnostics.
        text: String,
    },

    /// The first token was not a well-formed `SxFy` message header word.
    ///
    /// This also rejects headers with an attached W-Bit (`S1F1W`): strict
    /// SML requires the W-Bit as a separate `W` word.
    #[error("invalid message header {text:?} at {pos}; expected SxFy such as S1F13, with the W-Bit written as a separate W word")]
    InvalidMessageHeader {
        /// Position where the header word starts.
        pos: SourcePos,
        /// The word text that failed to match the header shape.
        text: String,
    },

    /// The stream number exceeds the seven-bit HSMS range 0..=127.
    #[error("stream number {value} at {pos} exceeds the seven-bit maximum 127")]
    StreamOutOfRange {
        /// Position of the header word carrying the stream number.
        pos: SourcePos,
        /// Parsed stream value that is out of range.
        value: u64,
    },

    /// The function number exceeds the eight-bit HSMS range 0..=255.
    #[error("function number {value} at {pos} exceeds the eight-bit maximum 255")]
    FunctionOutOfRange {
        /// Position of the header word carrying the function number.
        pos: SourcePos,
        /// Parsed function value that is out of range.
        value: u64,
    },

    /// An item type keyword is not part of SML at all.
    #[error("unknown item type {name:?} at {pos}")]
    UnknownType {
        /// Position of the unrecognized type word.
        pos: SourcePos,
        /// The word text that is not a known type keyword.
        name: String,
    },

    /// An item type exists in SECS-II but the strict SML dialect refuses it
    /// (JIS-8 and localized text) instead of degrading silently.
    #[error("item type {name:?} at {pos} is not supported by the strict SML dialect")]
    UnsupportedType {
        /// Position of the refused type word.
        pos: SourcePos,
        /// The refused type keyword, e.g. "J".
        name: String,
    },

    /// A BOOLEAN element was not exactly `TRUE` or `FALSE`.
    #[error("invalid BOOLEAN literal {found:?} at {pos}; only TRUE and FALSE are accepted")]
    InvalidBoolean {
        /// Position of the offending word.
        pos: SourcePos,
        /// The word text that is not a boolean literal.
        found: String,
    },

    /// A bracketed item count was not a plain non-negative decimal number.
    #[error("invalid item count at {pos}; expected a decimal number such as [3]")]
    InvalidCount {
        /// Position of the `[` that opens the count bracket.
        pos: SourcePos,
    },

    /// A declared count does not match the number of elements provided.
    ///
    /// Excess elements are detected as soon as the running count passes the
    /// declared value (the `actual` field then holds the count at detection,
    /// which for a multi-character ASCII fragment may exceed the declared
    /// value by more than one); shortfalls surface at the closing `>`.
    #[error("item at {pos} declares {declared} elements but {actual} were provided")]
    CountMismatch {
        /// Position of the item header token (`<` or type word).
        pos: SourcePos,
        /// Count declared inside the brackets.
        declared: usize,
        /// Element count observed when the excess was detected, or the count
        /// present at the closing `>` for a shortfall.
        actual: usize,
    },

    /// An integer literal does not fit the target SECS integer type.
    ///
    /// The diagnostic names the literal's original spelling so users can
    /// locate the offending token in the source text.
    #[error("integer literal {literal:?} at {pos} does not fit the {target} value range")]
    IntegerOverflow {
        /// Position of the offending literal.
        pos: SourcePos,
        /// The literal text as scanned, for diagnostics.
        literal: String,
        /// Target type name such as "I1" or "U4".
        target: &'static str,
    },

    /// A floating-point literal is not a finite value of the target type.
    #[error("floating-point literal {text:?} at {pos} is not a finite {target} value")]
    NonFiniteFloat {
        /// Position of the offending literal.
        pos: SourcePos,
        /// The literal text as scanned.
        text: String,
        /// Target type name, "F4" or "F8".
        target: &'static str,
    },

    /// A hexadecimal byte inside an `<A>` item is not an ASCII character.
    #[error("hexadecimal byte 0x{byte:02X} at {pos} is not ASCII; <A> accepts 0x00 through 0x7F")]
    InvalidAsciiByte {
        /// Position of the offending `0xHH` element.
        pos: SourcePos,
        /// Byte value at or above 0x80.
        byte: u8,
    },

    /// A token appeared where the grammar requires something else.
    #[error("unexpected {found} at {pos}: expected {expected}")]
    UnexpectedToken {
        /// Position of the unexpected token.
        pos: SourcePos,
        /// Short description of what the grammar requires at this point.
        expected: &'static str,
        /// Short description of what was found, e.g. `">"` or `"end of input"`.
        found: String,
    },

    /// The input ended before the terminating period of a message.
    #[error("message ended before the terminating period; input exhausted at {pos}")]
    MissingTerminator {
        /// Position of end of input where the period was expected.
        pos: SourcePos,
    },

    /// Syntactically complete input is followed by extra content.
    #[error("unexpected trailing content {preview:?} at {pos}")]
    TrailingInput {
        /// Position where the trailing content starts.
        pos: SourcePos,
        /// First few characters of the trailing content, for diagnostics.
        preview: String,
    },

    /// List nesting reached the configured maximum depth.
    #[error(
        "list nesting at {pos} reaches depth {depth}, exceeding the configured maximum {max_depth}"
    )]
    DepthExceeded {
        /// Position of the `<` that would exceed the limit.
        pos: SourcePos,
        /// Nesting depth this item would occupy.
        depth: usize,
        /// Configured maximum depth from `DecodeLimits`.
        max_depth: usize,
    },

    /// The total node count of the item tree reached the configured maximum.
    #[error("item tree would exceed the configured maximum of {max_total_items} nodes at {pos}")]
    TotalItemsExceeded {
        /// Position of the node that would exceed the limit.
        pos: SourcePos,
        /// Configured maximum total nodes from `DecodeLimits`.
        max_total_items: usize,
    },

    /// A non-List item body needs more bytes than the configured maximum.
    #[error("item body at {pos} needs {required_bytes} bytes, exceeding the configured maximum {max_item_bytes}")]
    ItemBytesExceeded {
        /// Position of the item type token.
        pos: SourcePos,
        /// Encoded body byte count the item requires.
        required_bytes: usize,
        /// Configured maximum per-item bytes from `DecodeLimits`.
        max_item_bytes: usize,
    },

    /// A List holds more direct children than the configured maximum.
    #[error("list at {pos} holds more than the configured maximum of {max_list_items} children")]
    ListItemsExceeded {
        /// Position of the list's type token.
        pos: SourcePos,
        /// Configured maximum direct children from `DecodeLimits`.
        max_list_items: usize,
    },
}

/// Failure while rendering an item or message as canonical SML text.
///
/// All checks run before any output text is written, so a formatting error
/// never yields a partial string. `non_exhaustive` mirrors the crate's other
/// error types.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum FormatError {
    /// The tree contains a SECS-II type the strict SML dialect cannot
    /// represent (JIS-8, localized text).
    #[error("item type {name} cannot be represented in SML")]
    UnsupportedType {
        /// Name of the refused variant, e.g. "Jis8" or "Localized".
        name: &'static str,
    },

    /// The tree contains a NaN or infinite F4/F8 value; SML only represents
    /// finite floating-point values.
    #[error(
        "{kind} contains a non-finite value (NaN or infinity); SML only represents finite floats"
    )]
    NonFiniteFloat {
        /// Type holding the value, "F4" or "F8".
        kind: &'static str,
    },

    /// The tree nests Lists deeper than any SML parser can be configured to
    /// accept: `DecodeLimits` caps nesting at 256, so the rendered text could
    /// never be reparsed.
    #[error("list nesting reaches depth {depth}, exceeding the maximum {max_depth} any SML parser can accept")]
    DepthExceeded {
        /// Nesting depth of the deepest List in the tree.
        depth: usize,
        /// Hard parser-side ceiling (`MAX_DECODE_NESTING_DEPTH`), always 256.
        max_depth: usize,
    },

    /// The tree cannot be measured as an E5-wire item (for example a body
    /// longer than the three-byte length field), so no canonical SML text
    /// exists for it.
    #[error("item is not encodable on the E5 wire")]
    NotEncodable {
        /// Underlying encoder failure from the measurement pass.
        #[from]
        source: EncodeError,
    },
}
