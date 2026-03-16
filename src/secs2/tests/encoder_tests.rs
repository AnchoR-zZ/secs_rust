use crate::secs2::encoder;
use crate::secs2::types::{FormatCode, Secs2};

/// 完整的往返测试：编码 → 解码 → 重新编码
/// 验证两次编码的结果完全一致，确保编码器和解码器的完全兼容性
fn test_complete_roundtrip(original_data: &Secs2) {
    // 第一次编码
    let first_encoded = encoder::encode(original_data).unwrap();

    // 解码
    let decoded_option = Secs2::decode(&first_encoded).unwrap();
    let decoded = decoded_option.expect("解码失败，返回None");

    // 第二次编码（将解码后的数据重新编码）
    let second_encoded = encoder::encode(&decoded).unwrap();

    // 验证两次编码的结果完全一致
    assert_eq!(
        first_encoded, second_encoded,
        "往返测试失败：两次编码结果不一致\n第一次编码: {:?}\n第二次编码: {:?}",
        first_encoded, second_encoded
    );

    // 额外验证：解码后的数据内容与原始数据相同（对于简单类型）
    // 这个验证在复杂嵌套结构中可能需要特殊处理
    verify_data_equivalence(original_data, &decoded);
}

/// 验证两个Secs2数据的等价性
fn verify_data_equivalence(original: &Secs2, decoded: &Secs2) {
    match (original, decoded) {
        (Secs2::ASCII(orig), Secs2::ASCII(dec)) => {
            assert_eq!(orig, dec, "ASCII字符串内容不匹配");
        }
        (Secs2::BINARY(orig), Secs2::BINARY(dec)) => {
            assert_eq!(orig, dec, "二进制数据内容不匹配");
        }
        (Secs2::BOOLEAN(orig), Secs2::BOOLEAN(dec)) => {
            assert_eq!(orig, dec, "布尔数组内容不匹配");
        }
        (Secs2::I1(orig), Secs2::I1(dec)) => {
            assert_eq!(orig, dec, "I1数组内容不匹配");
        }
        (Secs2::I2(orig), Secs2::I2(dec)) => {
            assert_eq!(orig, dec, "I2数组内容不匹配");
        }
        (Secs2::I4(orig), Secs2::I4(dec)) => {
            assert_eq!(orig, dec, "I4数组内容不匹配");
        }
        (Secs2::I8(orig), Secs2::I8(dec)) => {
            assert_eq!(orig, dec, "I8数组内容不匹配");
        }
        (Secs2::U1(orig), Secs2::U1(dec)) => {
            assert_eq!(orig, dec, "U1数组内容不匹配");
        }
        (Secs2::U2(orig), Secs2::U2(dec)) => {
            assert_eq!(orig, dec, "U2数组内容不匹配");
        }
        (Secs2::U4(orig), Secs2::U4(dec)) => {
            assert_eq!(orig, dec, "U4数组内容不匹配");
        }
        (Secs2::U8(orig), Secs2::U8(dec)) => {
            assert_eq!(orig, dec, "U8数组内容不匹配");
        }
        (Secs2::D4(orig), Secs2::D4(dec)) => {
            // 浮点数比较需要特殊处理，考虑精度问题
            assert_eq!(orig.len(), dec.len(), "D4数组长度不匹配");
            for (i, (&orig_val, &dec_val)) in orig.iter().zip(dec.iter()).enumerate() {
                // 处理无穷大和NaN的特殊情况
                if orig_val.is_infinite() && dec_val.is_infinite() {
                    assert_eq!(
                        orig_val.is_sign_positive(),
                        dec_val.is_sign_positive(),
                        "D4数组第{}个元素符号不匹配: {} vs {}",
                        i,
                        orig_val,
                        dec_val
                    );
                } else if orig_val.is_nan() && dec_val.is_nan() {
                    // NaN与NaN比较，总是相等
                } else {
                    assert!(
                        (orig_val - dec_val).abs() < f32::EPSILON,
                        "D4数组第{}个元素不匹配: {} vs {}",
                        i,
                        orig_val,
                        dec_val
                    );
                }
            }
        }
        (Secs2::D8(orig), Secs2::D8(dec)) => {
            // 双精度浮点数比较
            assert_eq!(orig.len(), dec.len(), "D8数组长度不匹配");
            for (i, (&orig_val, &dec_val)) in orig.iter().zip(dec.iter()).enumerate() {
                // 处理无穷大和NaN的特殊情况
                if orig_val.is_infinite() && dec_val.is_infinite() {
                    assert_eq!(
                        orig_val.is_sign_positive(),
                        dec_val.is_sign_positive(),
                        "D8数组第{}个元素符号不匹配: {} vs {}",
                        i,
                        orig_val,
                        dec_val
                    );
                } else if orig_val.is_nan() && dec_val.is_nan() {
                    // NaN与NaN比较，总是相等
                } else {
                    assert!(
                        (orig_val - dec_val).abs() < f64::EPSILON,
                        "D8数组第{}个元素不匹配: {} vs {}",
                        i,
                        orig_val,
                        dec_val
                    );
                }
            }
        }
        (Secs2::LIST(orig), Secs2::LIST(dec)) => {
            assert_eq!(orig.len(), dec.len(), "LIST长度不匹配");
            for (orig_item, dec_item) in orig.iter().zip(dec.iter()) {
                verify_data_equivalence(orig_item, dec_item);
            }
        }
        (Secs2::EMPTY, Secs2::EMPTY) => {
            // EMPTY类型总是相等的，无需额外验证
        }
        _ => panic!(
            "数据类型不匹配: original={:?}, decoded={:?}",
            original, decoded
        ),
    }
}

#[test]
fn test_build_header_single_byte() {
    use crate::secs2::encoder::build_header;
    let header = build_header(FormatCode::Ascii.code(), 5).unwrap();
    assert_eq!(header, vec![0x41, 0x05]); // 0x10<<2 | 1, length 5
}

#[test]
fn test_build_header_two_bytes() {
    use crate::secs2::encoder::build_header;
    let header = build_header(FormatCode::Binary.code(), 300).unwrap();
    assert_eq!(header, vec![0x22, 0x01, 0x2C]); // 0x08<<2 | 2, length 300
}

#[test]
fn test_build_header_three_bytes() {
    use crate::secs2::encoder::build_header;
    let header = build_header(FormatCode::List.code(), 0x10000).unwrap();
    assert_eq!(header, vec![0x03, 0x01, 0x00, 0x00]); // 0x00<<2 | 3, length 65536
}

#[test]
fn test_encode_ascii() {
    let data = Secs2::ASCII("Hello".to_string());
    let encoded = encoder::encode(&data).unwrap();

    // 验证头部
    assert_eq!(encoded[0], 0x41); // ASCII format byte
    assert_eq!(encoded[1], 0x05); // Length 5

    // 验证数据
    assert_eq!(&encoded[2..], b"Hello");
}

#[test]
fn test_encode_binary() {
    let data = Secs2::BINARY(vec![0x01, 0x02, 0x03]);
    let encoded = encoder::encode(&data).unwrap();

    // 验证头部
    assert_eq!(encoded[0], 0x21); // Binary format byte
    assert_eq!(encoded[1], 0x03); // Length 3

    // 验证数据
    assert_eq!(&encoded[2..], &[0x01, 0x02, 0x03]);
}

#[test]
fn test_encode_boolean() {
    let data = Secs2::BOOLEAN(vec![true, false, true]);
    let encoded = encoder::encode(&data).unwrap();

    // 验证头部
    assert_eq!(encoded[0], 0x25); // Boolean format byte
    assert_eq!(encoded[1], 0x03); // Length 3

    // 验证数据
    assert_eq!(&encoded[2..], &[0x01, 0x00, 0x01]);
}

#[test]
fn test_encode_i1() {
    let data = Secs2::I1(vec![1, -2, 3]);
    let encoded = encoder::encode(&data).unwrap();

    // 验证头部
    assert_eq!(encoded[0], 0x65); // I1 format byte
    assert_eq!(encoded[1], 0x03); // Length 3

    // 验证数据
    assert_eq!(&encoded[2..], &[0x01, 0xFE, 0x03]);
}

#[test]
fn test_encode_u1() {
    let data = Secs2::U1(vec![1, 2, 255]);
    let encoded = encoder::encode(&data).unwrap();

    // 验证头部
    assert_eq!(encoded[0], 0xA5); // U1 format byte
    assert_eq!(encoded[1], 0x03); // Length 3

    // 验证数据
    assert_eq!(&encoded[2..], &[0x01, 0x02, 0xFF]);
}

#[test]
fn test_encode_i2() {
    let data = Secs2::I2(vec![10, -20]);
    let encoded = encoder::encode(&data).unwrap();
    // Header: I2(0x1A) -> 0x69. Length: 2 items * 2 bytes = 4 bytes.
    // Expected: 0x69 0x04 0x00 0x0A 0xFF 0xEC
    assert_eq!(encoded, vec![0x69, 0x04, 0x00, 0x0A, 0xFF, 0xEC]);
}

#[test]
fn test_encode_list() {
    let inner = Secs2::I2(vec![10]);
    let list = Secs2::LIST(vec![inner]);

    let encoded = encoder::encode(&list).unwrap();

    // Check list length byte (index 1)
    // List format: 0x00 -> 0x01.
    // Length should be 1 (item count), NOT 4 (byte size of inner item)
    assert_eq!(
        encoded[1], 1,
        "List length should be item count (1), but found {}",
        encoded[1]
    );

    // Full verification
    // Header: 0x01 0x01
    // Item: 0x69 0x04 0x00 0x0A ... wait, previous test said I2(10) is 4 bytes?
    // I2(10) is 2 bytes data + 2 bytes header = 4 bytes total? No, header is 2 bytes (1 byte format + 1 byte length)
    // I2(10): Format 0x69, Len 0x02, Data 0x00 0x0A. Total 4 bytes.
    // So list content is 4 bytes.
    // List header: 0x01 0x01.
    // Total: 0x01 0x01 0x69 0x02 0x00 0x0A.
    assert_eq!(encoded, vec![0x01, 0x01, 0x69, 0x02, 0x00, 0x0A]);
}

#[test]
fn test_encode_empty_list() {
    let list = Secs2::LIST(vec![]);
    let encoded = encoder::encode(&list).unwrap();
    // Header: 0x01 0x00.
    assert_eq!(encoded, vec![0x01, 0x00]);
}

#[test]
fn test_encode_empty() {
    let data = Secs2::EMPTY;
    let encoded = encoder::encode(&data).unwrap();
    assert_eq!(encoded, vec![]);
}

#[test]
fn test_list_with_empty() {
    // A list containing an empty item?
    // EMPTY usually encodes to 0 bytes.
    // If a list has EMPTY item, it just has ... nothing?
    // Standard doesn't really have "EMPTY" item type unless it's a specific type with length 0.
    // Our Secs2::EMPTY produces 0 bytes.
    let list = Secs2::LIST(vec![Secs2::EMPTY]);
    let encoded = encoder::encode(&list).unwrap();

    // Header: 0x01 0x01 (1 item)
    // Content: empty
    // Result: 0x01 0x01
    // But this might be invalid for parser if it expects data?
    // Parser loop: for _ in 0..1 { parse }
    // parse(&[]) returns EMPTY, 0 bytes.
    // So it works?
    assert_eq!(encoded, vec![0x01, 0x01]);
}

#[test]
fn test_encode_d4() {
    let data = Secs2::D4(vec![1.5]);
    let encoded = encoder::encode(&data).unwrap();
    // D4(0x24) -> 0x91. Length 4.
    // 1.5 in f32 is 0x3FC00000
    assert_eq!(encoded, vec![0x91, 0x04, 0x3F, 0xC0, 0x00, 0x00]);
}

#[test]
fn test_encode_d8() {
    let data = Secs2::D8(vec![1.5]);
    let encoded = encoder::encode(&data).unwrap();
    // D8(0x20) -> 0x81. Length 8.
    // 1.5 in f64 is 0x3FF8000000000000
    assert_eq!(
        encoded,
        vec![0x81, 0x08, 0x3F, 0xF8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]
    );
}

// Roundtrip tests
#[test]
fn test_complete_roundtrip_ascii() {
    test_complete_roundtrip(&Secs2::ASCII("Hello World".to_string()));
}

#[test]
fn test_complete_roundtrip_binary() {
    test_complete_roundtrip(&Secs2::BINARY(vec![1, 2, 3, 4, 5]));
}

#[test]
fn test_complete_roundtrip_u1() {
    test_complete_roundtrip(&Secs2::U1(vec![0, 128, 255]));
}

#[test]
fn test_complete_roundtrip_i1() {
    test_complete_roundtrip(&Secs2::I1(vec![-128, 0, 127]));
}

#[test]
fn test_complete_roundtrip_u2() {
    test_complete_roundtrip(&Secs2::U2(vec![0, 65535]));
}

#[test]
fn test_complete_roundtrip_u4() {
    test_complete_roundtrip(&Secs2::U4(vec![0, 123456789]));
}

#[test]
fn test_complete_roundtrip_u8() {
    test_complete_roundtrip(&Secs2::U8(vec![0, 1234567890123456789]));
}

#[test]
fn test_complete_roundtrip_i8() {
    test_complete_roundtrip(&Secs2::I8(vec![-1234567890123456789, 1234567890123456789]));
}

#[test]
fn test_complete_roundtrip_d4() {
    test_complete_roundtrip(&Secs2::D4(vec![1.23, -4.56]));
}

#[test]
fn test_complete_roundtrip_d8() {
    test_complete_roundtrip(&Secs2::D8(vec![1.23456789, -9.87654321]));
}

#[test]
fn test_empty_roundtrip() {
    // Empty types
    test_complete_roundtrip(&Secs2::LIST(vec![]));
    test_complete_roundtrip(&Secs2::ASCII("".to_string()));
    test_complete_roundtrip(&Secs2::BINARY(vec![]));
}
