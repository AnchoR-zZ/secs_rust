//! Explicit-stack SECS-II decoder.
//!
//! The decoder is a single-pass, single-cursor iterator over the input
//! bytes. Lists are parsed with an explicit `Vec<ListFrame>` stack instead of
//! recursion, so parsing itself consumes `O(1)` host-call-stack space. Because
//! the public [`SecsItem`] tree is recursively owned, [`DecodeLimits`] also
//! enforces [`crate::secs2::MAX_DECODE_NESTING_DEPTH`]: successful result
//! destruction and error cleanup of completed subtrees therefore have a fixed
//! recursion bound. All declared resource limits are checked before payload
//! allocation, so a malicious length cannot force a large `Vec` before
//! rejection.
//!
//! [`Secs2Decoder`] is the sole public decode entry point. It accepts exactly
//! one complete item, rejects empty input and trailing bytes, and keeps HSMS
//! Message Text presence semantics outside the pure SECS-II codec.

use crate::secs2::{
    codec::{
        error::DecodeError,
        header::{FormatCode, LengthByteCount},
    },
    AsciiString, DecodeLimits, LocalizedEncodingCode, LocalizedString, SecsItem,
};

/// One entry on the explicit List stack.
///
/// A frame exists only while a non-empty List is being filled. Empty Lists
/// never push a frame because they resolve immediately at their header.
#[derive(Debug)]
struct ListFrame {
    /// Offset of the List's header inside the original input; used for
    /// diagnostics if input ends before all declared children arrive.
    header_offset: usize,
    /// Direct child count declared by the List's length bytes.
    expected_children: usize,
    /// Direct children decoded so far, in input order. The vector is never
    /// pre-allocated from the untrusted declared count; it grows as children
    /// arrive so a malformed `length` cannot trigger a huge allocation.
    children: Vec<SecsItem>,
}

/// Internal decoding cursor that walks `input` once, enforcing every
/// `DecodeLimits` field before any allocation.
struct DecodeCursor<'a> {
    /// Full input slice; `cursor` indexes into it.
    input: &'a [u8],
    /// Read position inside `input`.
    cursor: usize,
    /// Resource limits supplied by the caller.
    limits: DecodeLimits,
    /// Number of non-empty ancestor List frames currently open. A candidate
    /// List is checked at `depth + 1`; scalar items do not change this value.
    depth: usize,
    /// Number of successfully parsed and accepted item headers, including
    /// both scalar and List headers.
    accepted_nodes: usize,
    /// Explicit List stack, one entry per non-empty List currently being
    /// filled. Empty while decoding a scalar root item.
    list_stack: Vec<ListFrame>,
}

impl<'a> DecodeCursor<'a> {
    /// Constructs a fresh decoding cursor bound to `input` and `limits`.
    ///
    /// The read position, nesting depth and decoded-node counter all start at
    /// zero, and the explicit List stack begins empty. The supplied `limits`
    /// are consulted before every allocation, so an oversized declared length
    /// cannot force a large heap `Vec` to be built before being rejected.
    fn new(input: &'a [u8], limits: DecodeLimits) -> Self {
        Self {
            input,
            cursor: 0,
            limits,
            depth: 0,
            accepted_nodes: 0,
            list_stack: Vec::new(),
        }
    }

    /// Returns the number of bytes consumed so far.
    fn consumed(&self) -> usize {
        self.cursor
    }

    /// Accepts one successfully parsed item header and records its node.
    ///
    /// Returns [`DecodeError::TotalItemsExceeded`] when the resulting exact
    /// node requirement would exceed the configured maximum. Callers invoke
    /// this only after [`DecodeCursor::read_header`] succeeds and before allocating
    /// storage for the item.
    fn note_node(&mut self, header_offset: usize) -> Result<(), DecodeError> {
        let next = self
            .accepted_nodes
            .checked_add(1)
            .ok_or(DecodeError::ArithmeticOverflow {
                offset: header_offset,
            })?;
        if next > self.limits.max_total_items() {
            return Err(DecodeError::TotalItemsExceeded {
                offset: header_offset,
                required_items: next,
                max_total_items: self.limits.max_total_items(),
            });
        }
        self.accepted_nodes = next;
        Ok(())
    }

    /// Reads the format byte, Length Byte count, and declared length value for
    /// the item whose header starts at the current cursor.
    ///
    /// On success advances the cursor past the length bytes and returns the
    /// parsed format code together with the declared direct-child count for a
    /// List or declared body byte count for a non-List item.
    fn read_header(&mut self) -> Result<(FormatCode, usize), DecodeError> {
        let header_offset = self.cursor;
        let Some(&format_byte) = self.input.get(self.cursor) else {
            return Err(DecodeError::TruncatedHeader {
                offset: self.cursor,
            });
        };
        self.cursor += 1;

        let six_bit = format_byte >> 2;
        let low_bits = format_byte & 0b0000_0011;

        // Length-byte count 0 is explicitly illegal per E5 §6.2.1 / §6.3.1;
        // the other values represent the legal counts 1, 2 and 3.
        let count = match LengthByteCount::from_low_bits(low_bits) {
            Some(c) => c,
            None => {
                // `from_low_bits` returns None only for the value 0 because
                // the low bits are masked to two bits.
                return Err(DecodeError::ZeroLengthByteCount {
                    offset: header_offset,
                });
            }
        };

        let format_code =
            FormatCode::from_six_bit(six_bit).ok_or(DecodeError::UnknownFormatCode {
                offset: header_offset,
                format_code: six_bit,
            })?;

        // Read `count` big-endian length bytes without trusting them to fit
        // in `usize` (they always do for counts 1/2/3, but we compute
        // defensively).
        let length_bytes_start = self.cursor;
        let length_bytes_end = length_bytes_start.checked_add(count.as_count()).ok_or(
            DecodeError::ArithmeticOverflow {
                offset: header_offset,
            },
        )?;
        if length_bytes_end > self.input.len() {
            return Err(DecodeError::TruncatedHeader {
                offset: header_offset,
            });
        }
        let mut declared_length: usize = 0;
        for &byte in &self.input[length_bytes_start..length_bytes_end] {
            declared_length = declared_length
                .checked_shl(8)
                .ok_or(DecodeError::ArithmeticOverflow {
                    offset: header_offset,
                })?
                .checked_add(usize::from(byte))
                .ok_or(DecodeError::ArithmeticOverflow {
                    offset: header_offset,
                })?;
        }
        self.cursor = length_bytes_end;

        // One to three Length Bytes can represent at most 0x00FF_FFFF, so no
        // additional protocol-maximum check is necessary here.
        Ok((format_code, declared_length))
    }

    /// Enforces the per-List limits before any child allocation.
    ///
    /// Checks `max_list_items` (the declared direct child count), the
    /// projected `max_total_items` (each declared child contributes at least
    /// one node) and `max_depth` (entering this List increases depth).
    fn check_list_limits(
        &self,
        header_offset: usize,
        declared_children: usize,
    ) -> Result<(), DecodeError> {
        if declared_children > self.limits.max_list_items() {
            return Err(DecodeError::ListItemsExceeded {
                offset: header_offset,
                declared: declared_children,
                max_list_items: self.limits.max_list_items(),
            });
        }

        let required_items = self.accepted_nodes.checked_add(declared_children).ok_or(
            DecodeError::ArithmeticOverflow {
                offset: header_offset,
            },
        )?;
        if required_items > self.limits.max_total_items() {
            return Err(DecodeError::TotalItemsExceeded {
                offset: header_offset,
                required_items,
                max_total_items: self.limits.max_total_items(),
            });
        }

        let next_depth = self
            .depth
            .checked_add(1)
            .ok_or(DecodeError::ArithmeticOverflow {
                offset: header_offset,
            })?;
        if next_depth > self.limits.max_depth() {
            return Err(DecodeError::DepthExceeded {
                offset: header_offset,
                depth: next_depth,
                max_depth: self.limits.max_depth(),
            });
        }

        Ok(())
    }

    /// Top-level single-loop driver.
    ///
    /// Maintains an optional `pending` item that has just been decoded and
    /// must be delivered either to the innermost open List frame or, when
    /// the stack is empty, as the completed root. The loop reads exactly one
    /// header per scalar item and one header per List, with cascading pops
    /// when a frame fills up.
    fn run(&mut self) -> Result<SecsItem, DecodeError> {
        // The most recently decoded item awaiting delivery to its parent
        // (or to the caller when the stack is empty).
        let mut pending: Option<SecsItem> = None;

        loop {
            // Phase 1: deliver any pending item as far up the stack as
            // possible. This may close several ancestor Lists in succession.
            while let Some(item) = pending.take() {
                match self.list_stack.last_mut() {
                    None => {
                        // Stack empty: `item` is the root and decoding is done.
                        debug_assert_eq!(self.depth, 0, "depth must return to zero at the root");
                        return Ok(item);
                    }
                    Some(frame) => {
                        frame.children.push(item);
                        if frame.children.len() < frame.expected_children {
                            // Parent still wants more children; fall through
                            // to Phase 2 to read the next sibling.
                            break;
                        }
                        // Frame is full: pop it, decrease depth, and offer
                        // the completed List to its own parent on the next
                        // iteration of this delivery loop.
                        let completed = self
                            .list_stack
                            .pop()
                            .expect("frame was just inspected as non-empty");
                        self.depth = self
                            .depth
                            .checked_sub(1)
                            .expect("depth is balanced by matching List pushes");
                        pending = Some(SecsItem::List(completed.children));
                    }
                }
            }

            // Phase 2: read and decode one item header + body.
            let header_offset = self.cursor;
            let (format_code, declared_length) = match self.read_header() {
                Ok(parsed) => parsed,
                Err(DecodeError::TruncatedHeader { offset }) if offset == self.input.len() => {
                    // Input ended cleanly at an Item boundary, before another
                    // header could be read. If a List frame is still open,
                    // the missing item is the List's next declared child and
                    // the List is the offending item, reported as
                    // `TruncatedList` (element-count semantics). When no List
                    // is open, direct cursor use preserves
                    // `TruncatedHeader`; the strict public entry point rejects
                    // empty input before constructing this cursor.
                    if let Some(frame) = self.list_stack.last() {
                        return Err(DecodeError::TruncatedList {
                            offset: frame.header_offset,
                            expected_children: frame.expected_children,
                            decoded_children: frame.children.len(),
                        });
                    }
                    return Err(DecodeError::TruncatedHeader { offset });
                }
                Err(other) => return Err(other),
            };
            self.note_node(header_offset)?;

            if format_code == FormatCode::List {
                // Depth is checked for every List header, including empty
                // ones, so `L{L{L{}}}` with max_depth=2 is still rejected at
                // the innermost header.
                self.check_list_limits(header_offset, declared_length)?;
                if declared_length == 0 {
                    // Empty List resolves immediately without pushing a frame.
                    pending = Some(SecsItem::List(Vec::new()));
                } else {
                    // Enter the List: increase depth and push a fresh frame
                    // whose `children` vector will grow naturally.
                    self.depth =
                        self.depth
                            .checked_add(1)
                            .ok_or(DecodeError::ArithmeticOverflow {
                                offset: header_offset,
                            })?;
                    self.list_stack.push(ListFrame {
                        header_offset,
                        expected_children: declared_length,
                        children: Vec::new(),
                    });
                }
            } else {
                let item = self.decode_scalar(header_offset, format_code, declared_length)?;
                pending = Some(item);
            }
        }
    }

    /// Decodes a non-List item body whose header has already been consumed.
    ///
    /// Enforces `max_item_bytes`, remaining-input bounds, and numeric
    /// alignment before any heap allocation.
    fn decode_scalar(
        &mut self,
        header_offset: usize,
        format_code: FormatCode,
        body_len: usize,
    ) -> Result<SecsItem, DecodeError> {
        if body_len > self.limits.max_item_bytes() {
            return Err(DecodeError::ItemBytesExceeded {
                offset: header_offset,
                declared: body_len,
                max_item_bytes: self.limits.max_item_bytes(),
            });
        }

        // Verify the declared body still fits in the remaining input before
        // slicing; this catches truncation uniformly.
        let body_end =
            self.cursor
                .checked_add(body_len)
                .ok_or(DecodeError::ArithmeticOverflow {
                    offset: header_offset,
                })?;
        if body_end > self.input.len() {
            return Err(DecodeError::TruncatedBody {
                offset: header_offset,
                declared_bytes: body_len,
                available_bytes: self.input.len() - self.cursor,
            });
        }

        let body = &self.input[self.cursor..body_end];
        self.cursor = body_end;

        // For numeric formats, body_len must be a multiple of the element
        // width. Misaligned lengths are rejected before we interpret bytes.
        if let Some(elem_width) = format_code.element_width() {
            if !body_len.is_multiple_of(elem_width) {
                return Err(DecodeError::MisalignedNumericPayload {
                    offset: header_offset,
                    format_code: format_code.six_bit_value(),
                    body_len,
                    elem_width,
                });
            }
        }

        let item = match format_code {
            FormatCode::List => unreachable!("List is dispatched by the run loop"),
            FormatCode::Binary => SecsItem::Binary(body.to_vec()),
            FormatCode::Boolean => {
                // E5 §6.2: zero is false, any non-zero byte is true.
                let values = body.iter().map(|byte| *byte != 0).collect();
                SecsItem::Boolean(values)
            }
            FormatCode::Ascii => {
                for (index, &byte) in body.iter().enumerate() {
                    if !byte.is_ascii() {
                        return Err(DecodeError::NonAscii {
                            offset: header_offset,
                            index,
                            byte,
                        });
                    }
                }
                // Every byte is 7-bit ASCII, hence a valid UTF-8 prefix.
                let text = String::from_utf8(body.to_vec()).expect("validated ASCII body");
                let ascii = AsciiString::new(text).expect("validated ASCII body");
                SecsItem::Ascii(ascii)
            }
            FormatCode::Jis8 => SecsItem::Jis8(body.to_vec()),
            FormatCode::Localized => self.decode_localized(header_offset, body)?,
            FormatCode::I8 => {
                let mut values = Vec::with_capacity(body_len / 8);
                for chunk in body.chunks_exact(8) {
                    let arr: [u8; 8] = chunk.try_into().expect("chunk is exactly 8 bytes");
                    values.push(i64::from_be_bytes(arr));
                }
                SecsItem::I8(values)
            }
            FormatCode::I1 => {
                let values: Vec<i8> = body.iter().map(|byte| *byte as i8).collect();
                SecsItem::I1(values)
            }
            FormatCode::I2 => {
                let mut values = Vec::with_capacity(body_len / 2);
                for chunk in body.chunks_exact(2) {
                    let arr: [u8; 2] = chunk.try_into().expect("chunk is exactly 2 bytes");
                    values.push(i16::from_be_bytes(arr));
                }
                SecsItem::I2(values)
            }
            FormatCode::I4 => {
                let mut values = Vec::with_capacity(body_len / 4);
                for chunk in body.chunks_exact(4) {
                    let arr: [u8; 4] = chunk.try_into().expect("chunk is exactly 4 bytes");
                    values.push(i32::from_be_bytes(arr));
                }
                SecsItem::I4(values)
            }
            FormatCode::F8 => {
                let mut values = Vec::with_capacity(body_len / 8);
                for chunk in body.chunks_exact(8) {
                    let arr: [u8; 8] = chunk.try_into().expect("chunk is exactly 8 bytes");
                    values.push(f64::from_bits(u64::from_be_bytes(arr)));
                }
                SecsItem::F8(values)
            }
            FormatCode::F4 => {
                let mut values = Vec::with_capacity(body_len / 4);
                for chunk in body.chunks_exact(4) {
                    let arr: [u8; 4] = chunk.try_into().expect("chunk is exactly 4 bytes");
                    values.push(f32::from_bits(u32::from_be_bytes(arr)));
                }
                SecsItem::F4(values)
            }
            FormatCode::U8 => {
                let mut values = Vec::with_capacity(body_len / 8);
                for chunk in body.chunks_exact(8) {
                    let arr: [u8; 8] = chunk.try_into().expect("chunk is exactly 8 bytes");
                    values.push(u64::from_be_bytes(arr));
                }
                SecsItem::U8(values)
            }
            FormatCode::U1 => SecsItem::U1(body.to_vec()),
            FormatCode::U2 => {
                let mut values = Vec::with_capacity(body_len / 2);
                for chunk in body.chunks_exact(2) {
                    let arr: [u8; 2] = chunk.try_into().expect("chunk is exactly 2 bytes");
                    values.push(u16::from_be_bytes(arr));
                }
                SecsItem::U2(values)
            }
            FormatCode::U4 => {
                let mut values = Vec::with_capacity(body_len / 4);
                for chunk in body.chunks_exact(4) {
                    let arr: [u8; 4] = chunk.try_into().expect("chunk is exactly 4 bytes");
                    values.push(u32::from_be_bytes(arr));
                }
                SecsItem::U4(values)
            }
        };

        Ok(item)
    }

    /// Decodes a Localized Character String body (format code `22`).
    ///
    /// The first two bytes are the big-endian LSH encoding code; the rest is
    /// preserved verbatim. No character-encoding validation is performed:
    /// UTF-8, JIS, Big5 and custom encodings all flow through unchanged.
    fn decode_localized(&self, header_offset: usize, body: &[u8]) -> Result<SecsItem, DecodeError> {
        if body.len() < 2 {
            return Err(DecodeError::MissingLocalizedHeader {
                offset: header_offset,
            });
        }
        let encoding_value = u16::from_be_bytes([body[0], body[1]]);
        if encoding_value == 0 {
            return Err(DecodeError::ReservedLocalizedEncodingCode {
                offset: header_offset,
            });
        }
        // Keep the payload exactly as supplied on the wire.
        let payload = body[2..].to_vec();
        // `LocalizedEncodingCode::new` only rejects zero, which we already
        // ruled out, so this call is infallible here.
        let encoding = LocalizedEncodingCode::new(encoding_value)
            .expect("non-zero encoding code is accepted by LocalizedEncodingCode");
        Ok(SecsItem::Localized(LocalizedString::new(encoding, payload)))
    }
}

/// Reusable strict SECS-II item decoder configured with resource limits.
///
/// HSMS Message Text presence is deliberately outside this type: callers at
/// that boundary map absent text separately and invoke [`Self::decode_item`]
/// only for a present, non-empty item encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Secs2Decoder {
    /// Resource limits enforced for every item decoded by this value.
    limits: DecodeLimits,
}

impl Secs2Decoder {
    /// Creates a strict decoder that applies `limits` to every decoded item.
    ///
    /// The limits are copied into the decoder, so one configured value can be
    /// reused for any number of independent input slices.
    #[must_use]
    pub const fn new(limits: DecodeLimits) -> Self {
        Self { limits }
    }

    /// Returns the resource limits enforced by this decoder.
    #[must_use]
    pub const fn limits(&self) -> DecodeLimits {
        self.limits
    }

    /// Decodes `input` as exactly one complete SECS-II item.
    ///
    /// Empty input is not an item, and bytes remaining after the first
    /// complete item are rejected even when they encode another valid item.
    /// The decoder never panics on malformed wire input.
    ///
    /// # Errors
    ///
    /// Returns [`DecodeError::EmptyInput`] for an empty slice,
    /// [`DecodeError::TrailingBytes`] when one item does not consume the
    /// entire slice, or another [`DecodeError`] for malformed input and
    /// resource-limit violations.
    pub fn decode_item(&self, input: &[u8]) -> Result<SecsItem, DecodeError> {
        if input.is_empty() {
            return Err(DecodeError::EmptyInput);
        }

        let mut cursor = DecodeCursor::new(input, self.limits);
        let item = cursor.run()?;
        let consumed = cursor.consumed();
        if consumed != input.len() {
            return Err(DecodeError::TrailingBytes {
                consumed,
                total: input.len(),
            });
        }
        Ok(item)
    }
}

impl Default for Secs2Decoder {
    /// Creates a decoder using [`DecodeLimits::default`].
    fn default() -> Self {
        Self::new(DecodeLimits::default())
    }
}

#[cfg(test)]
mod tests {
    //! Decoder unit tests focused on rejection paths and limit enforcement.
    //! Round-trip coverage for every format lives in the parent module so
    //! that encoder and decoder tests sit together.
    use super::*;
    use crate::secs2::codec::header::{FormatCode, LengthByteCount};
    use crate::secs2::{SecsItem, MAX_DECODE_NESTING_DEPTH, MAX_ENCODED_ITEM_LENGTH};

    /// Returns the standard limits used by tests that do not target a limit.
    fn defaults() -> DecodeLimits {
        DecodeLimits::default()
    }

    /// Decodes one item with the supplied limits through the public API.
    fn decode_with_limits(input: &[u8], limits: DecodeLimits) -> Result<SecsItem, DecodeError> {
        Secs2Decoder::new(limits).decode_item(input)
    }

    /// Verifies that explicit and default decoder configuration can be read
    /// back unchanged through the public accessor.
    #[test]
    fn decoder_preserves_its_resource_limits() {
        let limits = DecodeLimits::new(8, 128, 4_096, 32).expect("valid limits");
        assert_eq!(Secs2Decoder::new(limits).limits(), limits);
        assert_eq!(Secs2Decoder::default().limits(), DecodeLimits::default());
    }

    /// Verifies that strict item decoding rejects absent input.
    #[test]
    fn empty_input_is_rejected() {
        assert!(matches!(
            decode_with_limits(&[], defaults()).unwrap_err(),
            DecodeError::EmptyInput
        ));
    }

    /// Verifies that strict decoding rejects bytes after the first item.
    #[test]
    fn trailing_bytes_are_rejected() {
        // A valid empty List followed by a stray byte.
        let bytes = &[0b0000_0001, 0x00, 0xFF];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        match err {
            DecodeError::TrailingBytes { consumed, total } => {
                assert_eq!(consumed, 2);
                assert_eq!(total, 3);
            }
            other => panic!("expected TrailingBytes, got {other:?}"),
        }
    }

    /// Verifies that strict decoding rejects a second complete valid item, not
    /// only malformed or partial trailing bytes.
    #[test]
    fn second_complete_item_is_rejected_as_trailing_bytes() {
        let empty_list = [LengthByteCount::One.format_byte(FormatCode::List), 0x00];
        let bytes = [empty_list, empty_list].concat();
        assert_eq!(
            decode_with_limits(&bytes, defaults()),
            Err(DecodeError::TrailingBytes {
                consumed: empty_list.len(),
                total: bytes.len(),
            })
        );
    }

    /// Verifies rejection of the E5-illegal zero Length Byte count.
    #[test]
    fn zero_length_byte_count_is_rejected() {
        // Format byte 0b0100_0000 = ASCII (0o20) with low bits 00.
        let bytes = &[0b0100_0000, b'H'];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::ZeroLengthByteCount { offset: 0 }
        ));
    }

    /// Verifies that a reserved six-bit format code is rejected with context.
    #[test]
    fn unknown_format_code_is_rejected() {
        // 0b0011_1101: six-bit code 0o17 (reserved) + 1 length byte.
        let bytes = &[0b0011_1101, 0x00];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        match err {
            DecodeError::UnknownFormatCode {
                offset,
                format_code,
            } => {
                assert_eq!(offset, 0);
                assert_eq!(format_code, 0b0011_1100 >> 2);
            }
            other => panic!("expected UnknownFormatCode, got {other:?}"),
        }
    }

    /// Verifies byte-oriented diagnostics for an incomplete scalar body.
    #[test]
    fn truncated_body_is_rejected() {
        // ASCII item claiming 5 bytes but only 3 supplied.
        let bytes = &[0x41, 0x05, b'A', b'B', b'C'];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        match err {
            DecodeError::TruncatedBody {
                offset,
                declared_bytes,
                available_bytes,
            } => {
                assert_eq!(offset, 0);
                assert_eq!(declared_bytes, 5);
                assert_eq!(available_bytes, 3);
            }
            other => panic!("expected TruncatedBody, got {other:?}"),
        }
    }

    /// Verifies element-oriented truncation when a root List lacks children.
    #[test]
    fn root_list_short_of_children_is_truncated_list() {
        // Root L[3] with only one complete child, then EOF at an item
        // boundary. The List is missing its remaining children, so this is a
        // TruncatedList (element-count semantics), not a TruncatedBody.
        let u1_byte = LengthByteCount::One.format_byte(FormatCode::U1);
        let bytes = &[0b0000_0001, 0x03, u1_byte, 0x01, 0x01];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        match err {
            DecodeError::TruncatedList {
                offset,
                expected_children,
                decoded_children,
            } => {
                assert_eq!(offset, 0);
                assert_eq!(expected_children, 3);
                assert_eq!(decoded_children, 1);
            }
            other => panic!("expected TruncatedList, got {other:?}"),
        }
    }

    /// Verifies that List truncation identifies the innermost incomplete List.
    #[test]
    fn nested_list_short_reports_the_innermost_open_list() {
        // L2 { L2 { U1=1 } }, then EOF. The inner List declared 2 children
        // but only got 1; the report must point at the inner List header
        // (offset 2), not the outer List header (offset 0).
        let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
        let u1_byte = LengthByteCount::One.format_byte(FormatCode::U1);
        let bytes = &[
            list_byte, 0x02, // outer List at offset 0, 2 children
            list_byte, 0x02, // inner List at offset 2, 2 children
            u1_byte, 0x01,
            0x01, // U1 = 1 (inner's first child)
                  // EOF here: inner List has 1/2 children.
        ];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        match err {
            DecodeError::TruncatedList {
                offset,
                expected_children,
                decoded_children,
            } => {
                assert_eq!(offset, 2, "must report the innermost open List");
                assert_eq!(expected_children, 2);
                assert_eq!(decoded_children, 1);
            }
            other => panic!("expected TruncatedList, got {other:?}"),
        }
    }

    /// Verifies that a partial child header remains a header truncation.
    #[test]
    fn truncated_child_header_inside_list_is_truncated_header() {
        // L[1] whose only child is a format byte with no length byte. Input
        // ends mid-header, so this is TruncatedHeader, not TruncatedList.
        let bytes = &[
            0b0000_0001,
            0x01,
            LengthByteCount::One.format_byte(FormatCode::U1),
        ];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        assert!(matches!(err, DecodeError::TruncatedHeader { offset: 2 }));
    }

    /// Verifies that an incomplete child payload remains a body truncation.
    #[test]
    fn truncated_child_payload_inside_list_is_truncated_body() {
        // L[1] whose only child is an ASCII item claiming 5 bytes but with
        // only 3 supplied. The child's body is incomplete, so this is a
        // byte-oriented TruncatedBody pointing at the child header.
        let bytes = &[
            0b0000_0001,
            0x01, // L[1] at offset 0
            0x41,
            0x05, // ASCII, 5 bytes declared
            b'A',
            b'B',
            b'C', // only 3 bytes supplied -> child body truncated
        ];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        match err {
            DecodeError::TruncatedBody {
                offset,
                declared_bytes,
                available_bytes,
            } => {
                assert_eq!(offset, 2, "must point at the child header");
                assert_eq!(declared_bytes, 5);
                assert_eq!(available_bytes, 3);
            }
            other => panic!("expected TruncatedBody, got {other:?}"),
        }
    }

    /// Verifies rejection of bytes outside seven-bit ASCII.
    #[test]
    fn non_ascii_payload_is_rejected() {
        // ASCII item whose body contains 0xE8 (not 7-bit ASCII).
        let bytes = &[0x41, 0x01, 0xE8];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        match err {
            DecodeError::NonAscii {
                offset,
                index,
                byte,
            } => {
                assert_eq!(offset, 0);
                assert_eq!(index, 0);
                assert_eq!(byte, 0xE8);
            }
            other => panic!("expected NonAscii, got {other:?}"),
        }
    }

    /// Verifies fixed-width numeric payload alignment validation.
    #[test]
    fn misaligned_numeric_payload_is_rejected() {
        // U2 with 1 length byte, claiming 3 bytes (not divisible by 2).
        let u2_byte = LengthByteCount::One.format_byte(FormatCode::U2);
        let bytes = &[u2_byte, 0x03, 0x00, 0x01, 0x02];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        match err {
            DecodeError::MisalignedNumericPayload {
                offset,
                elem_width,
                body_len,
                ..
            } => {
                assert_eq!(offset, 0);
                assert_eq!(elem_width, 2);
                assert_eq!(body_len, 3);
            }
            other => panic!("expected MisalignedNumericPayload, got {other:?}"),
        }
    }

    /// Verifies that Localized bodies must contain the complete two-byte LSH.
    #[test]
    fn missing_localized_header_is_rejected() {
        // Localized (0o22) with 1 length byte and body_len 1.
        let bytes = &[0b0100_1001, 0x01, 0xFF];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::MissingLocalizedHeader { offset: 0 }
        ));
    }

    /// Verifies rejection of the E5-reserved Localized encoding code zero.
    #[test]
    fn reserved_localized_encoding_code_is_rejected() {
        // Localized (0o22) with 1 length byte, body_len 2, LSH 0x0000.
        let bytes = &[0b0100_1001, 0x02, 0x00, 0x00];
        let err = decode_with_limits(bytes, defaults()).unwrap_err();
        assert!(matches!(
            err,
            DecodeError::ReservedLocalizedEncodingCode { offset: 0 }
        ));
    }

    /// Verifies enforcement of the configured scalar item byte limit.
    #[test]
    fn max_item_bytes_is_enforced() {
        let limits = DecodeLimits::new(64, 1_000_000, 2, 1_000_000).expect("limits");
        // ASCII body_len 3 exceeds the limit of 2.
        let bytes = &[0x41, 0x03, b'A', b'B', b'C'];
        let err = decode_with_limits(bytes, limits).unwrap_err();
        match err {
            DecodeError::ItemBytesExceeded {
                declared,
                max_item_bytes,
                ..
            } => {
                assert_eq!(declared, 3);
                assert_eq!(max_item_bytes, 2);
            }
            other => panic!("expected ItemBytesExceeded, got {other:?}"),
        }
    }

    /// Verifies enforcement of the configured direct List child limit.
    #[test]
    fn max_list_items_is_enforced() {
        let limits = DecodeLimits::new(64, 1_000_000, MAX_ENCODED_ITEM_LENGTH, 1).expect("limits");
        // List claiming 2 children exceeds the limit of 1.
        let bytes = &[0b0000_0001, 0x02];
        let err = decode_with_limits(bytes, limits).unwrap_err();
        match err {
            DecodeError::ListItemsExceeded {
                declared,
                max_list_items,
                ..
            } => {
                assert_eq!(declared, 2);
                assert_eq!(max_list_items, 1);
            }
            other => panic!("expected ListItemsExceeded, got {other:?}"),
        }
    }

    /// Verifies that even empty Lists contribute to nesting depth.
    #[test]
    fn max_depth_is_enforced_including_empty_lists() {
        // L1 { L1 { L1 {} } } with max_depth=2: innermost List header (even
        // though it is empty) is at depth 3 and must be rejected.
        let limits =
            DecodeLimits::new(2, 1_000_000, MAX_ENCODED_ITEM_LENGTH, 1_000_000).expect("limits");
        let bytes = &[
            0b0000_0001,
            0x01, // outer List, 1 child (depth 1)
            0b0000_0001,
            0x01, // middle List, 1 child (depth 2)
            0b0000_0001,
            0x00, // inner empty List (depth 3 -> reject)
        ];
        let err = decode_with_limits(bytes, limits).unwrap_err();
        match err {
            DecodeError::DepthExceeded {
                depth, max_depth, ..
            } => {
                assert_eq!(depth, 3);
                assert_eq!(max_depth, 2);
            }
            other => panic!("expected DepthExceeded, got {other:?}"),
        }
    }

    /// Verifies iterative parsing and bounded destruction at the hard depth
    /// ceiling.
    #[test]
    fn explicit_stack_accepts_the_nesting_safety_boundary() {
        // MAX_DECODE_NESTING_DEPTH - 1 single-child Lists plus an innermost
        // empty List reach the exact hard ceiling. Parsing is iterative and
        // the returned recursive tree remains bounded for safe destruction.
        let limits = DecodeLimits::new(
            MAX_DECODE_NESTING_DEPTH,
            1_000_000,
            MAX_ENCODED_ITEM_LENGTH,
            1_000_000,
        )
        .expect("limits");
        let mut bytes = Vec::new();
        for _ in 1..MAX_DECODE_NESTING_DEPTH {
            bytes.extend_from_slice(&[0b0000_0001, 0x01]); // List, 1 child
        }
        bytes.extend_from_slice(&[0b0000_0001, 0x00]); // innermost empty List
        let item = decode_with_limits(&bytes, limits).expect("nested lists decode");

        // Walk down to the innermost empty List to confirm structure.
        let mut cursor = &item;
        for _ in 1..MAX_DECODE_NESTING_DEPTH {
            match cursor {
                SecsItem::List(children) if children.len() == 1 => cursor = &children[0],
                _ => panic!("expected nested single-child List"),
            }
        }
        assert_eq!(cursor, &SecsItem::List(Vec::new()));
    }

    /// Exercises error cleanup after a maximum-depth completed subtree has
    /// already been attached to an open parent List.
    #[test]
    fn completed_deep_subtree_is_safely_cleaned_after_later_error() {
        let limits = DecodeLimits::new(
            MAX_DECODE_NESTING_DEPTH,
            1_000_000,
            MAX_ENCODED_ITEM_LENGTH,
            1_000_000,
        )
        .expect("limits");
        let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
        let mut bytes = vec![list_byte, 0x02]; // root List with two children

        // The first child contains MAX_DECODE_NESTING_DEPTH - 1 List levels;
        // together with the root, the deepest List reaches the safety ceiling.
        for _ in 2..MAX_DECODE_NESTING_DEPTH {
            bytes.extend_from_slice(&[list_byte, 0x01]);
        }
        bytes.extend_from_slice(&[list_byte, 0x00]);

        // Begin the root's second child but omit its Length Byte. Returning
        // this error drops the open root frame and its completed deep child.
        let malformed_offset = bytes.len();
        bytes.push(LengthByteCount::One.format_byte(FormatCode::U1));
        assert!(matches!(
            decode_with_limits(&bytes, limits),
            Err(DecodeError::TruncatedHeader { offset }) if offset == malformed_offset
        ));
    }

    /// Verifies conservative total-node projection at a List header.
    #[test]
    fn total_items_enforced_via_list_projection() {
        // max_total_items = 3; outer List declares 3 children, so the
        // projected count is outer(1) + 3 declared = 4 > 3.
        let limits = DecodeLimits::new(64, 3, MAX_ENCODED_ITEM_LENGTH, 1_000_000).expect("limits");
        let bytes = &[0b0000_0001, 0x03];
        let err = decode_with_limits(bytes, limits).unwrap_err();
        match err {
            DecodeError::TotalItemsExceeded {
                required_items,
                max_total_items,
                ..
            } => {
                assert_eq!(required_items, 4);
                assert_eq!(max_total_items, 3);
            }
            other => panic!("expected TotalItemsExceeded, got {other:?}"),
        }
    }

    /// Verifies the exact next-node path uses the same `required_items`
    /// semantics as the earlier List projection path.
    #[test]
    fn total_items_enforced_when_next_valid_header_exceeds_limit() {
        let limits = DecodeLimits::new(64, 3, MAX_ENCODED_ITEM_LENGTH, 2).expect("limits");
        let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
        let u1_byte = LengthByteCount::One.format_byte(FormatCode::U1);
        let bytes = [
            list_byte, 0x02, // root L[2], projected requirement = 3
            list_byte, 0x01, // first child L[1]
            u1_byte, 0x00, // nested empty U1, accepted node 3
            u1_byte, 0x00, // root's second child would require node 4
        ];
        assert!(matches!(
            decode_with_limits(&bytes, limits),
            Err(DecodeError::TotalItemsExceeded {
                offset: 6,
                required_items: 4,
                max_total_items: 3,
            })
        ));
    }

    /// Verifies EOF and a partial Header are diagnosed before node accounting
    /// when the already accepted tree has exactly exhausted its item budget.
    #[test]
    fn missing_or_partial_header_is_not_counted_as_an_item() {
        let limits = DecodeLimits::new(64, 3, MAX_ENCODED_ITEM_LENGTH, 2).expect("limits");
        let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
        let u1_byte = LengthByteCount::One.format_byte(FormatCode::U1);
        let input_at_child_boundary = [
            list_byte, 0x02, // root still expects two children
            list_byte, 0x01, // first child L[1]
            u1_byte, 0x00, // nested empty U1, accepted node 3
        ];

        assert!(matches!(
            decode_with_limits(&input_at_child_boundary, limits),
            Err(DecodeError::TruncatedList {
                offset: 0,
                expected_children: 2,
                decoded_children: 1,
            })
        ));

        let mut partial_header = input_at_child_boundary.to_vec();
        partial_header.push(u1_byte);
        assert!(matches!(
            decode_with_limits(&partial_header, limits),
            Err(DecodeError::TruncatedHeader { offset: 6 })
        ));
    }

    /// Verifies that completing nested frames cascades items to their parents.
    #[test]
    fn cascading_list_close_handles_multiple_full_parents() {
        // L2 { U1=1, L2 { U1=2, U1=3 } } — the inner List fills and must
        // cascade-pop only itself, leaving the outer frame waiting for its
        // second child, which is the inner List; the outer then also closes.
        let limits = DecodeLimits::default();
        let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
        let u1_byte = LengthByteCount::One.format_byte(FormatCode::U1);
        let bytes = &[
            list_byte, 0x02, // outer List, 2 children
            u1_byte, 0x01, 0x01, // U1 = 1
            list_byte, 0x02, // inner List, 2 children
            u1_byte, 0x01, 0x02, // U1 = 2
            u1_byte, 0x01, 0x03, // U1 = 3
        ];
        let item = decode_with_limits(bytes, limits).expect("nested decode");
        let SecsItem::List(outer) = item else {
            panic!("expected outer List");
        };
        assert_eq!(outer.len(), 2);
        assert!(matches!(outer[0], SecsItem::U1(_)));
        let SecsItem::List(inner) = &outer[1] else {
            panic!("expected inner List");
        };
        assert_eq!(inner.len(), 2);
    }
}
