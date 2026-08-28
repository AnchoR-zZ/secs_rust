//! SML text-adapter conformance tests.
//!
//! These tests exercise the public `secs_rust::sml` API against:
//!
//! - Hand-pinned canonical text goldens (Compact and Pretty) shared via the
//!   fixtures module, so formatter drift cannot pass silently.
//! - The fixed E5 byte vectors: decoding them, rendering SML, and parsing
//!   that SML back must reproduce identical items and wire bytes.
//! - Round-trips `item -> SML -> item` and `message -> SML -> message` for
//!   both styles, using the binary codec's byte output as the equality
//!   oracle so float bit patterns (negative zero, subnormals) must match.
//! - The strict dialect's distinction between an absent body and an empty
//!   list body, optional whitespace, and the `.` terminator rules.
//! - Rejection of malformed, unsupported, and resource-exceeding input.

#[path = "fixtures/mod.rs"]
mod fixtures;

use secs_rust::hsms::{Function, Stream};
use secs_rust::secs2::codec::{encode_to_vec, Secs2Decoder};
use secs_rust::sml::{
    parse_item, parse_message, FormatError, FormatStyle, ParseError, SmlFormatter, SmlMessage,
    SmlParser,
};
use secs_rust::{AsciiString, DecodeLimits, SecsItem};

/// Formats one item in the given style, panicking on refusal.
fn format_item(style: FormatStyle, item: &SecsItem) -> String {
    SmlFormatter::new(style)
        .format_item(item)
        .expect("fixture items are canonical SML representable")
}

/// Asserts that `text` parses to an item whose wire encoding is byte-identical
/// to `item`'s, which also demands bit-exact floats and identical structure.
fn assert_parses_to(text: &str, item: &SecsItem) {
    let parsed = parse_item(text).expect("canonical text must parse");
    assert_eq!(
        encode_to_vec(&parsed).expect("encode parsed"),
        encode_to_vec(item).expect("encode expected"),
        "text {text:?} must parse to the expected item"
    );
}

/// Round-trips an item through Compact and Pretty SML, asserting byte-exact
/// wire equality after each reparse (the oracle for float bit patterns).
fn round_trip_item(item: &SecsItem) {
    for style in [FormatStyle::Compact, FormatStyle::Pretty] {
        let text = format_item(style, item);
        assert_parses_to(&text, item);
    }
}

/// Builds an alarm-report body: `<L[2] <B[1] 0x04> <A[6] "LOT001">>`.
fn alarm_body() -> SecsItem {
    SecsItem::List(vec![
        SecsItem::Binary(vec![0x04]),
        SecsItem::Ascii(AsciiString::try_from("LOT001").expect("ASCII literal")),
    ])
}

/// Builds the S5F1 alarm-report message with the W-Bit set.
fn alarm_message() -> SmlMessage {
    SmlMessage::new(
        Stream::new(5).expect("stream 5"),
        Function::new(1),
        true,
        Some(alarm_body()),
    )
}

// ---------------------------------------------------------------------------
// Canonical goldens
// ---------------------------------------------------------------------------

/// Verifies the Compact formatter output for the S5F1 alarm report.
#[test]
fn compact_alarm_message_matches_pinned_golden() {
    let text = SmlFormatter::new(FormatStyle::Compact)
        .format_message(&alarm_message())
        .expect("alarm message formats");
    assert_eq!(text, fixtures::SML_S5F1_ALARM_COMPACT);
}

/// Verifies the Pretty formatter output for the S5F1 alarm report, including
/// indentation, line breaks, and the glued terminator.
#[test]
fn pretty_alarm_message_matches_pinned_golden() {
    let text = SmlFormatter::new(FormatStyle::Pretty)
        .format_message(&alarm_message())
        .expect("alarm message formats");
    assert_eq!(text, fixtures::SML_S5F1_ALARM_PRETTY);
}

/// Verifies Compact and Pretty item goldens for the nested-list shape.
#[test]
fn nested_list_item_matches_pinned_goldens() {
    let item = SecsItem::List(vec![
        SecsItem::Ascii(AsciiString::try_from("x").expect("ASCII literal")),
        SecsItem::Binary(Vec::new()),
    ]);
    assert_eq!(
        format_item(FormatStyle::Compact, &item),
        fixtures::SML_NESTED_LIST_COMPACT
    );
    assert_eq!(
        format_item(FormatStyle::Pretty, &item),
        fixtures::SML_NESTED_LIST_PRETTY
    );
}

/// Verifies the pinned golden for the float suffix rule: `1.0` keeps a `.0`.
#[test]
fn integral_float_keeps_fraction_suffix_in_golden() {
    let item = SecsItem::F4(vec![1.0]);
    assert_eq!(
        format_item(FormatStyle::Compact, &item),
        fixtures::SML_F4_ONE
    );
}

// ---------------------------------------------------------------------------
// Cross-checks against the fixed E5 byte vectors
// ---------------------------------------------------------------------------

/// Verifies that decoding a fixture and rendering it yields the pinned SML.
#[test]
fn decoded_fixture_bytes_render_as_pinned_sml() {
    let cases: &[(&[u8], &str)] = &[
        (fixtures::SECS2_ASCII_HELLO, fixtures::SML_ASCII_HELLO),
        (fixtures::SECS2_BINARY_AA, fixtures::SML_BINARY_AA),
        (
            fixtures::SECS2_I2_THREE_VALUES,
            fixtures::SML_I2_THREE_VALUES,
        ),
        (fixtures::SECS2_F4_ONE, fixtures::SML_F4_ONE),
    ];
    for (bytes, expected) in cases {
        let item = Secs2Decoder::default()
            .decode_item(bytes)
            .expect("fixture bytes decode");
        assert_eq!(&format_item(FormatStyle::Compact, &item), expected);
    }
}

/// Verifies that parsing the pinned SML re-encodes to the identical bytes.
#[test]
fn pinned_sml_reencodes_to_the_identical_fixture_bytes() {
    let cases: &[(&str, &[u8])] = &[
        (fixtures::SML_ASCII_HELLO, fixtures::SECS2_ASCII_HELLO),
        (fixtures::SML_BINARY_AA, fixtures::SECS2_BINARY_AA),
        (
            fixtures::SML_I2_THREE_VALUES,
            fixtures::SECS2_I2_THREE_VALUES,
        ),
        (fixtures::SML_F4_ONE, fixtures::SECS2_F4_ONE),
    ];
    for (text, expected) in cases {
        let item = parse_item(text).expect("pinned SML parses");
        assert_eq!(
            &encode_to_vec(&item).expect("encode"),
            expected,
            "text {text:?} must re-encode identically"
        );
    }
}

/// Verifies that a Localized item decoded from the wire fixture is refused by
/// the formatter instead of being degraded to another SML type.
#[test]
fn decoded_localized_fixture_is_refused_by_the_formatter() {
    let item = Secs2Decoder::default()
        .decode_item(fixtures::SECS2_LOCALIZED_UTF8_SHEBEI)
        .expect("localized fixture decodes");
    let error = SmlFormatter::new(FormatStyle::Compact)
        .format_item(&item)
        .expect_err("localized text has no SML representation");
    assert_eq!(error, FormatError::UnsupportedType { name: "Localized" });
}

// ---------------------------------------------------------------------------
// Item and message round-trips
// ---------------------------------------------------------------------------

/// Round-trips every integer width at its extremes through both styles.
#[test]
fn integer_extremes_round_trip_through_both_styles() {
    round_trip_item(&SecsItem::I1(vec![i8::MIN, -1, 0, 1, i8::MAX]));
    round_trip_item(&SecsItem::I2(vec![i16::MIN, i16::MAX]));
    round_trip_item(&SecsItem::I4(vec![i32::MIN, i32::MAX]));
    round_trip_item(&SecsItem::I8(vec![i64::MIN, i64::MAX]));
    round_trip_item(&SecsItem::U1(vec![u8::MIN, u8::MAX]));
    round_trip_item(&SecsItem::U2(vec![u16::MIN, u16::MAX]));
    round_trip_item(&SecsItem::U4(vec![u32::MIN, u32::MAX]));
    round_trip_item(&SecsItem::U8(vec![u64::MIN, u64::MAX]));
}

/// Round-trips float specials whose bit patterns must survive, including
/// negative zero, subnormals, and the finite extremes of each width.
#[test]
fn float_specials_round_trip_bit_exactly() {
    round_trip_item(&SecsItem::F4(vec![
        0.0,
        -0.0,
        f32::MIN_POSITIVE,
        f32::MIN,
        f32::MAX,
        1e-45,
    ]));
    round_trip_item(&SecsItem::F8(vec![
        0.0,
        -0.0,
        f64::MIN_POSITIVE,
        f64::MIN,
        f64::MAX,
        5e-324,
        1.5e-10,
    ]));
}

/// Round-trips a mixed nested tree covering every SML-representable type.
#[test]
fn mixed_nested_tree_round_trips_through_both_styles() {
    let tree = SecsItem::List(vec![
        SecsItem::Boolean(vec![true, false, true]),
        SecsItem::Ascii(AsciiString::try_from("a\tb\"c\\d").expect("ASCII literal")),
        SecsItem::Binary(vec![0x00, 0x0A, 0x7F, 0xFF]),
        SecsItem::I1(vec![-128]),
        SecsItem::U8(vec![u64::MAX]),
        SecsItem::F4(vec![-0.0, 2.5]),
        SecsItem::F8(vec![1e300]),
        SecsItem::List(vec![SecsItem::List(vec![SecsItem::Boolean(vec![false])])]),
    ]);
    round_trip_item(&tree);
}

/// Round-trips the S5F1 alarm message through both styles.
#[test]
fn alarm_message_round_trips_through_both_styles() {
    for style in [FormatStyle::Compact, FormatStyle::Pretty] {
        let text = SmlFormatter::new(style)
            .format_message(&alarm_message())
            .expect("alarm message formats");
        let parsed = parse_message(&text).expect("formatted message reparses");
        assert_eq!(parsed, alarm_message());
    }
}

// ---------------------------------------------------------------------------
// Strict message semantics
// ---------------------------------------------------------------------------

/// Verifies that a bodyless message parses with `body == None`.
#[test]
fn bodyless_message_parses_with_no_body() {
    let message = parse_message("S1F1.").expect("bodyless message parses");
    assert_eq!(message.stream().get(), 1);
    assert_eq!(message.function().get(), 1);
    assert!(!message.wait_bit());
    assert!(message.body().is_none());
}

/// Verifies that `S1F1 W <L [0]> .` is a W-Bit message with an EMPTY LIST
/// body, strictly distinct from the bodyless `S1F1 W.`.
#[test]
fn empty_list_body_is_distinct_from_absent_body() {
    let with_empty = parse_message("S1F1 W <L [0]> .").expect("empty-list body parses");
    assert!(with_empty.wait_bit());
    assert_eq!(with_empty.body(), Some(&SecsItem::List(Vec::new())));

    let bodyless = parse_message("S1F1 W.").expect("bodyless parses");
    assert!(bodyless.body().is_none());
}

/// Verifies the strict dialect rejects attached and doubled W-Bit forms;
/// only the separate-word `S1F1 W.` is accepted.
#[test]
fn attached_and_doubled_wait_bit_forms_are_rejected() {
    assert!(matches!(
        parse_message("S1F1W."),
        Err(ParseError::InvalidMessageHeader { .. })
    ));
    assert!(matches!(
        parse_message("S1F1W W."),
        Err(ParseError::InvalidMessageHeader { .. })
    ));
    assert!(matches!(
        parse_message("S1F1 W W."),
        Err(ParseError::UnexpectedToken { .. })
    ));
}

/// Verifies lazy scanning reports errors in source order: the malformed
/// header of `BAD,` surfaces before the comma is ever scanned.
#[test]
fn lazy_scanning_reports_source_ordered_errors() {
    assert!(matches!(
        parse_message("BAD,"),
        Err(ParseError::InvalidMessageHeader { .. })
    ));
    // Lexically invalid trailing characters surface as the scan error at
    // their own position, not as TrailingInput; lexically valid trailing
    // content still reports TrailingInput.
    assert!(matches!(
        parse_message("S1F1.,"),
        Err(ParseError::InvalidCharacter { .. })
    ));
    assert!(matches!(
        parse_message("S1F1 . 5"),
        Err(ParseError::TrailingInput { .. })
    ));
}

/// Verifies `TrailingInput` diagnostics use entry-point-neutral wording for
/// both standalone item and complete message parsing.
#[test]
fn trailing_input_display_is_entry_point_neutral() {
    let message_error = parse_message("S1F1 . 5").expect_err("trailing message content must fail");
    assert!(matches!(message_error, ParseError::TrailingInput { .. }));
    let message_text = message_error.to_string();
    assert!(message_text.contains("unexpected trailing content"));
    assert!(!message_text.contains("after the message"));

    let item_error = parse_item("<B[0]> <B[0]>").expect_err("trailing item content must fail");
    assert!(matches!(item_error, ParseError::TrailingInput { .. }));
    let item_text = item_error.to_string();
    assert!(item_text.contains("unexpected trailing content"));
    assert!(!item_text.contains("after the message"));
}

/// Verifies limits bind during parsing, before the rest of the input is
/// scanned: with a one-node budget, the sentinel comma past the first child
/// is never reached and TotalItemsExceeded is reported instead.
#[test]
fn lazy_scanning_stops_at_the_resource_limit() {
    let parser =
        SmlParser::new(DecodeLimits::new(64, 1, 0x00FF_FFFF, 1_000_000).expect("valid limits"));
    assert!(matches!(
        parser.parse_item("<L <B [0]> ,>"),
        Err(ParseError::TotalItemsExceeded {
            max_total_items: 1,
            ..
        })
    ));
}

/// Verifies list admission runs before the excess child subtree is parsed:
/// with max_list_items=1 the oversized second child triggers the hard limit,
/// and with default limits but a declared count of 1 the early CountMismatch
/// fires with the detection-time count.
#[test]
fn list_admission_precedes_subtree_parsing() {
    let input = "<L [1] <B [0]> <L [2] <A [1] \"x\"> <B [0]>>>";
    let tight =
        SmlParser::new(DecodeLimits::new(64, 1_000_000, 0x00FF_FFFF, 1).expect("valid limits"));
    assert!(matches!(
        tight.parse_item(input),
        Err(ParseError::ListItemsExceeded {
            max_list_items: 1,
            ..
        })
    ));
    assert!(matches!(
        parse_item(input),
        Err(ParseError::CountMismatch {
            declared: 1,
            actual: 2,
            ..
        })
    ));
}

/// Verifies an ASCII fragment contributes its character count, not fragment
/// count, to the early mismatch check.
#[test]
fn ascii_fragment_counts_elements_not_fragments() {
    assert!(matches!(
        parse_item("<A [2] \"abcd\">"),
        Err(ParseError::CountMismatch {
            declared: 2,
            actual: 4,
            ..
        })
    ));
    // Shortfalls remain close-time checks.
    assert!(matches!(
        parse_item("<U1 [3] 1 2>"),
        Err(ParseError::CountMismatch {
            declared: 3,
            actual: 2,
            ..
        })
    ));
}

/// Verifies header digits parse with leading zeros and saturate only on
/// true u64 overflow.
#[test]
fn header_digits_parse_with_leading_zeros_and_true_saturation() {
    let padded = parse_message("S00000000001F1.").expect("leading zeros parse");
    assert_eq!(padded.stream().get(), 1);

    let padded_function = parse_message("S1F00000000000000000255.").expect("function padding");
    assert_eq!(padded_function.function().get(), 255);

    // u64::MAX itself parses exactly; one past it saturates to u64::MAX.
    assert!(matches!(
        parse_message("S18446744073709551615F1."),
        Err(ParseError::StreamOutOfRange { value, .. }) if value == u64::MAX
    ));
    assert!(matches!(
        parse_message("S18446744073709551616F1."),
        Err(ParseError::StreamOutOfRange { value, .. }) if value == u64::MAX
    ));
}

/// Verifies integer overflow diagnostics quote the original literal
/// spelling, including radix prefixes and digit case.
#[test]
fn integer_overflow_literal_preserves_original_spelling() {
    assert!(matches!(
        parse_item("<U1 [1] 0x0100>"),
        Err(ParseError::IntegerOverflow { literal, target, .. })
            if literal == "0x0100" && target == "U1"
    ));
    assert!(matches!(
        parse_item("<I1 [1] 0xff>"),
        Err(ParseError::IntegerOverflow { literal, target, .. })
            if literal == "0xff" && target == "I1"
    ));
}

/// Verifies that stream/function identifiers accept their boundary values
/// (S0F0 through S127F255) and reject one past them.
#[test]
fn header_identifier_ranges_are_enforced() {
    let max = parse_message("S127F255 <A [0] > .").expect("boundary header parses");
    assert_eq!(max.stream().get(), 127);
    assert_eq!(max.function().get(), 255);

    let zero = parse_message("S0F0.").expect("zero header parses");
    assert_eq!(zero.stream().get(), 0);
    assert_eq!(zero.function().get(), 0);

    assert!(matches!(
        parse_message("S128F1."),
        Err(ParseError::StreamOutOfRange { value: 128, .. })
    ));
    assert!(matches!(
        parse_message("S1F256."),
        Err(ParseError::FunctionOutOfRange { value: 256, .. })
    ));
}

/// Verifies that whitespace between tokens is optional but multi-line Pretty
/// output also parses.
#[test]
fn whitespace_is_optional_between_tokens() {
    let no_space = parse_message(r#"S1F2<L[1]<A[1]"x">>."#).expect("no-space form parses");
    assert_eq!(
        no_space.body(),
        Some(&SecsItem::List(vec![SecsItem::Ascii(
            AsciiString::try_from("x").expect("ASCII literal")
        )]))
    );

    let pretty_text = fixtures::SML_S5F1_ALARM_PRETTY;
    let parsed = parse_message(pretty_text).expect("pretty text parses");
    assert_eq!(parsed, alarm_message());
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

/// Verifies count mismatches are rejected with the declared and actual values.
#[test]
fn count_mismatch_reports_declared_and_actual() {
    assert!(matches!(
        parse_item(r#"<L [2] <A [1] "x">>"#),
        Err(ParseError::CountMismatch {
            declared: 2,
            actual: 1,
            ..
        })
    ));
}

/// Verifies unsupported JIS-8 keywords are refused explicitly.
#[test]
fn jis8_keyword_is_unsupported() {
    assert!(matches!(
        parse_item("<J [1] 0x41>"),
        Err(ParseError::UnsupportedType { name, .. }) if name == "J"
    ));
    assert!(matches!(
        parse_item("<JIS8 [1] 0x41>"),
        Err(ParseError::UnsupportedType { name, .. }) if name == "JIS8"
    ));
}

/// Verifies a float literal too large for F4 is refused as non-finite rather
/// than silently clamping (Rust parses `1e40` as f32 infinity, not an error).
#[test]
fn f4_overflow_literal_is_non_finite() {
    let error = parse_item("<F4 [1] 1e40>").expect_err("1e40 exceeds finite f32");
    assert!(matches!(
        error,
        ParseError::NonFiniteFloat { target: "F4", .. }
    ));
}

/// Verifies hex bytes at or above 0x80 are refused inside `<A>`.
#[test]
fn non_ascii_hex_byte_is_refused_in_ascii_items() {
    let error = parse_item(r#"<A [2] "a" 0x80>"#).expect_err("0x80 is not ASCII");
    assert!(matches!(
        error,
        ParseError::InvalidAsciiByte { byte: 0x80, .. }
    ));
}

/// Verifies strict rejections of legacy aliases, bare counts, templates,
/// ranges, ellipses, message names, and quoted headers.
#[test]
fn legacy_and_template_syntax_is_rejected() {
    for text in [
        "<BOOL [1] T>",
        "<Boolean [1] true>",
        "<L2>",
        "<L [0x2]>",
        "S1F1 \"S1F13\" .",
        "S1F1 <L [1..8]> .",
    ] {
        assert!(parse_message(text).is_err(), "{text:?} must be rejected");
    }
}

/// Verifies missing terminator and trailing content rejections.
#[test]
fn terminator_rules_are_enforced() {
    assert!(matches!(
        parse_message("S1F1"),
        Err(ParseError::MissingTerminator { .. })
    ));
    assert!(matches!(
        parse_message("S1F1 . 5"),
        Err(ParseError::TrailingInput { .. })
    ));
    assert!(parse_message("S1F1 <A [1] \"x\"> <A [1] \"y\"> .").is_err());
}

// ---------------------------------------------------------------------------
// Resource limits through the public parser
// ---------------------------------------------------------------------------

/// Builds a limits set for the parser tests, panicking on invalid values.
fn limits(depth: usize, total: usize, bytes: usize, list: usize) -> SmlParser {
    SmlParser::new(DecodeLimits::new(depth, total, bytes, list).expect("test limits are valid"))
}

/// Verifies the depth limit rejects a nested list one level too deep.
#[test]
fn depth_limit_rejects_excessive_nesting() {
    let parser = limits(1, 1_000_000, 0x00FF_FFFF, 1_000_000);
    assert!(parser.parse_item("<L [0]>").is_ok());
    assert!(matches!(
        parser.parse_item("<L [1] <L [0]>>"),
        Err(ParseError::DepthExceeded {
            depth: 2,
            max_depth: 1,
            ..
        })
    ));
}

/// Verifies the total-node limit counts every item in the tree.
#[test]
fn total_items_limit_rejects_the_second_node() {
    let parser = limits(64, 1, 0x00FF_FFFF, 1_000_000);
    assert!(parser.parse_item("<L [0]>").is_ok());
    assert!(matches!(
        parser.parse_item("<L [1] <B [0]>>"),
        Err(ParseError::TotalItemsExceeded {
            max_total_items: 1,
            ..
        })
    ));
}

/// Verifies the per-item byte limit rejects an oversized scalar body.
#[test]
fn item_bytes_limit_rejects_oversized_bodies() {
    let parser = limits(64, 1_000_000, 4, 1_000_000);
    assert!(parser.parse_item("<U1 [4] 1 2 3 4>").is_ok());
    assert!(matches!(
        parser.parse_item("<U1 [5] 1 2 3 4 5>"),
        Err(ParseError::ItemBytesExceeded {
            required_bytes: 5,
            max_item_bytes: 4,
            ..
        })
    ));
}

/// Verifies the direct-children limit rejects a list one child too wide.
#[test]
fn list_items_limit_rejects_wide_lists() {
    let parser = limits(64, 1_000_000, 0x00FF_FFFF, 1);
    assert!(parser.parse_item("<L [1] <B [0]>>").is_ok());
    assert!(matches!(
        parser.parse_item("<L [2] <B [0]> <B [0]>>"),
        Err(ParseError::ListItemsExceeded {
            max_list_items: 1,
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// Error-priority and formatter-contract regressions
// ---------------------------------------------------------------------------

/// Builds `depth` nested Lists wrapping `leaf`; the root List is depth 1,
/// matching the parser's and formatter's shared depth semantics.
fn nested_lists(depth: usize, leaf: SecsItem) -> SecsItem {
    let mut item = leaf;
    for _ in 0..depth {
        item = SecsItem::List(vec![item]);
    }
    item
}

/// Verifies an unknown or unsupported type word is reported before any
/// later lexical error is ever scanned.
#[test]
fn type_classification_precedes_later_scan_errors() {
    assert!(matches!(
        parse_item("<NOPE,"),
        Err(ParseError::UnknownType { name, .. }) if name == "NOPE"
    ));
    assert!(matches!(
        parse_item("<J,"),
        Err(ParseError::UnsupportedType { name, .. }) if name == "J"
    ));
}

/// Verifies the List depth limit fires before an astronomically long
/// declared count is scanned at all.
#[test]
fn depth_limit_precedes_declared_count_scanning() {
    let input = format!("<L <L [{}]", "9".repeat(40));
    let parser = limits(1, 1_000_000, 0x00FF_FFFF, 1_000_000);
    assert!(matches!(
        parser.parse_item(&input),
        Err(ParseError::DepthExceeded {
            depth: 2,
            max_depth: 1,
            ..
        })
    ));
}

/// Verifies scalar element admission (hard byte limit, then declared count)
/// precedes element value validation across all bodies.
#[test]
fn element_admission_precedes_value_validation() {
    let one_byte = limits(64, 1_000_000, 1, 1_000_000);
    assert!(matches!(
        one_byte.parse_item("<U2 65536>"),
        Err(ParseError::ItemBytesExceeded {
            required_bytes: 2,
            max_item_bytes: 1,
            ..
        })
    ));
    for text in [
        "<U1[0] 0x0100>",
        "<BOOLEAN[0] MAYBE>",
        "<A[0] 0x80>",
        "<F4[0] 1e40>",
    ] {
        assert!(
            matches!(
                parse_item(text),
                Err(ParseError::CountMismatch {
                    declared: 0,
                    actual: 1,
                    ..
                })
            ),
            "{text} must fail admission before value validation"
        );
    }
    // Category errors and value errors keep their own precedence.
    assert!(matches!(
        parse_item("<U1[0] TRUE>"),
        Err(ParseError::UnexpectedToken { .. })
    ));
    assert!(matches!(
        parse_item("<U1[1] 0x0100>"),
        Err(ParseError::IntegerOverflow { .. })
    ));
}

/// Verifies the formatter depth contract: trees within the parser's hard
/// 256 ceiling format and reparse with a matching parser; deeper trees are
/// refused outright; 65-deep trees round-trip only past the default limits.
#[test]
fn formatter_depth_contract_boundaries() {
    let leaf = SecsItem::Binary(Vec::new());

    // 65-deep: formats, default parser refuses, parser with max_depth=65
    // accepts — the documented contract boundary.
    let text_65 = SmlFormatter::new(FormatStyle::Compact)
        .format_item(&nested_lists(65, leaf.clone()))
        .expect("65-deep tree formats");
    assert!(matches!(
        parse_item(&text_65),
        Err(ParseError::DepthExceeded {
            depth: 65,
            max_depth: 64,
            ..
        })
    ));
    assert!(limits(65, 1_000_000, 0x00FF_FFFF, 1_000_000)
        .parse_item(&text_65)
        .is_ok());

    // 256-deep: the hard ceiling itself still formats and reparses.
    let text_256 = SmlFormatter::new(FormatStyle::Compact)
        .format_item(&nested_lists(256, leaf.clone()))
        .expect("256-deep tree formats");
    assert!(limits(256, 1_000_000, 0x00FF_FFFF, 1_000_000)
        .parse_item(&text_256)
        .is_ok());

    // 257-deep: no legal parser exists, so the formatter refuses — both at
    // item level and message level.
    assert_eq!(
        SmlFormatter::new(FormatStyle::Compact).format_item(&nested_lists(257, leaf.clone())),
        Err(FormatError::DepthExceeded {
            depth: 257,
            max_depth: 256,
        })
    );
    let message = SmlMessage::new(
        Stream::new(1).expect("stream 1"),
        Function::new(1),
        false,
        Some(nested_lists(257, leaf)),
    );
    assert_eq!(
        SmlFormatter::new(FormatStyle::Compact).format_message(&message),
        Err(FormatError::DepthExceeded {
            depth: 257,
            max_depth: 256,
        })
    );
}

/// Verifies the IntegerOverflow diagnostic quotes the original literal
/// spelling in its Display text, not just in the structured field.
#[test]
fn integer_overflow_display_quotes_original_literal() {
    let display = parse_item("<U1[1] 0x0100>")
        .expect_err("0x0100 does not fit U1")
        .to_string();
    assert!(display.contains("0x0100"), "display was: {display}");
    assert!(display.contains("U1"), "display was: {display}");
}
