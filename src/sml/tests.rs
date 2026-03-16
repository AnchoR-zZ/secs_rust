use crate::secs2::Secs2;
use super::parser::{parse_sml, SmlMessage};

#[test]
fn test_simple_header_wait() {
    let input = "S1F1 W .";
    let (rem, msg) = parse_sml(input).unwrap();
    assert_eq!(rem.trim(), "");
    assert_eq!(msg, SmlMessage {
        stream: 1,
        function: 1,
        wait_bit: true,
        body: None,
    });
}

#[test]
fn test_simple_header_no_wait() {
    let input = "S1F13 .";
    let (rem, msg) = parse_sml(input).unwrap();
    assert_eq!(rem.trim(), "");
    assert_eq!(msg, SmlMessage {
        stream: 1,
        function: 13,
        wait_bit: false,
        body: None,
    });
}

#[test]
fn test_ascii_item() {
    let input = "S1F1 <A \"Hello World\"> .";
    let (_, msg) = parse_sml(input).unwrap();
    if let Some(Secs2::ASCII(s)) = msg.body {
        assert_eq!(s, "Hello World");
    } else {
        panic!("Expected ASCII body");
    }
}

#[test]
fn test_nested_list() {
    let input = r#"
        S1F1 W
        <L
            <A "MDLN">
            <L
                <U1 1>
                <U2 200>
            >
        >
        .
    "#;
    let (_, msg) = parse_sml(input).unwrap();
    match msg.body {
        Some(Secs2::LIST(items)) => {
            assert_eq!(items.len(), 2);
            match &items[0] {
                Secs2::ASCII(s) => assert_eq!(s, "MDLN"),
                _ => panic!("Expected ASCII at index 0"),
            }
            match &items[1] {
                Secs2::LIST(sub_items) => {
                    assert_eq!(sub_items.len(), 2);
                    match &sub_items[0] {
                        Secs2::U1(v) => assert_eq!(v[0], 1),
                        _ => panic!("Expected U1"),
                    }
                }
                _ => panic!("Expected List at index 1"),
            }
        }
        _ => panic!("Expected List body"),
    }
}

#[test]
fn test_numeric_types() {
    let input = "S2F2 <U4 100 200> .";
    let (_, msg) = parse_sml(input).unwrap();
    if let Some(Secs2::U4(v)) = msg.body {
        assert_eq!(v, vec![100, 200]);
    } else {
        panic!("Expected U4");
    }
}

#[test]
fn test_binary_type() {
    let input = "S2F2 <B 0x00 0xFF> .";
    let (_, msg) = parse_sml(input).unwrap();
    if let Some(Secs2::BINARY(v)) = msg.body {
        assert_eq!(v, vec![0x00, 0xFF]);
    } else {
        panic!("Expected BINARY");
    }
}

#[test]
fn test_boolean_type() {
    let input = "S2F2 <Boolean T F> .";
    let (_, msg) = parse_sml(input).unwrap();
    if let Some(Secs2::BOOLEAN(v)) = msg.body {
        assert_eq!(v, vec![true, false]);
    } else {
        panic!("Expected BOOLEAN");
    }
}
