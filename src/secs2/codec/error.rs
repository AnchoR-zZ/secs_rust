//! Binary codec errors for SECS-II encode and decode.
//!
//! These errors are intentionally distinct from
//! [`crate::secs2::SecsItemError`], which only covers construction-time
//! value validation. Where an offending item exists, codec errors carry byte
//! offsets so callers can report diagnostics at the relevant wire byte (and,
//! later, map them to SML line/column coordinates).
//!
//! Both public enums are marked `#[non_exhaustive]` so future codec failures
//! can gain specific variants without breaking downstream `match` arms.
//!
//! The byte-oriented variants carry only `Copy`-able scalars, so both enums
//! remain `Clone + PartialEq + Eq` for use in conformance assertions.

use thiserror::Error;

/// Failure while decoding SECS-II wire bytes.
///
/// When a variant carries `offset`, it refers to a position inside the
/// original `input` slice passed to the decoder. It normally points at the
/// item header where the problem was detected; for body-level failures
/// (non-ASCII, misaligned payload, ...) it still points at that item's header
/// so the caller can recover the full surrounding item.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The strict item decoder received no bytes.
    ///
    /// Empty input cannot encode a SECS-II item. Protocol layers such as HSMS
    /// must represent absent Message Text before invoking the item decoder.
    #[error("SECS-II item input is empty")]
    EmptyInput,

    /// Input ended while reading an item header.
    ///
    /// Raised whenever the decoder runs out of bytes partway through an
    /// item header, whether that header belongs to the root item or to a
    /// child nested inside a List. For example, `&[0x41]` contains an ASCII
    /// format byte but omits its required single Length Byte.
    ///
    /// When a List is still open and the input ends cleanly at an item
    /// boundary (where `offset` equals the input length) the run loop
    /// reclassifies the error as
    /// [`TruncatedList`](Self::TruncatedList) and points at the open
    /// List's header instead.
    #[error("SECS-II input ended before an item header at offset {offset}")]
    TruncatedHeader {
        /// Starting byte offset of the item header that was being read
        /// when the input ran out, i.e. the position of its format byte.
        /// When the format byte was present but its declared length bytes
        /// were missing, `offset` is the position of that partial header and
        /// is strictly less than the input length.
        offset: usize,
    },

    /// The low two bits of a format byte encoded `0`, which E5 §6.2.1 /
    /// §6.3.1 forbid.
    #[error("SECS-II item at offset {offset} declared an illegal zero length-byte count")]
    ZeroLengthByteCount {
        /// Header offset of the offending item.
        offset: usize,
    },

    /// 非 List Item 声明的数据区超出剩余输入。
    ///
    /// 字段一律以**字节**为单位:List 的元素计数永远不会进入此变体,
    /// 请改用 [`TruncatedList`](Self::TruncatedList)。
    #[error(
        "SECS-II item at offset {offset} declared {declared_bytes} body bytes \
         but only {available_bytes} bytes remain"
    )]
    TruncatedBody {
        /// 出错 Item Header 的字节偏移。
        offset: usize,
        /// Item Header 声明的数据区字节数。
        declared_bytes: usize,
        /// Length Byte 之后实际可用的字节数。
        available_bytes: usize,
    },

    /// 输入结束时,List 尚未收到其声明的全部直接子元素。
    #[error(
        "SECS-II list at offset {offset} declared {expected_children} children \
         but input ended after {decoded_children} children"
    )]
    TruncatedList {
        /// 未完成 List Header 的字节偏移。
        offset: usize,
        /// List Length Byte 声明的直接子元素数量。
        expected_children: usize,
        /// 输入结束前已完整解码的直接子元素数量。
        decoded_children: usize,
    },

    /// The six-bit format code is not one of the sixteen E5-defined codes
    /// (List plus fifteen non-List formats).
    #[error("SECS-II item at offset {offset} uses unknown format code 0x{format_code:02X}")]
    UnknownFormatCode {
        /// Header offset of the offending item.
        offset: usize,
        /// Raw six-bit format code (upper six bits of the format byte).
        format_code: u8,
    },

    /// A numeric item's body length is not a multiple of its element width.
    #[error(
        "SECS-II numeric item at offset {offset} has body length {body_len} not divisible by element width {elem_width}"
    )]
    MisalignedNumericPayload {
        /// Header offset of the offending item.
        offset: usize,
        /// Six-bit format code that failed alignment.
        format_code: u8,
        /// Body length declared by the length bytes.
        body_len: usize,
        /// Expected per-element width (1, 2, 4 or 8).
        elem_width: usize,
    },

    /// A Localized Character String body is shorter than its mandatory
    /// two-byte LSH.
    #[error("SECS-II localized item at offset {offset} is shorter than its 2-byte LSH")]
    MissingLocalizedHeader {
        /// Header offset of the offending localized item.
        offset: usize,
    },

    /// A Localized Character String carried LSH encoding code `0`, which E5
    /// §6.4 reserves.
    #[error("SECS-II localized item at offset {offset} uses reserved LSH encoding code 0")]
    ReservedLocalizedEncodingCode {
        /// Header offset of the offending localized item.
        offset: usize,
    },

    /// An ASCII item contained a byte outside the seven-bit ASCII range.
    #[error("SECS-II ASCII item at offset {offset} contains non-ASCII byte 0x{byte:02X} at body index {index}")]
    NonAscii {
        /// Header offset of the offending ASCII item.
        offset: usize,
        /// Zero-based byte index inside the item body.
        index: usize,
        /// Offending byte value.
        byte: u8,
    },

    /// Strict item decoding completed one item but input still has bytes.
    ///
    /// Strict item decoding accepts exactly one item, so this error is
    /// returned whether the trailing bytes are malformed, incomplete, or
    /// encode one or more additional complete valid items.
    #[error("SECS-II decode_item consumed {consumed} of {total} bytes and left trailing data")]
    TrailingBytes {
        /// Number of bytes consumed by the single decoded item.
        consumed: usize,
        /// Total length of the input slice.
        total: usize,
    },

    /// Current List nesting depth exceeded `DecodeLimits::max_depth`.
    #[error("SECS-II nesting depth {depth} exceeds max_depth {max_depth} at offset {offset}")]
    DepthExceeded {
        /// Header offset of the item that would have pushed depth past the limit.
        offset: usize,
        /// Depth that would have been reached.
        depth: usize,
        /// Configured maximum depth.
        max_depth: usize,
    },

    /// Decoding the current header or accepting a List's declared children
    /// would require more nodes than `DecodeLimits::max_total_items` permits.
    #[error(
        "SECS-II item tree requires at least {required_items} nodes, exceeding max_total_items {max_total_items} at offset {offset}"
    )]
    TotalItemsExceeded {
        /// Header offset where the limit was hit.
        offset: usize,
        /// Minimum total node count required by the successfully parsed
        /// header: exact for the next node, or a lower bound when projecting
        /// the direct children declared by a List.
        required_items: usize,
        /// Configured maximum node count.
        max_total_items: usize,
    },

    /// A non-List item body exceeded `DecodeLimits::max_item_bytes`.
    #[error("SECS-II item at offset {offset} declares {declared} body bytes, exceeding max_item_bytes {max_item_bytes}")]
    ItemBytesExceeded {
        /// Header offset of the offending item.
        offset: usize,
        /// Declared body length.
        declared: usize,
        /// Configured per-item byte limit.
        max_item_bytes: usize,
    },

    /// A List item declared more direct children than
    /// `DecodeLimits::max_list_items`.
    #[error("SECS-II list at offset {offset} declares {declared} children, exceeding max_list_items {max_list_items}")]
    ListItemsExceeded {
        /// Header offset of the offending list.
        offset: usize,
        /// Declared direct child count.
        declared: usize,
        /// Configured per-list child limit.
        max_list_items: usize,
    },

    /// The item tree referenced more nodes than can be counted in `usize`
    /// without overflow.
    ///
    /// This is purely defensive: legitimate inputs never reach it.
    #[error("SECS-II decoder hit an internal arithmetic overflow at offset {offset}")]
    ArithmeticOverflow {
        /// Header offset where the overflow was detected.
        offset: usize,
    },
}

/// Failure while encoding a [`crate::secs2::SecsItem`] into wire bytes.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum EncodeError {
    /// A non-List item's payload byte count exceeded the E5 24-bit length
    /// field.
    #[error("SECS-II item with format code 0x{format_code:02X} has {body_bytes} body bytes, exceeding the 0xFF_FFFF maximum")]
    ItemBodyTooLarge {
        /// Six-bit format code of the offending item.
        format_code: u8,
        /// Computed non-List body length in bytes.
        body_bytes: usize,
    },

    /// A List's direct child count exceeded the E5 24-bit length field.
    #[error("SECS-II list has {child_count} direct children, exceeding the 0xFF_FFFF maximum")]
    ListTooLarge {
        /// Number of direct List children that cannot fit in the length field.
        child_count: usize,
    },

    /// An internal size or node-count computation overflowed `usize`.
    ///
    /// Defensive only; legitimate in-memory trees never reach it.
    #[error("SECS-II encoder hit an internal arithmetic overflow")]
    ArithmeticOverflow,
}
