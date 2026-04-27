#![allow(dead_code)]

use secs_rust::hsms::message::HsmsMessage;
use secs_rust::secs2::Secs2;
use std::f64::consts::{E, PI};

pub const SCALAR_SIZES: [usize; 7] = [10, 50, 100, 500, 1_000, 5_000, 10_000];
pub const LIST_DEPTHS: [usize; 6] = [1, 2, 3, 5, 7, 10];
pub const HSMS_BODY_SIZES: [usize; 3] = [10, 100, 1_000];

pub type Secs2Factory = fn(usize) -> Secs2;

pub fn secs2_scalar_factories() -> Vec<(&'static str, Secs2Factory)> {
    vec![
        ("ascii", ascii),
        ("binary", binary),
        ("boolean", boolean),
        ("i1", i1),
        ("i2", i2),
        ("i4", i4),
        ("i8", i8),
        ("u1", u1),
        ("u2", u2),
        ("u4", u4),
        ("u8", u8),
        ("d4", d4),
        ("d8", d8),
    ]
}

pub fn ascii(size: usize) -> Secs2 {
    let alphabet = b"SECSGEMBENCHMARKDATA";
    let text: String = (0..size)
        .map(|index| alphabet[index % alphabet.len()] as char)
        .collect();
    Secs2::ASCII(text)
}

pub fn binary(size: usize) -> Secs2 {
    let payload = (0..size).map(|index| (index % 251) as u8).collect();
    Secs2::BINARY(payload)
}

pub fn boolean(size: usize) -> Secs2 {
    let payload = (0..size).map(|index| index % 2 == 0).collect();
    Secs2::BOOLEAN(payload)
}

pub fn i1(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| ((index % 127) as i16 - 63) as i8)
        .collect();
    Secs2::I1(payload)
}

pub fn i2(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| ((index as i32 * 13) % i16::MAX as i32) as i16 - 1_024)
        .collect();
    Secs2::I2(payload)
}

pub fn i4(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| ((index as i64 * 104_729) % i32::MAX as i64) as i32 - 65_536)
        .collect();
    Secs2::I4(payload)
}

pub fn i8(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| index as i64 * 1_000_003 - 500_000)
        .collect();
    Secs2::I8(payload)
}

pub fn u1(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| (index % u8::MAX as usize) as u8)
        .collect();
    Secs2::U1(payload)
}

pub fn u2(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| ((index * 29) % u16::MAX as usize) as u16)
        .collect();
    Secs2::U2(payload)
}

pub fn u4(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| (index as u32).wrapping_mul(65_537))
        .collect();
    Secs2::U4(payload)
}

pub fn u8(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| (index as u64).wrapping_mul(4_294_967_311))
        .collect();
    Secs2::U8(payload)
}

pub fn d4(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| index as f32 * 1.25 + (index % 7) as f32 * 0.125)
        .collect();
    Secs2::D4(payload)
}

pub fn d8(size: usize) -> Secs2 {
    let payload = (0..size)
        .map(|index| index as f64 * 1.125 + (index % 11) as f64 * 0.03125)
        .collect();
    Secs2::D8(payload)
}

pub fn nested_list(depth: usize) -> Secs2 {
    fn build_level(level: usize, depth: usize) -> Secs2 {
        let mut children = Vec::with_capacity(10);

        for index in 0..10 {
            let child = if index == 0 && level + 1 < depth {
                build_level(level + 1, depth)
            } else {
                match index % 5 {
                    0 => Secs2::ASCII(format!("L{level}_I{index}")),
                    1 => Secs2::U4(vec![
                        (level * 100 + index) as u32,
                        (level * 100 + index + 1) as u32,
                    ]),
                    2 => Secs2::BOOLEAN(vec![index % 2 == 0, index % 3 == 0, true]),
                    3 => Secs2::BINARY(vec![level as u8, index as u8, (level + index) as u8]),
                    _ => Secs2::D8(vec![level as f64 + index as f64 / 10.0]),
                }
            };
            children.push(child);
        }

        Secs2::LIST(children)
    }

    build_level(0, depth.max(1))
}

pub fn secs2_real_world_cases() -> Vec<(&'static str, Secs2)> {
    vec![
        ("s1f1_are_you_there", Secs2::EMPTY),
        (
            "s1f2_online_data",
            Secs2::LIST(vec![
                Secs2::ASCII("SECS-SIMULATOR".to_string()),
                Secs2::ASCII("1.0.0".to_string()),
            ]),
        ),
        (
            "s2f13_ec_request",
            Secs2::LIST(vec![Secs2::U4((1..=32).collect())]),
        ),
        (
            "s2f14_ec_data",
            Secs2::LIST(vec![
                Secs2::LIST(vec![
                    Secs2::U4(vec![1001]),
                    Secs2::ASCII("LOT-A".to_string()),
                ]),
                Secs2::LIST(vec![Secs2::U4(vec![1002]), Secs2::BOOLEAN(vec![true])]),
                Secs2::LIST(vec![Secs2::U4(vec![1003]), Secs2::D8(vec![12.5, 18.75])]),
                Secs2::LIST(vec![
                    Secs2::U4(vec![1004]),
                    Secs2::BINARY(vec![0x12, 0x34, 0x56]),
                ]),
            ]),
        ),
        (
            "s6f11_event_report",
            Secs2::LIST(vec![
                Secs2::U4(vec![42]),
                Secs2::U4(vec![7]),
                Secs2::LIST(vec![
                    Secs2::LIST(vec![Secs2::U4(vec![1]), Secs2::ASCII("IDLE".to_string())]),
                    Secs2::LIST(vec![
                        Secs2::U4(vec![2]),
                        Secs2::BOOLEAN(vec![true, false, true]),
                    ]),
                    Secs2::LIST(vec![Secs2::U4(vec![3]), nested_list(3)]),
                ]),
            ]),
        ),
        ("complex_mixed_nested", complex_nested_mixed_structure()),
    ]
}

pub fn complex_nested_mixed_structure() -> Secs2 {
    Secs2::LIST(vec![
        Secs2::ASCII("ROOT".to_string()),
        Secs2::LIST(vec![
            Secs2::U1(vec![1, 2, 3, 4]),
            Secs2::LIST(vec![
                Secs2::BOOLEAN(vec![true, false, true, true]),
                Secs2::LIST(vec![
                    Secs2::I4(vec![-1, 0, 1, 2, 3]),
                    Secs2::LIST(vec![
                        Secs2::D8(vec![PI, E]),
                        Secs2::LIST(vec![Secs2::ASCII("L5".to_string()), nested_list(2)]),
                    ]),
                ]),
            ]),
        ]),
    ])
}

pub fn hsms_data_message(body_size: usize) -> HsmsMessage {
    HsmsMessage::build_request_data_message(0x1001, 6, 11, 0x0102_0304, binary(body_size))
}

pub fn hsms_control_messages() -> Vec<(&'static str, HsmsMessage)> {
    let select_req = HsmsMessage::select_req(0x1001, 0x1000_0001);
    let deselect_req = HsmsMessage::deselect_req(0x1001, 0x1000_0003);
    let linktest_req = HsmsMessage::linktest_req(0x1000_0002);

    vec![
        ("select_req", select_req.clone()),
        ("select_rsp", HsmsMessage::select_rsp(&select_req, 0)),
        ("deselect_req", deselect_req.clone()),
        ("deselect_rsp", HsmsMessage::deselect_rsp(&deselect_req, 0)),
        ("linktest_req", linktest_req.clone()),
        ("linktest_rsp", HsmsMessage::linktest_rsp(&linktest_req)),
        (
            "separate_req",
            HsmsMessage::separate_req(0x1001, 0x1000_0004),
        ),
    ]
}
