//! Canonical SML writer for the strict dialect.
//!
//! This module renders [`SecsItem`] trees and [`SmlMessage`] values as
//! canonical SECS Message Language text in one of two layouts:
//! [`FormatStyle::Compact`] keeps everything on a single line, while
//! [`FormatStyle::Pretty`] breaks lists with children across lines using
//! two-space indentation. Both styles emit byte-identical token sequences and
//! differ only in layout whitespace, so both round-trip identically through
//! any parser whose limits admit the tree.
//!
//! Guarantees held by every public entry point:
//!
//! - Validation runs completely before any output text is produced: the tree
//!   is first walked for SML-unsupported types (`Jis8`, `Localized`),
//!   non-finite F4/F8 values, and List nesting deeper than any parser could
//!   ever accept, and then measured by the E5 encoder plan (ruling out bodies
//!   and list child counts that exceed the 24-bit length field). A formatting
//!   failure therefore never yields a partial string.
//! - Rendering traverses the tree with an explicit work stack, never
//!   recursion, so host stack usage is constant regardless of tree depth.

use std::fmt::{Display, LowerExp, Write as _};

use super::error::FormatError;
use super::message::SmlMessage;
use crate::secs2::codec::EncodedItemPlan;
use crate::secs2::{SecsItem, MAX_DECODE_NESTING_DEPTH};

/// Layout selection for canonical SML rendering.
///
/// Both styles produce the same tokens with the same spelling; they differ
/// only in where whitespace and line breaks are inserted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FormatStyle {
    /// Single-line layout with one space between adjacent tokens.
    #[default]
    Compact,
    /// Two-space indented layout; lists with children place each child on
    /// its own line and the closing `>` on a line of its own.
    Pretty,
}

/// Canonical SML formatter for items and complete messages.
///
/// A formatter is a plain layout configuration: it holds no mutable state
/// between calls and can be freely copied. Construct it with
/// [`SmlFormatter::new`], or via `Default`, which selects
/// [`FormatStyle::Compact`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SmlFormatter {
    /// Layout style applied by every formatting call on this instance.
    style: FormatStyle,
}

impl SmlFormatter {
    /// Creates a formatter that renders in `style`.
    ///
    /// Returns the stateless formatter value; see [`FormatStyle`] for how the
    /// two layouts differ.
    #[must_use]
    pub const fn new(style: FormatStyle) -> Self {
        Self { style }
    }

    /// Renders `item` as one canonical SML item text such as
    /// `<L[2] <A[1] "x"> <B[0]>>`.
    ///
    /// The item is fully validated before any text is produced, so a refusal
    /// never yields a partial string. Returns the complete item text in this
    /// formatter's layout style.
    ///
    /// # Errors
    ///
    /// Returns [`FormatError::UnsupportedType`] when the tree contains a
    /// `Jis8` or `Localized` node, [`FormatError::NonFiniteFloat`] when the
    /// tree contains a NaN or infinite F4/F8 value, and
    /// [`FormatError::DepthExceeded`] when Lists nest deeper than 256, the
    /// hard ceiling no SML parser can be configured beyond. These semantic
    /// refusals are reported before [`FormatError::NotEncodable`], which is
    /// returned when the tree cannot be measured as an E5 wire item.
    pub fn format_item(&self, item: &SecsItem) -> Result<String, FormatError> {
        let encoded_length = validate_and_measure(item)?;
        // SML text is never shorter than the binary form; doubling the
        // measured E5 length is a capacity heuristic that avoids most buffer
        // regrowth without over-committing for large arrays.
        let capacity_hint = encoded_length.saturating_mul(2);
        Ok(render_root(item, self.style, capacity_hint))
    }

    /// Renders `message` as canonical SML text ending in a period.
    ///
    /// The output is `SxFy`, optionally ` W`, optionally the body's item
    /// text, and the terminating period attached directly to the final
    /// character with no preceding space. A present body is fully validated
    /// before any text is produced, so a refusal never yields a partial
    /// string; a bodyless message needs no validation. Returns the complete
    /// message text in this formatter's layout style.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`SmlFormatter::format_item`] for the
    /// message body when one is present; in particular the body's List
    /// nesting must stay within the parser-side hard ceiling of 256. A
    /// bodyless message needs no validation and never fails.
    pub fn format_message(&self, message: &SmlMessage) -> Result<String, FormatError> {
        let pretty = matches!(self.style, FormatStyle::Pretty);
        // Validate the whole body (when present) before writing any text so
        // an error can never escape together with a partially built string.
        let (body, body_encoded_length) = match message.body() {
            Some(body) => (Some(body), validate_and_measure(body)?),
            None => (None, 0),
        };
        let stream = message.stream().get();
        let function = message.function().get();
        // The body dominates the output size, so the same doubled-lower-bound
        // heuristic applies; the small addition covers the header words.
        let mut out =
            String::with_capacity(body_encoded_length.saturating_mul(2).saturating_add(16));
        write!(out, "S{stream}F{function}").expect("writing into a String cannot fail");
        if message.wait_bit() {
            out.push_str(" W");
        }
        if let Some(body) = body {
            out.push(' ');
            write_item_text(&mut out, body, pretty);
        }
        // The terminating period attaches directly to the final character.
        out.push('.');
        Ok(out)
    }
}

/// Validates `item` for SML rendering and returns its E5 encoded byte length.
///
/// Validation is complete before any caller writes text: a cheap semantic
/// walk first rules out SML-unsupported types, non-finite floats, and List
/// nesting no parser could accept, and only then does the E5 measurement pass
/// rule out unencodable trees. The returned length is used only as an
/// output-capacity hint.
///
/// # Errors
///
/// Returns [`FormatError::UnsupportedType`] for `Jis8`/`Localized` nodes,
/// [`FormatError::NonFiniteFloat`] for NaN or infinite F4/F8 values, and
/// [`FormatError::DepthExceeded`] for List nesting beyond the parser-side
/// hard ceiling, all before [`FormatError::NotEncodable`] from the encoder
/// measurement pass.
fn validate_and_measure(item: &SecsItem) -> Result<usize, FormatError> {
    check_sml_semantics(item)?; // cheap semantic + depth rejection first
    let plan = EncodedItemPlan::new(item)?; // E5 encodability/length second
    Ok(plan.encoded_length())
}

/// Walks the tree with an explicit stack, rejecting SML-unrenderable nodes.
///
/// `root` is visited in preorder. Every stack entry carries the List nesting
/// level of its item, counted exactly the way the parser counts it: a root
/// List sits at level 1, each List inside another List adds one, and scalar
/// leaves never add depth. A List whose own level exceeds
/// [`MAX_DECODE_NESTING_DEPTH`] — the hard ceiling that no parser can be
/// configured beyond — aborts the walk, as does the first `Jis8`/`Localized`
/// node or the first non-finite F4/F8 value (scanned in element order within
/// its item). Returns `Ok(())` when the whole tree is SML-renderable.
///
/// # Errors
///
/// Returns [`FormatError::UnsupportedType`] for `Jis8`/`Localized` nodes,
/// [`FormatError::NonFiniteFloat`] for NaN or infinite F4/F8 values, and
/// [`FormatError::DepthExceeded`] for List nesting deeper than
/// [`MAX_DECODE_NESTING_DEPTH`].
fn check_sml_semantics(root: &SecsItem) -> Result<(), FormatError> {
    // The root enters at List nesting level 1; only List items consume depth.
    let mut stack: Vec<(&SecsItem, usize)> = vec![(root, 1)];
    while let Some((item, level)) = stack.pop() {
        match item {
            SecsItem::Jis8(_) => {
                return Err(FormatError::UnsupportedType { name: "Jis8" });
            }
            SecsItem::Localized(_) => {
                return Err(FormatError::UnsupportedType { name: "Localized" });
            }
            SecsItem::F4(values) => {
                if values.iter().any(|value| !value.is_finite()) {
                    return Err(FormatError::NonFiniteFloat { kind: "F4" });
                }
            }
            SecsItem::F8(values) => {
                if values.iter().any(|value| !value.is_finite()) {
                    return Err(FormatError::NonFiniteFloat { kind: "F8" });
                }
            }
            // Children are pushed reversed so they pop in source order,
            // keeping the walk (and any error it reports) preorder. A List
            // consumes one nesting level, so its children enter at `level + 1`.
            SecsItem::List(children) => {
                if level > MAX_DECODE_NESTING_DEPTH {
                    return Err(FormatError::DepthExceeded {
                        depth: level,
                        max_depth: MAX_DECODE_NESTING_DEPTH,
                    });
                }
                stack.extend(children.iter().rev().map(|child| (child, level + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

/// One deferred unit of rendering work on the explicit traversal stack.
#[derive(Clone, Copy)]
enum WorkEntry<'a> {
    /// Render the referenced item; `indent` is its Pretty nesting level.
    Item {
        /// Item whose token text is written when this entry pops.
        item: &'a SecsItem,
        /// Pretty indent level (two spaces per level) for this item's lines.
        indent: usize,
    },
    /// Write the pending closing `>` of a list opened earlier.
    CloseList {
        /// Pretty indent level of the list being closed (its own level, not
        /// the children's).
        indent: usize,
    },
}

/// Renders `root` into a fresh `String`, reserving `capacity_hint` bytes first.
///
/// `capacity_hint` is only a performance hint (callers pass a lower-bound
/// estimate); correctness never depends on it. Returns the complete canonical
/// item text in the requested `style`.
fn render_root(root: &SecsItem, style: FormatStyle, capacity_hint: usize) -> String {
    let pretty = matches!(style, FormatStyle::Pretty);
    let mut out = String::with_capacity(capacity_hint);
    write_item_text(&mut out, root, pretty);
    out
}

/// Appends the canonical text of `root` to `out` using an explicit work stack.
///
/// `pretty` selects the line-broken layout; compact layout is produced
/// otherwise. The caller has already positioned the cursor, so the first
/// token continues the current line; every later token is preceded by the
/// layout gap written by [`write_token_gap`]. Only lists with at least one
/// child span multiple lines in Pretty layout; every other item is always a
/// single token.
fn write_item_text(out: &mut String, root: &SecsItem, pretty: bool) {
    let mut stack = vec![WorkEntry::Item {
        item: root,
        indent: 0,
    }];
    // The first token continues the caller's line; later item tokens get a
    // layout gap. A list's closing `>` never takes a gap in compact.
    let mut at_first_token = true;
    while let Some(entry) = stack.pop() {
        match entry {
            WorkEntry::CloseList { indent } => {
                // The closing `>` follows the list's last child directly:
                // glued with no space in compact, alone on its own line at
                // the list's indent in pretty.
                if pretty {
                    write_line_start(out, indent);
                }
                out.push('>');
            }
            WorkEntry::Item { item, indent } => {
                if at_first_token {
                    at_first_token = false;
                } else if pretty {
                    write_line_start(out, indent);
                } else {
                    out.push(' ');
                }
                if let Some(children) = write_item_open(out, item) {
                    // Schedule the deferred `>` first so it pops after the
                    // children; the children are then pushed reversed on top
                    // and therefore pop in source order.
                    stack.push(WorkEntry::CloseList { indent });
                    let child_indent = indent.saturating_add(1);
                    stack.extend(children.iter().rev().map(|child| WorkEntry::Item {
                        item: child,
                        indent: child_indent,
                    }));
                }
            }
        }
    }
}

/// Starts a new Pretty line in `out`, indented by `indent` levels of two
/// spaces.
///
/// Used before every token that does not continue the current line: each
/// list child and each list's standalone closing `>`.
fn write_line_start(out: &mut String, indent: usize) {
    out.push('\n');
    for _ in 0..indent {
        out.push_str("  ");
    }
}

/// Appends the leading text of one item and reports deferred children.
///
/// Leaves and empty lists are written completely (for example
/// `<B[3] 0x01 0xAA 0xFF>` or `<L[0]>`). A list with children writes only its
/// `<L[n]` header and returns `Some(children)` so the caller can schedule the
/// children followed by the list's deferred closing `>`. Returns `None`
/// whenever the item's text is already complete.
///
/// # Panics
///
/// Panicking here is unreachable through the public API: reaching a `Jis8`
/// or `Localized` node would mean [`validate_and_measure`] was skipped, which
/// every public entry point performs first.
fn write_item_open<'a>(out: &mut String, item: &'a SecsItem) -> Option<&'a [SecsItem]> {
    match item {
        SecsItem::List(children) => {
            let count = children.len();
            write!(out, "<L[{count}]").expect("writing into a String cannot fail");
            if children.is_empty() {
                out.push('>');
                None
            } else {
                Some(children)
            }
        }
        SecsItem::Binary(bytes) => {
            let count = bytes.len();
            write!(out, "<B[{count}]").expect("writing into a String cannot fail");
            for &byte in bytes {
                write!(out, " 0x{byte:02X}").expect("writing into a String cannot fail");
            }
            out.push('>');
            None
        }
        SecsItem::Boolean(values) => {
            let count = values.len();
            write!(out, "<BOOLEAN[{count}]").expect("writing into a String cannot fail");
            for &value in values {
                out.push_str(if value { " TRUE" } else { " FALSE" });
            }
            out.push('>');
            None
        }
        SecsItem::Ascii(text) => {
            let bytes = text.as_str().as_bytes();
            let count = bytes.len();
            write!(out, "<A[{count}]").expect("writing into a String cannot fail");
            write_ascii_body(out, bytes);
            out.push('>');
            None
        }
        SecsItem::Jis8(_) | SecsItem::Localized(_) => {
            unreachable!("SML validation rejects Jis8 and Localized before rendering")
        }
        SecsItem::I8(values) => {
            write_scalar_item(out, "I8", values);
            None
        }
        SecsItem::I1(values) => {
            write_scalar_item(out, "I1", values);
            None
        }
        SecsItem::I2(values) => {
            write_scalar_item(out, "I2", values);
            None
        }
        SecsItem::I4(values) => {
            write_scalar_item(out, "I4", values);
            None
        }
        SecsItem::F8(values) => {
            write_float_item(out, "F8", values);
            None
        }
        SecsItem::F4(values) => {
            write_float_item(out, "F4", values);
            None
        }
        SecsItem::U8(values) => {
            write_scalar_item(out, "U8", values);
            None
        }
        SecsItem::U1(values) => {
            write_scalar_item(out, "U1", values);
            None
        }
        SecsItem::U2(values) => {
            write_scalar_item(out, "U2", values);
            None
        }
        SecsItem::U4(values) => {
            write_scalar_item(out, "U4", values);
            None
        }
    }
}

/// Writes one complete scalar item token `<TYPE[n] v1 v2 ...>`.
///
/// `type_name` is the SML type keyword (`I4`, `U8`, ...). Each value is
/// written with its own Rust `Display`, which is plain decimal for integers.
fn write_scalar_item<T>(out: &mut String, type_name: &str, values: &[T])
where
    T: Display,
{
    let count = values.len();
    write!(out, "<{type_name}[{count}]").expect("writing into a String cannot fail");
    for value in values {
        write!(out, " {value}").expect("writing into a String cannot fail");
    }
    out.push('>');
}

/// Positional float renderings longer than this many characters are
/// re-rendered in scientific notation (`LowerExp`), which is also a
/// shortest round-trip form. Twenty characters keeps every
/// human-friendly positional value (`1.5`, `-0.0`, `0.0000001`) as-is
/// while capping denormal and extreme-exponent output.
const SCIENTIFIC_NOTATION_SWITCH_LENGTH: usize = 20;

/// Writes one complete floating-point item token `<F4/F8[n] v1 v2 ...>`.
///
/// Each value is written with Rust's positional shortest decimal that
/// round-trips bit-exactly, switching to scientific notation (`LowerExp`,
/// itself a shortest round-trip form) when the positional rendering would
/// exceed [`SCIENTIFIC_NOTATION_SWITCH_LENGTH`] characters, which keeps
/// denormals and extreme exponents compact while ordinary values stay
/// human-friendly. Because the positional `Display` form drops the fraction
/// for integral values (`5.0` prints as `5`, negative zero as `-0`), and the
/// strict scanner would classify such text as an integer token rather than a
/// float token, a `.0` suffix is appended whenever the rendered text
/// contains neither `.` nor an exponent marker. Both spellings reparse
/// bit-exactly, so the formatter's own output always reparses into the
/// identical `SecsItem`. The scratch buffer is reused for every value so
/// formatting a large array does not allocate once per element.
fn write_float_item<T>(out: &mut String, type_name: &str, values: &[T])
where
    T: Display + LowerExp,
{
    let count = values.len();
    write!(out, "<{type_name}[{count}]").expect("writing into a String cannot fail");
    let mut text = String::new();
    for value in values {
        text.clear();
        write!(text, "{value}").expect("writing into a String cannot fail");
        // Float formatting emits only ASCII characters, so byte length is
        // identical to character count and avoids scanning UTF-8 codepoints.
        if text.len() > SCIENTIFIC_NOTATION_SWITCH_LENGTH {
            // The positional form is a denormal or extreme-exponent sprawl;
            // LowerExp is equally round-trip exact and far shorter.
            text.clear();
            write!(text, "{value:e}").expect("writing into a String cannot fail");
        }
        if !text.contains('.') && !text.contains(['e', 'E']) {
            text.push_str(".0");
        }
        write!(out, " {text}").expect("writing into a String cannot fail");
    }
    out.push('>');
}

/// Appends the body fragments of an ASCII item between its header and `>`.
///
/// Scans `bytes` left to right: every maximal run of quotable bytes
/// (0x20..=0x7E except `"` and `\`) becomes one quoted fragment `"run"`, and
/// every other byte becomes a standalone `0xHH` fragment with uppercase
/// digits. Fragments are separated by single spaces; an empty input writes
/// nothing, leaving the bare `<A[0]` header plus `>`.
fn write_ascii_body(out: &mut String, bytes: &[u8]) {
    let mut index = 0;
    while index < bytes.len() {
        if is_quotable_ascii(bytes[index]) {
            let run_start = index;
            while index < bytes.len() && is_quotable_ascii(bytes[index]) {
                index += 1;
            }
            out.push_str(" \"");
            // Quotable bytes are all printable ASCII, so widening to `char`
            // is exact and needs no validation.
            for &byte in &bytes[run_start..index] {
                out.push(byte as char);
            }
            out.push('"');
        } else {
            let byte = bytes[index];
            write!(out, " 0x{byte:02X}").expect("writing into a String cannot fail");
            index += 1;
        }
    }
}

/// Returns whether `byte` may appear inside a quoted SML ASCII fragment.
///
/// Quotable bytes are the printable ASCII range 0x20..=0x7E excluding the
/// double quote (0x22) and backslash (0x5C), which SML always spells as
/// `0xHH` so quoted runs stay unambiguous.
fn is_quotable_ascii(byte: u8) -> bool {
    (0x20..=0x7E).contains(&byte) && byte != b'"' && byte != b'\\'
}

#[cfg(test)]
mod tests {
    //! Unit tests pinning the canonical token text of every item format, the
    //! two layout styles, message assembly, and every validation refusal.

    use super::*;
    use crate::hsms::{Function, Stream};
    use crate::secs2::codec::header::FormatCode;
    use crate::secs2::codec::EncodeError;
    use crate::secs2::{AsciiString, LocalizedEncodingCode, LocalizedString};

    /// Formats `item` in Compact style, failing the test on validation errors.
    fn compact(item: &SecsItem) -> String {
        SmlFormatter::new(FormatStyle::Compact)
            .format_item(item)
            .expect("test item must validate")
    }

    /// Formats `item` in Pretty style, failing the test on validation errors.
    fn pretty(item: &SecsItem) -> String {
        SmlFormatter::new(FormatStyle::Pretty)
            .format_item(item)
            .expect("test item must validate")
    }

    /// Formats `message` in Compact style, failing the test on validation errors.
    fn compact_message(message: &SmlMessage) -> String {
        SmlFormatter::new(FormatStyle::Compact)
            .format_message(message)
            .expect("test message must validate")
    }

    /// Formats `message` in Pretty style, failing the test on validation errors.
    fn pretty_message(message: &SmlMessage) -> String {
        SmlFormatter::new(FormatStyle::Pretty)
            .format_message(message)
            .expect("test message must validate")
    }

    /// Builds an ASCII item from `text`, failing the test on non-ASCII input.
    fn ascii_item(text: &str) -> SecsItem {
        SecsItem::Ascii(AsciiString::new(text).expect("test text must be ASCII"))
    }

    /// Builds a message from raw identifiers, failing the test on an
    /// out-of-range stream number.
    fn message(stream: u8, function: u8, wait_bit: bool, body: Option<SecsItem>) -> SmlMessage {
        SmlMessage::new(
            Stream::new(stream).expect("test stream is in range"),
            Function::new(function),
            wait_bit,
            body,
        )
    }

    /// Removes every whitespace character from `text`, erasing all layout-only
    /// differences between the two styles.
    ///
    /// Full removal (not collapsing runs to single spaces) is required
    /// because Compact glues a list's closing `>` onto its last child while
    /// Pretty separates it with a newline; intra-token spaces occur
    /// identically in both styles, so removing them symmetrically keeps the
    /// comparison meaningful.
    fn strip_whitespace(text: &str) -> String {
        text.chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    /// Wraps `inner` in `depth` nested single-child Lists.
    ///
    /// The outermost List sits at nesting level 1 (matching how the parser
    /// counts depth) and the innermost wrapper at `depth`; `inner` itself is
    /// a leaf that adds no depth.
    fn nested_lists(depth: usize, inner: SecsItem) -> SecsItem {
        let mut item = inner;
        for _ in 0..depth {
            item = SecsItem::List(vec![item]);
        }
        item
    }

    /// Confirms every empty item prints its zero element count with no body.
    #[test]
    fn compact_empty_items_render_zero_counts() {
        assert_eq!(compact(&SecsItem::List(Vec::new())), "<L[0]>");
        assert_eq!(compact(&SecsItem::Binary(Vec::new())), "<B[0]>");
        assert_eq!(compact(&ascii_item("")), "<A[0]>");
        assert_eq!(compact(&SecsItem::Boolean(Vec::new())), "<BOOLEAN[0]>");
    }

    /// Confirms binary bytes render as `0x` plus two uppercase hex digits.
    #[test]
    fn compact_binary_renders_uppercase_hex_byte_tokens() {
        assert_eq!(
            compact(&SecsItem::Binary(vec![0x01, 0xAA, 0xFF])),
            "<B[3] 0x01 0xAA 0xFF>"
        );
    }

    /// Confirms a zero byte still uses exactly two hex digits.
    #[test]
    fn compact_binary_zero_byte_uses_two_hex_digits() {
        assert_eq!(compact(&SecsItem::Binary(vec![0x00])), "<B[1] 0x00>");
    }

    /// Confirms booleans render as the words TRUE and FALSE.
    #[test]
    fn compact_boolean_pair_renders_true_false_words() {
        assert_eq!(
            compact(&SecsItem::Boolean(vec![true, false])),
            "<BOOLEAN[2] TRUE FALSE>"
        );
    }

    /// Confirms plain printable ASCII is one quoted run.
    #[test]
    fn compact_ascii_plain_text_is_one_quoted_run() {
        assert_eq!(compact(&ascii_item("abc")), "<A[3] \"abc\">");
    }

    /// Confirms a tab byte splits the quoted run and renders as 0x09.
    #[test]
    fn compact_ascii_tab_byte_splits_into_hex_fragment() {
        assert_eq!(compact(&ascii_item("a\tb")), "<A[3] \"a\" 0x09 \"b\">");
    }

    /// Confirms an embedded double quote renders as 0x22 between quoted runs.
    ///
    /// The prose example in the task description lists `A[13]`, but the
    /// string `he said "hi"` is 12 bytes (8 + 1 + 2 + 1); the formatter
    /// prints the true byte count, which this golden pins.
    #[test]
    fn compact_ascii_embedded_double_quote_becomes_hex_fragment() {
        assert_eq!(
            compact(&ascii_item("he said \"hi\"")),
            "<A[12] \"he said \" 0x22 \"hi\" 0x22>"
        );
    }

    /// Confirms a backslash byte renders as 0x5C, never inside a quoted run.
    #[test]
    fn compact_ascii_backslash_byte_becomes_hex_fragment() {
        assert_eq!(compact(&ascii_item("a\\b")), "<A[3] \"a\" 0x5C \"b\">");
    }

    /// Confirms spaces (0x20) are quotable and stay inside the quoted run.
    #[test]
    fn compact_ascii_inner_spaces_stay_inside_the_quoted_run() {
        assert_eq!(compact(&ascii_item("a b c")), "<A[5] \"a b c\">");
    }

    /// Confirms I4 renders plain decimals including -1 and i32::MAX.
    #[test]
    fn compact_i4_triple_renders_decimal_including_extremes() {
        assert_eq!(
            compact(&SecsItem::I4(vec![-1, 0, i32::MAX])),
            "<I4[3] -1 0 2147483647>"
        );
    }

    /// Confirms U1 renders plain decimals including 255.
    #[test]
    fn compact_u1_triple_renders_decimal_including_255() {
        assert_eq!(
            compact(&SecsItem::U1(vec![0, 128, 255])),
            "<U1[3] 0 128 255>"
        );
    }

    /// Confirms I8 renders the full i64::MIN decimal.
    #[test]
    fn compact_i8_min_renders_full_i64_decimal() {
        assert_eq!(
            compact(&SecsItem::I8(vec![i64::MIN])),
            "<I8[1] -9223372036854775808>"
        );
    }

    /// Confirms U8 renders the full u64::MAX decimal.
    #[test]
    fn compact_u8_max_renders_full_u64_decimal() {
        assert_eq!(
            compact(&SecsItem::U8(vec![u64::MAX])),
            "<U8[1] 18446744073709551615>"
        );
    }

    /// Confirms the remaining integer widths render their own decimal ranges.
    #[test]
    fn compact_other_integer_widths_render_decimal() {
        assert_eq!(compact(&SecsItem::I1(vec![i8::MIN])), "<I1[1] -128>");
        assert_eq!(compact(&SecsItem::I2(vec![i16::MAX])), "<I2[1] 32767>");
        assert_eq!(compact(&SecsItem::U2(vec![u16::MAX])), "<U2[1] 65535>");
        assert_eq!(compact(&SecsItem::U4(vec![u32::MAX])), "<U4[1] 4294967295>");
    }

    /// Confirms F4 uses shortest round-trip Display, printing `-0.0` for
    /// negative zero (the bare Display form `-0` would lex as an integer
    /// token, so the `.0` suffix keeps the output reparseable).
    #[test]
    fn compact_f4_pair_renders_display_shortest_values() {
        assert_eq!(compact(&SecsItem::F4(vec![1.5, -0.0])), "<F4[2] 1.5 -0.0>");
        assert_eq!(
            "-0.0".parse::<f32>().map(f32::to_bits),
            Ok((-0.0_f32).to_bits())
        );
    }

    /// Confirms integral floats gain a `.0` suffix so the strict scanner
    /// always sees a float token, preserving item-level round-trips.
    #[test]
    fn compact_integral_floats_gain_fraction_suffix() {
        assert_eq!(compact(&SecsItem::F4(vec![5.0])), "<F4[1] 5.0>");
        assert_eq!(compact(&SecsItem::F4(vec![0.0])), "<F4[1] 0.0>");
        assert_eq!(compact(&SecsItem::F8(vec![-3.0, 2.0])), "<F8[2] -3.0 2.0>");
    }

    /// Confirms F8 renders 1e-7 positionally and round-trips bit-exactly.
    ///
    /// Rust float `Display` never uses exponent notation, so 1e-7 prints as
    /// the shortest positional round-trip decimal `0.0000001`, not `1e-7`.
    #[test]
    fn compact_f8_small_value_renders_positional_display_form() {
        assert_eq!(compact(&SecsItem::F8(vec![1e-7])), "<F8[1] 0.0000001>");
        assert_eq!(
            "0.0000001".parse::<f64>().map(f64::to_bits),
            Ok(1e-7_f64.to_bits())
        );
    }

    /// Confirms the smallest positive F8 denormal (5e-324) switches to
    /// scientific notation instead of a 326-character positional sprawl, and
    /// that the pinned exponent literal reparses to the identical bits.
    #[test]
    fn compact_f8_smallest_denormal_switches_to_scientific_notation() {
        assert_eq!(compact(&SecsItem::F8(vec![5e-324])), "<F8[1] 5e-324>");
        assert_eq!(
            "5e-324".parse::<f64>().map(f64::to_bits),
            Ok(5e-324_f64.to_bits())
        );
    }

    /// Confirms the negative smallest denormal switches too, carrying its
    /// sign into the exponent spelling, and reparses to the same bits.
    #[test]
    fn compact_f8_negative_smallest_denormal_switches_to_scientific_notation() {
        assert_eq!(compact(&SecsItem::F8(vec![-5e-324])), "<F8[1] -5e-324>");
        assert_eq!(
            "-5e-324".parse::<f64>().map(f64::to_bits),
            Ok((-5e-324_f64).to_bits())
        );
    }

    /// Confirms the smallest normal F8 (f64::MIN_POSITIVE) switches to the
    /// pinned `LowerExp` shortest round-trip spelling and reparses exactly.
    #[test]
    fn compact_f8_min_positive_switches_to_scientific_notation() {
        assert_eq!(
            compact(&SecsItem::F8(vec![f64::MIN_POSITIVE])),
            "<F8[1] 2.2250738585072014e-308>"
        );
        assert_eq!(
            "2.2250738585072014e-308".parse::<f64>().map(f64::to_bits),
            Ok(f64::MIN_POSITIVE.to_bits())
        );
    }

    /// Confirms the largest finite F4 switches from its 39-digit positional
    /// integer form to the compact exponent spelling, bit-exactly.
    #[test]
    fn compact_f4_max_switches_to_scientific_notation() {
        assert_eq!(
            compact(&SecsItem::F4(vec![f32::MAX])),
            "<F4[1] 3.4028235e38>"
        );
        assert_eq!(
            "3.4028235e38".parse::<f32>().map(f32::to_bits),
            Ok(f32::MAX.to_bits())
        );
    }

    /// Pins the switch boundary's lower edge: 1e-18's positional form is
    /// exactly [`SCIENTIFIC_NOTATION_SWITCH_LENGTH`] characters, so it stays
    /// positional rather than switching to `1e-18`.
    #[test]
    fn float_positional_form_of_exactly_switch_length_stays_positional() {
        assert_eq!(format!("{}", 1e-18_f64).chars().count(), 20);
        assert_eq!(
            compact(&SecsItem::F8(vec![1e-18])),
            "<F8[1] 0.000000000000000001>"
        );
    }

    /// Pins the switch boundary's upper edge: 1e-19's positional form is one
    /// character past [`SCIENTIFIC_NOTATION_SWITCH_LENGTH`], so it switches
    /// to the equally round-trip exact exponent spelling.
    #[test]
    fn float_positional_form_one_past_switch_length_switches_to_exponent() {
        assert_eq!(format!("{}", 1e-19_f64).chars().count(), 21);
        assert_eq!(compact(&SecsItem::F8(vec![1e-19])), "<F8[1] 1e-19>");
    }

    /// Extracts the space-separated value literals from a rendered scalar
    /// item token `<TYPE[n] v1 v2 ...>`.
    ///
    /// `text` must be a single-line item token as produced by `compact`;
    /// returns the slice between the header's first space and the final
    /// `>`, split on single spaces.
    fn value_literals(text: &str) -> Vec<&str> {
        let start = text.find(' ').expect("header is followed by a space") + 1;
        text[start..text.len() - 1].split(' ').collect()
    }

    /// Confirms a mixed positional/exponent F8 item renders both spellings
    /// in one token and every rendered literal reparses to its original
    /// value's exact bit pattern.
    #[test]
    fn rendered_float_literals_of_mixed_item_reparse_to_identical_bits() {
        let values = [1.5_f64, 5e-324, f64::MIN_POSITIVE, 1e-19, -0.0];
        let text = compact(&SecsItem::F8(values.to_vec()));
        assert_eq!(
            text,
            "<F8[5] 1.5 5e-324 2.2250738585072014e-308 1e-19 -0.0>"
        );
        for (literal, value) in value_literals(&text).into_iter().zip(values) {
            assert_eq!(
                literal.parse::<f64>().map(f64::to_bits),
                Ok(value.to_bits()),
                "literal {literal} must reparse bit-exactly"
            );
        }
    }

    /// Pins the Compact golden for a list mixing an ASCII leaf and an empty
    /// binary leaf on one line.
    #[test]
    fn compact_nested_list_renders_on_one_line() {
        let item = SecsItem::List(vec![ascii_item("x"), SecsItem::Binary(Vec::new())]);
        assert_eq!(compact(&item), "<L[2] <A[1] \"x\"> <B[0]>>");
    }

    /// Pins the Pretty golden for the same tree: one line per child and the
    /// closing `>` alone at the root indent.
    #[test]
    fn pretty_nested_list_puts_each_child_on_its_own_line() {
        let item = SecsItem::List(vec![ascii_item("x"), SecsItem::Binary(Vec::new())]);
        assert_eq!(pretty(&item), "<L[2]\n  <A[1] \"x\">\n  <B[0]>\n>");
    }

    /// Pins the Pretty golden for two-level nesting: the inner list's
    /// children indent twice and its closing `>` returns to indent one.
    #[test]
    fn pretty_list_inside_list_indents_two_levels() {
        let inner = SecsItem::List(vec![ascii_item("x"), SecsItem::Binary(Vec::new())]);
        let item = SecsItem::List(vec![inner]);
        assert_eq!(
            pretty(&item),
            "<L[1]\n  <L[2]\n    <A[1] \"x\">\n    <B[0]>\n  >\n>"
        );
    }

    /// Confirms an empty list never breaks lines in Pretty style.
    #[test]
    fn pretty_empty_list_stays_on_one_line() {
        assert_eq!(pretty(&SecsItem::List(Vec::new())), "<L[0]>");
    }

    /// Confirms a non-List item is a single line in Pretty style too.
    #[test]
    fn pretty_scalar_item_stays_on_one_line() {
        assert_eq!(
            pretty(&SecsItem::Binary(vec![0x01, 0xAA, 0xFF])),
            "<B[3] 0x01 0xAA 0xFF>"
        );
    }

    /// Confirms a bodyless message is the bare header plus period.
    #[test]
    fn message_without_body_appends_bare_period() {
        assert_eq!(compact_message(&message(1, 1, false, None)), "S1F1.");
    }

    /// Confirms the W-Bit appears between header and period without a body.
    #[test]
    fn message_without_body_and_wait_bit_appends_bare_period() {
        assert_eq!(compact_message(&message(1, 1, true, None)), "S1F1 W.");
    }

    /// Confirms a scalar message body keeps its period glued to the token in
    /// both styles, since scalars never break lines.
    #[test]
    fn message_with_ascii_body_attaches_period_to_token_in_both_styles() {
        let with_body = message(1, 2, false, Some(ascii_item("x")));
        assert_eq!(compact_message(&with_body), "S1F2 <A[1] \"x\">.");
        assert_eq!(pretty_message(&with_body), "S1F2 <A[1] \"x\">.");
    }

    /// Pins the Compact golden for an S5F1 W alarm report with a two-element
    /// body.
    #[test]
    fn message_with_body_and_wait_bit_compact_renders_nested_list() {
        let body = SecsItem::List(vec![SecsItem::Binary(vec![0x04]), ascii_item("LOT001")]);
        assert_eq!(
            compact_message(&message(5, 1, true, Some(body))),
            "S5F1 W <L[2] <B[1] 0x04> <A[6] \"LOT001\">>."
        );
    }

    /// Pins the Pretty golden for the same S5F1 W message; the period
    /// attaches directly to the root list's closing `>` line.
    #[test]
    fn message_with_body_and_wait_bit_pretty_ends_with_closing_bracket_period() {
        let body = SecsItem::List(vec![SecsItem::Binary(vec![0x04]), ascii_item("LOT001")]);
        assert_eq!(
            pretty_message(&message(5, 1, true, Some(body))),
            "S5F1 W <L[2]\n  <B[1] 0x04>\n  <A[6] \"LOT001\">\n>."
        );
    }

    /// Confirms bodyless messages render identically in both styles, since
    /// only lists with children introduce line breaks.
    #[test]
    fn message_bodyless_output_is_identical_in_both_styles() {
        let bodyless = message(2, 17, true, None);
        assert_eq!(compact_message(&bodyless), "S2F17 W.");
        assert_eq!(pretty_message(&bodyless), "S2F17 W.");
    }

    /// Confirms a top-level Jis8 item is refused with its variant name.
    #[test]
    fn jis8_item_is_rejected_as_unsupported_type() {
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_item(&SecsItem::Jis8(Vec::new()))
            .expect_err("Jis8 has no SML token");
        assert_eq!(error, FormatError::UnsupportedType { name: "Jis8" });
    }

    /// Confirms a Jis8 item nested inside a list is still found by the walk.
    #[test]
    fn jis8_nested_inside_list_is_rejected_as_unsupported_type() {
        let tree = SecsItem::List(vec![ascii_item("ok"), SecsItem::Jis8(vec![0x00])]);
        let error = SmlFormatter::new(FormatStyle::Pretty)
            .format_item(&tree)
            .expect_err("nested Jis8 must be refused too");
        assert_eq!(error, FormatError::UnsupportedType { name: "Jis8" });
    }

    /// Confirms a Localized item is refused with its variant name.
    #[test]
    fn localized_item_is_rejected_as_unsupported_type() {
        let localized = LocalizedString::new(
            LocalizedEncodingCode::new(1).expect("encoding code 1 is non-zero"),
            Vec::new(),
        );
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_item(&SecsItem::Localized(localized))
            .expect_err("localized text has no SML token");
        assert_eq!(error, FormatError::UnsupportedType { name: "Localized" });
    }

    /// Confirms a NaN F4 value is refused with the F4 kind tag.
    #[test]
    fn nan_f4_value_is_rejected_as_non_finite_float() {
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_item(&SecsItem::F4(vec![f32::NAN]))
            .expect_err("SML cannot represent NaN");
        assert_eq!(error, FormatError::NonFiniteFloat { kind: "F4" });
    }

    /// Confirms an infinite F8 value is refused with the F8 kind tag.
    #[test]
    fn infinity_f8_value_is_rejected_as_non_finite_float() {
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_item(&SecsItem::F8(vec![f64::INFINITY]))
            .expect_err("SML cannot represent infinity");
        assert_eq!(error, FormatError::NonFiniteFloat { kind: "F8" });
    }

    /// Confirms a 16,777,216-byte U1 body fails E5 measurement with the
    /// encoder's `ItemBodyTooLarge` error wrapped in `NotEncodable`.
    #[test]
    fn oversized_u1_body_is_rejected_as_not_encodable() {
        let oversized = SecsItem::U1(vec![0u8; 0x100_0000]);
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_item(&oversized)
            .expect_err("0x100_0000 body bytes exceed the E5 24-bit length field");
        assert_eq!(
            error,
            FormatError::NotEncodable {
                source: EncodeError::ItemBodyTooLarge {
                    format_code: FormatCode::U1.six_bit_value(),
                    body_bytes: 0x100_0000,
                }
            }
        );
    }

    /// Confirms a tree at the parser-side hard ceiling itself — 256 nested
    /// Lists counting the root as level 1 — still formats in both styles:
    /// the formatter refuses only what no parser could ever accept, and a
    /// parser configured at the ceiling can accept this tree.
    #[test]
    fn list_nesting_at_the_hard_ceiling_of_256_formats_in_both_styles() {
        let tree = nested_lists(256, SecsItem::Binary(Vec::new()));
        let compact_text = SmlFormatter::new(FormatStyle::Compact)
            .format_item(&tree)
            .expect("depth 256 is the ceiling itself and must format");
        let pretty_text = SmlFormatter::new(FormatStyle::Pretty)
            .format_item(&tree)
            .expect("depth 256 is the ceiling itself and must format");
        // Compact golden: 256 `<L[1]` headers around an empty binary leaf,
        // closed by 256 list closers plus the binary's own `>`.
        assert_eq!(
            compact_text,
            format!("{}<B[0]>{}", "<L[1] ".repeat(256), ">".repeat(256))
        );
        assert_eq!(
            strip_whitespace(&compact_text),
            strip_whitespace(&pretty_text),
            "both styles must emit identical tokens at the ceiling"
        );
    }

    /// Confirms one List level past the ceiling is refused for item
    /// formatting with the exact offending level: the deepest List sits at
    /// nesting depth 257 against the hard maximum 256.
    #[test]
    fn list_nesting_one_past_the_hard_ceiling_is_rejected_for_format_item() {
        let tree = nested_lists(257, SecsItem::Binary(Vec::new()));
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_item(&tree)
            .expect_err("no SML parser can be configured to accept depth 257");
        assert_eq!(
            error,
            FormatError::DepthExceeded {
                depth: 257,
                max_depth: 256,
            }
        );
    }

    /// Confirms message-level formatting refuses the same over-deep tree as
    /// a body through the identical validation walk, while bodyless messages
    /// remain unaffected (pinned by the bodyless goldens above).
    #[test]
    fn message_with_body_nesting_one_past_the_hard_ceiling_is_rejected() {
        let tree = nested_lists(257, SecsItem::Binary(Vec::new()));
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_message(&message(1, 1, false, Some(tree)))
            .expect_err("message bodies pass through the same depth check");
        assert_eq!(
            error,
            FormatError::DepthExceeded {
                depth: 257,
                max_depth: 256,
            }
        );
    }

    /// Confirms a tree deeper than the default `DecodeLimits` depth of 64
    /// still formats: the formatter refuses only what no parser could ever
    /// accept, not what default-configured parsers merely refuse.
    #[test]
    fn list_nesting_of_65_formats_despite_exceeding_default_parser_depth() {
        let tree = nested_lists(65, ascii_item("x"));
        assert_eq!(
            compact(&tree),
            format!("{}<A[1] \"x\">{}", "<L[1] ".repeat(65), ">".repeat(65))
        );
    }

    /// Confirms the semantic walk runs before the E5 measurement: a 257-deep
    /// tree that also carries an unencodable 16 MiB U1 leaf at its base
    /// reports `DepthExceeded`, never `NotEncodable`.
    #[test]
    fn depth_exceeded_is_reported_before_not_encodable_for_a_257_deep_oversized_tree() {
        let tree = nested_lists(257, SecsItem::U1(vec![0u8; 0x100_0000]));
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_item(&tree)
            .expect_err("the depth refusal must win over the size refusal");
        assert_eq!(
            error,
            FormatError::DepthExceeded {
                depth: 257,
                max_depth: 256,
            }
        );
    }

    /// Confirms message-level formatting refuses a Jis8 body outright,
    /// producing no partial header-plus-body text.
    #[test]
    fn message_with_jis8_body_is_rejected_without_partial_output() {
        let error = SmlFormatter::new(FormatStyle::Compact)
            .format_message(&message(6, 12, false, Some(SecsItem::Jis8(Vec::new()))))
            .expect_err("messages carrying Jis8 bodies must be refused");
        assert_eq!(error, FormatError::UnsupportedType { name: "Jis8" });
    }

    /// Verifies Compact and Pretty emit identical token sequences by
    /// removing all layout whitespace from both outputs and comparing the
    /// remaining token characters for four representative trees.
    #[test]
    fn compact_and_pretty_emit_identical_tokens_for_representative_trees() {
        let trees = [
            // Nested list mixing a quoted ASCII leaf and an empty binary leaf.
            SecsItem::List(vec![ascii_item("x"), SecsItem::Binary(Vec::new())]),
            // Two-level nesting from the Pretty golden tests.
            SecsItem::List(vec![SecsItem::List(vec![
                ascii_item("x"),
                SecsItem::Binary(Vec::new()),
            ])]),
            // The S5F1 alarm-report shape: binary and ASCII leaves under a list.
            SecsItem::List(vec![SecsItem::Binary(vec![0x04]), ascii_item("LOT001")]),
            // Scalar variety: integers, floats, and booleans together.
            SecsItem::List(vec![
                SecsItem::I4(vec![-1, 0, 2147483647]),
                SecsItem::U1(vec![0, 128, 255]),
                SecsItem::F4(vec![1.5, -0.0]),
                SecsItem::F8(vec![1e-7]),
                SecsItem::Boolean(vec![true, false]),
            ]),
        ];
        for tree in &trees {
            assert_eq!(
                strip_whitespace(&compact(tree)),
                strip_whitespace(&pretty(tree)),
                "token sequences must match for {tree:?}"
            );
        }
    }

    /// Confirms the default style is Compact and matches an explicit
    /// construction of the default formatter.
    #[test]
    fn default_format_style_is_compact() {
        assert_eq!(FormatStyle::default(), FormatStyle::Compact);
        assert_eq!(
            SmlFormatter::default(),
            SmlFormatter::new(FormatStyle::Compact)
        );
    }
}
