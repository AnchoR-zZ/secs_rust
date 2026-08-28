//! SML parser: strict-dialect grammar and semantic construction.
//!
//! The SML text adapter is layered in three steps: the scanner turns source
//! text into positioned tokens, this parser matches the strict message and
//! item grammar over a lazily scanned, one-token-lookahead cursor and
//! constructs validated [`SecsItem`] trees plus [`SmlMessage`] values, and
//! the formatter renders canonical text from those same values. Because
//! tokens are produced on demand, resource limits bind while parsing —
//! nothing beyond the current lookahead token is ever scanned — and errors
//! surface in source order: a malformed header is reported before any later
//! character is even looked at, an item's type word is classified (and a
//! List's depth checked) before the item's count is scanned, and a lexically
//! invalid character after a complete item or message surfaces as the
//! scanner's own error rather than as trailing input. List nesting is tracked
//! with an explicit stack of open list frames instead of recursive descent, so
//! no input, however deeply nested, grows the native call stack here.

use super::error::{ParseError, SourcePos};
use super::message::SmlMessage;
use super::scanner::{Scanner, Token, TokenKind};
use crate::hsms::{Function, Stream};
use crate::secs2::{AsciiString, DecodeLimits, SecsItem};

/// Largest HSMS stream number representable in the seven header bits.
const MAX_STREAM: u64 = 127;

/// Largest HSMS function number representable in the eight header bits.
const MAX_FUNCTION: u64 = 255;

/// Strict-dialect SML parser that enforces [`DecodeLimits`] while building.
///
/// Input is scanned lazily through a one-token-lookahead cursor; whitespace
/// significance is therefore resolved entirely by the scanner and never
/// re-checked here, and limits bind before unseen input is ever scanned.
pub struct SmlParser {
    /// Resource limits enforced while constructing item trees.
    limits: DecodeLimits,
}

impl SmlParser {
    /// Creates a parser that enforces `limits` on every parsed item tree.
    ///
    /// Returns the parser directly; construction cannot fail because
    /// [`DecodeLimits`] values are validated where they are built.
    #[must_use]
    pub const fn new(limits: DecodeLimits) -> Self {
        Self { limits }
    }

    /// Parses exactly one complete item such as `<L [1] <A "x">>` from
    /// `input` and returns the constructed tree.
    ///
    /// All of `input` must be consumed by that single item; anything after
    /// its closing `>` is trailing input and fails the parse.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] for any lexical, grammatical, semantic, or
    /// resource-limit failure, each carrying the position of the offending
    /// token. Scanning is lazy with one token of lookahead, so errors are
    /// source-ordered: a lexically invalid character after the closing `>`
    /// surfaces as the scanner's own error (e.g. `InvalidCharacter`) because
    /// it is the lookahead scan that reaches it, while lexically valid
    /// trailing content fails with [`ParseError::TrailingInput`]. Limits
    /// likewise bind before unseen input is scanned: an error detected inside
    /// the item is reported even when later, not-yet-scanned input is
    /// lexically broken.
    pub fn parse_item(&self, input: &str) -> Result<SecsItem, ParseError> {
        let mut items = ItemCursor::new(TokenCursor::new(Scanner::new(input)), self.limits);
        let item = items.parse_root_item()?;
        let mut cursor = items.into_tokens();
        if let Some(token) = cursor.peek()? {
            return Err(ParseError::TrailingInput {
                pos: token.pos,
                preview: trailing_preview(input, token.pos),
            });
        }
        Ok(item)
    }

    /// Parses one complete message `SxFy W? item? .` from `input` and
    /// returns it with validated stream and function identifiers.
    ///
    /// A message with no item between the header and the terminating period
    /// carries `body: None`, which is distinct from an empty root item such
    /// as `<L [0]>`.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] describing the first header, item,
    /// terminator, trailing-content, or resource-limit failure. Scanning is
    /// lazy with one token of lookahead, so errors are source-ordered: a
    /// malformed header such as `BAD,` fails with
    /// [`ParseError::InvalidMessageHeader`] before the trailing comma is
    /// ever scanned; a lexically invalid character after the terminating
    /// period (as in `S1F1.,`) surfaces as the scanner's own error (e.g.
    /// [`ParseError::InvalidCharacter`]) because it is the lookahead scan
    /// that reaches it, while lexically valid trailing content (as in
    /// `S1F1 . 5`) fails with [`ParseError::TrailingInput`].
    pub fn parse_message(&self, input: &str) -> Result<SmlMessage, ParseError> {
        let mut cursor = TokenCursor::new(Scanner::new(input));

        // Header word: strictly 'S', digits, 'F', digits; the W-Bit is never
        // attached to the header word (Fix 4) and must be a separate 'W'.
        let (header_word, header_pos) = match cursor.next()? {
            Some(Token {
                kind: TokenKind::Word(word),
                pos,
            }) => (word, pos),
            Some(token) => {
                return Err(unexpected(
                    token.pos,
                    "a message header such as S1F13",
                    &describe_kind(&token.kind),
                ));
            }
            None => {
                return Err(unexpected(
                    cursor.eof_pos(),
                    "a message header such as S1F13",
                    "end of input",
                ));
            }
        };
        let parts =
            parse_header_word(header_word).ok_or_else(|| ParseError::InvalidMessageHeader {
                pos: header_pos,
                text: header_word.to_owned(),
            })?;
        if parts.stream > MAX_STREAM {
            return Err(ParseError::StreamOutOfRange {
                pos: header_pos,
                value: parts.stream,
            });
        }
        if parts.function > MAX_FUNCTION {
            return Err(ParseError::FunctionOutOfRange {
                pos: header_pos,
                value: parts.function,
            });
        }

        // One optional standalone W-Bit word between header and body; a
        // second 'W' must fail later at the body/terminator stage.
        let mut wait_bit = false;
        if matches!(
            cursor.peek()?,
            Some(Token {
                kind: TokenKind::Word("W"),
                ..
            })
        ) {
            cursor.next()?;
            wait_bit = true;
        }

        // One optional root item; the item cursor takes over the token
        // cursor by value and hands it back after the item's '>'.
        let body = if matches!(
            cursor.peek()?,
            Some(Token {
                kind: TokenKind::Lt,
                ..
            })
        ) {
            let mut items = ItemCursor::new(cursor, self.limits);
            let item = items.parse_root_item()?;
            cursor = items.into_tokens();
            Some(item)
        } else {
            None
        };

        // The terminating period.
        match cursor.next()? {
            Some(Token {
                kind: TokenKind::Dot,
                ..
            }) => {}
            Some(token) if body.is_some() => {
                return Err(unexpected(token.pos, "'.'", &describe_kind(&token.kind)));
            }
            Some(token) => {
                return Err(unexpected(
                    token.pos,
                    "an item or '.'",
                    &describe_kind(&token.kind),
                ));
            }
            None => {
                return Err(ParseError::MissingTerminator {
                    pos: cursor.eof_pos(),
                })
            }
        }

        // Nothing may follow the terminator. Because the trailing check is a
        // lookahead scan, a lexically invalid trailing character surfaces as
        // the scanner's own error rather than as TrailingInput.
        if let Some(token) = cursor.peek()? {
            return Err(ParseError::TrailingInput {
                pos: token.pos,
                preview: trailing_preview(input, token.pos),
            });
        }

        // The stream was range-checked above; this mapping is defensive only.
        let stream = Stream::new(parts.stream as u8).map_err(|_| ParseError::StreamOutOfRange {
            pos: header_pos,
            value: parts.stream,
        })?;
        Ok(SmlMessage::new(
            stream,
            Function::new(parts.function as u8),
            wait_bit,
            body,
        ))
    }
}

/// Parses one complete item from `input` with the default [`DecodeLimits`].
///
/// # Errors
///
/// Returns a [`ParseError`] for any lexical, grammatical, semantic, or
/// resource-limit failure in the item text; see [`SmlParser::parse_item`]
/// for the lazy, source-ordered error priority.
pub fn parse_item(input: &str) -> Result<SecsItem, ParseError> {
    SmlParser::new(DecodeLimits::default()).parse_item(input)
}

/// Parses one complete message from `input` with the default
/// [`DecodeLimits`].
///
/// # Errors
///
/// Returns a [`ParseError`] for any header, item, terminator,
/// trailing-content, or resource-limit failure in the message text; see
/// [`SmlParser::parse_message`] for the lazy, source-ordered error priority.
pub fn parse_message(input: &str) -> Result<SmlMessage, ParseError> {
    SmlParser::new(DecodeLimits::default()).parse_message(input)
}

/// Fields extracted from a well-formed `SxFy` header word.
struct HeaderParts {
    /// Stream number parsed from the digits after 'S'.
    stream: u64,
    /// Function number parsed from the digits after 'F'.
    function: u64,
}

/// Recognizes the strict header shape `S<digits>F<digits>` and nothing else.
///
/// Returns the parsed parts, or `None` when `word` does not match the shape
/// exactly — including split headers such as `S1`, and headers with a `W`
/// attached directly to the word (as in `S1F1W`), because strict SML
/// requires the W-Bit as a separate `W` word.
#[must_use]
fn parse_header_word(word: &str) -> Option<HeaderParts> {
    let bytes = word.as_bytes();
    if bytes.first() != Some(&b'S') {
        return None;
    }
    let (stream, after_stream) = digit_run(word, 1)?;
    if bytes.get(after_stream) != Some(&b'F') {
        return None;
    }
    let (function, after_function) = digit_run(word, after_stream + 1)?;
    if after_function != bytes.len() {
        return None;
    }
    Some(HeaderParts { stream, function })
}

/// Parses one run of ASCII digits in `word` starting at byte index `start`.
///
/// Returns the parsed value plus the index just past the run, or `None` when
/// no digit is present at `start`. The value folds the ENTIRE digit run with
/// checked arithmetic — leading zeros are harmless, so `S00000000001F1`
/// yields stream 1 — and only a run that genuinely overflows `u64`
/// saturates to `u64::MAX`, which the caller then reports as a range error.
#[must_use]
fn digit_run(word: &str, start: usize) -> Option<(u64, usize)> {
    let bytes = word.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == start {
        return None;
    }
    let digits = &bytes[start..end];
    let value = digits
        .iter()
        .try_fold(0u64, |value, &digit| {
            value.checked_mul(10)?.checked_add(u64::from(digit - b'0'))
        })
        .unwrap_or(u64::MAX);
    Some((value, end))
}

/// Conversion of one scanned integer literal into a typed element.
///
/// Implemented for every SECS integer element type so
/// [`ItemCursor::parse_integer_body`] is generic over the target: the trait
/// supplies the diagnostic name, the wire width for byte accounting, and the
/// range-checked conversion from the scanner's widest intermediate `i128`.
trait IntegerElement: Sized {
    /// Target SECS type name for diagnostics, e.g. "I1".
    const TARGET: &'static str;
    /// Wire element width in bytes.
    const WIDTH: usize;
    /// Accepts an `i128` that already fits the type, or `None` when out of
    /// range.
    ///
    /// # Errors
    ///
    /// This method does not return `Result`; `None` signals the caller to
    /// build an [`ParseError::IntegerOverflow`] quoting the literal's
    /// original source spelling.
    fn from_scanned(value: i128) -> Option<Self>;
}

/// Converts scanned `i128` literals into I1 (`i8`) elements.
impl IntegerElement for i8 {
    /// Target SECS type name for diagnostics.
    const TARGET: &'static str = "I1";
    /// One byte per element on the wire.
    const WIDTH: usize = 1;
    /// Accepts exactly the `-128..=127` slice of `i128`.
    fn from_scanned(value: i128) -> Option<Self> {
        Self::try_from(value).ok()
    }
}

/// Converts scanned `i128` literals into I2 (`i16`) elements.
impl IntegerElement for i16 {
    /// Target SECS type name for diagnostics.
    const TARGET: &'static str = "I2";
    /// Two bytes per element on the wire.
    const WIDTH: usize = 2;
    /// Accepts exactly the `-32768..=32767` slice of `i128`.
    fn from_scanned(value: i128) -> Option<Self> {
        Self::try_from(value).ok()
    }
}

/// Converts scanned `i128` literals into I4 (`i32`) elements.
impl IntegerElement for i32 {
    /// Target SECS type name for diagnostics.
    const TARGET: &'static str = "I4";
    /// Four bytes per element on the wire.
    const WIDTH: usize = 4;
    /// Accepts exactly the `-2147483648..=2147483647` slice of `i128`.
    fn from_scanned(value: i128) -> Option<Self> {
        Self::try_from(value).ok()
    }
}

/// Converts scanned `i128` literals into I8 (`i64`) elements.
impl IntegerElement for i64 {
    /// Target SECS type name for diagnostics.
    const TARGET: &'static str = "I8";
    /// Eight bytes per element on the wire.
    const WIDTH: usize = 8;
    /// Accepts exactly the `i64::MIN..=i64::MAX` slice of `i128`.
    fn from_scanned(value: i128) -> Option<Self> {
        Self::try_from(value).ok()
    }
}

/// Converts scanned `i128` literals into U1 (`u8`) elements.
impl IntegerElement for u8 {
    /// Target SECS type name for diagnostics.
    const TARGET: &'static str = "U1";
    /// One byte per element on the wire.
    const WIDTH: usize = 1;
    /// Accepts exactly the `0..=255` slice of `i128`.
    fn from_scanned(value: i128) -> Option<Self> {
        Self::try_from(value).ok()
    }
}

/// Converts scanned `i128` literals into U2 (`u16`) elements.
impl IntegerElement for u16 {
    /// Target SECS type name for diagnostics.
    const TARGET: &'static str = "U2";
    /// Two bytes per element on the wire.
    const WIDTH: usize = 2;
    /// Accepts exactly the `0..=65535` slice of `i128`.
    fn from_scanned(value: i128) -> Option<Self> {
        Self::try_from(value).ok()
    }
}

/// Converts scanned `i128` literals into U4 (`u32`) elements.
impl IntegerElement for u32 {
    /// Target SECS type name for diagnostics.
    const TARGET: &'static str = "U4";
    /// Four bytes per element on the wire.
    const WIDTH: usize = 4;
    /// Accepts exactly the `0..=4294967295` slice of `i128`.
    fn from_scanned(value: i128) -> Option<Self> {
        Self::try_from(value).ok()
    }
}

/// Converts scanned `i128` literals into U8 (`u64`) elements.
impl IntegerElement for u64 {
    /// Target SECS type name for diagnostics.
    const TARGET: &'static str = "U8";
    /// Eight bytes per element on the wire.
    const WIDTH: usize = 8;
    /// Accepts exactly the `0..=u64::MAX` slice of `i128`.
    fn from_scanned(value: i128) -> Option<Self> {
        Self::try_from(value).ok()
    }
}

/// One open List item on the parser's explicit nesting stack.
struct ListFrame {
    /// Child items completed so far for this List.
    children: Vec<SecsItem>,
    /// Count declared in `[n]`, when the header carried one.
    declared_count: Option<usize>,
    /// Position of the `L` type word, used for count and size diagnostics.
    type_pos: SourcePos,
}

/// Outcome of starting one item at a `<` token.
enum StartedItem {
    /// A List whose frame is now on the stack; children and the closing `>`
    /// follow in the token stream.
    OpenedList,
    /// A complete non-List item, ready to attach to its enclosing frame.
    Finished(SecsItem),
}

/// The strict-dialect classification of one item type word.
///
/// Produced by [`classify_item_type`] the moment the type word is read,
/// before any further token of the item (its count, body, or closing `>`)
/// is scanned. The `J`/`JIS8` words are refused by the classifier itself,
/// so a classification failure always precedes any count scanning.
enum ItemKind {
    /// List container, the only type that consumes nesting depth.
    List,
    /// Binary byte string `<B>`.
    Binary,
    /// BOOLEAN element run.
    Boolean,
    /// ASCII fragment run.
    Ascii,
    /// Signed one-byte integer elements.
    I1,
    /// Signed two-byte integer elements.
    I2,
    /// Signed four-byte integer elements.
    I4,
    /// Signed eight-byte integer elements.
    I8,
    /// Unsigned one-byte integer elements.
    U1,
    /// Unsigned two-byte integer elements.
    U2,
    /// Unsigned four-byte integer elements.
    U4,
    /// Unsigned eight-byte integer elements.
    U8,
    /// Four-byte floating-point elements.
    F4,
    /// Eight-byte floating-point elements.
    F8,
}

/// Resolves one scanned type word to its [`ItemKind`].
///
/// `word` is the item's type word as scanned and `pos` its position; the
/// match is total and case-sensitive, so the caller can dispatch on the
/// kind without re-matching strings, and an unusable type is rejected
/// before any further token of the item is read.
///
/// # Errors
///
/// Returns [`ParseError::UnsupportedType`] for `J`/`JIS8`, which exist in
/// SECS-II but are refused by the strict dialect, and
/// [`ParseError::UnknownType`] for any other unrecognized word — in both
/// cases before the item's count or body is scanned.
fn classify_item_type(word: &str, pos: SourcePos) -> Result<ItemKind, ParseError> {
    match word {
        "L" => Ok(ItemKind::List),
        "B" => Ok(ItemKind::Binary),
        "BOOLEAN" => Ok(ItemKind::Boolean),
        "A" => Ok(ItemKind::Ascii),
        "I1" => Ok(ItemKind::I1),
        "I2" => Ok(ItemKind::I2),
        "I4" => Ok(ItemKind::I4),
        "I8" => Ok(ItemKind::I8),
        "U1" => Ok(ItemKind::U1),
        "U2" => Ok(ItemKind::U2),
        "U4" => Ok(ItemKind::U4),
        "U8" => Ok(ItemKind::U8),
        "F4" => Ok(ItemKind::F4),
        "F8" => Ok(ItemKind::F8),
        "J" | "JIS8" => Err(ParseError::UnsupportedType {
            pos,
            name: word.to_owned(),
        }),
        _ => Err(ParseError::UnknownType {
            pos,
            name: word.to_owned(),
        }),
    }
}

/// One-token-lookahead cursor over the scanner, the parser's only token
/// source.
///
/// Wrapping the scanner this way keeps parsing lazy: the underlying scanner
/// advances only when the lookahead buffer is empty, so nothing beyond the
/// current lookahead token is ever scanned and limits bind during parsing.
struct TokenCursor<'a> {
    /// Underlying scanner, advanced only when the lookahead buffer is empty.
    scanner: Scanner<'a>,
    /// Buffered peeked token, if any; `None` means "not yet peeked" or EOF.
    lookahead: Option<Token<'a>>,
}

impl<'a> TokenCursor<'a> {
    /// Creates a cursor that draws tokens from `scanner`.
    ///
    /// Returns the cursor with an empty lookahead buffer; the first
    /// [`TokenCursor::peek`] or [`TokenCursor::next`] scans the first token.
    fn new(scanner: Scanner<'a>) -> Self {
        Self {
            scanner,
            lookahead: None,
        }
    }

    /// Returns (does not consume) the next token, scanning it on demand.
    ///
    /// Once end of input is reached, repeated calls keep returning
    /// `Ok(None)`: the buffered state stays empty and the scanner reports a
    /// stable end of input, so re-peeking after EOF is cheap and idempotent.
    ///
    /// # Errors
    ///
    /// Propagates any scanner error (invalid character, unterminated string,
    /// oversized literal, ...) with its source position; this is how
    /// lexically broken trailing input surfaces as a scan error rather than
    /// as trailing content.
    fn peek(&mut self) -> Result<Option<Token<'a>>, ParseError> {
        if self.lookahead.is_none() {
            self.lookahead = self.scanner.next_token()?;
        }
        Ok(self.lookahead)
    }

    /// Consumes and returns the next token (favouring the lookahead buffer);
    /// `Ok(None)` at end of input.
    ///
    /// # Errors
    ///
    /// Propagates any scanner error encountered while producing the token.
    fn next(&mut self) -> Result<Option<Token<'a>>, ParseError> {
        if let Some(token) = self.lookahead.take() {
            return Ok(Some(token));
        }
        self.scanner.next_token()
    }

    /// Returns the position where the next token would start.
    ///
    /// Meaningful ONLY once [`TokenCursor::peek`] or [`TokenCursor::next`]
    /// has returned `Ok(None)`: then the scanner is exhausted and this is
    /// exactly the end-of-input position. While a token sits in the
    /// lookahead buffer the scanner has already moved past it, so the
    /// reported position lies after the buffered token rather than at the
    /// next token's start.
    #[must_use]
    const fn eof_pos(&self) -> SourcePos {
        self.scanner.current_pos()
    }
}

/// Lazy token cursor that builds one item tree using an explicit list stack.
struct ItemCursor<'a> {
    /// One-token-lookahead token source; the parser's only scanner access.
    tokens: TokenCursor<'a>,
    /// Resource limits enforced while the tree is built.
    limits: DecodeLimits,
    /// Stack of currently open List frames; nesting never uses recursion.
    stack: Vec<ListFrame>,
    /// Number of item nodes created so far, counting every type.
    node_count: usize,
}

impl<'a> ItemCursor<'a> {
    /// Creates a cursor that draws tokens from `tokens` and enforces
    /// `limits` while constructing the tree.
    fn new(tokens: TokenCursor<'a>, limits: DecodeLimits) -> Self {
        Self {
            tokens,
            limits,
            stack: Vec::new(),
            node_count: 0,
        }
    }

    /// Consumes the item cursor and returns its token cursor, so a
    /// message-level caller can resume token flow (terminator and trailing
    /// checks) right after the root item's `>`.
    fn into_tokens(self) -> TokenCursor<'a> {
        self.tokens
    }

    /// Parses exactly one item starting at the current token, which must be
    /// a `<`, and leaves the cursor positioned just past that item's `>`.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] for the first lexical, grammatical,
    /// semantic, or resource-limit failure inside the item tree; source order
    /// decides priority because nothing beyond the lookahead token is
    /// scanned.
    fn parse_root_item(&mut self) -> Result<SecsItem, ParseError> {
        loop {
            match self.parse_one_item()? {
                StartedItem::Finished(item) => {
                    if let Some(root) = self.attach(item)? {
                        return Ok(root);
                    }
                }
                StartedItem::OpenedList => {}
            }
            // List body: either a further child item starts here (after
            // passing admission against the innermost open list), or '>'
            // closes the innermost open list, possibly in a cascade.
            loop {
                match self.tokens.peek()? {
                    Some(Token {
                        kind: TokenKind::Lt,
                        ..
                    }) => {
                        // Admission runs BEFORE the child subtree is parsed,
                        // so limit breaches reject the child without ever
                        // scanning its contents.
                        self.admit_list_child()?;
                        break;
                    }
                    Some(Token {
                        kind: TokenKind::Gt,
                        pos,
                    }) => {
                        self.tokens.next()?;
                        let Some(frame) = self.stack.pop() else {
                            // Defensively unreachable: '>' in a list body
                            // implies an open list frame.
                            return Err(unexpected(pos, "an item or '>'", ">"));
                        };
                        if let Some(declared) = frame.declared_count {
                            if declared != frame.children.len() {
                                return Err(ParseError::CountMismatch {
                                    pos: frame.type_pos,
                                    declared,
                                    actual: frame.children.len(),
                                });
                            }
                        }
                        let closed = SecsItem::List(frame.children);
                        if let Some(root) = self.attach(closed)? {
                            return Ok(root);
                        }
                    }
                    Some(token) => {
                        return Err(unexpected(
                            token.pos,
                            "an item or '>'",
                            &describe_kind(&token.kind),
                        ));
                    }
                    None => {
                        return Err(unexpected(
                            self.tokens.eof_pos(),
                            "an item or '>'",
                            "end of input",
                        ));
                    }
                }
            }
        }
    }

    /// Starts one item at the current `<` token: consumes and classifies
    /// the type word, applies the List depth check, consumes the optional
    /// count, then either opens a List frame or parses the leaf body up to
    /// its closing `>`.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] when the item header is malformed, the type
    /// is unknown or unsupported, or a resource limit is exceeded. The type
    /// word is classified — and a List's nesting depth checked — before the
    /// count is scanned, so those failures surface even when the
    /// not-yet-scanned count text would itself fail.
    fn parse_one_item(&mut self) -> Result<StartedItem, ParseError> {
        let lt_pos = match self.tokens.next()? {
            Some(Token {
                kind: TokenKind::Lt,
                pos,
            }) => pos,
            Some(token) => {
                return Err(unexpected(
                    token.pos,
                    "an item",
                    &describe_kind(&token.kind),
                ));
            }
            None => {
                return Err(unexpected(self.tokens.eof_pos(), "an item", "end of input"));
            }
        };
        self.node_count = self.node_count.saturating_add(1);
        if self.node_count > self.limits.max_total_items() {
            return Err(ParseError::TotalItemsExceeded {
                pos: lt_pos,
                max_total_items: self.limits.max_total_items(),
            });
        }

        let (type_word, type_pos) = match self.tokens.next()? {
            Some(Token {
                kind: TokenKind::Word(word),
                pos,
            }) => (word, pos),
            Some(token) => {
                return Err(unexpected(
                    token.pos,
                    "an item type such as L or A",
                    &describe_kind(&token.kind),
                ));
            }
            None => {
                return Err(unexpected(
                    self.tokens.eof_pos(),
                    "an item type such as L or A",
                    "end of input",
                ));
            }
        };

        // Classification runs before the count is scanned, so an unknown or
        // refused type word is reported without reading any further token
        // of the item.
        let kind = classify_item_type(type_word, type_pos)?;

        // Lists are the only kind that consumes nesting depth; the check
        // precedes count scanning so a depth breach is reported even when
        // the count text that follows would itself fail to scan.
        if matches!(kind, ItemKind::List) {
            let depth = self.stack.len() + 1;
            if depth > self.limits.max_depth() {
                return Err(ParseError::DepthExceeded {
                    pos: lt_pos,
                    depth,
                    max_depth: self.limits.max_depth(),
                });
            }
        }

        let declared_count = self.parse_declared_count()?;

        match kind {
            ItemKind::List => {
                if declared_count.is_some_and(|count| count > self.limits.max_list_items()) {
                    return Err(ParseError::ListItemsExceeded {
                        pos: type_pos,
                        max_list_items: self.limits.max_list_items(),
                    });
                }
                self.stack.push(ListFrame {
                    children: Vec::new(),
                    declared_count,
                    type_pos,
                });
                Ok(StartedItem::OpenedList)
            }
            ItemKind::Binary => Ok(StartedItem::Finished(SecsItem::Binary(
                self.parse_binary_body(type_pos, declared_count)?,
            ))),
            ItemKind::Boolean => Ok(StartedItem::Finished(SecsItem::Boolean(
                self.parse_boolean_body(type_pos, declared_count)?,
            ))),
            ItemKind::Ascii => Ok(StartedItem::Finished(SecsItem::Ascii(
                self.parse_ascii_body(type_pos, declared_count)?,
            ))),
            ItemKind::I1 => Ok(StartedItem::Finished(SecsItem::I1(
                self.parse_integer_body::<i8>(type_pos, declared_count)?,
            ))),
            ItemKind::I2 => Ok(StartedItem::Finished(SecsItem::I2(
                self.parse_integer_body::<i16>(type_pos, declared_count)?,
            ))),
            ItemKind::I4 => Ok(StartedItem::Finished(SecsItem::I4(
                self.parse_integer_body::<i32>(type_pos, declared_count)?,
            ))),
            ItemKind::I8 => Ok(StartedItem::Finished(SecsItem::I8(
                self.parse_integer_body::<i64>(type_pos, declared_count)?,
            ))),
            ItemKind::U1 => Ok(StartedItem::Finished(SecsItem::U1(
                self.parse_integer_body::<u8>(type_pos, declared_count)?,
            ))),
            ItemKind::U2 => Ok(StartedItem::Finished(SecsItem::U2(
                self.parse_integer_body::<u16>(type_pos, declared_count)?,
            ))),
            ItemKind::U4 => Ok(StartedItem::Finished(SecsItem::U4(
                self.parse_integer_body::<u32>(type_pos, declared_count)?,
            ))),
            ItemKind::U8 => Ok(StartedItem::Finished(SecsItem::U8(
                self.parse_integer_body::<u64>(type_pos, declared_count)?,
            ))),
            ItemKind::F4 => Ok(StartedItem::Finished(SecsItem::F4(self.parse_float_body(
                type_pos,
                declared_count,
                "F4",
                4,
                |text| text.parse::<f32>().ok().filter(|value| value.is_finite()),
            )?))),
            ItemKind::F8 => Ok(StartedItem::Finished(SecsItem::F8(self.parse_float_body(
                type_pos,
                declared_count,
                "F8",
                8,
                |text| text.parse::<f64>().ok().filter(|value| value.is_finite()),
            )?))),
        }
    }

    /// Consumes an optional bracketed count `[n]` after an item type word.
    ///
    /// A plain decimal, non-negative integer that fits `usize` is required;
    /// hex counts and anything else are rejected at the `[` position.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::InvalidCount`] for a malformed count and
    /// [`ParseError::UnexpectedToken`] when `]` is missing; a lexically
    /// broken token between the brackets propagates the scanner's own error.
    fn parse_declared_count(&mut self) -> Result<Option<usize>, ParseError> {
        let Some(Token {
            kind: TokenKind::LBracket,
            pos: bracket_pos,
        }) = self.tokens.peek()?
        else {
            return Ok(None);
        };
        self.tokens.next()?;

        let count = match self.tokens.next()? {
            Some(Token {
                kind: TokenKind::Int {
                    value, hex: false, ..
                },
                ..
            }) if value >= 0 && value <= usize::MAX as i128 => value as usize,
            _ => return Err(ParseError::InvalidCount { pos: bracket_pos }),
        };

        match self.tokens.next()? {
            Some(Token {
                kind: TokenKind::RBracket,
                ..
            }) => Ok(Some(count)),
            Some(token) => Err(unexpected(token.pos, "']'", &describe_kind(&token.kind))),
            None => Err(unexpected(self.tokens.eof_pos(), "']'", "end of input")),
        }
    }

    /// Runs the two admission gates for one more child of the innermost open
    /// List, at the child's `<` and before its subtree is parsed.
    ///
    /// The gates run in fixed priority: the hard direct-child limit first,
    /// then the declared-count excess with the detection-time count.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::ListItemsExceeded`] when the next child would
    /// pass the configured direct-child limit, then
    /// [`ParseError::CountMismatch`] with `actual` set to the detection-time
    /// child count when a declared count would be exceeded.
    fn admit_list_child(&self) -> Result<(), ParseError> {
        let Some(frame) = self.stack.last() else {
            // Defensively unreachable: admission runs only inside a list
            // body, where the innermost frame is open.
            return Ok(());
        };
        let next_count = frame.children.len().saturating_add(1);
        if next_count > self.limits.max_list_items() {
            return Err(ParseError::ListItemsExceeded {
                pos: frame.type_pos,
                max_list_items: self.limits.max_list_items(),
            });
        }
        if let Some(declared) = frame.declared_count {
            if next_count > declared {
                return Err(ParseError::CountMismatch {
                    pos: frame.type_pos,
                    declared,
                    actual: next_count,
                });
            }
        }
        Ok(())
    }

    /// Attaches a completed item to the innermost open list, or completes
    /// the tree when no list is open.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::ListItemsExceeded`] defensively when an
    /// omitted-count list grew beyond the configured direct-child limit;
    /// the real gate for every child is [`ItemCursor::admit_list_child`],
    /// which runs at the child's `<` before its subtree is parsed.
    fn attach(&mut self, item: SecsItem) -> Result<Option<SecsItem>, ParseError> {
        let Some(frame) = self.stack.last_mut() else {
            return Ok(Some(item));
        };
        frame.children.push(item);
        if frame.declared_count.is_none() && frame.children.len() > self.limits.max_list_items() {
            return Err(ParseError::ListItemsExceeded {
                pos: frame.type_pos,
                max_list_items: self.limits.max_list_items(),
            });
        }
        Ok(None)
    }

    /// Parses `<B … >` byte elements up to the closing `>`.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] for a non-byte element, end of input inside
    /// the body, a count mismatch (surplus elements at detection time,
    /// shortfalls at `>`), or an oversized body per the declared-count
    /// pre-check or the per-element byte gate.
    fn parse_binary_body(
        &mut self,
        type_pos: SourcePos,
        declared_count: Option<usize>,
    ) -> Result<Vec<u8>, ParseError> {
        self.check_declared_bytes(type_pos, declared_count, 1)?;
        let mut values = Vec::new();
        let mut body_bytes = 0usize;
        loop {
            match self.tokens.next()? {
                Some(Token {
                    kind: TokenKind::Gt,
                    ..
                }) => {
                    self.check_declared_count(type_pos, declared_count, values.len())?;
                    return Ok(values);
                }
                Some(Token {
                    kind: TokenKind::HexByte { value, .. },
                    ..
                }) => {
                    body_bytes = self.admit_body_element(
                        type_pos,
                        declared_count,
                        body_bytes,
                        values.len(),
                        1,
                        1,
                    )?;
                    values.push(value);
                }
                Some(token) => {
                    return Err(unexpected(
                        token.pos,
                        "a 0xHH byte",
                        &describe_kind(&token.kind),
                    ));
                }
                None => {
                    return Err(unexpected(
                        self.tokens.eof_pos(),
                        "a 0xHH byte",
                        "end of input",
                    ))
                }
            }
        }
    }

    /// Parses `<BOOLEAN … >` elements up to the closing `>`.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] with a fixed element priority: the element
    /// category first (a non-word element is
    /// [`ParseError::UnexpectedToken`]), then the hard per-item byte limit,
    /// then the declared count (surplus elements at detection time,
    /// shortfalls at `>`), and only then the element value semantics — a
    /// word that is neither TRUE nor FALSE is [`ParseError::InvalidBoolean`].
    /// End of input inside the body and the declared-count byte pre-check
    /// precede all element processing.
    fn parse_boolean_body(
        &mut self,
        type_pos: SourcePos,
        declared_count: Option<usize>,
    ) -> Result<Vec<bool>, ParseError> {
        self.check_declared_bytes(type_pos, declared_count, 1)?;
        let mut values = Vec::new();
        let mut body_bytes = 0usize;
        loop {
            match self.tokens.next()? {
                Some(Token {
                    kind: TokenKind::Gt,
                    ..
                }) => {
                    self.check_declared_count(type_pos, declared_count, values.len())?;
                    return Ok(values);
                }
                Some(Token {
                    kind: TokenKind::Word(word),
                    pos,
                }) => {
                    // Admission precedes value validation: a byte-limit or
                    // count breach rejects the element even when its word is
                    // also not a boolean literal.
                    body_bytes = self.admit_body_element(
                        type_pos,
                        declared_count,
                        body_bytes,
                        values.len(),
                        1,
                        1,
                    )?;
                    let value = match word {
                        "TRUE" => true,
                        "FALSE" => false,
                        _ => {
                            return Err(ParseError::InvalidBoolean {
                                pos,
                                found: word.to_owned(),
                            });
                        }
                    };
                    values.push(value);
                }
                Some(token) => {
                    return Err(unexpected(
                        token.pos,
                        "TRUE or FALSE",
                        &describe_kind(&token.kind),
                    ));
                }
                None => {
                    return Err(unexpected(
                        self.tokens.eof_pos(),
                        "TRUE or FALSE",
                        "end of input",
                    ));
                }
            }
        }
    }

    /// Parses `<A … >` string fragments and hex bytes up to the closing `>`
    /// and wraps the accumulated seven-bit ASCII text.
    ///
    /// The element count compared against a declared count is the total
    /// character count, not the number of fragments; a whole `Str` fragment
    /// adds all of its characters at once, so an excess surfaces with the
    /// detection-time count.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] with a fixed element priority: the element
    /// category first (anything but a quoted fragment or `0xHH` byte is
    /// [`ParseError::UnexpectedToken`]), then the hard per-item byte limit,
    /// then the declared count (surplus characters at detection time,
    /// shortfalls at `>`), and only then the element value semantics — a
    /// hex byte above 0x7F is [`ParseError::InvalidAsciiByte`]. End of
    /// input inside the body and the declared-count byte pre-check precede
    /// all element processing.
    fn parse_ascii_body(
        &mut self,
        type_pos: SourcePos,
        declared_count: Option<usize>,
    ) -> Result<AsciiString, ParseError> {
        self.check_declared_bytes(type_pos, declared_count, 1)?;
        let mut text = String::new();
        let mut char_count = 0usize;
        loop {
            match self.tokens.next()? {
                Some(Token {
                    kind: TokenKind::Gt,
                    ..
                }) => {
                    self.check_declared_count(type_pos, declared_count, char_count)?;
                    // Defensively unreachable: only <= 0x7F bytes were
                    // appended above, so the ASCII validation cannot fail.
                    return AsciiString::try_from(text).map_err(|_| ParseError::InvalidAsciiByte {
                        pos: type_pos,
                        byte: 0,
                    });
                }
                Some(Token {
                    kind: TokenKind::Str(fragment),
                    ..
                }) => {
                    let added = fragment.chars().count();
                    char_count = self.admit_body_element(
                        type_pos,
                        declared_count,
                        char_count,
                        char_count,
                        added,
                        added,
                    )?;
                    text.push_str(fragment);
                }
                Some(Token {
                    kind: TokenKind::HexByte { value, .. },
                    pos,
                }) => {
                    // Admission precedes value validation: a byte-limit or
                    // count breach rejects the element even when the byte is
                    // also above seven-bit ASCII.
                    char_count = self.admit_body_element(
                        type_pos,
                        declared_count,
                        char_count,
                        char_count,
                        1,
                        1,
                    )?;
                    if value > 0x7F {
                        return Err(ParseError::InvalidAsciiByte { pos, byte: value });
                    }
                    text.push(char::from(value));
                }
                Some(token) => {
                    return Err(unexpected(
                        token.pos,
                        "a quoted string or 0xHH byte",
                        &describe_kind(&token.kind),
                    ));
                }
                None => {
                    return Err(unexpected(
                        self.tokens.eof_pos(),
                        "a quoted string or 0xHH byte",
                        "end of input",
                    ));
                }
            }
        }
    }

    /// Parses integer elements (decimal, hex, or `0xHH` bytes) directly into
    /// the typed vector of `T`, range-checking each literal through
    /// [`IntegerElement::from_scanned`].
    ///
    /// On range failure the [`ParseError::IntegerOverflow`] `literal` quotes
    /// the token's ORIGINAL source spelling (`0x0100` stays `0x0100`), and no
    /// string is allocated on success paths.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] with a fixed element priority: the element
    /// category first (a non-integer element is
    /// [`ParseError::UnexpectedToken`]), then the hard per-item byte limit,
    /// then the declared count (surplus elements at detection time,
    /// shortfalls at `>`), and only then the element value semantics — an
    /// out-of-range literal is [`ParseError::IntegerOverflow`]. End of
    /// input inside the body and the declared-count byte pre-check precede
    /// all element processing.
    fn parse_integer_body<T: IntegerElement>(
        &mut self,
        type_pos: SourcePos,
        declared_count: Option<usize>,
    ) -> Result<Vec<T>, ParseError> {
        self.check_declared_bytes(type_pos, declared_count, T::WIDTH)?;
        let mut values = Vec::new();
        let mut body_bytes = 0usize;
        loop {
            match self.tokens.next()? {
                Some(Token {
                    kind: TokenKind::Gt,
                    ..
                }) => {
                    self.check_declared_count(type_pos, declared_count, values.len())?;
                    return Ok(values);
                }
                Some(Token {
                    kind: TokenKind::Int { value, text, .. },
                    pos,
                }) => {
                    // Admission precedes value validation: a byte-limit or
                    // count breach rejects the element even when the literal
                    // is also out of range.
                    body_bytes = self.admit_body_element(
                        type_pos,
                        declared_count,
                        body_bytes,
                        values.len(),
                        T::WIDTH,
                        1,
                    )?;
                    let element =
                        T::from_scanned(value).ok_or_else(|| ParseError::IntegerOverflow {
                            pos,
                            literal: text.to_owned(),
                            target: T::TARGET,
                        })?;
                    values.push(element);
                }
                Some(Token {
                    kind: TokenKind::HexByte { value, text },
                    pos,
                }) => {
                    // Admission precedes value validation, mirroring the
                    // decimal-literal arm above.
                    body_bytes = self.admit_body_element(
                        type_pos,
                        declared_count,
                        body_bytes,
                        values.len(),
                        T::WIDTH,
                        1,
                    )?;
                    let element = T::from_scanned(i128::from(value)).ok_or_else(|| {
                        ParseError::IntegerOverflow {
                            pos,
                            literal: text.to_owned(),
                            target: T::TARGET,
                        }
                    })?;
                    values.push(element);
                }
                Some(token) => {
                    return Err(unexpected(
                        token.pos,
                        "an integer literal",
                        &describe_kind(&token.kind),
                    ));
                }
                None => {
                    return Err(unexpected(
                        self.tokens.eof_pos(),
                        "an integer literal",
                        "end of input",
                    ));
                }
            }
        }
    }

    /// Parses floating-point elements for one of F4/F8, converting each
    /// literal with `convert` and rejecting non-finite results.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] with a fixed element priority: the element
    /// category first (a non-float element is
    /// [`ParseError::UnexpectedToken`]), then the hard per-item byte limit,
    /// then the declared count (surplus elements at detection time,
    /// shortfalls at `>`), and only then the element value semantics — a
    /// non-finite or unparseable literal is [`ParseError::NonFiniteFloat`].
    /// End of input inside the body and the declared-count byte pre-check
    /// precede all element processing.
    fn parse_float_body<T>(
        &mut self,
        type_pos: SourcePos,
        declared_count: Option<usize>,
        target: &'static str,
        width: usize,
        convert: impl Fn(&str) -> Option<T>,
    ) -> Result<Vec<T>, ParseError> {
        self.check_declared_bytes(type_pos, declared_count, width)?;
        let mut values = Vec::new();
        let mut body_bytes = 0usize;
        loop {
            match self.tokens.next()? {
                Some(Token {
                    kind: TokenKind::Gt,
                    ..
                }) => {
                    self.check_declared_count(type_pos, declared_count, values.len())?;
                    return Ok(values);
                }
                Some(Token {
                    kind: TokenKind::Float(text),
                    pos,
                }) => {
                    // Admission precedes value validation: a byte-limit or
                    // count breach rejects the element even when the literal
                    // is also non-finite for the target type.
                    body_bytes = self.admit_body_element(
                        type_pos,
                        declared_count,
                        body_bytes,
                        values.len(),
                        width,
                        1,
                    )?;
                    let Some(value) = convert(text) else {
                        return Err(ParseError::NonFiniteFloat {
                            pos,
                            text: text.to_owned(),
                            target,
                        });
                    };
                    values.push(value);
                }
                Some(token) => {
                    return Err(unexpected(
                        token.pos,
                        "a floating-point literal",
                        &describe_kind(&token.kind),
                    ));
                }
                None => {
                    return Err(unexpected(
                        self.tokens.eof_pos(),
                        "a floating-point literal",
                        "end of input",
                    ));
                }
            }
        }
    }

    /// Pre-checks a scalar item's declared count against the per-item byte
    /// budget before any element is read.
    ///
    /// `element_width` is the wire width of one element: one for B, BOOLEAN,
    /// and A (where the declared count is a character count), `T::WIDTH` for
    /// integers, and 4/8 for F4/F8.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::ItemBytesExceeded`] with `required_bytes`
    /// carrying the projected byte total (`declared * element_width`,
    /// saturating to `usize::MAX` when the multiplication overflows) when it
    /// exceeds the configured per-item maximum.
    fn check_declared_bytes(
        &self,
        type_pos: SourcePos,
        declared_count: Option<usize>,
        element_width: usize,
    ) -> Result<(), ParseError> {
        if let Some(declared) = declared_count {
            let required_bytes = declared.saturating_mul(element_width);
            if required_bytes > self.limits.max_item_bytes() {
                return Err(ParseError::ItemBytesExceeded {
                    pos: type_pos,
                    required_bytes,
                    max_item_bytes: self.limits.max_item_bytes(),
                });
            }
        }
        Ok(())
    }

    /// Runs the two admission gates every scalar body applies before
    /// accepting one more element, in fixed priority: the hard per-item byte
    /// limit first, then the declared element count.
    ///
    /// `body_bytes` and `element_count` are the running totals BEFORE the
    /// incoming element; `element_bytes` is what the element adds to the
    /// byte total (a whole `Str` fragment adds all its characters at once)
    /// and `element_gain` what it adds to the element count (one for every
    /// non-A body, the character count for a fragment).
    ///
    /// Returns the projected byte total after admission.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::ItemBytesExceeded`] with the projected byte
    /// total (saturating to `usize::MAX` when the addition overflows)
    /// when it passes the configured maximum, then
    /// [`ParseError::CountMismatch`] with `actual` set to the detection-time
    /// element count when a declared count would be exceeded.
    fn admit_body_element(
        &self,
        type_pos: SourcePos,
        declared_count: Option<usize>,
        body_bytes: usize,
        element_count: usize,
        element_bytes: usize,
        element_gain: usize,
    ) -> Result<usize, ParseError> {
        let projected = body_bytes.saturating_add(element_bytes);
        if projected > self.limits.max_item_bytes() {
            return Err(ParseError::ItemBytesExceeded {
                pos: type_pos,
                required_bytes: projected,
                max_item_bytes: self.limits.max_item_bytes(),
            });
        }
        if let Some(declared) = declared_count {
            // Cannot overflow in practice once the byte gate above passed
            // (every counted element adds at least one byte); saturating
            // keeps the check total regardless.
            let next_count = element_count.saturating_add(element_gain);
            if next_count > declared {
                return Err(ParseError::CountMismatch {
                    pos: type_pos,
                    declared,
                    actual: next_count,
                });
            }
        }
        Ok(projected)
    }

    /// Compares a declared count against the number of elements actually
    /// parsed once an item's `>` has been consumed.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError::CountMismatch`] when the counts differ; this
    /// close-time check is the only place a SHORTFALL (fewer elements than
    /// declared) can be detected.
    fn check_declared_count(
        &self,
        type_pos: SourcePos,
        declared_count: Option<usize>,
        actual: usize,
    ) -> Result<(), ParseError> {
        match (declared_count, actual) {
            (Some(declared), actual) if declared != actual => Err(ParseError::CountMismatch {
                pos: type_pos,
                declared,
                actual,
            }),
            _ => Ok(()),
        }
    }
}

/// Assembles an [`ParseError::UnexpectedToken`] from positioned parts.
///
/// `expected` describes what the grammar requires and `found` the short
/// human description of the offending token.
fn unexpected(pos: SourcePos, expected: &'static str, found: &str) -> ParseError {
    ParseError::UnexpectedToken {
        pos,
        expected,
        found: found.to_owned(),
    }
}

/// Renders a short human description of a token kind for error messages.
///
/// Punctuation is rendered bare (`>`, `<`, `[`, `]`, `.`), words as
/// `word "X"`, and literals by their class such as `integer literal`.
fn describe_kind(kind: &TokenKind) -> String {
    match kind {
        TokenKind::Word(word) => format!("word \"{word}\""),
        TokenKind::Str(_) => "quoted ASCII string".to_string(),
        TokenKind::HexByte { .. } => "hexadecimal byte".to_string(),
        TokenKind::Int { .. } => "integer literal".to_string(),
        TokenKind::Float(_) => "floating-point literal".to_string(),
        TokenKind::Lt => "<".to_string(),
        TokenKind::Gt => ">".to_string(),
        TokenKind::LBracket => "[".to_string(),
        TokenKind::RBracket => "]".to_string(),
        TokenKind::Dot => ".".to_string(),
    }
}

/// Extracts up to twenty characters of `input` from `pos` for a trailing
/// content diagnostic.
///
/// Returns an empty preview when `pos` is not a valid character boundary in
/// `input`, which cannot happen for scanner-produced token positions.
fn trailing_preview(input: &str, pos: SourcePos) -> String {
    input
        .get(pos.offset..)
        .unwrap_or("")
        .chars()
        .take(20)
        .collect()
}

#[cfg(test)]
mod tests {
    //! Unit tests for the strict-dialect SML parser.

    use super::*;

    /// Builds a parser with the four `DecodeLimits` values, panicking only
    /// on invalid test fixtures.
    fn parser_with_limits(
        max_depth: usize,
        max_total_items: usize,
        max_item_bytes: usize,
        max_list_items: usize,
    ) -> SmlParser {
        SmlParser::new(
            DecodeLimits::new(max_depth, max_total_items, max_item_bytes, max_list_items)
                .expect("valid test limits"),
        )
    }

    /// Builds an `n`-deep nested `<L[1] … <L[0]> …>` source string.
    fn nested_lists(depth: usize) -> String {
        let mut source = String::new();
        for _ in 0..depth - 1 {
            source.push_str("<L[1] ");
        }
        source.push_str("<L[0]>");
        for _ in 0..depth - 1 {
            source.push('>');
        }
        source
    }

    /// Builds a flat list holding `width` empty `<A>` child items.
    fn wide_flat_list(width: usize) -> String {
        let mut source = format!("<L [{width}] ");
        for _ in 0..width {
            source.push_str("<A>");
        }
        source.push('>');
        source
    }

    /// Confirms `<L>` with no count parses as an empty list.
    #[test]
    fn empty_list_without_count_parses() {
        assert_eq!(parse_item("<L>"), Ok(SecsItem::List(Vec::new())));
    }

    /// Confirms `<L [0]>` parses as an empty list.
    #[test]
    fn empty_list_with_declared_zero_count_parses() {
        assert_eq!(parse_item("<L [0]>"), Ok(SecsItem::List(Vec::new())));
    }

    /// Confirms `<B>` parses as empty binary data.
    #[test]
    fn empty_binary_parses() {
        assert_eq!(parse_item("<B>"), Ok(SecsItem::Binary(Vec::new())));
    }

    /// Confirms `<BOOLEAN [0]>` parses as an empty boolean vector.
    #[test]
    fn empty_boolean_with_declared_zero_count_parses() {
        assert_eq!(
            parse_item("<BOOLEAN [0]>"),
            Ok(SecsItem::Boolean(Vec::new()))
        );
    }

    /// Confirms `<A>` parses as an empty ASCII item.
    #[test]
    fn empty_ascii_parses() {
        assert_eq!(
            parse_item("<A>"),
            Ok(SecsItem::Ascii(
                AsciiString::try_from("").expect("empty ASCII is valid")
            ))
        );
    }

    /// Confirms binary bytes parse with an exact declared count.
    #[test]
    fn binary_multi_element_with_exact_count_parses() {
        assert_eq!(
            parse_item("<B [3] 0x00 0x7F 0xFF>"),
            Ok(SecsItem::Binary(vec![0x00, 0x7F, 0xFF]))
        );
    }

    /// Confirms a single binary byte parses with the count omitted.
    #[test]
    fn binary_single_element_with_omitted_count_parses() {
        assert_eq!(parse_item("<B 0x41>"), Ok(SecsItem::Binary(vec![0x41])));
    }

    /// Confirms TRUE and FALSE parse with an exact declared count.
    #[test]
    fn boolean_true_false_values_parse() {
        assert_eq!(
            parse_item("<BOOLEAN [2] TRUE FALSE>"),
            Ok(SecsItem::Boolean(vec![true, false]))
        );
    }

    /// Confirms a boolean scalar parses with the count omitted.
    #[test]
    fn boolean_scalar_with_omitted_count_parses() {
        assert_eq!(
            parse_item("<BOOLEAN TRUE>"),
            Ok(SecsItem::Boolean(vec![true]))
        );
    }

    /// Confirms quoted fragments and hex bytes interleave in `<A>` and count
    /// as characters, reproducing `"ab" 0x09 "cd"` as `ab\tcd`.
    #[test]
    fn ascii_mixed_fragments_and_hex_byte_parses() {
        assert_eq!(
            parse_item("<A [5] \"ab\" 0x09 \"cd\">"),
            Ok(SecsItem::Ascii(
                AsciiString::try_from("ab\tcd").expect("tab is seven-bit ASCII")
            ))
        );
    }

    /// Confirms a DEL character can be spelled as hex inside `<A>`.
    #[test]
    fn ascii_del_character_via_hex_byte_parses() {
        assert_eq!(
            parse_item("<A \"a\" 0x7F>"),
            Ok(SecsItem::Ascii(
                AsciiString::try_from("a\u{7F}").expect("DEL is seven-bit ASCII")
            ))
        );
    }

    /// Confirms a hex byte at 0x80 is rejected inside `<A>`.
    #[test]
    fn ascii_hex_byte_above_seven_bits_rejected() {
        assert_eq!(
            parse_item("<A \"a\" 0x80>"),
            Err(ParseError::InvalidAsciiByte {
                pos: SourcePos::new(7, 1, 8),
                byte: 0x80,
            })
        );
    }

    /// Confirms an `<A>` count shortfall counts characters, not fragments,
    /// and surfaces at the closing `>`.
    #[test]
    fn ascii_count_counts_characters_not_fragments() {
        assert!(matches!(
            parse_item("<A [3] \"ab\">"),
            Err(ParseError::CountMismatch {
                declared: 3,
                actual: 2,
                ..
            })
        ));
    }

    /// Confirms a multi-character `<A>` fragment that overshoots a declared
    /// count is rejected with the detection-time character count.
    #[test]
    fn ascii_fragment_excess_reports_detection_time_count() {
        assert_eq!(
            parse_item("<A [2] \"abcd\">"),
            Err(ParseError::CountMismatch {
                pos: SourcePos::new(1, 1, 2),
                declared: 2,
                actual: 4,
            })
        );
    }

    /// Confirms I1 accepts exactly its full signed range.
    #[test]
    fn i1_boundaries_parse() {
        assert_eq!(
            parse_item("<I1 [2] -128 127>"),
            Ok(SecsItem::I1(vec![-128, 127]))
        );
    }

    /// Confirms I1 rejects 129 as out of range.
    #[test]
    fn i1_positive_overflow_rejected() {
        assert_eq!(
            parse_item("<I1 129>"),
            Err(ParseError::IntegerOverflow {
                pos: SourcePos::new(4, 1, 5),
                literal: "129".to_string(),
                target: "I1",
            })
        );
    }

    /// Confirms I1 rejects -129 and the hex byte 0xFF as out of range.
    #[test]
    fn i1_negative_and_hex_overflows_rejected() {
        assert!(matches!(
            parse_item("<I1 -129>"),
            Err(ParseError::IntegerOverflow { target: "I1", .. })
        ));
        assert!(matches!(
            parse_item("<I1 0xFF>"),
            Err(ParseError::IntegerOverflow {
                literal,
                target: "I1",
                ..
            }) if literal == "0xFF"
        ));
    }

    /// Confirms U1 accepts 0 through 255, including hex byte elements.
    #[test]
    fn u1_boundaries_parse() {
        assert_eq!(
            parse_item("<U1 [3] 0 255 0x41>"),
            Ok(SecsItem::U1(vec![0, 255, 0x41]))
        );
    }

    /// Confirms U1 rejects 256 as out of range.
    #[test]
    fn u1_overflow_rejected() {
        assert!(matches!(
            parse_item("<U1 256>"),
            Err(ParseError::IntegerOverflow { target: "U1", .. })
        ));
    }

    /// Confirms the signed multi-byte types accept their boundary values.
    #[test]
    fn i2_i4_i8_boundaries_parse() {
        assert_eq!(
            parse_item("<I2 [2] -32768 32767>"),
            Ok(SecsItem::I2(vec![-32768, 32767]))
        );
        assert_eq!(
            parse_item("<I4 [2] -2147483648 2147483647>"),
            Ok(SecsItem::I4(vec![-2147483648, 2147483647]))
        );
        assert_eq!(
            parse_item("<I8 [2] -9223372036854775808 9223372036854775807>"),
            Ok(SecsItem::I8(vec![i64::MIN, i64::MAX]))
        );
    }

    /// Confirms the unsigned multi-byte types accept their boundary values.
    #[test]
    fn u2_u4_u8_boundaries_parse() {
        assert_eq!(parse_item("<U2 [1] 65535>"), Ok(SecsItem::U2(vec![65535])));
        assert_eq!(
            parse_item("<U4 [1] 4294967295>"),
            Ok(SecsItem::U4(vec![4294967295]))
        );
        assert_eq!(
            parse_item("<U8 18446744073709551615>"),
            Ok(SecsItem::U8(vec![u64::MAX]))
        );
    }

    /// Confirms I8 rejects one past its maximum.
    #[test]
    fn i8_above_maximum_rejected() {
        assert!(matches!(
            parse_item("<I8 9223372036854775808>"),
            Err(ParseError::IntegerOverflow { target: "I8", .. })
        ));
    }

    /// Confirms hex integer literals parse in unsigned contexts.
    #[test]
    fn u1_hex_literal_parses_as_sixteen() {
        assert_eq!(parse_item("<U1 0x10>"), Ok(SecsItem::U1(vec![16])));
    }

    /// Confirms negative hex integer literals parse in signed contexts.
    #[test]
    fn i1_negative_hex_literal_parses() {
        assert_eq!(parse_item("<I1 -0x10>"), Ok(SecsItem::I1(vec![-16])));
    }

    /// Confirms a U1 hex-integer overflow quotes the original source
    /// spelling (`0x0100`, never `256` or a re-case-folded literal).
    #[test]
    fn u1_hex_overflow_preserves_original_spelling() {
        assert_eq!(
            parse_item("<U1 [1] 0x0100>"),
            Err(ParseError::IntegerOverflow {
                pos: SourcePos::new(8, 1, 9),
                literal: "0x0100".to_string(),
                target: "U1",
            })
        );
    }

    /// Confirms an I1 hex-byte overflow keeps the lowercase spelling
    /// (`0xff` stays `0xff`).
    #[test]
    fn i1_hex_byte_overflow_preserves_lowercase_spelling() {
        assert!(matches!(
            parse_item("<I1 [1] 0xff>"),
            Err(ParseError::IntegerOverflow { literal, target, .. })
                if literal == "0xff" && target == "I1"
        ));
    }

    /// Confirms F4 keeps the sign of -0.0, accepts the smallest subnormal
    /// underflow, and parses plain values.
    ///
    /// Negative zero must be written `-0.0`: the frozen scanner contract
    /// classifies a bare `-0` (no fraction or exponent) as an integer
    /// token, which float bodies refuse.
    #[test]
    fn f4_negative_zero_and_subnormal_parse() {
        let Ok(SecsItem::F4(values)) = parse_item("<F4 [3] 1.5 -0.0 1e-45>") else {
            panic!("F4 values including -0.0 and underflow must parse");
        };
        assert_eq!(values.len(), 3);
        assert_eq!(values[0], 1.5);
        assert_eq!(values[1].to_bits(), 0x8000_0000, "minus zero sign");
        assert_eq!(values[2].to_bits(), 1, "1e-45 rounds to min subnormal");
    }

    /// Confirms a bare `-0`, which scans as an integer token, is refused in
    /// a float body because strict SML requires float-shaped literals.
    #[test]
    fn f4_bare_negative_zero_integer_token_rejected() {
        assert!(matches!(
            parse_item("<F4 -0>"),
            Err(ParseError::UnexpectedToken { expected, found, .. })
                if expected == "a floating-point literal" && found == "integer literal"
        ));
    }

    /// Confirms F8 scalars parse with and without a declared count.
    #[test]
    fn f8_scalars_parse() {
        assert_eq!(
            parse_item("<F8 [2] 0.5 -2.5>"),
            Ok(SecsItem::F8(vec![0.5, -2.5]))
        );
        assert_eq!(parse_item("<F8 3.25>"), Ok(SecsItem::F8(vec![3.25])));
    }

    /// Confirms F4 rejects literals that overflow to infinity.
    #[test]
    fn f4_overflow_to_infinity_rejected() {
        assert_eq!(
            parse_item("<F4 1e40>"),
            Err(ParseError::NonFiniteFloat {
                pos: SourcePos::new(4, 1, 5),
                text: "1e40".to_string(),
                target: "F4",
            })
        );
    }

    /// Confirms F8 rejects literals that overflow to infinity.
    #[test]
    fn f8_overflow_to_infinity_rejected() {
        assert!(matches!(
            parse_item("<F8 1e400>"),
            Err(ParseError::NonFiniteFloat { target: "F8", .. })
        ));
    }

    /// Confirms a mixed-type nested list builds the exact expected tree.
    #[test]
    fn nested_list_tree_parses() {
        assert_eq!(
            parse_item("<L [2] <U1 [2] 1 2> <BOOLEAN TRUE>>"),
            Ok(SecsItem::List(vec![
                SecsItem::U1(vec![1, 2]),
                SecsItem::Boolean(vec![true]),
            ]))
        );
    }

    /// Confirms tokens may be glued together without any whitespace.
    #[test]
    fn compact_item_without_whitespace_parses() {
        assert_eq!(
            parse_item("<L[1]<A[1]\"x\">>"),
            Ok(SecsItem::List(vec![SecsItem::Ascii(
                AsciiString::try_from("x").expect("x is ASCII")
            )]))
        );
    }

    /// Confirms sixty nesting levels parse under default limits, proving the
    /// iterative stack handles deep trees.
    #[test]
    fn sixty_level_nested_lists_parse_under_default_limits() {
        let item = parse_item(&nested_lists(60)).expect("60 levels fit depth 64");
        let mut depth = 0usize;
        let mut current = &item;
        while let SecsItem::List(children) = current {
            depth += 1;
            match children.first() {
                Some(first) => current = first,
                None => break,
            }
        }
        assert_eq!(depth, 60);
    }

    /// Confirms a flat two-hundred-child list parses under default limits.
    #[test]
    fn wide_flat_list_with_two_hundred_children_parses() {
        let item = parse_item(&wide_flat_list(200)).expect("200 children fit defaults");
        let SecsItem::List(children) = item else {
            panic!("root must be a list");
        };
        assert_eq!(children.len(), 200);
        assert_eq!(children[0], SecsItem::Ascii(AsciiString::default()));
    }

    /// Confirms scalar items reject missing and surplus elements against a
    /// declared count: the surplus surfaces immediately at the element, the
    /// shortfall at the closing `>`.
    #[test]
    fn scalar_declared_count_mismatches_rejected() {
        assert!(matches!(
            parse_item("<U1 [3] 1 2>"),
            Err(ParseError::CountMismatch {
                declared: 3,
                actual: 2,
                ..
            })
        ));
        assert!(matches!(
            parse_item("<BOOLEAN [1] TRUE FALSE>"),
            Err(ParseError::CountMismatch {
                declared: 1,
                actual: 2,
                ..
            })
        ));
    }

    /// Confirms `< >` without a type word is rejected.
    #[test]
    fn missing_type_word_rejected() {
        assert!(matches!(
            parse_item("< >"),
            Err(ParseError::UnexpectedToken { found, .. }) if found == ">"
        ));
    }

    /// Confirms `<L2>` with an unbracketed count is rejected: the scanner
    /// absorbs `L2` into one word, so the count-less shape surfaces as an
    /// unknown type name.
    #[test]
    fn list_bare_count_rejected() {
        assert_eq!(
            parse_item("<L2>"),
            Err(ParseError::UnknownType {
                pos: SourcePos::new(1, 1, 2),
                name: "L2".to_string(),
            })
        );
    }

    /// Confirms the abbreviated `BOOL` type is unknown, not boolean.
    #[test]
    fn unknown_type_bool_uppercase_only_rejected() {
        assert_eq!(
            parse_item("<BOOL T>"),
            Err(ParseError::UnknownType {
                pos: SourcePos::new(1, 1, 2),
                name: "BOOL".to_string(),
            })
        );
    }

    /// Confirms mixed-case type words are unknown because matching is
    /// case-sensitive.
    #[test]
    fn mixed_case_type_word_rejected() {
        assert_eq!(
            parse_item("<Boolean true>"),
            Err(ParseError::UnknownType {
                pos: SourcePos::new(1, 1, 2),
                name: "Boolean".to_string(),
            })
        );
    }

    /// Confirms a lowercase boolean literal inside BOOLEAN is invalid.
    #[test]
    fn invalid_boolean_literal_rejected() {
        assert!(matches!(
            parse_item("<BOOLEAN true>"),
            Err(ParseError::InvalidBoolean { found, .. }) if found == "true"
        ));
    }

    /// Confirms JIS-8 type words are recognized but refused.
    #[test]
    fn jis_type_words_rejected() {
        assert_eq!(
            parse_item("<J [1] 0x41>"),
            Err(ParseError::UnsupportedType {
                pos: SourcePos::new(1, 1, 2),
                name: "J".to_string(),
            })
        );
        assert_eq!(
            parse_item("<JIS8>"),
            Err(ParseError::UnsupportedType {
                pos: SourcePos::new(1, 1, 2),
                name: "JIS8".to_string(),
            })
        );
    }

    /// Confirms an unrecognized type word is classified before the item's
    /// count is scanned: the comma in `<NOPE,`, never reached, would
    /// otherwise fail as the scanner's own invalid-character error.
    #[test]
    fn unknown_type_word_classified_before_count_scan() {
        assert_eq!(
            parse_item("<NOPE,"),
            Err(ParseError::UnknownType {
                pos: SourcePos::new(1, 1, 2),
                name: "NOPE".to_string(),
            })
        );
    }

    /// Confirms JIS-8 type words are refused before the item's count is
    /// scanned: the commas in `<J,` and `<JIS8,` are never reached, so the
    /// failures are `UnsupportedType`, not scanner errors.
    #[test]
    fn jis_type_words_refused_before_count_scan() {
        assert_eq!(
            parse_item("<J,"),
            Err(ParseError::UnsupportedType {
                pos: SourcePos::new(1, 1, 2),
                name: "J".to_string(),
            })
        );
        assert_eq!(
            parse_item("<JIS8,"),
            Err(ParseError::UnsupportedType {
                pos: SourcePos::new(1, 1, 2),
                name: "JIS8".to_string(),
            })
        );
    }

    /// Confirms a hex count `[0x2]` is rejected at the bracket.
    #[test]
    fn hex_count_rejected() {
        assert_eq!(
            parse_item("<L [0x2]>"),
            Err(ParseError::InvalidCount {
                pos: SourcePos::new(3, 1, 4),
            })
        );
    }

    /// Confirms a negative count is rejected at the bracket.
    #[test]
    fn negative_count_rejected() {
        assert!(matches!(
            parse_item("<L [-1]>"),
            Err(ParseError::InvalidCount { .. })
        ));
    }

    /// Confirms a list declaring two children but receiving one fails with
    /// the declared and actual counts in the error.
    #[test]
    fn list_count_mismatch_rejected() {
        assert_eq!(
            parse_item("<L [2] <A [1] \"x\">>"),
            Err(ParseError::CountMismatch {
                pos: SourcePos::new(1, 1, 2),
                declared: 2,
                actual: 1,
            })
        );
    }

    /// Confirms `<B>` rejects decimal integer elements.
    #[test]
    fn binary_decimal_element_rejected() {
        assert!(matches!(
            parse_item("<B 5>"),
            Err(ParseError::UnexpectedToken { expected, found, .. })
                if expected == "a 0xHH byte" && found == "integer literal"
        ));
    }

    /// Confirms `<B>` rejects one-digit hex, which scans as a hex integer.
    #[test]
    fn binary_one_digit_hex_rejected() {
        assert!(matches!(
            parse_item("<B 0x1>"),
            Err(ParseError::UnexpectedToken { expected, found, .. })
                if expected == "a 0xHH byte" && found == "integer literal"
        ));
    }

    /// Confirms `<I1>` rejects floating-point elements.
    #[test]
    fn integer_float_element_rejected() {
        assert!(matches!(
            parse_item("<I1 1.5>"),
            Err(ParseError::UnexpectedToken { expected, found, .. })
                if expected == "an integer literal" && found == "floating-point literal"
        ));
    }

    /// Confirms `<F4>` rejects word elements such as TRUE.
    #[test]
    fn float_word_element_rejected() {
        assert!(matches!(
            parse_item("<F4 TRUE>"),
            Err(ParseError::UnexpectedToken { found, .. })
                if found == "word \"TRUE\""
        ));
    }

    /// Confirms a second root item after a complete item is trailing input.
    #[test]
    fn second_root_item_rejected() {
        assert_eq!(
            parse_item("<L[0]><L[0]>"),
            Err(ParseError::TrailingInput {
                pos: SourcePos::new(6, 1, 7),
                preview: "<L[0]>".to_string(),
            })
        );
    }

    /// Confirms a stray closing `>` with no open item is rejected.
    #[test]
    fn stray_closing_angle_rejected() {
        assert!(matches!(
            parse_item(">"),
            Err(ParseError::UnexpectedToken { expected, found, .. })
                if expected == "an item" && found == ">"
        ));
    }

    /// Confirms end of input right after `<` reports the missing type word.
    #[test]
    fn eof_after_opening_angle_rejected() {
        assert!(matches!(
            parse_item("<"),
            Err(ParseError::UnexpectedToken { found, .. }) if found == "end of input"
        ));
    }

    /// Confirms end of input inside an open list is rejected.
    #[test]
    fn eof_inside_open_list_rejected() {
        assert!(matches!(
            parse_item("<L"),
            Err(ParseError::UnexpectedToken { found, .. }) if found == "end of input"
        ));
    }

    /// Confirms scanner errors such as unterminated strings propagate.
    #[test]
    fn unterminated_string_propagates_scanner_error() {
        assert!(matches!(
            parse_item("<A \"abc"),
            Err(ParseError::UnterminatedString { .. })
        ));
    }

    /// Confirms integer literals beyond every SECS range fail via the
    /// scanner's `NumberTooLarge` (2^127 does not fit i128).
    #[test]
    fn literal_beyond_i128_propagates_scanner_error() {
        assert!(matches!(
            parse_item("<U8 170141183460469231731687303715884105728>"),
            Err(ParseError::NumberTooLarge { .. })
        ));
    }

    /// Confirms empty item input reports end of input.
    #[test]
    fn empty_item_input_rejected() {
        assert!(matches!(
            parse_item(""),
            Err(ParseError::UnexpectedToken { expected, found, .. })
                if expected == "an item" && found == "end of input"
        ));
    }

    /// Confirms a bodyless message parses with all header fields.
    #[test]
    fn bodyless_message_parses() {
        let message = parse_message("S1F1.").expect("bodyless message parses");
        assert_eq!(message.stream().get(), 1);
        assert_eq!(message.function().get(), 1);
        assert!(!message.wait_bit());
        assert_eq!(message.body(), None);
    }

    /// Confirms a standalone `W` word sets the wait bit.
    #[test]
    fn standalone_wait_bit_message_parses() {
        let message = parse_message("S1F1 W.").expect("standalone W parses");
        assert!(message.wait_bit());
        assert_eq!(message.body(), None);
    }

    /// Confirms an attached `W` on the header word (`S1F1W.`) is rejected:
    /// strict SML requires the W-Bit as a separate `W` word.
    #[test]
    fn attached_wait_bit_header_rejected() {
        assert_eq!(
            parse_message("S1F1W."),
            Err(ParseError::InvalidMessageHeader {
                pos: SourcePos::new(0, 1, 1),
                text: "S1F1W".to_string(),
            })
        );
    }

    /// Confirms an attached `W` is rejected even when a standalone `W`
    /// follows, because the malformed header word fails first.
    #[test]
    fn attached_wait_bit_with_standalone_w_rejected() {
        assert_eq!(
            parse_message("S1F1W W."),
            Err(ParseError::InvalidMessageHeader {
                pos: SourcePos::new(0, 1, 1),
                text: "S1F1W".to_string(),
            })
        );
    }

    /// Confirms a second standalone `W` fails at the body/terminator stage
    /// with an unexpected-token error, not a header error.
    #[test]
    fn second_standalone_wait_bit_rejected() {
        assert!(matches!(
            parse_message("S1F1 W W."),
            Err(ParseError::UnexpectedToken { found, .. }) if found == "word \"W\""
        ));
    }

    /// Confirms `S1F1 W <L [0]> .` carries an empty-list body, which is
    /// distinct from being bodyless.
    #[test]
    fn message_with_empty_list_body_is_distinct_from_bodyless() {
        let message = parse_message("S1F1 W <L [0]> .").expect("empty list body parses");
        assert!(message.wait_bit());
        assert_eq!(message.body(), Some(&SecsItem::List(Vec::new())));
    }

    /// Confirms a message with a numeric body parses with all accessors.
    #[test]
    fn message_with_item_body_parses() {
        let message = parse_message("S2F42 <B [2] 0x01 0x02> .").expect("message with body parses");
        assert_eq!(message.stream().get(), 2);
        assert_eq!(message.function().get(), 42);
        assert!(!message.wait_bit());
        assert_eq!(message.body(), Some(&SecsItem::Binary(vec![1, 2])));
    }

    /// Confirms the maximum header numbers S127F255 parse.
    #[test]
    fn maximum_header_numbers_parse() {
        let message = parse_message("S127F255.").expect("maximum header parses");
        assert_eq!(message.stream().get(), 127);
        assert_eq!(message.function().get(), 255);
    }

    /// Confirms stream 128 is out of range.
    #[test]
    fn stream_above_127_rejected() {
        assert_eq!(
            parse_message("S128F1."),
            Err(ParseError::StreamOutOfRange {
                pos: SourcePos::new(0, 1, 1),
                value: 128,
            })
        );
    }

    /// Confirms function 256 is out of range.
    #[test]
    fn function_above_255_rejected() {
        assert_eq!(
            parse_message("S1F256."),
            Err(ParseError::FunctionOutOfRange {
                pos: SourcePos::new(0, 1, 1),
                value: 256,
            })
        );
    }

    /// Confirms a long digit run with leading zeros parses as its true
    /// value (checked folding, not a blanket bail on run length).
    #[test]
    fn long_leading_zero_header_stream_parses_as_one() {
        let message = parse_message("S00000000001F1.").expect("leading zeros are harmless");
        assert_eq!(message.stream().get(), 1);
        assert_eq!(message.function().get(), 1);
    }

    /// Confirms a stream digit run of exactly `u64::MAX` reports that value.
    #[test]
    fn u64_max_header_stream_reports_range_error() {
        assert_eq!(
            parse_message("S18446744073709551615F1."),
            Err(ParseError::StreamOutOfRange {
                pos: SourcePos::new(0, 1, 1),
                value: u64::MAX,
            })
        );
    }

    /// Confirms a genuinely overflowing stream digit run saturates to
    /// `u64::MAX` and reports the same range error.
    #[test]
    fn overflowing_header_stream_saturates_to_range_error() {
        assert_eq!(
            parse_message("S18446744073709551616F1."),
            Err(ParseError::StreamOutOfRange {
                pos: SourcePos::new(0, 1, 1),
                value: u64::MAX,
            })
        );
    }

    /// Confirms a long function digit run with leading zeros parses as its
    /// true value.
    #[test]
    fn long_leading_zero_header_function_parses() {
        let message = parse_message("S1F00000000000000000255.").expect("leading zeros parse");
        assert_eq!(message.stream().get(), 1);
        assert_eq!(message.function().get(), 255);
    }

    /// Confirms a word that is not a header shape is invalid.
    #[test]
    fn garbage_header_word_rejected() {
        assert_eq!(
            parse_message("HELLO."),
            Err(ParseError::InvalidMessageHeader {
                pos: SourcePos::new(0, 1, 1),
                text: "HELLO".to_string(),
            })
        );
    }

    /// Confirms a malformed header is reported before any later character is
    /// scanned: the trailing comma in `BAD,` is never reached.
    #[test]
    fn header_error_precedes_trailing_scan_error() {
        assert_eq!(
            parse_message("BAD,"),
            Err(ParseError::InvalidMessageHeader {
                pos: SourcePos::new(0, 1, 1),
                text: "BAD".to_string(),
            })
        );
    }

    /// Confirms a message header fails before the terminator when input ends.
    #[test]
    fn missing_terminator_rejected() {
        assert!(matches!(
            parse_message("S1F1"),
            Err(ParseError::MissingTerminator { .. })
        ));
    }

    /// Confirms a lexically invalid character after the terminator surfaces
    /// as the scanner's own error, not as trailing input.
    #[test]
    fn trailing_invalid_character_reports_scan_error() {
        assert_eq!(
            parse_message("S1F1.,"),
            Err(ParseError::InvalidCharacter {
                pos: SourcePos::new(5, 1, 6),
                found: ',',
            })
        );
    }

    /// Confirms lexically valid trailing content still fails with
    /// `TrailingInput` and its position and preview.
    ///
    /// The trailing word is `y` because the delivered scanner refuses
    /// word-initial `x`/`X` as a presumed botched hex prefix.
    #[test]
    fn trailing_valid_token_is_trailing_input() {
        assert_eq!(
            parse_message("S1F1 . y"),
            Err(ParseError::TrailingInput {
                pos: SourcePos::new(7, 1, 8),
                preview: "y".to_string(),
            })
        );
        assert_eq!(
            parse_message("S1F1 . 5"),
            Err(ParseError::TrailingInput {
                pos: SourcePos::new(7, 1, 8),
                preview: "5".to_string(),
            })
        );
    }

    /// Confirms the trailing preview is capped at twenty characters.
    #[test]
    fn trailing_preview_caps_at_twenty_characters() {
        let Err(ParseError::TrailingInput { preview, .. }) =
            parse_message(&format!("S1F1 . {}", "A".repeat(30)))
        else {
            panic!("trailing content must be rejected");
        };
        assert_eq!(preview, "A".repeat(20));
    }

    /// Confirms a split header `S1 F1` is invalid as a whole.
    #[test]
    fn split_header_word_rejected() {
        assert_eq!(
            parse_message("S1 F1 ."),
            Err(ParseError::InvalidMessageHeader {
                pos: SourcePos::new(0, 1, 1),
                text: "S1".to_string(),
            })
        );
    }

    /// Confirms a compact glued message parses with its body.
    #[test]
    fn compact_message_without_spaces_parses() {
        let message = parse_message("S1F2<L[1]<A[1]\"x\">>.").expect("compact message parses");
        assert_eq!(message.stream().get(), 1);
        assert_eq!(message.function().get(), 2);
        assert_eq!(
            message.body(),
            Some(&SecsItem::List(vec![SecsItem::Ascii(
                AsciiString::try_from("x").expect("x is ASCII")
            )]))
        );
    }

    /// Confirms a pretty multi-line message parses identically.
    #[test]
    fn multiline_message_parses() {
        let source = "S2F41 W\n  <L [2]\n    <A \"PPID\">\n    <U1 [1] 5>\n  >\n.\n";
        let message = parse_message(source).expect("multi-line message parses");
        assert!(message.wait_bit());
        assert_eq!(
            message.body(),
            Some(&SecsItem::List(vec![
                SecsItem::Ascii(AsciiString::try_from("PPID").expect("PPID is ASCII")),
                SecsItem::U1(vec![5]),
            ]))
        );
    }

    /// Confirms empty message input reports end of input at the header.
    #[test]
    fn empty_message_input_rejected() {
        assert!(matches!(
            parse_message(""),
            Err(ParseError::UnexpectedToken { expected, found, .. })
                if expected == "a message header such as S1F13" && found == "end of input"
        ));
    }

    /// Confirms limits bind before unseen input is scanned: the node budget
    /// rejects the second item while the trailing comma, never scanned,
    /// would have failed first under eager scanning.
    #[test]
    fn limits_bind_before_trailing_garbage_is_scanned() {
        let parser = parser_with_limits(64, 1, 0x00FF_FFFF, 1_000_000);
        assert_eq!(
            parser.parse_item("<L <B [0]> ,>"),
            Err(ParseError::TotalItemsExceeded {
                pos: SourcePos::new(3, 1, 4),
                max_total_items: 1,
            })
        );
    }

    /// Confirms depth limit one still accepts a flat list.
    #[test]
    fn depth_limit_one_accepts_flat_list() {
        let parser = parser_with_limits(1, 1_000_000, 0x00FF_FFFF, 1_000_000);
        assert_eq!(parser.parse_item("<L [0]>"), Ok(SecsItem::List(Vec::new())));
    }

    /// Confirms depth limit one rejects a nested list at depth two.
    #[test]
    fn depth_limit_one_rejects_nested_list() {
        let parser = parser_with_limits(1, 1_000_000, 0x00FF_FFFF, 1_000_000);
        assert!(matches!(
            parser.parse_item("<L [1] <L [0]>>"),
            Err(ParseError::DepthExceeded {
                depth: 2,
                max_depth: 1,
                ..
            })
        ));
    }

    /// Confirms the List depth check runs before the declared count is
    /// scanned: with `max_depth` 1, the inner list of `<L <L [99…]>` fails
    /// with `DepthExceeded`, and the forty-digit count — which would drown
    /// in the scanner's `NumberTooLarge` if scanned — is never read.
    #[test]
    fn list_depth_exceeded_before_count_scan() {
        let parser = parser_with_limits(1, 1_000_000, 0x00FF_FFFF, 1_000_000);
        let source = format!("<L <L [{}]>", "9".repeat(40));
        assert_eq!(
            parser.parse_item(&source),
            Err(ParseError::DepthExceeded {
                pos: SourcePos::new(3, 1, 4),
                depth: 2,
                max_depth: 1,
            })
        );
    }

    /// Confirms a one-node budget rejects the second node of a tree.
    #[test]
    fn total_items_one_rejects_second_node() {
        let parser = parser_with_limits(64, 1, 0x00FF_FFFF, 1_000_000);
        assert_eq!(
            parser.parse_item("<L [1] <A>>"),
            Err(ParseError::TotalItemsExceeded {
                pos: SourcePos::new(7, 1, 8),
                max_total_items: 1,
            })
        );
    }

    /// Confirms a four-byte budget rejects a five-element U1 body from the
    /// declared-count pre-check, with the exact required byte count, before
    /// any element is read.
    #[test]
    fn item_bytes_limit_rejects_five_byte_body() {
        let parser = parser_with_limits(64, 1_000_000, 4, 1_000_000);
        assert_eq!(
            parser.parse_item("<U1 [5] 1 2 3 4 5>"),
            Err(ParseError::ItemBytesExceeded {
                pos: SourcePos::new(1, 1, 2),
                required_bytes: 5,
                max_item_bytes: 4,
            })
        );
    }

    /// Confirms a four-byte budget also rejects an `<A>` whose declared
    /// character count alone exceeds it, before any fragment is read.
    #[test]
    fn ascii_declared_pre_check_fires_before_elements() {
        let parser = parser_with_limits(64, 1_000_000, 4, 1_000_000);
        assert_eq!(
            parser.parse_item("<A [5] \"ab\">"),
            Err(ParseError::ItemBytesExceeded {
                pos: SourcePos::new(1, 1, 2),
                required_bytes: 5,
                max_item_bytes: 4,
            })
        );
    }

    /// Confirms a one-child list budget rejects a declared two-child list
    /// at the open-time declared-count check.
    #[test]
    fn list_items_limit_rejects_declared_wide_list() {
        let parser = parser_with_limits(64, 1_000_000, 0x00FF_FFFF, 1);
        assert_eq!(
            parser.parse_item("<L [2] <A> <B>>"),
            Err(ParseError::ListItemsExceeded {
                pos: SourcePos::new(1, 1, 2),
                max_list_items: 1,
            })
        );
    }

    /// Confirms a one-child list budget rejects an omitted-count list that
    /// grows to two children, at the second child's `<` admission gate.
    #[test]
    fn list_items_limit_rejects_omitted_count_growth() {
        let parser = parser_with_limits(64, 1_000_000, 0x00FF_FFFF, 1);
        assert!(matches!(
            parser.parse_item("<L <A> <B>>"),
            Err(ParseError::ListItemsExceeded {
                max_list_items: 1,
                ..
            })
        ));
    }

    /// Confirms list admission fires at the second child's `<`, before its
    /// subtree is parsed: the hard limit wins over the later close-time
    /// count check, and a lexically broken second child is never scanned.
    #[test]
    fn list_admission_hard_limit_fires_before_second_child_subtree() {
        let parser = parser_with_limits(64, 1_000_000, 0x00FF_FFFF, 1);
        assert_eq!(
            parser.parse_item("<L [1] <B [0]> <L [2] <A [1] \"x\"> <B [0]>>>"),
            Err(ParseError::ListItemsExceeded {
                pos: SourcePos::new(1, 1, 2),
                max_list_items: 1,
            })
        );
        // The comma inside the never-parsed second child is never scanned;
        // under subtree-first parsing this would fail with
        // InvalidCharacter instead.
        assert!(matches!(
            parser.parse_item("<L [1] <B [0]> <B ,>>"),
            Err(ParseError::ListItemsExceeded {
                max_list_items: 1,
                ..
            })
        ));
    }

    /// Confirms the declared-count admission gate fires immediately at the
    /// second child's `<` with the detection-time count, before any of that
    /// child's subtree is parsed.
    #[test]
    fn list_declared_count_admission_fires_immediately() {
        assert_eq!(
            parse_item("<L [1] <B [0]> <L [2] <A [1] \"x\"> <B [0]>>>"),
            Err(ParseError::CountMismatch {
                pos: SourcePos::new(1, 1, 2),
                declared: 1,
                actual: 2,
            })
        );
    }

    /// Confirms the hard per-item byte gate outranks element value
    /// semantics: with a one-byte budget, `<U2 65536>` fails on the
    /// projected two body bytes, not on the out-of-range literal.
    #[test]
    fn item_bytes_gate_precedes_integer_overflow() {
        let parser = parser_with_limits(64, 1_000_000, 1, 1_000_000);
        assert_eq!(
            parser.parse_item("<U2 65536>"),
            Err(ParseError::ItemBytesExceeded {
                pos: SourcePos::new(1, 1, 2),
                required_bytes: 2,
                max_item_bytes: 1,
            })
        );
    }

    /// Confirms the declared-count admission gate outranks element value
    /// semantics in every scalar body: a zero-declared item rejects its
    /// first element with `CountMismatch` before the element's own value
    /// error (overflow, bad boolean word, non-ASCII byte, non-finite
    /// float) can fire.
    #[test]
    fn declared_count_gate_precedes_element_value_errors() {
        assert_eq!(
            parse_item("<U1[0] 0x0100>"),
            Err(ParseError::CountMismatch {
                pos: SourcePos::new(1, 1, 2),
                declared: 0,
                actual: 1,
            })
        );
        assert_eq!(
            parse_item("<BOOLEAN[0] MAYBE>"),
            Err(ParseError::CountMismatch {
                pos: SourcePos::new(1, 1, 2),
                declared: 0,
                actual: 1,
            })
        );
        assert_eq!(
            parse_item("<A[0] 0x80>"),
            Err(ParseError::CountMismatch {
                pos: SourcePos::new(1, 1, 2),
                declared: 0,
                actual: 1,
            })
        );
        assert_eq!(
            parse_item("<F4[0] 1e40>"),
            Err(ParseError::CountMismatch {
                pos: SourcePos::new(1, 1, 2),
                declared: 0,
                actual: 1,
            })
        );
    }

    /// Confirms element value semantics still fire once admission passes:
    /// wrong-category tokens at dispatch, integer overflow with the
    /// original spelling preserved, non-finite floats, invalid boolean
    /// words, and non-ASCII hex bytes.
    #[test]
    fn element_value_errors_survive_after_admission() {
        assert!(matches!(
            parse_item("<U1[0] TRUE>"),
            Err(ParseError::UnexpectedToken { expected, found, .. })
                if expected == "an integer literal" && found == "word \"TRUE\""
        ));
        assert!(matches!(
            parse_item("<U1[1] 0x0100>"),
            Err(ParseError::IntegerOverflow { literal, target, .. })
                if literal == "0x0100" && target == "U1"
        ));
        assert!(matches!(
            parse_item("<F4[1] 1e40>"),
            Err(ParseError::NonFiniteFloat { target: "F4", .. })
        ));
        assert!(matches!(
            parse_item("<BOOLEAN[1] MAYBE>"),
            Err(ParseError::InvalidBoolean { found, .. }) if found == "MAYBE"
        ));
        assert!(matches!(
            parse_item("<A[1] 0x80>"),
            Err(ParseError::InvalidAsciiByte { byte: 0x80, .. })
        ));
    }
}
