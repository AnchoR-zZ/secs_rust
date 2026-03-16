//! SECS-II 混合负载生成器
//!
//! 提供统一口径的测试数据集，覆盖小报文、中等报文、大报文和深层嵌套四种场景，
//! confirmed with real SECS/GEM message distribution:
//!   - small  ≈ 心跳类 S1F1/S1F2，几个标量或极短 ASCII
//!   - medium ≈ 设备状态上报 S6F11，中等 ASCII + 数值混合
//!   - large  ≈ 配方/采集数据 S7F3/S14F19，含大体积 Binary 或 U1 数组
//!   - nested ≈ 多层嵌套 LIST，测试递归编解码路径

use secs_rust::secs2::Secs2;

// ──────────────────────────────── 小报文 ────────────────────────────────────

/// 模拟 S1F1 Are-You-There 类消息：单个 BOOLEAN 或几个 U2 标量
pub fn small_scalar() -> Secs2 {
    Secs2::U2(vec![1, 2, 3, 4])
}

/// 单个短 ASCII 字符串（16 字节），典型设备 ID
pub fn small_ascii() -> Secs2 {
    Secs2::ASCII("EQUIP_01".to_string())
}

// ──────────────────────────────── 中等报文 ──────────────────────────────────

/// 包含 64 个 I4 整数的数值数组，模拟采集变量
pub fn medium_i4_array() -> Secs2 {
    Secs2::I4((0_i32..64).collect())
}

/// 256 字节 ASCII 字符串，模拟配方名称或说明字段
pub fn medium_ascii() -> Secs2 {
    Secs2::ASCII("A".repeat(256))
}

/// 模拟 S6F11 Event Report —— 2 层 LIST，包含 ASCII + U4 + I2 混合
pub fn medium_event_report() -> Secs2 {
    Secs2::LIST(vec![
        Secs2::ASCII("PROCESS_COMPLETE".to_string()),
        Secs2::U4(vec![42, 100, 200]),
        Secs2::I2(vec![-1, 0, 1, 2, 3]),
    ])
}

// ──────────────────────────────── 大报文 ────────────────────────────────────

/// 8 KB Binary payload，模拟 S7/S14 配方数据块
pub fn large_binary() -> Secs2 {
    Secs2::BINARY(vec![0xAB_u8; 8 * 1024])
}

/// 4096 个 U1 元素，模拟大型传感器数据阵列
pub fn large_u1_array() -> Secs2 {
    Secs2::U1(vec![0xFF_u8; 4096])
}

/// 4096 个 F4 浮点，模拟波形/频谱数据
pub fn large_f4_array() -> Secs2 {
    Secs2::D4(vec![1.23456_f32; 4096])
}

/// 4096 个 D8 浮点，模拟波形/频谱数据
pub fn large_d8_array() -> Secs2 {
    Secs2::D8(vec![1.23456_f64; 4096])
}

// ──────────────────────────────── 边界与类型盲区 ───────────────────────────────

/// BOOLEAN 数组：parse_boolean 有独立的 bit-level 路径，此前完全未覆盖
pub fn boolean_array() -> Secs2 {
    Secs2::BOOLEAN(vec![true, false, true, false, true, true, false, false])
}

/// 负整数 I1（i8 范围）：测试符号位扩展路径，与正值 I4 互补
pub fn negative_i1() -> Secs2 {
    Secs2::I1(vec![-128, -64, -1, 0, 64, 127])
}

/// 极值 I8（i64 范围）：测试 8 字节整数的字节序转换边界
pub fn negative_i8() -> Secs2 {
    Secs2::I8(vec![i64::MIN, -1, 0, 1, i64::MAX])
}

/// EMPTY：编解码快速返回路径，代表高频空心跳包（S1F1 body）
pub fn empty_item() -> Secs2 {
    Secs2::EMPTY
}

/// 零长度 ASCII：`A[0]` 边界分支，易在优化时被意外破坏
pub fn zero_length_ascii() -> Secs2 {
    Secs2::ASCII(String::new())
}

/// 零长度 LIST：`L[0]` 边界分支，测试空列表头部编解码
pub fn zero_length_list() -> Secs2 {
    Secs2::LIST(vec![])
}

/// 碎片化混合类型 LIST：128 个交替 U2/ASCII/BOOLEAN 子项
/// 不同于 wide_list 的单一子项类型，能压榨堆分配器并触发分支预测失败
pub fn fragmented_mixed_list() -> Secs2 {
    let items = (0_u16..128)
        .map(|i| match i % 3 {
            0 => Secs2::U2(vec![i]),
            1 => Secs2::ASCII(format!("item_{i}")),
            _ => Secs2::BOOLEAN(vec![i % 2 == 0]),
        })
        .collect::<Vec<_>>();
    Secs2::LIST(items)
}

// ──────────────────────────────── 嵌套 LIST ──────────────────────────────────

/// 构造 `depth` 层深度嵌套的 LIST，每层包含一个叶节点 U1([depth as u8])
/// 用于测试递归 encode/decode 路径的栈深度和分配模式
pub fn nested_list(depth: usize) -> Secs2 {
    if depth == 0 {
        return Secs2::U1(vec![0]);
    }
    Secs2::LIST(vec![nested_list(depth - 1)])
}

/// 宽型 LIST：单层包含大量子项（256 个 U2 scalar），测试迭代路径
pub fn wide_list() -> Secs2 {
    let items = (0_u16..256)
        .map(|i| Secs2::U2(vec![i]))
        .collect::<Vec<_>>();
    Secs2::LIST(items)
}

// ──────────────────────────────── 混合负载集 ─────────────────────────────────

/// 返回所有负载类型的描述与数据对，供 benchmark 迭代
///
/// 每个元素为 `(label, payload)` 可直接传入 criterion benchmark_id
pub fn mixed_payloads() -> Vec<(&'static str, Secs2)> {
    vec![
        ("small/scalar_u2", small_scalar()),
        ("small/ascii_16", small_ascii()),
        ("medium/i4_64", medium_i4_array()),
        ("large/d8_4k", large_d8_array()),
        ("medium/ascii_256", medium_ascii()),
        ("medium/event_report", medium_event_report()),
        ("large/binary_8k", large_binary()),
        ("large/u1_4k", large_u1_array()),
        ("large/f4_4k", large_f4_array()),
        ("nested/depth_8", nested_list(8)),
        ("nested/depth_16", nested_list(16)),
        ("nested/wide_256", wide_list()),
        // 类型盲区补全
        ("boundary/boolean_8", boolean_array()),
        ("boundary/negative_i1", negative_i1()),
        ("boundary/negative_i8", negative_i8()),
        ("boundary/empty", empty_item()),
        ("boundary/zero_ascii", zero_length_ascii()),
        ("boundary/zero_list", zero_length_list()),
        ("boundary/fragmented_mixed", fragmented_mixed_list()),
    ]
}
