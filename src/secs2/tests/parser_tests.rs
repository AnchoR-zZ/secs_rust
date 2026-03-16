//! Parser相关测试
//! 包含SECS-II解析器的各种测试用例

use crate::secs2::types::*;

#[test]
fn test_parse_item_long() {
    // === 正常情况测试 ===

    // 测试 1 字节情况
    assert_eq!(Secs2::parse_item_long(&[0x00], 1).unwrap(), 0); // 最小值
    assert_eq!(Secs2::parse_item_long(&[0xFF], 1).unwrap(), 255); // 最大值

    // 测试 2 字节情况
    assert_eq!(Secs2::parse_item_long(&[0x00, 0x00], 2).unwrap(), 0); // 最小值
    assert_eq!(Secs2::parse_item_long(&[0xFF, 0xFF], 2).unwrap(), 65535); // 最大值
    assert_eq!(Secs2::parse_item_long(&[0x12, 0x34], 2).unwrap(), 0x1234); // 大端序验证

    // 测试 3 字节情况
    assert_eq!(Secs2::parse_item_long(&[0x00, 0x00, 0x00], 3).unwrap(), 0); // 最小值
    assert_eq!(
        Secs2::parse_item_long(&[0xFF, 0xFF, 0xFF], 3).unwrap(),
        16777215
    ); // 最大值
    assert_eq!(
        Secs2::parse_item_long(&[0x12, 0x34, 0x56], 3).unwrap(),
        0x123456
    ); // 大端序验证

    // === 错误情况测试 ===

    // 数据不足测试
    assert!(Secs2::parse_item_long(&[], 1).is_err()); // 需要1字节但无数据
    assert!(Secs2::parse_item_long(&[0x12], 2).is_err()); // 需要2字节但只有1字节
    assert!(Secs2::parse_item_long(&[0x12, 0x34], 3).is_err()); // 需要3字节但只有2字节

    // 无效 length_bytes_count 测试
    assert!(Secs2::parse_item_long(&[0x00], 4).is_err()); // 超出范围
    assert!(Secs2::parse_item_long(&[0x00], 255).is_err()); // 无效值
}

#[test]
fn test_list() {
    // 测试空LIST
    {
        let bytes = [
            0x01, // 格式字节: LIST + 1字节长度
            0x00, // 长度: 0个元素
        ];
        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::LIST(items) => assert_eq!(items.len(), 0),
            _ => panic!("Expected LIST type"),
        }
    }

    // 测试包含ASCII元素的LIST
    {
        let bytes = [
            0x01, // 格式字节: LIST + 1字节长度
            0x01, // 长度: 1个元素
            0x41, // ASCII格式字节 + 1字节长度
            0x05, // 长度: 5个字符
            0x48, 0x45, 0x4C, 0x4C, 0x4F, // "HELLO"
        ];

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::LIST(items) => {
                assert_eq!(items.len(), 1);
                match &items[0] {
                    Secs2::ASCII(s) => assert_eq!(s, "HELLO"),
                    _ => panic!("Expected ASCII element"),
                }
            }
            _ => panic!("Expected LIST type"),
        }
    }

    // 测试多字节长度的LIST
    {
        let mut bytes = vec![0x01]; // 格式字节: LIST + 1字节长度
        bytes.extend_from_slice(&[0x02]); // 长度: 2个元素

        // 两个ASCII元素
        bytes.extend_from_slice(&[0x41, 0x01, 0x41]); // "A"
        bytes.extend_from_slice(&[0x41, 0x01, 0x42]); // "B"

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::LIST(items) => {
                assert_eq!(items.len(), 2);
                match &items[0] {
                    Secs2::ASCII(s) => assert_eq!(s, "A"),
                    _ => panic!("Expected ASCII element"),
                }
                match &items[1] {
                    Secs2::ASCII(s) => assert_eq!(s, "B"),
                    _ => panic!("Expected ASCII element"),
                }
            }
            _ => panic!("Expected LIST type"),
        }
    }
}

#[test]
fn test_ascii() {
    // 测试用户提供的例子: "T1 HIGH" (7个字符)
    {
        let bytes = [
            0x41, // 格式字节: ASCII + 1字节长度
            0x07, // 长度: 7个字符
            0x54, 0x31, 0x20, 0x48, 0x49, 0x47, 0x48, // "T1 HIGH"
        ];

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::ASCII(s) => assert_eq!(s, "T1 HIGH"),
            _ => panic!("Expected ASCII type"),
        }
    }

    // 测试空字符串
    {
        let bytes = [
            0x41, // 格式字节: ASCII + 1字节长度
            0x00, // 长度: 0个字符
        ];
        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::ASCII(s) => assert_eq!(s, ""),
            _ => panic!("Expected ASCII type"),
        }
    }

    // 测试单字符
    {
        let bytes = [
            0x41, // 格式字节: ASCII + 1字节长度
            0x01, // 长度: 1个字符
            0x41, // 'A'
        ];
        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::ASCII(s) => assert_eq!(s, "A"),
            _ => panic!("Expected ASCII type"),
        }
    }

    // 测试多字节长度
    {
        let mut data = vec![0x42]; // 格式字节: ASCII + 2字节长度
        data.extend_from_slice(&[0x01, 0x00]); // 长度: 256个字符
        data.extend(vec![0x41; 256]); // 'A' * 256

        let result = Secs2::decode(&data).unwrap().unwrap();
        match result {
            Secs2::ASCII(s) => {
                assert_eq!(s.len(), 256);
                assert!(s.chars().all(|c| c == 'A'));
            }
            _ => panic!("Expected ASCII type"),
        }
    }

    // 测试长字符串（需要2字节长度）
    {
        let long_string = "A".repeat(300);
        let mut bytes = vec![
            0x42, // 格式字节: ASCII + 2字节长度
            0x01, 0x2C, // 长度: 300个字符
        ];
        bytes.extend_from_slice(long_string.as_bytes());

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::ASCII(s) => assert_eq!(s, long_string),
            _ => panic!("Expected ASCII type"),
        }
    }
}

#[test]
fn test_boolean() {
    // 测试用户提供的例子: 25 02 FF 00 (一个true和一个false)
    {
        let bytes = [
            0x25, // 格式字节: BOOLEAN + 1字节长度
            0x02, // 长度: 2个boolean
            0xFF, // true
            0x00, // false
        ];

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BOOLEAN(values) => assert_eq!(values, vec![true, false]),
            _ => panic!("Expected BOOLEAN type"),
        }
    }

    // 测试空boolean数组
    {
        let bytes = [
            0x25, // 格式字节: BOOLEAN + 1字节长度
            0x00, // 长度: 0个boolean
        ];
        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BOOLEAN(values) => assert_eq!(values.len(), 0),
            _ => panic!("Expected BOOLEAN type"),
        }
    }

    // 测试多个boolean（覆盖true/false和混合情况）
    {
        let bytes = [
            0x25, // 格式字节: BOOLEAN + 1字节长度
            0x05, // 长度: 5个boolean
            0xFF, // true
            0x00, // false
            0x01, // true (non-zero)
            0x80, // true (non-zero)
            0xFF, // true
        ];

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BOOLEAN(b) => {
                assert_eq!(b.len(), 5);
                assert_eq!(b, [true, false, true, true, true]);
            }
            _ => panic!("Expected BOOLEAN type"),
        }
    }

    // 测试多字节长度字段
    {
        let mut bytes = vec![0x26]; // 格式字节: BOOLEAN + 2字节长度
        bytes.extend_from_slice(&[0x00, 0x10]); // 长度: 16个boolean
        bytes.extend(vec![0xFF; 16]); // 16个boolean，都设为true

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BOOLEAN(b) => {
                assert_eq!(b.len(), 16);
                assert!(b.iter().all(|&v| v));
            }
            _ => panic!("Expected BOOLEAN type"),
        }
    }

    // 测试长boolean数组（需要2字节长度）
    {
        let boolean_count = 300;
        let mut bytes = vec![
            0x26, // 格式字节: BOOLEAN + 2字节长度
            0x01, 0x2C, // 长度: 300个boolean
        ];
        for i in 0..boolean_count {
            bytes.push(if i % 2 == 0 { 0xFF } else { 0x00 }); // 交替true/false
        }

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BOOLEAN(values) => {
                assert_eq!(values.len(), boolean_count);
                for (i, &val) in values.iter().enumerate() {
                    assert_eq!(val, i % 2 == 0);
                }
            }
            _ => panic!("Expected BOOLEAN type"),
        }
    }
}

#[test]
fn test_binary() {
    // 测试用户提供的例子: 包含一个二进制代码 10101010
    {
        let bytes = [
            0x21, // 格式字节: BINARY + 1字节长度
            0x01, // 长度: 1个字节
            0xAA, // 二进制数据: 10101010
        ];

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BINARY(data) => assert_eq!(data, vec![0xAA]),
            _ => panic!("Expected BINARY type"),
        }
    }

    // 测试空二进制数据
    {
        let bytes = [
            0x21, // 格式字节: BINARY + 1字节长度
            0x00, // 长度: 0个字节
        ];
        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BINARY(data) => assert_eq!(data.len(), 0),
            _ => panic!("Expected BINARY type"),
        }
    }

    // 测试单字节数据
    {
        let bytes = [
            0x21, // 格式字节: BINARY + 1字节长度
            0x01, // 长度: 1个字节
            0xFF, // 数据: 255
        ];
        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BINARY(data) => assert_eq!(data, vec![0xFF]),
            _ => panic!("Expected BINARY type"),
        }
    }

    // 测试多字节数据
    {
        let test_data = vec![0x01, 0x02, 0x03, 0x04, 0x05];
        let mut bytes = vec![
            0x21, // 格式字节: BINARY + 1字节长度
            0x05, // 长度: 5个字节
        ];
        bytes.extend_from_slice(&test_data);

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BINARY(data) => assert_eq!(data, test_data),
            _ => panic!("Expected BINARY type"),
        }
    }

    // 测试长二进制数据（需要2字节长度）
    {
        let binary_size = 1000;
        let test_data: Vec<u8> = (0..binary_size).map(|x| (x % 256) as u8).collect();
        let mut bytes = vec![
            0x22, // 格式字节: BINARY + 2字节长度
            0x03, 0xE8, // 长度: 1000个字节
        ];
        bytes.extend_from_slice(&test_data);

        let result = Secs2::decode(&bytes).unwrap().unwrap();
        match result {
            Secs2::BINARY(data) => {
                assert_eq!(data.len(), binary_size);
                for (i, &val) in data.iter().enumerate() {
                    assert_eq!(val, (i % 256) as u8);
                }
            }
            _ => panic!("Expected BINARY type"),
        }
    }
}

#[test]
fn test_empty_decode() {
    // 测试空字节数组解析为EMPTY
    let bytes = [];
    let result = Secs2::decode(&bytes).unwrap();

    match result {
        Some(Secs2::EMPTY) => {} // 期望的结果
        Some(other) => panic!("Expected EMPTY, got: {:?}", other),
        None => panic!("Expected Some(EMPTY), got None"),
    }
}

#[test]
fn test_empty_roundtrip() {
    // 测试EMPTY编解码往返
    let empty = Secs2::EMPTY;

    // 编码
    let encoded = crate::secs2::encoder::encode(&empty).unwrap();
    assert_eq!(encoded, vec![]); // EMPTY应该编码为空字节数组

    // 解码
    let decoded = Secs2::decode(&encoded).unwrap().unwrap();
    match decoded {
        Secs2::EMPTY => {} // 期望的结果
        other => panic!("Expected EMPTY, got: {:?}", other),
    }
}
