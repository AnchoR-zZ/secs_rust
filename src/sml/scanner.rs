//! SML lexical scanner.
//!
//! Turns raw SML source text into the positioned token stream consumed by the
//! parser (`super::parser`). Each [`Token`] carries its [`TokenKind`] plus the
//! [`SourcePos`] of its first character: a zero-based byte offset together
//! with a one-based line number and a one-based column counted in characters.
//! The scanner performs all lexical validation of the strict dialect —
//! printable-ASCII-only string fragments, `0xHH` byte literals, decimal/hex
//! integers that must fit `i128`, and float text kept verbatim so the parser
//! can convert F4/F8 values from the original text without double rounding —
//! while leaving every grammar decision (keyword recognition, counts, ranges)
//! to the parser.
//!
//! Tokens borrow their text from the input: `Word`, `Str`, and `Float` carry
//! `&str` slices, and the numeric variants additionally retain their original
//! literal text so parser diagnostics can quote the source spelling exactly
//! (`0xff` stays `0xff`). Scanning a token therefore performs no heap
//! allocation; owned `String`s are built only inside error payloads.

use super::error::{ParseError, SourcePos};

/// Lexical categories produced by the SML scanner.
///
/// All text payloads are zero-copy borrows of the scanned input (see the
/// module documentation). The enum is `Copy` because every field is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum TokenKind<'a> {
    /// One maximal ASCII-alphanumeric run starting with a letter; covers
    /// SxFy headers (`S1F1`), the standalone `W` word, type keywords, and
    /// `TRUE`/`FALSE`. Lexically attached forms such as `S1F1W` also scan as
    /// one `Word`; whether a header may carry an attached W-Bit is a grammar
    /// decision the strict parser rejects, not a lexical one.
    Word(&'a str),
    /// Double-quoted printable ASCII fragment, verbatim content without the
    /// quotes.
    Str(&'a str),
    /// `0xHH` two-hex-digit byte literal; `text` preserves the exact source
    /// spelling (hex digit case included) for diagnostics.
    HexByte {
        /// Parsed byte value.
        value: u8,
        /// Original literal text including the `0x` prefix.
        text: &'a str,
    },
    /// Integer literal in decimal or `0x` hex with optional leading `-`;
    /// `value` always fits i128, `hex` records whether the radix was 16, and
    /// `text` preserves the original spelling (sign, prefix, leading zeros,
    /// digit case) for diagnostics.
    Int {
        /// Parsed integer value; guaranteed to fit `i128` when this variant
        /// is produced (larger literals fail with `NumberTooLarge` instead).
        value: i128,
        /// Whether the literal was written in `0x` hexadecimal radix.
        hex: bool,
        /// Original literal text including any sign and `0x` prefix.
        text: &'a str,
    },
    /// Floating-point literal text (optional sign, digits, optional fraction
    /// and/or exponent); numeric conversion is deferred to the parser so F4
    /// and F8 convert from the original text without double rounding.
    Float(&'a str),
    /// `<`
    Lt,
    /// `>`
    Gt,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `.` message terminator (a dot not absorbed by a float literal).
    Dot,
}

/// One token with the position of its first character.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Token<'a> {
    /// Lexical category and payload of the token.
    pub kind: TokenKind<'a>,
    /// Position of the token's first character (the `-` of a signed number).
    pub pos: SourcePos,
}

/// Scans complete SML source text into tokens.
///
/// The scanner is a single forward pass: [`Scanner::next_token`] skips
/// whitespace, classifies exactly one token, and advances the internal
/// position. Position is tracked as a byte offset plus one-based line and
/// column (column counted in characters). Every byte the scanner consumes on
/// success paths is ASCII — non-ASCII input is refused with
/// `InvalidCharacter` before being consumed — so advancing never splits a
/// UTF-8 sequence and the byte offset always rests on a `str` char boundary.
pub(crate) struct Scanner<'a> {
    /// Full source text being scanned, borrowed for the scanner's lifetime.
    input: &'a str,
    /// Byte index of the next unread byte; always on a UTF-8 char boundary.
    offset: usize,
    /// One-based line number of the next unread byte.
    line: usize,
    /// One-based column of the next unread byte, counted in characters.
    column: usize,
}

impl<'a> Scanner<'a> {
    /// Creates a scanner positioned at the start of `input`.
    ///
    /// The scanner starts at byte offset 0, line 1, column 1. `input` is only
    /// borrowed, never copied; the scanner holds no other state than the
    /// borrowed text and its cursor. Returns the ready-to-use scanner.
    #[must_use]
    pub(crate) const fn new(input: &'a str) -> Self {
        Self {
            input,
            offset: 0,
            line: 1,
            column: 1,
        }
    }

    /// Returns the next token, or `Ok(None)` at end of input.
    ///
    /// Skips any leading whitespace (` `, `\t`, `\n`, `\r`, with `\r\n`
    /// counted as one newline), then classifies one token starting at the
    /// next character. On success the cursor rests on the first byte after
    /// the token. Repeated calls after end of input keep returning
    /// `Ok(None)` and leave the cursor unchanged.
    ///
    /// # Errors
    ///
    /// Returns a [`ParseError`] when the upcoming input cannot begin or
    /// complete a legal token: `InvalidCharacter` for any character that
    /// cannot start a token (including non-ASCII anywhere and a `-` not
    /// followed by a digit), `UnterminatedString` when a quoted fragment
    /// reaches end of input, `InvalidAsciiFragment` for control bytes, `0x7F`,
    /// or `\` inside quotes, `InvalidHexLiteral` for a bare `0x` prefix, and
    /// `NumberTooLarge` when an integer literal does not fit `i128`.
    pub(crate) fn next_token(&mut self) -> Result<Option<Token<'a>>, ParseError> {
        self.skip_whitespace();
        let Some(first) = self.peek() else {
            return Ok(None);
        };
        let pos = self.current_pos();
        if first >= 0x80 {
            // Non-ASCII is illegal everywhere in the strict dialect; decode
            // the full character for the diagnostic before consuming it.
            let found = self.current_char();
            return Err(ParseError::InvalidCharacter { pos, found });
        }
        let kind = match first {
            b'.' => {
                self.bump_ascii();
                TokenKind::Dot
            }
            b'<' => {
                self.bump_ascii();
                TokenKind::Lt
            }
            b'>' => {
                self.bump_ascii();
                TokenKind::Gt
            }
            b'[' => {
                self.bump_ascii();
                TokenKind::LBracket
            }
            b']' => {
                self.bump_ascii();
                TokenKind::RBracket
            }
            b'"' => self.scan_string(pos)?,
            b'-' => {
                if self.peek_ahead(1).is_some_and(|b| b.is_ascii_digit()) {
                    self.bump_ascii();
                    self.scan_number(pos, true)?
                } else {
                    return Err(ParseError::InvalidCharacter { pos, found: '-' });
                }
            }
            b'0'..=b'9' => self.scan_number(pos, false)?,
            // `x`/`X` never legitimately begins a word in the strict dialect:
            // the only legal `x` is the `0x` hex prefix, so a bare `x` (as in
            // the malformed `00x1`, where the digit run `00` blocks hex
            // detection) is presumed a botched hex literal and refused.
            b'x' | b'X' => {
                return Err(ParseError::InvalidCharacter {
                    pos,
                    found: char::from(first),
                });
            }
            b'a'..=b'z' | b'A'..=b'Z' => self.scan_word(),
            _ => {
                return Err(ParseError::InvalidCharacter {
                    pos,
                    found: char::from(first),
                });
            }
        };
        Ok(Some(Token { kind, pos }))
    }

    /// Returns the position the NEXT token would start at (i.e. end-of-input
    /// position once exhausted).
    ///
    /// The returned [`SourcePos`] combines the current byte offset with the
    /// current one-based line and character column. After `next_token` has
    /// returned `Ok(None)` this is the end-of-input position, which the
    /// parser reports for `MissingTerminator`.
    #[must_use]
    pub(crate) const fn current_pos(&self) -> SourcePos {
        SourcePos::new(self.offset, self.line, self.column)
    }

    /// Consumes the maximal ASCII-alphanumeric run starting at the cursor and
    /// returns it as a `Word` token kind.
    ///
    /// The cursor must rest on an ASCII letter (guaranteed by `next_token`);
    /// digits are absorbed into the run, so `S1F1W` and `L2` each scan as one
    /// word. Returns the word as a slice of the input; this method cannot
    /// fail.
    fn scan_word(&mut self) -> TokenKind<'a> {
        let start = self.offset;
        while self.peek().is_some_and(|b| b.is_ascii_alphanumeric()) {
            self.bump_ascii();
        }
        TokenKind::Word(&self.input[start..self.offset])
    }

    /// Scans a double-quoted ASCII string fragment whose opening `"` sits at
    /// the cursor.
    ///
    /// `open_pos` is the position of that opening quote (already captured by
    /// the caller); it is echoed in `UnterminatedString`. Content bytes must
    /// be printable ASCII `0x20..=0x7E` other than `"` (terminator) and `\`
    /// (escape): controls, `0x7F`, and `\` must be written as `0xHH` in
    /// strict SML and fail with `InvalidAsciiFragment`, while non-ASCII
    /// fails with `InvalidCharacter`. The cursor advances past the closing
    /// quote on success. Returns `Str` with the verbatim content as a slice.
    ///
    /// # Errors
    ///
    /// Returns `UnterminatedString` (at the opening quote) when input ends
    /// before the closing quote, `InvalidAsciiFragment` for a refused byte at
    /// its own position, or `InvalidCharacter` for a non-ASCII character.
    fn scan_string(&mut self, open_pos: SourcePos) -> Result<TokenKind<'a>, ParseError> {
        self.bump_ascii(); // opening quote
        let content_start = self.offset;
        loop {
            let Some(byte) = self.peek() else {
                return Err(ParseError::UnterminatedString { pos: open_pos });
            };
            match byte {
                b'"' => {
                    let content_end = self.offset;
                    self.bump_ascii();
                    return Ok(TokenKind::Str(&self.input[content_start..content_end]));
                }
                b'\\' => {
                    return Err(ParseError::InvalidAsciiFragment {
                        pos: self.current_pos(),
                        byte,
                    });
                }
                0x20..=0x7E => {
                    self.bump_ascii();
                }
                byte if byte >= 0x80 => {
                    let found = self.current_char();
                    return Err(ParseError::InvalidCharacter {
                        pos: self.current_pos(),
                        found,
                    });
                }
                _ => {
                    // Controls `0x00..=0x1F` and DEL `0x7F`.
                    return Err(ParseError::InvalidAsciiFragment {
                        pos: self.current_pos(),
                        byte,
                    });
                }
            }
        }
    }

    /// Scans an integer or float literal whose first digit sits at the cursor
    /// and returns its token kind.
    ///
    /// `start_pos` is the position of the literal's first character including
    /// a leading `-` already consumed by the caller, and `negative` records
    /// whether such a sign was present. The leading maximal decimal digit run
    /// is scanned first; when it is exactly `0` followed by `x`, the hex
    /// rules apply (two unsigned hex digits make a `HexByte`, otherwise an
    /// `Int` with radix 16). Otherwise optional fraction and exponent are
    /// appended; a literal with either becomes a `Float` carrying the exact
    /// source text, and a plain digit run becomes a decimal `Int`. Numeric
    /// variants carry the original literal text for diagnostics.
    ///
    /// # Errors
    ///
    /// Returns `InvalidHexLiteral` when `0x` is not followed by any hex digit,
    /// or `NumberTooLarge` when the digit run does not fit `i128` in the
    /// literal's radix; both carry the position of the literal's first
    /// character and (for the latter) the full literal text including sign.
    fn scan_number(
        &mut self,
        start_pos: SourcePos,
        negative: bool,
    ) -> Result<TokenKind<'a>, ParseError> {
        let digits_start = self.offset;
        while self.peek().is_some_and(|b| b.is_ascii_digit()) {
            self.bump_ascii();
        }
        let int_digits = &self.input[digits_start..self.offset];

        // Hex detection requires the leading digit run to be exactly "0":
        // `00x1` stays a decimal 0 followed by a refused `x`.
        if int_digits == "0" && self.peek() == Some(b'x') {
            return self.scan_hex_tail(start_pos, negative);
        }

        // Fraction: `.` accepted only when a digit follows, so `1.` lexes as
        // Int 1 plus a separate Dot token.
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.peek_ahead(1).is_some_and(|b| b.is_ascii_digit()) {
            self.bump_ascii();
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.bump_ascii();
            }
            is_float = true;
        }
        // Exponent: `e`/`E` then either a digit directly, or a single `+`/`-`
        // (the only place `+` is ever legal) followed by a digit.
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let after = self.peek_ahead(1);
            let has_exponent_digits = after.is_some_and(|b| b.is_ascii_digit())
                || (matches!(after, Some(b'+' | b'-'))
                    && self.peek_ahead(2).is_some_and(|b| b.is_ascii_digit()));
            if has_exponent_digits {
                self.bump_ascii(); // e/E
                if matches!(self.peek(), Some(b'+' | b'-')) {
                    self.bump_ascii();
                }
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.bump_ascii();
                }
                is_float = true;
            }
        }

        let text = self.literal_slice(start_pos);
        if is_float {
            // Numeric conversion is the parser's job; keep the exact text.
            return Ok(TokenKind::Float(text));
        }
        // Decimal parsing via `str::parse` is exactly `from_str_radix(.., 10)`
        // on this pure-digit string; overflow surfaces as `NumberTooLarge`.
        match int_digits.parse::<i128>() {
            Ok(value) => Ok(TokenKind::Int {
                value: if negative { -value } else { value },
                hex: false,
                text,
            }),
            Err(_) => Err(ParseError::NumberTooLarge {
                pos: start_pos,
                text: text.to_owned(),
            }),
        }
    }

    /// Scans the hex-digit tail of a `0x` literal; the cursor rests on the
    /// `x` on entry.
    ///
    /// `start_pos` is the literal's start (a leading `-` included) and
    /// `negative` records that sign. Consumes the `x` plus the maximal hex
    /// digit run, then classifies: exactly two digits without a sign make a
    /// `HexByte` (a signed two-digit run such as `-0x10` is an `Int` instead,
    /// since byte literals are unsigned), and one or three-or-more digits
    /// make a radix-16 `Int` with the sign applied. One refinement keeps
    /// concatenated byte literals self-delimiting: once exactly two digits
    /// are consumed, a following `0x` (digit zero then `x`) begins the next
    /// `0xHH` literal, so the run stops there — `0x010x02` scans as two
    /// HexBytes while `0x100` (no such boundary) stays one radix-16 Int.
    /// Both variants carry the original literal text.
    ///
    /// # Errors
    ///
    /// Returns `InvalidHexLiteral` when no hex digit follows the `0x`
    /// prefix, or `NumberTooLarge` when the hex run does not fit `i128`;
    /// both carry the literal's start position and scanned text.
    fn scan_hex_tail(
        &mut self,
        start_pos: SourcePos,
        negative: bool,
    ) -> Result<TokenKind<'a>, ParseError> {
        self.bump_ascii(); // the 'x' of the prefix
        let hex_start = self.offset;
        while self.peek().is_some_and(|b| b.is_ascii_hexdigit()) {
            // With two digits already consumed, `0x` next would start a fresh
            // literal (its `0` is also a hex digit, so absorb it no longer).
            if self.offset - hex_start == 2
                && self.peek() == Some(b'0')
                && self.peek_ahead(1) == Some(b'x')
            {
                break;
            }
            self.bump_ascii();
        }
        let hex_digits = &self.input[hex_start..self.offset];
        if hex_digits.is_empty() {
            return Err(ParseError::InvalidHexLiteral {
                pos: start_pos,
                text: self.literal_slice(start_pos).to_owned(),
            });
        }
        if hex_digits.len() == 2 && !negative {
            // Exactly two hex digits always encode a u8 (max 0xFF) and the
            // run holds only hex digits, so this parse cannot fail; the
            // `if let` merely avoids panicking on any input.
            if let Ok(value) = u8::from_str_radix(hex_digits, 16) {
                return Ok(TokenKind::HexByte {
                    value,
                    text: self.literal_slice(start_pos),
                });
            }
        }
        match i128::from_str_radix(hex_digits, 16) {
            Ok(value) => Ok(TokenKind::Int {
                value: if negative { -value } else { value },
                hex: true,
                text: self.literal_slice(start_pos),
            }),
            Err(_) => Err(ParseError::NumberTooLarge {
                pos: start_pos,
                text: self.literal_slice(start_pos).to_owned(),
            }),
        }
    }

    /// Returns the literal source text spanning from `start_pos` to the
    /// cursor, as a borrowed slice of the input.
    ///
    /// Used for `Float` payloads and diagnostic texts; the slice is safe
    /// because the scanner only advances over ASCII bytes.
    #[must_use]
    fn literal_slice(&self, start_pos: SourcePos) -> &'a str {
        &self.input[start_pos.offset..self.offset]
    }

    /// Skips ` `, `\t`, `\n`, and `\r` starting at the cursor.
    ///
    /// `\n` and a lone `\r` each start a new line (line += 1, column = 1);
    /// a `\r\n` pair counts as ONE newline, implemented by consuming the
    /// `\r` as the newline and silently absorbing a following `\n`. The
    /// cursor stops on the first non-whitespace byte or at end of input.
    fn skip_whitespace(&mut self) {
        while let Some(byte) = self.peek() {
            match byte {
                b' ' | b'\t' => self.bump_ascii(),
                b'\n' => self.advance_newline(),
                b'\r' => {
                    self.advance_newline();
                    if self.peek() == Some(b'\n') {
                        self.offset += 1;
                    }
                }
                _ => break,
            }
        }
    }

    /// Returns the byte at the cursor, or `None` at end of input.
    #[must_use]
    fn peek(&self) -> Option<u8> {
        self.input.as_bytes().get(self.offset).copied()
    }

    /// Returns the byte `ahead` positions after the cursor, or `None` when
    /// that far past end of input; `peek_ahead(0)` equals `peek`.
    #[must_use]
    fn peek_ahead(&self, ahead: usize) -> Option<u8> {
        self.input.as_bytes().get(self.offset + ahead).copied()
    }

    /// Advances the cursor over one ASCII byte (not a newline).
    ///
    /// The caller must ensure the byte at the cursor is single-byte ASCII;
    /// column increments by one because one byte is one character.
    fn bump_ascii(&mut self) {
        self.offset += 1;
        self.column += 1;
    }

    /// Advances the cursor over a newline byte, starting a new line.
    ///
    /// Consumes exactly the one newline character (`\n` or `\r`), increments
    /// the line number, and resets the column to one.
    fn advance_newline(&mut self) {
        self.offset += 1;
        self.line += 1;
        self.column = 1;
    }

    /// Decodes the character starting at the cursor.
    ///
    /// Callers must only invoke this when the byte at the cursor is at least
    /// `0x80` (a UTF-8 lead byte) and the cursor is on a char boundary, which
    /// always holds because the scanner advances solely over ASCII. The
    /// replacement-character fallback is unreachable on those paths and keeps
    /// the method total without panicking.
    #[must_use]
    fn current_char(&self) -> char {
        match self.input[self.offset..].chars().next() {
            Some(found) => found,
            None => char::REPLACEMENT_CHARACTER,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the SML lexical scanner.

    use super::*;

    /// Builds a [`SourcePos`] shorthand for expected-position assertions.
    fn pos(offset: usize, line: usize, column: usize) -> SourcePos {
        SourcePos::new(offset, line, column)
    }

    /// Builds an `Int` token kind shorthand with an explicit original text.
    fn int_as(value: i128, text: &'static str) -> TokenKind<'static> {
        TokenKind::Int {
            value,
            hex: false,
            text,
        }
    }

    /// Builds a radix-16 `Int` token kind shorthand with explicit text.
    fn int_hex(value: i128, text: &'static str) -> TokenKind<'static> {
        TokenKind::Int {
            value,
            hex: true,
            text,
        }
    }

    /// Builds a `HexByte` token kind shorthand with explicit text.
    fn hex(value: u8, text: &'static str) -> TokenKind<'static> {
        TokenKind::HexByte { value, text }
    }

    /// Scans `input` to completion, asserting that no lexical error occurs,
    /// and returns every produced token.
    fn scan_all(input: &str) -> Vec<Token<'_>> {
        let mut scanner = Scanner::new(input);
        let mut tokens = Vec::new();
        while let Some(token) = scanner.next_token().expect("scan should succeed") {
            tokens.push(token);
        }
        tokens
    }

    /// Scans `input` to completion and returns only the token kinds, for
    /// assertions that do not care about positions.
    fn kinds(input: &str) -> Vec<TokenKind<'_>> {
        scan_all(input)
            .into_iter()
            .map(|token| token.kind)
            .collect()
    }

    /// Confirms a full representative single-line message yields the exact
    /// token sequence, with correct positions on the first, a middle, and the
    /// final token.
    #[test]
    fn full_single_line_message_yields_expected_token_stream() {
        let tokens = scan_all("S5F1 W <L [2] <B [1] 0x04> <A \"LOT001\">> .");
        let expected = vec![
            TokenKind::Word("S5F1"),
            TokenKind::Word("W"),
            TokenKind::Lt,
            TokenKind::Word("L"),
            TokenKind::LBracket,
            TokenKind::Int {
                value: 2,
                hex: false,
                text: "2",
            },
            TokenKind::RBracket,
            TokenKind::Lt,
            TokenKind::Word("B"),
            TokenKind::LBracket,
            TokenKind::Int {
                value: 1,
                hex: false,
                text: "1",
            },
            TokenKind::RBracket,
            TokenKind::HexByte {
                value: 0x04,
                text: "0x04",
            },
            TokenKind::Gt,
            TokenKind::Lt,
            TokenKind::Word("A"),
            TokenKind::Str("LOT001"),
            TokenKind::Gt,
            TokenKind::Gt,
            TokenKind::Dot,
        ];
        let actual: Vec<TokenKind> = tokens.iter().map(|t| t.kind).collect();
        assert_eq!(actual, expected);
        assert_eq!(tokens[0].pos, pos(0, 1, 1), "first token S5F1");
        assert_eq!(tokens[12].pos, pos(21, 1, 22), "middle token 0x04");
        assert_eq!(tokens[19].pos, pos(41, 1, 42), "terminating dot");
    }

    /// Confirms multi-line input tracks one-based line and character column
    /// per token, treats `\r\n` as a single newline, and reports byte-based
    /// offsets after each newline.
    #[test]
    fn multi_line_input_tracks_lines_columns_and_byte_offsets() {
        let tokens = scan_all("S1F13\nW <B [1]\r\n0x01.");
        let expected = vec![
            TokenKind::Word("S1F13"),
            TokenKind::Word("W"),
            TokenKind::Lt,
            TokenKind::Word("B"),
            TokenKind::LBracket,
            TokenKind::Int {
                value: 1,
                hex: false,
                text: "1",
            },
            TokenKind::RBracket,
            TokenKind::HexByte {
                value: 0x01,
                text: "0x01",
            },
            TokenKind::Dot,
        ];
        let actual: Vec<TokenKind> = tokens.iter().map(|t| t.kind).collect();
        assert_eq!(actual, expected);
        assert_eq!(tokens[0].pos, pos(0, 1, 1), "header on line 1");
        assert_eq!(tokens[1].pos, pos(6, 2, 1), "W directly after \\n");
        assert_eq!(tokens[2].pos, pos(8, 2, 3), "< after 'W '");
        // \r\n occupies bytes 14-15 and counts as ONE newline: 0x01 is on
        // line 3, not line 4.
        assert_eq!(tokens[7].pos, pos(16, 3, 1), "0x01 after \\r\\n");
        assert_eq!(tokens[8].pos, pos(20, 3, 5), "dot after 0x01");
    }

    /// Confirms a lone `\r` (with no following `\n`) also counts as one
    /// newline.
    #[test]
    fn lone_carriage_return_counts_as_one_newline() {
        let tokens = scan_all("S1F1\r.");
        assert_eq!(tokens.len(), 2);
        assert_eq!(tokens[1].pos, pos(5, 2, 1), "dot lands on line 2");
    }

    /// Confirms tokens need no separating whitespace: attached forms like
    /// `S1F1W.`, `<B[3]0x010x02 0x03>` still split correctly because words,
    /// brackets, and two-digit hex bytes are self-delimiting. `S1F1W` is
    /// lexically one word; rejecting the attached W-Bit is the parser's job.
    #[test]
    fn whitespace_between_tokens_is_optional() {
        assert_eq!(
            kinds("S1F1W."),
            vec![TokenKind::Word("S1F1W"), TokenKind::Dot]
        );
        assert_eq!(
            kinds("S1F1 W ."),
            vec![
                TokenKind::Word("S1F1"),
                TokenKind::Word("W"),
                TokenKind::Dot
            ]
        );
        assert_eq!(
            kinds("<B[3]0x010x02 0x03>"),
            vec![
                TokenKind::Lt,
                TokenKind::Word("B"),
                TokenKind::LBracket,
                TokenKind::Int {
                    value: 3,
                    hex: false,
                    text: "3",
                },
                TokenKind::RBracket,
                TokenKind::HexByte {
                    value: 0x01,
                    text: "0x01",
                },
                TokenKind::HexByte {
                    value: 0x02,
                    text: "0x02",
                },
                TokenKind::HexByte {
                    value: 0x03,
                    text: "0x03",
                },
                TokenKind::Gt,
            ]
        );
    }

    /// Confirms the empty string literal lexes as an empty `Str` and a plain
    /// string with spaces keeps its content verbatim.
    #[test]
    fn empty_and_plain_string_literals_lex_verbatim() {
        assert_eq!(kinds("\"\" ."), vec![TokenKind::Str(""), TokenKind::Dot]);
        assert_eq!(
            kinds("\"plain text with spaces\""),
            vec![TokenKind::Str("plain text with spaces")]
        );
    }

    /// Confirms a string without a closing quote fails with
    /// `UnterminatedString` positioned at the OPENING quote.
    #[test]
    fn unterminated_string_reports_opening_quote_position() {
        let mut scanner = Scanner::new("\"abc");
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::UnterminatedString { pos: pos(0, 1, 1) })
        );
    }

    /// Confirms a raw control byte (newline, 0x0A) inside quotes fails with
    /// `InvalidAsciiFragment` at that byte's own position.
    #[test]
    fn control_char_in_string_is_invalid_ascii_fragment() {
        let mut scanner = Scanner::new("\"\n\"");
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidAsciiFragment {
                pos: pos(1, 1, 2),
                byte: 0x0A,
            })
        );
    }

    /// Confirms a backslash (0x5C) inside quotes fails with
    /// `InvalidAsciiFragment` instead of acting as an escape.
    #[test]
    fn backslash_in_string_is_invalid_ascii_fragment() {
        let mut scanner = Scanner::new("\"a\\b\"");
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidAsciiFragment {
                pos: pos(2, 1, 3),
                byte: 0x5C,
            })
        );
    }

    /// Confirms plain decimal integers scan with leading zeros allowed and a
    /// leading minus folded into the value; the text keeps the original
    /// spelling including leading zeros.
    #[test]
    fn decimal_integers_accept_leading_zeros_and_minus() {
        assert_eq!(kinds("0"), vec![int_as(0, "0")]);
        assert_eq!(kinds("007"), vec![int_as(7, "007")]);
        assert_eq!(kinds("-10"), vec![int_as(-10, "-10")]);
    }

    /// Confirms hex literals classify by digit count and sign: two unsigned
    /// digits are a `HexByte`, anything else (including signed two-digit
    /// forms like `-0x10`) is a radix-16 `Int`; texts keep sign and prefix.
    #[test]
    fn hex_literals_classify_by_digit_count_and_sign() {
        assert_eq!(kinds("-0x10"), vec![int_hex(-16, "-0x10")]);
        assert_eq!(kinds("0x1"), vec![int_hex(1, "0x1")]);
        assert_eq!(kinds("0x100"), vec![int_hex(256, "0x100")]);
        assert_eq!(kinds("0xAA"), vec![hex(0xAA, "0xAA")]);
        assert_eq!(kinds("0xaa"), vec![hex(0xAA, "0xaa")]);
    }

    /// Confirms float literals carry their exact source text, including the
    /// sign, fraction, and exponent forms.
    #[test]
    fn float_literals_preserve_original_text() {
        assert_eq!(kinds("1.5"), vec![TokenKind::Float("1.5")]);
        assert_eq!(kinds("-2.5e-3"), vec![TokenKind::Float("-2.5e-3")]);
        assert_eq!(kinds("1e5"), vec![TokenKind::Float("1e5")]);
        assert_eq!(kinds("1e+5"), vec![TokenKind::Float("1e+5")]);
    }

    /// Confirms concatenated two-digit hex literals self-delimit (`0x010x02`
    /// is two HexBytes, each with its own text) while a genuine three-digit
    /// run such as `0x100` still lexes as one radix-16 Int.
    #[test]
    fn concatenated_hex_bytes_self_delimit_without_spaces() {
        assert_eq!(
            kinds("0x010x02"),
            vec![hex(0x01, "0x01"), hex(0x02, "0x02")]
        );
        assert_eq!(
            kinds("0x010x020x03"),
            vec![hex(0x01, "0x01"), hex(0x02, "0x02"), hex(0x03, "0x03")]
        );
        assert_eq!(kinds("0x100"), vec![int_hex(256, "0x100")]);
    }

    /// Confirms a dot not followed by a digit is NOT part of a number:
    /// `1.` lexes as Int 1 followed by a separate Dot terminator.
    #[test]
    fn integer_followed_by_bare_dot_splits_into_int_and_dot() {
        assert_eq!(kinds("1."), vec![int_as(1, "1"), TokenKind::Dot]);
    }

    /// Confirms a `0x` prefix without any hex digit fails with
    /// `InvalidHexLiteral` carrying exactly the scanned text `"0x"`.
    #[test]
    fn hex_prefix_without_digits_is_rejected() {
        let mut scanner = Scanner::new("0xz");
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidHexLiteral {
                pos: pos(0, 1, 1),
                text: "0x".into(),
            })
        );
    }

    /// Confirms integer literals wider than `i128` fail with
    /// `NumberTooLarge` in both decimal and hex radix, echoing the full
    /// literal text.
    #[test]
    fn oversized_decimal_and_hex_integers_are_number_too_large() {
        let decimal = "9".repeat(40);
        let mut scanner = Scanner::new(&decimal);
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::NumberTooLarge {
                pos: pos(0, 1, 1),
                text: decimal.clone(),
            })
        );
        let hexish = format!("0x{}", "F".repeat(40));
        let mut scanner = Scanner::new(&hexish);
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::NumberTooLarge {
                pos: pos(0, 1, 1),
                text: hexish.clone(),
            })
        );
    }

    /// Confirms the hex-detection subtlety: a leading digit run of `00` is a
    /// decimal Int 0, and the following `x` is refused as an invalid
    /// character rather than starting a word.
    #[test]
    fn double_zero_before_hex_prefix_is_int_zero_then_invalid_x() {
        let mut scanner = Scanner::new("00x1");
        assert_eq!(
            scanner.next_token(),
            Ok(Some(Token {
                kind: int_as(0, "00"),
                pos: pos(0, 1, 1),
            }))
        );
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidCharacter {
                pos: pos(2, 1, 3),
                found: 'x',
            })
        );
    }

    /// Confirms a comma cannot begin any token.
    #[test]
    fn comma_is_invalid_character() {
        let mut scanner = Scanner::new(",");
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidCharacter {
                pos: pos(0, 1, 1),
                found: ',',
            })
        );
    }

    /// Confirms a non-ASCII character is refused with the full character in
    /// the error.
    #[test]
    fn non_ascii_character_is_invalid_character() {
        let mut scanner = Scanner::new("é");
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidCharacter {
                pos: pos(0, 1, 1),
                found: 'é',
            })
        );
    }

    /// Confirms a non-ASCII character after valid tokens is reported at its
    /// byte offset (5, after `0x01 `) and character column (6).
    #[test]
    fn non_ascii_after_ascii_reports_byte_offset_and_char_column() {
        let mut scanner = Scanner::new("0x01 é");
        assert_eq!(
            scanner.next_token(),
            Ok(Some(Token {
                kind: hex(0x01, "0x01"),
                pos: pos(0, 1, 1),
            }))
        );
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidCharacter {
                pos: pos(5, 1, 6),
                found: 'é',
            })
        );
    }

    /// Confirms `+` is only legal inside a float exponent and that a `-` not
    /// followed by a digit is refused at token start.
    #[test]
    fn plus_and_lone_dash_are_invalid_characters() {
        let mut scanner = Scanner::new("+5");
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidCharacter {
                pos: pos(0, 1, 1),
                found: '+',
            })
        );
        let mut scanner = Scanner::new("- x");
        assert_eq!(
            scanner.next_token(),
            Err(ParseError::InvalidCharacter {
                pos: pos(0, 1, 1),
                found: '-',
            })
        );
    }

    /// Confirms `current_pos` after exhaustion equals the position just past
    /// the last character and stays stable across further `next_token` calls.
    #[test]
    fn current_pos_after_exhaustion_is_position_after_last_char() {
        let mut scanner = Scanner::new("S1F1.");
        while scanner.next_token().expect("tokens should scan").is_some() {}
        assert_eq!(scanner.current_pos(), pos(5, 1, 6));
        assert_eq!(scanner.next_token(), Ok(None));
        assert_eq!(scanner.current_pos(), pos(5, 1, 6));
    }

    /// Confirms a fresh scanner over empty input reports the origin position.
    #[test]
    fn current_pos_on_empty_input_is_origin() {
        let mut scanner = Scanner::new("");
        assert_eq!(scanner.current_pos(), pos(0, 1, 1));
        assert_eq!(scanner.next_token(), Ok(None));
        assert_eq!(scanner.current_pos(), pos(0, 1, 1));
    }
}
