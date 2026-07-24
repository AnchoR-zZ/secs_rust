//! SECS-II codec conformance tests.
//!
//! These tests exercise the public `secs2::codec` API against:
//!
//! - The fixed E5 §6.5 example vectors a/b/c/d plus the ASCII `HELLO` and
//!   Localized UTF-8 fixtures.
//! - Round-trip behaviour for every supported E5 format, including the
//!   typed-empty variants and the floating-point special values whose bit
//!   patterns must survive encode/decode unchanged.
//! - Length-byte boundary behaviour (1/2/3 length bytes).
//! - Decode rejection of malformed input: unknown format codes, zero length
//!   byte counts, truncated headers, truncated bodies, trailing bytes,
//!   misaligned numeric payloads, missing or reserved LSH, non-ASCII bodies.
//! - Enforcement of every `DecodeLimits` field.
//! - A fuzz-style sweep that ensures the decoder never panics on
//!   pseudo-randomly truncated and corrupted inputs.

#[path = "fixtures/mod.rs"]
mod fixtures;

use secs_rust::secs2::codec::header::{FormatCode, LengthByteCount};
use secs_rust::secs2::codec::{encode_to_vec, DecodeError, EncodeError, Secs2Decoder};
use secs_rust::secs2::MAX_ENCODED_ITEM_LENGTH;
use secs_rust::{AsciiString, DecodeLimits, LocalizedEncodingCode, LocalizedString, SecsItem};

/// Decodes one complete item with the supplied resource limits.
fn decode_with_limits(input: &[u8], limits: DecodeLimits) -> Result<SecsItem, DecodeError> {
    Secs2Decoder::new(limits).decode_item(input)
}

/// Convenience wrapper that rounds an item through the strict public codec,
/// asserting structural equality.
fn round_trip(item: &SecsItem) -> SecsItem {
    let bytes = encode_to_vec(item).expect("encode");
    Secs2Decoder::default().decode_item(&bytes).expect("decode")
}

// ---------------------------------------------------------------------------
// E5 §6.5 fixed example vectors
// ---------------------------------------------------------------------------

/// Verifies decoding of E5 §6.5 example a as one Binary octet.
#[test]
fn example_a_single_binary_octet_decodes_to_the_expected_value() {
    let item = decode_with_limits(fixtures::SECS2_BINARY_AA, DecodeLimits::default())
        .expect("decode example a");
    let SecsItem::Binary(bytes) = item else {
        panic!("expected Binary variant, got {item:?}");
    };
    assert_eq!(bytes, vec![0xAA]);
}

/// Verifies decoding of E5 §6.5 example b as the ASCII string `ABC`.
#[test]
fn example_b_ascii_abc_decodes_to_the_expected_string() {
    let item = decode_with_limits(fixtures::SECS2_ASCII_ABC, DecodeLimits::default())
        .expect("decode example b");
    let SecsItem::Ascii(text) = item else {
        panic!("expected Ascii variant, got {item:?}");
    };
    assert_eq!(text.as_str(), "ABC");
}

/// Verifies big-endian decoding of the three I2 values in E5 §6.5 example c.
#[test]
fn example_c_three_two_byte_signed_integers_decode_big_endian() {
    let item = decode_with_limits(fixtures::SECS2_I2_THREE_VALUES, DecodeLimits::default())
        .expect("decode example c");
    let SecsItem::I2(values) = item else {
        panic!("expected I2 variant, got {item:?}");
    };
    assert_eq!(values, vec![0x0102, 0x0304, 0x0506]);
}

/// Verifies decoding of the four-byte floating value in E5 §6.5 example d.
#[test]
fn example_d_single_four_byte_float_decodes_to_one() {
    let item = decode_with_limits(fixtures::SECS2_F4_ONE, DecodeLimits::default())
        .expect("decode example d");
    let SecsItem::F4(values) = item else {
        panic!("expected F4 variant, got {item:?}");
    };
    assert_eq!(values, vec![1.0_f32]);
}

/// Verifies byte-exact round-trip behavior for the ASCII `HELLO` fixture.
#[test]
fn ascii_hello_fixture_round_trips_byte_for_byte() {
    // The fixture is the canonical HSMS Message Text for ASCII HELLO; it
    // must survive a decode/encode round-trip unchanged.
    let item = decode_with_limits(fixtures::SECS2_ASCII_HELLO, DecodeLimits::default())
        .expect("decode HELLO");
    let re_encoded = encode_to_vec(&item).expect("encode HELLO");
    assert_eq!(re_encoded.as_slice(), fixtures::SECS2_ASCII_HELLO);
}

/// Verifies preservation of both LSH code and payload for a Localized fixture.
#[test]
fn localized_utf8_fixture_round_trips_with_lsh_preserved() {
    let item = decode_with_limits(
        fixtures::SECS2_LOCALIZED_UTF8_SHEBEI,
        DecodeLimits::default(),
    )
    .expect("decode localized");
    let SecsItem::Localized(value) = &item else {
        panic!("expected Localized variant, got {item:?}");
    };
    assert_eq!(value.encoding().get(), 0x0002);
    assert_eq!(value.as_bytes(), "设备".as_bytes());

    let re_encoded = encode_to_vec(&item).expect("encode localized");
    assert_eq!(re_encoded.as_slice(), fixtures::SECS2_LOCALIZED_UTF8_SHEBEI);
}

// ---------------------------------------------------------------------------
// Typed-empty distinctness
// ---------------------------------------------------------------------------

/// Verifies that every typed-empty item is stable and variant-distinct.
#[test]
fn typed_empty_variants_are_self_equal_and_mutually_distinct() {
    let empties = [
        SecsItem::List(Vec::new()),
        SecsItem::Binary(Vec::new()),
        SecsItem::Boolean(Vec::new()),
        SecsItem::Ascii(AsciiString::default()),
        SecsItem::Jis8(Vec::new()),
        SecsItem::I8(Vec::new()),
        SecsItem::I1(Vec::new()),
        SecsItem::I2(Vec::new()),
        SecsItem::I4(Vec::new()),
        SecsItem::F8(Vec::new()),
        SecsItem::F4(Vec::new()),
        SecsItem::U8(Vec::new()),
        SecsItem::U1(Vec::new()),
        SecsItem::U2(Vec::new()),
        SecsItem::U4(Vec::new()),
    ];

    // Each variant round-trips to itself.
    for variant in &empties {
        assert_eq!(round_trip(variant), variant.clone());
    }

    // No two typed-empty variants are equal.
    for (i, left) in empties.iter().enumerate() {
        for (j, right) in empties.iter().enumerate() {
            if i != j {
                assert_ne!(left, right, "variants {i} and {j} must differ");
            }
        }
    }

    // And none equals an absent Message Text (`None`).
    for variant in &empties {
        assert_ne!(Some(variant), None);
    }
}

// ---------------------------------------------------------------------------
// Empty Localized items
// ---------------------------------------------------------------------------

/// Verifies that an empty Localized payload still carries its two-byte LSH.
#[test]
fn empty_localized_string_carries_only_its_lsh() {
    // An empty Localized item still has its two-byte LSH in the body.
    let encoding = LocalizedEncodingCode::new(2).expect("non-zero encoding");
    let item = SecsItem::Localized(LocalizedString::new(encoding, Vec::new()));
    let bytes = encode_to_vec(&item).expect("encode empty localized");
    // Format byte 0x49 (Localized + 1 length byte), length 0x02, LSH 0x0002.
    assert_eq!(bytes, &[0x49, 0x02, 0x00, 0x02]);
    let round_tripped = round_trip(&item);
    let SecsItem::Localized(value) = &round_tripped else {
        panic!("expected Localized variant");
    };
    assert_eq!(value.encoding().get(), 0x0002);
    assert!(value.as_bytes().is_empty());
}

// ---------------------------------------------------------------------------
// Numeric, floating-point and Boolean round-trips
// ---------------------------------------------------------------------------

/// Verifies round trips across all signed and unsigned integer formats.
#[test]
fn every_numeric_format_round_trips() {
    let items = [
        SecsItem::I8(vec![i64::MIN, -1, 0, 1, i64::MAX]),
        SecsItem::I1(vec![i8::MIN, -1, 0, 1, i8::MAX]),
        SecsItem::I2(vec![i16::MIN, -1, 0, 1, i16::MAX]),
        SecsItem::I4(vec![i32::MIN, -1, 0, 1, i32::MAX]),
        SecsItem::U8(vec![0, 1, 0xDE, 0xAD, u64::MAX]),
        SecsItem::U1(vec![0, 1, 0x7F, 0x80, 0xFF]),
        SecsItem::U2(vec![0, 1, 0xBEEF, 0xFFFF]),
        SecsItem::U4(vec![0, 1, 0xDEAD_BEEF, u32::MAX]),
    ];
    for item in items {
        assert_eq!(round_trip(&item), item);
    }
}

/// Verifies bit-preserving round trips for floating-point special values.
#[test]
fn floating_point_special_values_preserve_their_bit_patterns() {
    // NaN payloads, infinities and negative zero must round-trip by bit
    // pattern; the codec uses to_bits/from_bits so this is exact.
    let f32_values = vec![
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        -0.0_f32,
        0.0_f32,
        f32::MIN_POSITIVE,
        f32::EPSILON,
        // A NaN with a non-canonical payload.
        f32::from_bits(0x7FC0_0042),
    ];
    let f64_values = vec![
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.0_f64,
        0.0_f64,
        f64::MIN_POSITIVE,
        f64::EPSILON,
        f64::from_bits(0x7FF8_0000_0000_0042),
    ];

    let f32_item = SecsItem::F4(f32_values.clone());
    let f64_item = SecsItem::F8(f64_values.clone());

    let SecsItem::F4(decoded_f32) = round_trip(&f32_item) else {
        panic!("expected F4 variant");
    };
    let SecsItem::F8(decoded_f64) = round_trip(&f64_item) else {
        panic!("expected F8 variant");
    };

    // Bit-for-bit comparison handles NaN equality that `==` cannot.
    assert_eq!(
        decoded_f32.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        f32_values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
    );
    assert_eq!(
        decoded_f64.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        f64_values.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
    );
}

/// Verifies permissive Boolean decoding and canonical true encoding.
#[test]
fn boolean_decode_accepts_any_non_zero_byte_and_encode_normalises_to_one() {
    // Build a raw Boolean item with mixed byte values to confirm the decoder
    // treats every non-zero byte as true. The body length (4) matches the
    // four trailing data bytes.
    let bool_byte = LengthByteCount::One.format_byte(FormatCode::Boolean);
    let raw = &[bool_byte, 0x04, 0x00, 0x01, 0x02, 0xFF];
    let item = decode_with_limits(raw, DecodeLimits::default()).expect("decode bool");
    let SecsItem::Boolean(values) = item else {
        panic!("expected Boolean variant");
    };
    assert_eq!(values, vec![false, true, true, true]);

    // Encoder normalises true to 0x01 regardless of the original source.
    let re_encoded =
        encode_to_vec(&SecsItem::Boolean(vec![false, true, true, true])).expect("encode bool");
    assert_eq!(re_encoded, vec![bool_byte, 0x04, 0x00, 0x01, 0x01, 0x01]);
}

// ---------------------------------------------------------------------------
// Length-byte boundaries (1, 2, 3)
// ---------------------------------------------------------------------------

/// Verifies minimal Length Byte selection at all representable boundaries.
#[test]
fn length_byte_boundaries_use_the_minimum_representable_count() {
    let cases: &[(usize, LengthByteCount)] = &[
        (0x0000_0000, LengthByteCount::One),
        (0x0000_00FF, LengthByteCount::One),
        (0x0000_0100, LengthByteCount::Two),
        (0x0000_FFFF, LengthByteCount::Two),
        (0x0001_0000, LengthByteCount::Three),
        (MAX_ENCODED_ITEM_LENGTH, LengthByteCount::Three),
    ];
    for &(body_len, expected) in cases {
        assert_eq!(
            LengthByteCount::for_declared_length(body_len),
            Some(expected),
            "body_len={body_len:#x}"
        );
    }
}

/// Verifies encode/decode behavior at exact one-, two-, and three-byte limits.
#[test]
fn length_byte_boundaries_round_trip_through_exact_lengths() {
    // Encode a binary item at each boundary body length and confirm the
    // produced header uses the expected length-byte count, then decode it
    // back to the same vector.
    for body_len in [
        0usize,
        0xFF,
        0x100,
        0xFFFF,
        0x1_0000,
        MAX_ENCODED_ITEM_LENGTH,
    ] {
        let original = SecsItem::Binary(vec![0xA5; body_len]);
        let encoded = encode_to_vec(&original).expect("encode boundary");
        let expected_count = LengthByteCount::for_declared_length(body_len).expect("representable");
        // Header byte = format byte with the count in the low bits.
        assert_eq!(encoded[0], expected_count.format_byte(FormatCode::Binary));
        let decoded =
            decode_with_limits(&encoded, DecodeLimits::default()).expect("decode boundary");
        assert_eq!(decoded, original);
    }
}

/// Verifies the byte-specific encoder error above the E5 scalar-body maximum.
#[test]
fn encoder_rejects_body_lengths_above_the_e5_maximum() {
    let overlong = SecsItem::Binary(vec![0u8; MAX_ENCODED_ITEM_LENGTH + 1]);
    let err = encode_to_vec(&overlong).unwrap_err();
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

/// Verifies that List length width depends on child count, not encoded bytes.
#[test]
fn list_length_byte_count_is_selected_from_child_count_not_byte_total() {
    // Regression for problem 1: a List whose child count fits one Length Byte
    // must use one Length Byte even though the sum of the children's wire
    // bytes (256) exceeds 255. List Length counts direct child elements.
    let children: Vec<SecsItem> = (0..128).map(|_| SecsItem::U1(Vec::new())).collect();
    let bytes = encode_to_vec(&SecsItem::List(children)).expect("list");
    assert_eq!(bytes[0], LengthByteCount::One.format_byte(FormatCode::List));
    assert_eq!(bytes[1], 128);
    // The two boundary crossings for the child count must still occur.
    let at_256: Vec<SecsItem> = (0..256).map(|_| SecsItem::U1(Vec::new())).collect();
    assert_eq!(
        encode_to_vec(&SecsItem::List(at_256)).expect("list")[0],
        LengthByteCount::Two.format_byte(FormatCode::List)
    );
    let at_65536: Vec<SecsItem> = (0..65536).map(|_| SecsItem::U1(Vec::new())).collect();
    assert_eq!(
        encode_to_vec(&SecsItem::List(at_65536)).expect("list")[0],
        LengthByteCount::Three.format_byte(FormatCode::List)
    );
}

// ---------------------------------------------------------------------------
// Nested List round-trip
// ---------------------------------------------------------------------------

/// Verifies structural round-trip behavior for mixed nested Lists.
#[test]
fn nested_lists_round_trip() {
    let tree = SecsItem::List(vec![
        SecsItem::Ascii(AsciiString::new("A").expect("ascii")),
        SecsItem::List(vec![
            SecsItem::U1(vec![1, 2, 3]),
            SecsItem::Boolean(vec![true, false]),
        ]),
        SecsItem::List(Vec::new()),
    ]);
    assert_eq!(round_trip(&tree), tree);
}

// ---------------------------------------------------------------------------
// Decoder rejection of malformed input
// ---------------------------------------------------------------------------

/// Verifies rejection of a reserved six-bit format code on the wire.
#[test]
fn unknown_format_code_is_rejected() {
    // 0o17 (reserved) with 1 length byte: byte = (0o17 << 2) | 1 = 0x3D.
    let bytes = &[0x3D, 0x00];
    assert!(matches!(
        decode_with_limits(bytes, DecodeLimits::default()).unwrap_err(),
        DecodeError::UnknownFormatCode {
            offset: 0,
            format_code: 0o17
        }
    ));
}

/// Verifies rejection of an E5-illegal zero Length Byte count.
#[test]
fn zero_length_byte_count_is_rejected() {
    // ASCII format code with low bits 00.
    let bytes = &[
        LengthByteCount::One.format_byte(FormatCode::Ascii) & !0b11,
        b'H',
    ];
    assert!(matches!(
        decode_with_limits(bytes, DecodeLimits::default()).unwrap_err(),
        DecodeError::ZeroLengthByteCount { offset: 0 }
    ));
}

/// Verifies distinct structured errors for empty input and a partial header.
#[test]
fn empty_input_and_truncated_header_are_rejected() {
    assert!(matches!(
        decode_with_limits(&[], DecodeLimits::default()).unwrap_err(),
        DecodeError::EmptyInput
    ));
    // Single format byte without any length byte.
    let bytes = &[LengthByteCount::One.format_byte(FormatCode::Ascii)];
    assert!(matches!(
        decode_with_limits(bytes, DecodeLimits::default()).unwrap_err(),
        DecodeError::TruncatedHeader { offset: 0 }
    ));
}

/// Verifies byte-oriented diagnostics for an incomplete scalar body.
#[test]
fn truncated_body_is_rejected() {
    // ASCII item claiming 5 bytes but supplying only 3.
    let bytes = &[0x41, 0x05, b'A', b'B', b'C'];
    let err = decode_with_limits(bytes, DecodeLimits::default()).unwrap_err();
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

/// Verifies child-count diagnostics for an incomplete root List.
#[test]
fn truncated_root_list_reports_truncated_list() {
    // Root L[3] with a single complete child and then EOF at an item
    // boundary: the List is short of children, so TruncatedList (element
    // count semantics) must be reported instead of the byte-oriented
    // TruncatedBody.
    let u1_byte = LengthByteCount::One.format_byte(FormatCode::U1);
    let bytes = &[0b0000_0001, 0x03, u1_byte, 0x01, 0x01];
    let err = decode_with_limits(bytes, DecodeLimits::default()).unwrap_err();
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

/// Verifies that nested List truncation identifies the innermost open List.
#[test]
fn truncated_nested_list_reports_the_innermost_open_list() {
    // L2 { L2 { U1=1 } } then EOF: the inner List has 1/2 children, so the
    // report must point at the inner List header (offset 2), not the outer.
    let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
    let u1_byte = LengthByteCount::One.format_byte(FormatCode::U1);
    let bytes = &[
        list_byte, 0x02, list_byte, 0x02, u1_byte, 0x01,
        0x01, // inner List's first child
             // EOF here: inner List still wants one more child.
    ];
    let err = decode_with_limits(bytes, DecodeLimits::default()).unwrap_err();
    match err {
        DecodeError::TruncatedList {
            offset,
            expected_children,
            decoded_children,
        } => {
            assert_eq!(offset, 2);
            assert_eq!(expected_children, 2);
            assert_eq!(decoded_children, 1);
        }
        other => panic!("expected TruncatedList, got {other:?}"),
    }
}

/// Verifies that strict decoding rejects data after one complete item.
#[test]
fn trailing_bytes_are_rejected_by_strict_decode() {
    // Empty list plus a stray trailing byte.
    let bytes = &[
        LengthByteCount::One.format_byte(FormatCode::List),
        0x00,
        0xFF,
    ];
    let err = decode_with_limits(bytes, DecodeLimits::default()).unwrap_err();
    match err {
        DecodeError::TrailingBytes { consumed, total } => {
            assert_eq!(consumed, 2);
            assert_eq!(total, 3);
        }
        other => panic!("expected TrailingBytes, got {other:?}"),
    }
}

/// Verifies that a second complete item is still classified as trailing data.
#[test]
fn second_complete_item_is_rejected_as_trailing_data() {
    let empty_list = [LengthByteCount::One.format_byte(FormatCode::List), 0x00];
    let bytes = [empty_list, empty_list].concat();
    assert_eq!(
        decode_with_limits(&bytes, DecodeLimits::default()),
        Err(DecodeError::TrailingBytes {
            consumed: empty_list.len(),
            total: bytes.len(),
        })
    );
}

/// Verifies fixed-width numeric alignment validation.
#[test]
fn misaligned_numeric_payload_is_rejected() {
    let u2_byte = LengthByteCount::One.format_byte(FormatCode::U2);
    // Body length 3 is not divisible by the U2 element width of 2.
    let bytes = &[u2_byte, 0x03, 0x00, 0x01, 0x02];
    let err = decode_with_limits(bytes, DecodeLimits::default()).unwrap_err();
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

/// Verifies that a Localized item must contain a complete two-byte LSH.
#[test]
fn missing_localized_lsh_is_rejected() {
    // Localized body shorter than the two-byte LSH.
    let localized_byte = LengthByteCount::One.format_byte(FormatCode::Localized);
    let bytes = &[localized_byte, 0x01, 0xFF];
    assert!(matches!(
        decode_with_limits(bytes, DecodeLimits::default()).unwrap_err(),
        DecodeError::MissingLocalizedHeader { offset: 0 }
    ));
}

/// Verifies rejection of the E5-reserved Localized encoding code zero.
#[test]
fn reserved_localized_encoding_code_is_rejected() {
    let localized_byte = LengthByteCount::One.format_byte(FormatCode::Localized);
    let bytes = &[localized_byte, 0x02, 0x00, 0x00];
    assert!(matches!(
        decode_with_limits(bytes, DecodeLimits::default()).unwrap_err(),
        DecodeError::ReservedLocalizedEncodingCode { offset: 0 }
    ));
}

/// Verifies seven-bit validation for an ASCII wire payload.
#[test]
fn non_ascii_ascii_payload_is_rejected() {
    let bytes = &[0x41, 0x01, 0xE8];
    let err = decode_with_limits(bytes, DecodeLimits::default()).unwrap_err();
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

/// Verifies byte preservation when Localized content is not valid UTF-8.
#[test]
fn localized_payload_is_preserved_even_when_not_valid_utf8() {
    // LSH claims UTF-8 but the payload is not valid UTF-8. The decoder must
    // still succeed and preserve the raw bytes (codec stays byte-faithful).
    let localized_byte = LengthByteCount::One.format_byte(FormatCode::Localized);
    let bytes = &[localized_byte, 0x04, 0x00, 0x02, 0xFF, 0xFF];
    let item = decode_with_limits(bytes, DecodeLimits::default()).expect("byte-faithful decode");
    let SecsItem::Localized(value) = item else {
        panic!("expected Localized");
    };
    assert_eq!(value.encoding().get(), 0x0002);
    assert_eq!(value.as_bytes(), &[0xFF, 0xFF]);
}

// ---------------------------------------------------------------------------
// DecodeLimits enforcement
// ---------------------------------------------------------------------------

/// Returns otherwise-default limits with one named field replaced by `value`.
///
/// The helper panics when `field` is unknown or the replacement is invalid,
/// because every caller supplies a compile-time test case.
fn permissive_with(field: &str, value: usize) -> DecodeLimits {
    // Defaults except for the requested field; used to isolate each limit.
    let mut limits = DecodeLimits::default();
    limits = match field {
        "max_depth" => DecodeLimits::new(
            value,
            limits.max_total_items(),
            limits.max_item_bytes(),
            limits.max_list_items(),
        )
        .unwrap(),
        "max_total_items" => DecodeLimits::new(
            limits.max_depth(),
            value,
            limits.max_item_bytes(),
            limits.max_list_items(),
        )
        .unwrap(),
        "max_item_bytes" => DecodeLimits::new(
            limits.max_depth(),
            limits.max_total_items(),
            value,
            limits.max_list_items(),
        )
        .unwrap(),
        "max_list_items" => DecodeLimits::new(
            limits.max_depth(),
            limits.max_total_items(),
            limits.max_item_bytes(),
            value,
        )
        .unwrap(),
        _ => unreachable!("unknown limit {field}"),
    };
    limits
}

/// Verifies depth rejection at the first List beyond the configured limit.
#[test]
fn max_depth_rejects_an_over_nested_list() {
    let limits = permissive_with("max_depth", 2);
    let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
    let bytes = &[
        list_byte, 0x01, // depth 1
        list_byte, 0x01, // depth 2
        list_byte, 0x00, // depth 3 -> reject
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

/// Verifies scalar body rejection above the configured byte limit.
#[test]
fn max_item_bytes_rejects_an_overlong_scalar_body() {
    let limits = permissive_with("max_item_bytes", 2);
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

/// Verifies List rejection above the configured direct-child limit.
#[test]
fn max_list_items_rejects_a_too_wide_list() {
    let limits = permissive_with("max_list_items", 1);
    let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
    let bytes = &[list_byte, 0x02];
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

/// Verifies total-node projection rejects an impossible List before allocation.
#[test]
fn max_total_items_rejects_a_projected_overflow_at_the_list_header() {
    // max_total_items = 3. Outer List header counts as 1, declaring 3
    // children would push the projected total to 4 > 3.
    let limits = permissive_with("max_total_items", 3);
    let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
    let bytes = &[list_byte, 0x03];
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

// ---------------------------------------------------------------------------
// Fuzz: the decoder must never panic on arbitrary bytes.
// ---------------------------------------------------------------------------

/// Verifies that every truncation of representative valid bytes returns safely.
#[test]
fn decoder_never_panics_on_truncations_of_a_valid_payload() {
    // Take a known-valid payload and stop it at every byte boundary. The
    // decoder must either succeed or return a structured error.
    let mut valid = Vec::new();
    valid.extend_from_slice(fixtures::SECS2_ASCII_HELLO);
    valid.extend_from_slice(fixtures::SECS2_I2_THREE_VALUES);
    valid.extend_from_slice(fixtures::SECS2_LOCALIZED_UTF8_SHEBEI);

    let limits = DecodeLimits::default();
    for end in 0..=valid.len() {
        let _ = decode_with_limits(&valid[..end], limits);
    }
}

/// Verifies that every single-byte mutation returns a result without panicking.
#[test]
fn decoder_never_panics_on_single_byte_corruption() {
    // Flip each byte of a valid payload to every other value in 0..=255 and
    // confirm the decoder returns Result rather than panicking.
    let valid = fixtures::SECS2_ASCII_HELLO.to_vec();
    let limits = DecodeLimits::default();
    for index in 0..valid.len() {
        let original = valid[index];
        for replacement in 0u8..=255 {
            if replacement == original {
                continue;
            }
            let mut mutated = valid.clone();
            mutated[index] = replacement;
            let _ = decode_with_limits(&mutated, limits);
        }
    }
}

/// Verifies clean limit rejection for input far beyond the default depth.
#[test]
fn decoder_never_panics_on_pathological_nesting() {
    // A deep chain of single-child Lists followed by a non-List item: even
    // with default limits the decoder must return DepthExceeded cleanly.
    let list_byte = LengthByteCount::One.format_byte(FormatCode::List);
    let mut bytes = Vec::new();
    // Far deeper than the default max_depth (64) to confirm rejection.
    for _ in 0..500 {
        bytes.push(list_byte);
        bytes.push(0x01);
    }
    bytes.push(LengthByteCount::One.format_byte(FormatCode::U1));
    bytes.push(0x01);
    bytes.push(0x00);
    let result = decode_with_limits(&bytes, DecodeLimits::default());
    assert!(matches!(result, Err(DecodeError::DepthExceeded { .. })));
}
