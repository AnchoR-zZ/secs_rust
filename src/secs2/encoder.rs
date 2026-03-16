use crate::secs2::{Secs2Error, types::{FormatCode, Secs2}};

/// 编码SECS2数据为字节数组
pub fn encode(data: &Secs2) -> Result<Vec<u8>, Secs2Error> {
    match data {
        Secs2::LIST(items) => encode_list(items),
        Secs2::BINARY(bytes) => encode_binary(bytes),
        Secs2::BOOLEAN(values) => encode_boolean(values),
        Secs2::ASCII(string) => encode_ascii(string),
        Secs2::I1(values) => encode_i1(values),
        Secs2::I2(values) => encode_item(values, FormatCode::I2),
        Secs2::I4(values) => encode_item(values, FormatCode::I4),
        Secs2::I8(values) => encode_item(values, FormatCode::I8),
        Secs2::U1(values) => encode_u1(values),
        Secs2::U2(values) => encode_item(values, FormatCode::U2),
        Secs2::U4(values) => encode_item(values, FormatCode::U4),
        Secs2::U8(values) => encode_item(values, FormatCode::U8),
        Secs2::D4(values) => encode_item(values, FormatCode::D4),
        Secs2::D8(values) => encode_item(values, FormatCode::D8),
        Secs2::EMPTY => Ok(vec![]),
    }
}

/// 构建SECS-II格式字节和长度字段
pub(crate) fn build_header(
    format_code: u8,
    length: usize,
) -> Result<Vec<u8>, Secs2Error> {
    if length > 0xFFFFFF {
        return Err(Secs2Error::LengthTooLarge { length });
    }

    // 计算长度字节数：1-3字节
    let length_bytes = match length {
        0..=0xFF => 1,
        0x100..=0xFFFF => 2,
        _ => 3,
    };

    // 格式字节：格式代码(高6位) + 长度字节数(低2位)
    let format_byte = (format_code << 2) | (length_bytes & 0x03);

    // 编码长度为big-endian
    let length_data = match length_bytes {
        1 => vec![length as u8],
        2 => vec![(length >> 8) as u8, length as u8],
        3 => vec![(length >> 16) as u8, (length >> 8) as u8, length as u8],
        _ => return Err(Secs2Error::InvalidLengthBytesCount { count: length_bytes }),
    };

    // 组合：格式字节 + 长度字段
    let mut header = vec![format_byte];
    header.extend_from_slice(&length_data);
    Ok(header)
}

/// LIST编码 - 递归调用主函数
fn encode_list(items: &[Secs2]) -> Result<Vec<u8>, Secs2Error> {
    let mut encoded_items = Vec::new();

    // 编码所有子项
    for item in items {
        let encoded = encode(item)?;
        encoded_items.push(encoded);
    }

    // 构建LIST头部
    let header = build_header(FormatCode::List.code(), items.len())?;

    // 组合结果
    let mut result = header;
    for encoded_item in encoded_items {
        result.extend_from_slice(&encoded_item);
    }

    Ok(result)
}

/// 简单数据类型编码
fn encode_binary(data: &[u8]) -> Result<Vec<u8>, Secs2Error> {
    let header = build_header(FormatCode::Binary.code(), data.len())?;
    let mut result = header;
    result.extend_from_slice(data);
    Ok(result)
}

fn encode_ascii(text: &str) -> Result<Vec<u8>, Secs2Error> {
    let data = text.as_bytes();
    let header = build_header(FormatCode::Ascii.code(), data.len())?;
    let mut result = header;
    result.extend_from_slice(data);
    Ok(result)
}

fn encode_boolean(values: &[bool]) -> Result<Vec<u8>, Secs2Error> {
    let header = build_header(FormatCode::Boolean.code(), values.len())?;

    let mut result = header;
    for &value in values {
        result.push(value as u8);
    }
    Ok(result)
}

/// 整数编码 - 有符号
fn encode_i1(values: &[i8]) -> Result<Vec<u8>, Secs2Error> {
    let data_size = values.len();
    let header = build_header(FormatCode::I1.code(), data_size)?;

    let mut result = header;
    for &value in values {
        result.push(value as u8);
    }
    Ok(result)
}

fn encode_u1(values: &[u8]) -> Result<Vec<u8>, Secs2Error> {
    let header = build_header(FormatCode::U1.code(), values.len())?;
    let mut result = header;
    result.extend_from_slice(values);
    Ok(result)
}

trait SecsItemEncoder {
    fn encode_to_vec(&self, result: &mut Vec<u8>);
    fn byte_size() -> usize;
}

macro_rules! impl_secs_item_encoder {
    ($($t:ty),*) => {
        $(
            impl SecsItemEncoder for $t {
                fn encode_to_vec(&self, result: &mut Vec<u8>) {
                    result.extend_from_slice(&self.to_be_bytes());
                }
                fn byte_size() -> usize {
                    std::mem::size_of::<$t>()
                }
            }
        )*
    };
}

impl_secs_item_encoder!(i16, i32, i64, u16, u32, u64, f32, f64);

fn encode_item<T: SecsItemEncoder>(
    values: &[T],
    code: FormatCode,
) -> Result<Vec<u8>, Secs2Error> {
    let data_size = values.len() * T::byte_size();
    let header = build_header(code.code(), data_size)?;

    let mut result = header;
    // 预分配空间优化性能
    result.reserve(data_size);

    for value in values {
        value.encode_to_vec(&mut result);
    }
    Ok(result)
}
