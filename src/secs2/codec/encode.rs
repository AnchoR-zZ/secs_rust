//! Two-pass, explicit-stack SECS-II encoder.
//!
//! Pass 1 validates the in-memory [`SecsItem`] tree and computes its exact
//! on-wire size before allocating or writing the final output buffer. Pass 2
//! allocates that output [`Vec<u8>`] once with the measured capacity, then
//! writes the header and body for every node. Each pass uses an independent
//! explicit traversal stack whose backing `Vec` may allocate separately.
//!
//! Both passes share an iterative preorder walker (the private `ItemWalker`
//! below). Its host-call-stack consumption is `O(1)` and its explicit heap
//! stack is `O(depth)`: each open List contributes exactly one slice iterator
//! to a `Vec`, so a wide List never queues all children at once. This bounds
//! the encoder's traversal; recursive data-structure operations on manually
//! constructed `SecsItem` values remain the caller's responsibility.
//!
//! The two-pass structure avoids the bookkeeping that an in-place length
//! back-patch would require and lets the encoder validate the E5 24-bit
//! length ceiling before allocating or writing the final output buffer.
//!
//! Length semantics (per SEMI E5): a List's length field counts its
//! *direct child elements*, while every other item's length field counts
//! payload *bytes*. The size pass therefore treats List and non-List items
//! differently when selecting the Length Byte count.

use crate::secs2::SecsItem;
use crate::secs2::{
    codec::{
        error::EncodeError,
        header::{FormatCode, LengthByteCount},
    },
    MAX_ENCODED_ITEM_LENGTH,
};

/// Iterative preorder walker over a [`SecsItem`] tree.
///
/// Each open List contributes one [`std::slice::Iter`] to `parents`, so a wide
/// List never pushes all of its child references onto the host stack. The
/// walker is consumed by repeatedly calling [`ItemWalker::next`], which
/// returns the items in the same order a recursive preorder visit would
/// (root, then each child subtree left to right). The `parents` backing `Vec`
/// may allocate as nesting grows; it is auxiliary storage, not encoded output.
struct ItemWalker<'a> {
    /// 下一个等待访问的 Item;`None` 表示 walker 已穷尽。
    next: Option<&'a SecsItem>,
    /// 每层 List 尚未访问的直接子元素迭代器,栈顶对应最内层未闭合的
    /// List。迭代器只持有共享引用且按需推进,宽 List 不会一次性把所有
    /// 子元素引用压入栈中。
    parents: Vec<std::slice::Iter<'a, SecsItem>>,
}

impl<'a> ItemWalker<'a> {
    /// 构造一个从 `root` 开始的 preorder walker。
    ///
    /// 调用方传入树的根节点(借用),walker 内部维护一个待访问项与若干
    /// 层 List 子元素迭代器。构造时不会复制节点或立即分配空间；后续
    /// 遍历进入嵌套 List 时,`parents` 的增长可能触发辅助堆分配。
    fn new(root: &'a SecsItem) -> Self {
        Self {
            next: Some(root),
            parents: Vec::new(),
        }
    }

    /// 返回 preorder 序列中的下一个 Item,没有更多节点时返回 `None`。
    ///
    /// 当前节点返回后:若它是非空 List,则压入其子元素迭代器,使下一次
    /// 调用返回第一个子元素;否则尝试从栈顶迭代器取下一个兄弟,迭代器
    /// 耗尽则逐层弹栈,直到某个祖先仍有兄弟或整棵树访问完毕。整棵树
    /// 的访问顺序与递归 preorder 完全一致。
    fn next(&mut self) -> Option<&'a SecsItem> {
        let current = self.next.take()?;
        // 进入非空 List:把它的子元素迭代器压栈,让下一次 next() 取到
        // 第一个子元素。空 List 没有子元素,无需压栈。
        if let SecsItem::List(children) = current {
            if !children.is_empty() {
                self.parents.push(children.iter());
            }
        }
        self.advance_to_next_sibling_or_descendant();
        Some(current)
    }

    /// 弹栈与推进,直到找到下一个待访问的兄弟节点,或确认整棵树已
    /// 穷尽。
    ///
    /// 仅在 [`ItemWalker::next`] 交付当前节点之后调用,负责把 `next`
    /// 指向当前节点之后的 preorder 后继(可能是直接子元素、兄弟节点,
    /// 或某个祖先的兄弟节点)。
    fn advance_to_next_sibling_or_descendant(&mut self) {
        // 反复尝试从栈顶迭代器取出下一个兄弟;迭代器耗尽则弹栈回到
        // 上一层,直至所有祖先的兄弟都遍历完毕。
        while let Some(top) = self.parents.last_mut() {
            if let Some(sibling) = top.next() {
                self.next = Some(sibling);
                return;
            }
            self.parents.pop();
        }
        // 栈空:树已穷尽,保持 self.next == None。
    }
}

/// Computes the total on-wire size (header + body) of `item` and all of its
/// descendants by walking the tree with an explicit stack.
///
/// List Length semantics follow SEMI E5: a List's Length field counts direct
/// child elements, so the Length Byte count for a List is selected from its
/// child count, while for every other format it is selected from the payload
/// byte count. Each node's contribution is added with `checked_add`; the
/// function returns the first [`EncodeError`] encountered in preorder.
fn measure(root: &SecsItem) -> Result<usize, EncodeError> {
    let mut walker = ItemWalker::new(root);
    let mut total = 0usize;
    while let Some(node) = walker.next() {
        let node_size = node_wire_size(node)?;
        total = total
            .checked_add(node_size)
            .ok_or(EncodeError::ArithmeticOverflow)?;
    }
    Ok(total)
}

/// Returns the on-wire byte size of a single node's header plus its own body,
/// excluding descendants for List items.
///
/// For a List the body is its children (encoded separately by the walker), so
/// the node's own contribution is only its header, whose Length Byte count is
/// chosen from the declared child element count. For every other format the
/// contribution is the header (Length Byte count chosen from payload byte
/// count) plus the payload bytes.
///
/// # Errors
///
/// Returns [`EncodeError::ListTooLarge`] when a List's direct child count, or
/// [`EncodeError::ItemBodyTooLarge`] when a non-List item's payload byte count,
/// exceeds the E5 24-bit length field.
fn node_wire_size(item: &SecsItem) -> Result<usize, EncodeError> {
    let (declared_length, is_list) = declared_length_of(item)?;
    let length_byte_count = LengthByteCount::for_declared_length(declared_length)
        .expect("declared_length is bounded by MAX_ENCODED_ITEM_LENGTH, so a count always exists");
    let header_len = 1usize
        .checked_add(length_byte_count.as_count())
        .expect("1 + (1..=3) cannot overflow usize on any supported platform");
    if is_list {
        // A List's own bytes are just its header; children are summed by the
        // caller as separate preorder nodes.
        Ok(header_len)
    } else {
        // Non-List: header + declared payload bytes.
        header_len
            .checked_add(declared_length)
            .ok_or(EncodeError::ArithmeticOverflow)
    }
}

/// Returns `(declared_length, is_list)` for `item`, where `declared_length`
/// is the value carried by the item's Length field: the direct child element
/// count for Lists, or the payload byte count for every other format.
///
/// # Errors
///
/// Returns [`EncodeError::ListTooLarge`] or
/// [`EncodeError::ItemBodyTooLarge`] when the corresponding declared value
/// exceeds the E5 24-bit ceiling (`0x00FF_FFFF`). All payload-length
/// computations use `checked_mul`/`checked_add` to defend against `usize`
/// overflow.
fn declared_length_of(item: &SecsItem) -> Result<(usize, bool), EncodeError> {
    match item {
        SecsItem::List(children) => {
            let count = bounded_list_children(children.len())?;
            Ok((count, true))
        }
        SecsItem::Binary(bytes) => Ok((bounded_payload(bytes.len(), FormatCode::Binary)?, false)),
        SecsItem::Boolean(values) => {
            Ok((bounded_payload(values.len(), FormatCode::Boolean)?, false))
        }
        SecsItem::Ascii(text) => Ok((
            bounded_payload(text.as_str().len(), FormatCode::Ascii)?,
            false,
        )),
        SecsItem::Jis8(bytes) => Ok((bounded_payload(bytes.len(), FormatCode::Jis8)?, false)),
        SecsItem::Localized(value) => {
            let payload = value.as_bytes().len();
            // The two-byte LSH always precedes the payload and counts toward
            // the declared item length per E5 §6.4.
            let body_len = payload
                .checked_add(2)
                .ok_or(EncodeError::ArithmeticOverflow)?;
            Ok((bounded_payload(body_len, FormatCode::Localized)?, false))
        }
        SecsItem::I8(values) => numeric_payload(values.len(), 8, FormatCode::I8),
        SecsItem::I1(values) => numeric_payload(values.len(), 1, FormatCode::I1),
        SecsItem::I2(values) => numeric_payload(values.len(), 2, FormatCode::I2),
        SecsItem::I4(values) => numeric_payload(values.len(), 4, FormatCode::I4),
        SecsItem::F8(values) => numeric_payload(values.len(), 8, FormatCode::F8),
        SecsItem::F4(values) => numeric_payload(values.len(), 4, FormatCode::F4),
        SecsItem::U8(values) => numeric_payload(values.len(), 8, FormatCode::U8),
        SecsItem::U1(values) => Ok((bounded_payload(values.len(), FormatCode::U1)?, false)),
        SecsItem::U2(values) => numeric_payload(values.len(), 2, FormatCode::U2),
        SecsItem::U4(values) => numeric_payload(values.len(), 4, FormatCode::U4),
    }
}

/// Computes a fixed-width numeric payload length as `count * elem_width` and
/// verifies it fits the E5 24-bit length field.
///
/// # Errors
///
/// Returns [`EncodeError::ArithmeticOverflow`] when `count * elem_width`
/// overflows `usize`, or [`EncodeError::ItemBodyTooLarge`] when the product
/// exceeds `MAX_ENCODED_ITEM_LENGTH`.
fn numeric_payload(
    count: usize,
    elem_width: usize,
    code: FormatCode,
) -> Result<(usize, bool), EncodeError> {
    let body_len = count
        .checked_mul(elem_width)
        .ok_or(EncodeError::ArithmeticOverflow)?;
    Ok((bounded_payload(body_len, code)?, false))
}

/// Verifies that a byte-oriented payload length fits the E5 24-bit length
/// field, returning it for use as the item's declared length.
///
/// # Errors
///
/// Returns [`EncodeError::ItemBodyTooLarge`] when `body_len` exceeds
/// `MAX_ENCODED_ITEM_LENGTH`.
fn bounded_payload(body_len: usize, code: FormatCode) -> Result<usize, EncodeError> {
    if body_len > MAX_ENCODED_ITEM_LENGTH {
        Err(EncodeError::ItemBodyTooLarge {
            format_code: code.six_bit_value(),
            body_bytes: body_len,
        })
    } else {
        Ok(body_len)
    }
}

/// Verifies that a List direct-child count fits the E5 24-bit length field,
/// returning the count for use as the List's declared length.
///
/// # Errors
///
/// Returns [`EncodeError::ListTooLarge`] when `child_count` exceeds
/// `MAX_ENCODED_ITEM_LENGTH`.
fn bounded_list_children(child_count: usize) -> Result<usize, EncodeError> {
    if child_count > MAX_ENCODED_ITEM_LENGTH {
        Err(EncodeError::ListTooLarge { child_count })
    } else {
        Ok(child_count)
    }
}

/// Returns the number of header bytes (format byte plus Length Bytes) for an
/// item whose declared length equals `declared_length`.
///
/// Panics only if `declared_length` exceeds `MAX_ENCODED_ITEM_LENGTH`, which
/// the size pass guarantees cannot happen before this is called.
fn header_byte_count(declared_length: usize) -> usize {
    let count = LengthByteCount::for_declared_length(declared_length)
        .expect("declared_length is bounded by MAX_ENCODED_ITEM_LENGTH");
    // 1 format byte + the Length Byte count (1, 2 or 3); cannot overflow usize.
    1 + count.as_count()
}

/// Writes the format byte plus the big-endian length bytes for an item with
/// the supplied `declared_length` into `dst`.
///
/// `declared_length` is the value to place in the Length field: a child
/// element count for Lists, or a payload byte count for other formats. It is
/// the caller's responsibility to have already bounded it at
/// `MAX_ENCODED_ITEM_LENGTH`.
fn write_header(dst: &mut Vec<u8>, code: FormatCode, declared_length: usize) {
    let count = LengthByteCount::for_declared_length(declared_length)
        .expect("declared_length is bounded by MAX_ENCODED_ITEM_LENGTH");
    dst.push(count.format_byte(code));
    let byte_count = count.as_count();
    // Always emit `byte_count` bytes, big-endian, zero-padded on the left.
    // We unroll the three sizes because the count is statically 1/2/3.
    match byte_count {
        1 => dst.push(declared_length as u8),
        2 => dst.extend_from_slice(&(declared_length as u16).to_be_bytes()),
        3 => {
            // 24-bit big-endian representation of declared_length.
            dst.push((declared_length >> 16) as u8);
            dst.push((declared_length >> 8) as u8);
            dst.push(declared_length as u8);
        }
        // The count enum only allows 1, 2 or 3; this branch is unreachable.
        _ => unreachable!("length byte count is bounded to 1..=3"),
    }
}

/// Writes the payload of one non-List item into `dst` after its header.
///
/// List items carry no body bytes of their own (their children are written as
/// separate preorder nodes by the caller), so this function is only invoked
/// for non-List items. The caller validates that the payload byte count
/// matches the value written into the header.
fn write_payload(dst: &mut Vec<u8>, item: &SecsItem) {
    match item {
        SecsItem::List(_) => {
            // Lists carry no body bytes of their own; their children are
            // written as separate preorder nodes by the caller. This arm is
            // unreachable from the encoder but kept exhaustive.
        }
        SecsItem::Binary(bytes) => dst.extend_from_slice(bytes),
        SecsItem::Boolean(values) => {
            for value in values {
                // Normalised, deterministic form: false => 0x00, true => 0x01.
                dst.push(if *value { 0x01 } else { 0x00 });
            }
        }
        SecsItem::Ascii(text) => dst.extend_from_slice(text.as_str().as_bytes()),
        SecsItem::Jis8(bytes) => dst.extend_from_slice(bytes),
        SecsItem::Localized(value) => {
            // LSH is the big-endian two-byte encoding code; payload follows
            // unmodified to preserve bytes verbatim.
            dst.extend_from_slice(&value.encoding().get().to_be_bytes());
            dst.extend_from_slice(value.as_bytes());
        }
        SecsItem::I8(values) => {
            for value in values {
                dst.extend_from_slice(&value.to_be_bytes());
            }
        }
        SecsItem::I1(values) => {
            for value in values {
                dst.push((*value) as u8);
            }
        }
        SecsItem::I2(values) => {
            for value in values {
                dst.extend_from_slice(&value.to_be_bytes());
            }
        }
        SecsItem::I4(values) => {
            for value in values {
                dst.extend_from_slice(&value.to_be_bytes());
            }
        }
        SecsItem::F8(values) => {
            for value in values {
                // to_bits preserves NaN payloads, infinities and negative
                // zero exactly as required by E5 §6.2 IEEE-754 conformance.
                dst.extend_from_slice(&value.to_bits().to_be_bytes());
            }
        }
        SecsItem::F4(values) => {
            for value in values {
                dst.extend_from_slice(&value.to_bits().to_be_bytes());
            }
        }
        SecsItem::U8(values) => {
            for value in values {
                dst.extend_from_slice(&value.to_be_bytes());
            }
        }
        SecsItem::U1(values) => {
            for value in values {
                dst.push(*value);
            }
        }
        SecsItem::U2(values) => {
            for value in values {
                dst.extend_from_slice(&value.to_be_bytes());
            }
        }
        SecsItem::U4(values) => {
            for value in values {
                dst.extend_from_slice(&value.to_be_bytes());
            }
        }
    }
}

/// Looks up the E5 format code that represents the given item variant.
fn format_code_of(item: &SecsItem) -> FormatCode {
    match item {
        SecsItem::List(_) => FormatCode::List,
        SecsItem::Binary(_) => FormatCode::Binary,
        SecsItem::Boolean(_) => FormatCode::Boolean,
        SecsItem::Ascii(_) => FormatCode::Ascii,
        SecsItem::Jis8(_) => FormatCode::Jis8,
        SecsItem::Localized(_) => FormatCode::Localized,
        SecsItem::I8(_) => FormatCode::I8,
        SecsItem::I1(_) => FormatCode::I1,
        SecsItem::I2(_) => FormatCode::I2,
        SecsItem::I4(_) => FormatCode::I4,
        SecsItem::F8(_) => FormatCode::F8,
        SecsItem::F4(_) => FormatCode::F4,
        SecsItem::U8(_) => FormatCode::U8,
        SecsItem::U1(_) => FormatCode::U1,
        SecsItem::U2(_) => FormatCode::U2,
        SecsItem::U4(_) => FormatCode::U4,
    }
}

/// Validated, exactly measured plan for appending one SECS-II item.
///
/// The plan borrows an immutable item tree, so its measured length cannot
/// become stale between validation and writing. It lets enclosing protocols
/// reserve one larger final buffer without first materializing a temporary
/// SECS-II byte vector.
#[derive(Debug)]
pub(crate) struct EncodedItemPlan<'a> {
    /// Immutable item tree validated by the measurement pass.
    item: &'a SecsItem,
    /// Exact number of bytes the item tree appends on success.
    encoded_length: usize,
}

impl<'a> EncodedItemPlan<'a> {
    /// Validates and measures `item`, returning a reusable append plan.
    ///
    /// # Errors
    ///
    /// Returns the same E5 length or arithmetic errors as [`encode_to_vec`].
    pub(crate) fn new(item: &'a SecsItem) -> Result<Self, EncodeError> {
        Ok(Self {
            item,
            encoded_length: measure(item)?,
        })
    }

    /// Returns the exact number of encoded bytes this plan will append.
    pub(crate) const fn encoded_length(&self) -> usize {
        self.encoded_length
    }

    /// Appends the measured item to an existing `output` vector.
    ///
    /// This method does not reserve capacity; callers composing a larger
    /// protocol frame should reserve the complete final size first.
    ///
    /// # Errors
    ///
    /// Returns an [`EncodeError`] if an internal item-length invariant fails
    /// during the write pass. Because the borrowed item is immutable and was
    /// already measured successfully, such a failure would indicate encoder
    /// drift; callers should discard any partially built enclosing buffer.
    pub(crate) fn write_into(&self, output: &mut Vec<u8>) -> Result<(), EncodeError> {
        let start = output.len();
        write_item_tree(self.item, output)?;
        debug_assert_eq!(
            output.len() - start,
            self.encoded_length,
            "encoder must append exactly the measured size"
        );
        Ok(())
    }
}

/// Appends one complete item tree to `output` using an explicit-stack walk.
///
/// # Errors
///
/// Returns the first E5 length or arithmetic error observed while deriving
/// node headers. A successfully created [`EncodedItemPlan`] has already ruled
/// these errors out for its immutable tree.
fn write_item_tree(item: &SecsItem, output: &mut Vec<u8>) -> Result<(), EncodeError> {
    let mut walker = ItemWalker::new(item);
    while let Some(node) = walker.next() {
        let (declared_length, _) = declared_length_of(node)?;
        let code = format_code_of(node);
        let before = output.len();
        write_header(output, code, declared_length);
        if !matches!(node, SecsItem::List(_)) {
            write_payload(output, node);
            debug_assert_eq!(
                output.len() - before - header_byte_count(declared_length),
                declared_length,
                "payload must emit exactly declared_length bytes"
            );
        }
    }
    Ok(())
}

/// Encodes `item` into a freshly allocated [`Vec<u8>`].
///
/// After the measurement pass succeeds, the final output vector receives one
/// exact-capacity allocation and does not need to grow while bytes are
/// written. The explicit traversal stacks used by either pass are separate
/// auxiliary vectors and may perform their own depth-dependent allocations.
///
/// Both the measurement pass and the write pass walk the tree using an
/// iterative preorder walker whose host-call-stack consumption is `O(1)` and
/// whose explicit heap stack is `O(depth)`, so traversal itself does not grow
/// the host stack with tree depth. Decoder-produced trees are bounded by
/// [`crate::secs2::MAX_DECODE_NESTING_DEPTH`]; callers that manually construct
/// deeper trees remain responsible for recursive `SecsItem` operations such
/// as `Drop`, `Clone`, `Debug`, and `PartialEq`.
///
/// # Errors
///
/// Returns [`EncodeError::ItemBodyTooLarge`] when any single non-List item body
/// exceeds the E5 24-bit length field, [`EncodeError::ListTooLarge`] when a
/// List declares more than `0xFF_FFFF` direct children, or
/// [`EncodeError::ArithmeticOverflow`] if internal size arithmetic overflows.
pub fn encode_to_vec(item: &SecsItem) -> Result<Vec<u8>, EncodeError> {
    let plan = EncodedItemPlan::new(item)?;
    let mut output = Vec::with_capacity(plan.encoded_length());
    plan.write_into(&mut output)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    //! Encoder unit tests: header layout, length-byte selection, byte-level
    //! vectors for every format, Boolean normalisation, and the iterative
    //! walker behaviour that keeps the encoder's stack usage bounded by tree
    //! depth rather than width.
    use super::*;
    use crate::secs2::{AsciiString, LocalizedEncodingCode, LocalizedString};

    /// Verifies the canonical single-Length-Byte representation of an empty List.
    #[test]
    fn empty_list_uses_one_length_byte_with_value_zero() {
        let bytes = encode_to_vec(&SecsItem::List(Vec::new())).expect("empty list");
        // Format byte 0b0000_0001 (List + 1 length byte), then 0x00.
        assert_eq!(bytes, &[0b0000_0001, 0x00]);
    }

    /// Verifies the canonical single-Length-Byte representation of empty ASCII.
    #[test]
    fn empty_ascii_uses_one_length_byte_with_value_zero() {
        let bytes = encode_to_vec(&SecsItem::Ascii(AsciiString::default())).expect("empty ascii");
        assert_eq!(bytes, &[0b0100_0001, 0x00]);
    }

    /// Verifies the canonical single-Length-Byte representation of empty Binary.
    #[test]
    fn empty_binary_uses_one_length_byte_with_value_zero() {
        let bytes = encode_to_vec(&SecsItem::Binary(Vec::new())).expect("empty binary");
        assert_eq!(bytes, &[0b0010_0001, 0x00]);
    }

    /// Verifies that a short ASCII payload uses one Length Byte.
    #[test]
    fn single_byte_length_field_for_short_ascii() {
        let item = SecsItem::Ascii(AsciiString::new("HELLO").expect("ascii"));
        let bytes = encode_to_vec(&item).expect("ascii");
        // 41 05 48 45 4C 4C 4F — the canonical E5 ASCII HELLO vector.
        assert_eq!(bytes, &[0x41, 0x05, b'H', b'E', b'L', b'L', b'O']);
    }

    /// Verifies the transition to two Length Bytes above 255 payload bytes.
    #[test]
    fn two_byte_length_field_is_chosen_above_255_body_bytes() {
        // 256-byte ASCII payload forces a 2-byte length field.
        let payload = "A".repeat(256);
        let item = SecsItem::Ascii(AsciiString::new(payload).expect("ascii"));
        let bytes = encode_to_vec(&item).expect("ascii");
        // Format byte 0b0100_0010 (ASCII + 2 length bytes), 0x01 0x00.
        assert_eq!(&bytes[..3], &[0b0100_0010, 0x01, 0x00]);
        assert_eq!(bytes.len(), 3 + 256);
    }

    /// Verifies the transition to three Length Bytes above 65,535 payload bytes.
    #[test]
    fn three_byte_length_field_is_chosen_above_65535_body_bytes() {
        let payload = vec![0u8; 0x1_0000];
        let item = SecsItem::Binary(payload);
        let bytes = encode_to_vec(&item).expect("binary");
        // Format byte 0b0010_0011 (Binary + 3 length bytes), 0x01 0x00 0x00.
        assert_eq!(&bytes[..4], &[0b0010_0011, 0x01, 0x00, 0x00]);
    }

    /// Verifies deterministic Boolean encoding as zero or one.
    #[test]
    fn boolean_normalises_true_to_one() {
        let item = SecsItem::Boolean(vec![false, true, true]);
        let bytes = encode_to_vec(&item).expect("bool");
        // 25 03 00 01 01 — matches the documented Boolean wire form.
        assert_eq!(bytes, &[0x25, 0x03, 0x00, 0x01, 0x01]);
    }

    /// Verifies that a Localized item's length and bytes include its LSH.
    #[test]
    fn localized_string_includes_lsh_in_length_and_payload() {
        // LSH = UTF-8 (0x0002), payload = UTF-8 bytes for "设备".
        let encoding = LocalizedEncodingCode::new(2).expect("encoding");
        let value = LocalizedString::new(encoding, vec![0xE8, 0xAE, 0xBE, 0xE5, 0xA4, 0x87]);
        let bytes = encode_to_vec(&SecsItem::Localized(value)).expect("localized");
        // 49 08 00 02 E8 AE BE E5 A4 87 — matches the requested fixed vector.
        assert_eq!(
            bytes,
            &[0x49, 0x08, 0x00, 0x02, 0xE8, 0xAE, 0xBE, 0xE5, 0xA4, 0x87]
        );
    }

    /// Verifies that an oversized scalar reports its payload length in bytes.
    #[test]
    fn item_body_too_large_is_reported_in_bytes() {
        // A single binary body of MAX_ENCODED_ITEM_LENGTH + 1 cannot fit in
        // the three-byte length field.
        let overlong = vec![0u8; MAX_ENCODED_ITEM_LENGTH + 1];
        let item = SecsItem::Binary(overlong);
        let err = encode_to_vec(&item).unwrap_err();
        match err {
            EncodeError::ItemBodyTooLarge {
                format_code,
                body_bytes,
            } => {
                assert_eq!(format_code, FormatCode::Binary.six_bit_value());
                assert_eq!(body_bytes, MAX_ENCODED_ITEM_LENGTH + 1);
            }
            other => panic!("expected ItemBodyTooLarge, got {other:?}"),
        }
    }

    /// Verifies that an unrepresentable List child count has a dedicated
    /// element-count error rather than a byte-oriented item-body error.
    #[test]
    fn list_too_large_is_reported_in_direct_children() {
        let child_count = MAX_ENCODED_ITEM_LENGTH + 1;
        assert_eq!(
            bounded_list_children(child_count),
            Err(EncodeError::ListTooLarge { child_count })
        );
    }

    /// Regression for problem 1: a List whose child count is small enough to
    /// fit one Length Byte must use one Length Byte, *even* when the sum of
    /// the children's wire byte sizes would exceed 255. List Length counts
    /// child elements, not bytes.
    #[test]
    fn list_uses_one_length_byte_when_child_count_fits_even_if_bytes_exceed_255() {
        // 128 empty U1 children: each is 2 wire bytes (format + length byte,
        // no payload), so the total body is 256 bytes. The List Length must
        // still be the element count 128, encoded with a single Length Byte.
        let children = (0..128).map(|_| SecsItem::U1(Vec::new())).collect();
        let bytes = encode_to_vec(&SecsItem::List(children)).expect("list");
        // Format byte 0b0000_0001 (List + 1 length byte), length 0x80 (128).
        assert_eq!(&bytes[..2], &[0b0000_0001, 0x80]);
        assert_eq!(bytes.len(), 2 + 128 * 2);
    }

    /// A List with exactly 255 children must still use a single Length Byte.
    #[test]
    fn list_with_255_children_uses_one_length_byte() {
        let children = (0..255).map(|_| SecsItem::U1(Vec::new())).collect();
        let bytes = encode_to_vec(&SecsItem::List(children)).expect("list");
        assert_eq!(bytes[0], LengthByteCount::One.format_byte(FormatCode::List));
        assert_eq!(bytes[1], 255);
    }

    /// A List with 256 children must cross over to two Length Bytes.
    #[test]
    fn list_with_256_children_uses_two_length_bytes() {
        let children = (0..256).map(|_| SecsItem::U1(Vec::new())).collect();
        let bytes = encode_to_vec(&SecsItem::List(children)).expect("list");
        assert_eq!(bytes[0], LengthByteCount::Two.format_byte(FormatCode::List));
        assert_eq!(&bytes[1..3], &[0x01, 0x00]); // 256 big-endian.
    }

    /// A List with exactly 65535 children must still use two Length Bytes.
    #[test]
    fn list_with_65535_children_uses_two_length_bytes() {
        let children = (0..65535).map(|_| SecsItem::U1(Vec::new())).collect();
        let bytes = encode_to_vec(&SecsItem::List(children)).expect("list");
        assert_eq!(bytes[0], LengthByteCount::Two.format_byte(FormatCode::List));
        assert_eq!(&bytes[1..3], &[0xFF, 0xFF]); // 65535 big-endian.
    }

    /// A List with 65536 children must cross over to three Length Bytes.
    #[test]
    fn list_with_65536_children_uses_three_length_bytes() {
        let children = (0..65536).map(|_| SecsItem::U1(Vec::new())).collect();
        let bytes = encode_to_vec(&SecsItem::List(children)).expect("list");
        assert_eq!(
            bytes[0],
            LengthByteCount::Three.format_byte(FormatCode::List)
        );
        assert_eq!(&bytes[1..4], &[0x01, 0x00, 0x00]); // 65536 big-endian.
    }

    /// A List child whose own payload exceeds 255 bytes must still be encoded
    /// correctly; the parent List's Length Byte count is unaffected because
    /// it counts children, not bytes.
    #[test]
    fn list_with_a_large_child_item_is_encoded_correctly() {
        // One child: a 300-byte Binary (needs 2 Length Bytes for itself).
        let big_child = SecsItem::Binary(vec![0u8; 300]);
        let list = SecsItem::List(vec![big_child]);
        let bytes = encode_to_vec(&list).expect("list with large child");
        // List header: 1 Length Byte, count 1.
        assert_eq!(&bytes[..2], &[0b0000_0001, 0x01]);
        // Child header: Binary + 2 Length Bytes, length 300 = 0x012C.
        assert_eq!(&bytes[2..5], &[0b0010_0010, 0x01, 0x2C]);
        assert_eq!(bytes.len(), 2 + 3 + 300);
    }

    /// A deeply nested chain of single-child Lists must encode without
    /// overflowing the host call stack, because both encoder passes use an
    /// iterative walker. The tree is torn down iteratively afterwards so the
    /// recursive `Drop` of `SecsItem` does not itself overflow.
    #[test]
    fn deeply_nested_single_child_lists_encode_without_stack_overflow() {
        const DEPTH: usize = 2000;
        // Build the chain iteratively from the leaf up; allocation is also
        // iterative so construction does not overflow.
        let mut item = SecsItem::List(Vec::new()); // leaf empty List
        for _ in 0..DEPTH {
            item = SecsItem::List(vec![item]);
        }
        let bytes = encode_to_vec(&item).expect("deep encode");
        // Each level contributes exactly 2 wire bytes (List header, 1 child).
        assert_eq!(bytes.len(), DEPTH * 2 + 2);

        // Iteratively peel one layer at a time so the recursive Drop does not
        // run on a 2000-deep tree (which would overflow the host stack).
        let mut current = item;
        let mut peeled = 0;
        loop {
            match current {
                SecsItem::List(children) if children.len() == 1 => {
                    // Move the single child out, leaving an empty List behind
                    // so only one level's children Drop at a time.
                    let mut children = children;
                    current = children.pop().expect("single-child List");
                    peeled += 1;
                }
                SecsItem::List(_) => break, // reached the empty leaf
                _ => panic!("expected only List nodes in the chain"),
            }
        }
        assert_eq!(peeled, DEPTH);
    }

    /// The measured size from pass 1 must equal the actual number of bytes
    /// written by pass 2. The encoder guards this with a `debug_assert`; this
    /// test asserts it explicitly in release builds too.
    #[test]
    fn measured_size_equals_written_size_for_a_mixed_tree() {
        let tree = SecsItem::List(vec![
            SecsItem::Ascii(AsciiString::new("AB").expect("ascii")),
            SecsItem::List(vec![SecsItem::U1(vec![1, 2, 3])]),
            SecsItem::Binary(vec![0xAA; 300]),
        ]);
        let measured = measure(&tree).expect("measure");
        let written = encode_to_vec(&tree).expect("encode").len();
        assert_eq!(measured, written);
    }
}
