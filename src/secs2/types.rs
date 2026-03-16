use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Secs2 {
    LIST(Vec<Secs2>),
    BINARY(Vec<u8>),
    BOOLEAN(Vec<bool>),
    ASCII(String),
    I8(Vec<i64>),
    I1(Vec<i8>),
    I2(Vec<i16>),
    I4(Vec<i32>),
    D8(Vec<f64>),
    D4(Vec<f32>),
    U8(Vec<u64>),
    U1(Vec<u8>),
    U2(Vec<u16>),
    U4(Vec<u32>),
    EMPTY,
}

/// SECS-II 数据格式代码枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FormatCode {
    List,      // 0x00, 0 位
    Binary,    // 0x08, 8 位
    Boolean,   // 0x09, 1 位
    Ascii,     // 0x10, 8 位
    Character, // 0x12, 8 位
    I8,        // 0x18, 64 位
    I1,        // 0x19, 8 位
    I2,        // 0x1A, 16 位
    I4,        // 0x1C, 32 位
    D8,        // 0x20, 64 位
    D4,        // 0x24, 32 位
    U8,        // 0x28, 64 位
    U1,        // 0x29, 8 位
    U2,        // 0x2A, 16 位
    U4,        // 0x2C, 32 位
}

impl FormatCode {
    /// 获取格式代码值
    pub(crate) fn code(self) -> u8 {
        match self {
            FormatCode::List => 0x00,
            FormatCode::Binary => 0x08,
            FormatCode::Boolean => 0x09,
            FormatCode::Ascii => 0x10,
            FormatCode::Character => 0x12,
            FormatCode::I8 => 0x18,
            FormatCode::I1 => 0x19,
            FormatCode::I2 => 0x1A,
            FormatCode::I4 => 0x1C,
            FormatCode::D8 => 0x20,
            FormatCode::D4 => 0x24,
            FormatCode::U8 => 0x28,
            FormatCode::U1 => 0x29,
            FormatCode::U2 => 0x2A,
            FormatCode::U4 => 0x2C,
        }
    }

    // /// 获取单个元素位数
    // pub(crate) fn bit_size(self) -> u8 {
    //     match self {
    //         FormatCode::LIST => 0,
    //         FormatCode::BINARY => 8,
    //         FormatCode::BOOLEAN => 1,
    //         FormatCode::ASCII => 8,
    //         FormatCode::CHARACTER => 8,
    //         FormatCode::I8 => 64,
    //         FormatCode::I1 => 8,
    //         FormatCode::I2 => 16,
    //         FormatCode::I4 => 32,
    //         FormatCode::D8 => 64,
    //         FormatCode::D4 => 32,
    //         FormatCode::U8 => 64,
    //         FormatCode::U1 => 8,
    //         FormatCode::U2 => 16,
    //         FormatCode::U4 => 32,
    //     }
    // }

    /// 从格式代码创建 FormatCode 枚举
    pub(crate) fn from_u8(code: u8) -> Option<Self> {
        match code {
            0x00 => Some(FormatCode::List),
            0x08 => Some(FormatCode::Binary),
            0x09 => Some(FormatCode::Boolean),
            0x10 => Some(FormatCode::Ascii),
            0x12 => Some(FormatCode::Character),
            0x18 => Some(FormatCode::I8),
            0x19 => Some(FormatCode::I1),
            0x1A => Some(FormatCode::I2),
            0x1C => Some(FormatCode::I4),
            0x20 => Some(FormatCode::D8),
            0x24 => Some(FormatCode::D4),
            0x28 => Some(FormatCode::U8),
            0x29 => Some(FormatCode::U1),
            0x2A => Some(FormatCode::U2),
            0x2C => Some(FormatCode::U4),
            _ => None,
        }
    }
}
