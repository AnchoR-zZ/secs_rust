use crate::secs2::{Secs2Error, types::{FormatCode, Secs2}};

/// decode
impl Secs2 {
    pub fn decode(bytes: &[u8]) -> Result<Option<Secs2>, Secs2Error> {
        let (result, _) = Self::parse(bytes)?;
        Ok(result)
    }

    /// 输入bytes，返回解析后的Secs2对象和已使用的字节数
    fn parse(bytes: &[u8]) -> Result<(Option<Secs2>, usize), Secs2Error> {
        if bytes.is_empty() {
            return Ok((Some(Secs2::EMPTY), 0));
        }
        if bytes.len() < 2 {
            return Err(Secs2Error::IncompleteData {
                context: "message",
                needed: 2,
                actual: bytes.len(),
            });
        }

        // 解析头部
        let (format_code, length_bytes_count) = Self::parse_head(bytes[0])?;

        // 解析数据部分
        let (result, bytes_offset) = match format_code {
            FormatCode::List => Self::parse_list(&bytes[1..], length_bytes_count)?,
            FormatCode::Binary => Self::parse_binary(&bytes[1..], length_bytes_count)?,
            FormatCode::Boolean => Self::parse_boolean(&bytes[1..], length_bytes_count)?,
            FormatCode::Ascii => Self::parse_ascii(&bytes[1..], length_bytes_count)?,
            FormatCode::Character => Self::parse_character(&bytes[1..], length_bytes_count)?,
            FormatCode::I8 => {
                Self::parse_numeric::<i64>(&bytes[1..], length_bytes_count, Secs2::I8)?
            }
            FormatCode::I1 => {
                Self::parse_numeric::<i8>(&bytes[1..], length_bytes_count, Secs2::I1)?
            }
            FormatCode::I2 => {
                Self::parse_numeric::<i16>(&bytes[1..], length_bytes_count, Secs2::I2)?
            }
            FormatCode::I4 => {
                Self::parse_numeric::<i32>(&bytes[1..], length_bytes_count, Secs2::I4)?
            }
            FormatCode::D8 => {
                Self::parse_numeric::<f64>(&bytes[1..], length_bytes_count, Secs2::D8)?
            }
            FormatCode::D4 => {
                Self::parse_numeric::<f32>(&bytes[1..], length_bytes_count, Secs2::D4)?
            }
            FormatCode::U8 => {
                Self::parse_numeric::<u64>(&bytes[1..], length_bytes_count, Secs2::U8)?
            }
            FormatCode::U1 => {
                Self::parse_numeric::<u8>(&bytes[1..], length_bytes_count, Secs2::U1)?
            }
            FormatCode::U2 => {
                Self::parse_numeric::<u16>(&bytes[1..], length_bytes_count, Secs2::U2)?
            }
            FormatCode::U4 => {
                Self::parse_numeric::<u32>(&bytes[1..], length_bytes_count, Secs2::U4)?
            }
        };

        // offset+1是因为头部占用了1个字节
        Ok((Some(result), bytes_offset + 1))
    }

    fn parse_head(byte: u8) -> Result<(FormatCode, u8), Secs2Error> {
        let format_code = match FormatCode::from_u8((byte >> 2) & 0x3F) {
            Some(v) => v,
            None => {
                return Err(Secs2Error::InvalidFormatCode {
                    code: (byte >> 2) & 0x3F,
                });
            }
        }; // 取高6位作为item
        let length_bytes_count = byte & 0x03; // 取低2位作为bytes长度

        Ok((format_code, length_bytes_count))
    }

    pub(crate) fn parse_item_long(
        bytes: &[u8],
        length_bytes_count: u8,
    ) -> Result<usize, Secs2Error> {
        // 根据SEMI规范，字节长度至少为1
        match length_bytes_count {
            0 => Err(Secs2Error::InvalidLengthBytesCount { count: 0 }),
            1 => {
                if bytes.is_empty() {
                    return Err(Secs2Error::IncompleteData {
                        context: "length bytes",
                        needed: 1,
                        actual: bytes.len(),
                    });
                }
                Ok(bytes[0] as usize)
            }
            2 => {
                if bytes.len() < 2 {
                    return Err(Secs2Error::IncompleteData {
                        context: "length bytes",
                        needed: 2,
                        actual: bytes.len(),
                    });
                }
                Ok(((bytes[0] as usize) << 8) | (bytes[1] as usize))
            }
            3 => {
                if bytes.len() < 3 {
                    return Err(Secs2Error::IncompleteData {
                        context: "length bytes",
                        needed: 3,
                        actual: bytes.len(),
                    });
                }
                Ok(((bytes[0] as usize) << 16) | ((bytes[1] as usize) << 8) | (bytes[2] as usize))
            }
            _ => Err(Secs2Error::InvalidLengthBytesCount {
                count: length_bytes_count,
            }),
        }
    }

    /// 解析LIST类型
    fn parse_list(
        bytes: &[u8],
        length_bytes_count: u8,
    ) -> Result<(Secs2, usize), Secs2Error> {
        // 解析元素数量
        let element_count = Self::parse_item_long(bytes, length_bytes_count)?;

        // 跳过长度字段，定位到数据开始
        let data_start = length_bytes_count as usize;
        let mut current_pos = data_start;
        let mut items = Vec::new();

        // 循环解析每个元素
        for _ in 0..element_count {
            if current_pos >= bytes.len() {
                return Err(Secs2Error::IncompleteData {
                    context: "list elements",
                    needed: current_pos + 1,
                    actual: bytes.len(),
                });
            }

            let (item, bytes_used) = Self::parse(&bytes[current_pos..])?;
            let item = item.ok_or_else(|| Secs2Error::InvalidData {
                message: "解析LIST元素失败".to_string(),
            })?;
            items.push(item);
            current_pos += bytes_used;
        }

        let total_bytes = current_pos;
        Ok((Secs2::LIST(items), total_bytes))
    }

    /// 解析BINARY类型
    fn parse_binary(
        bytes: &[u8],
        length_bytes_count: u8,
    ) -> Result<(Secs2, usize), Secs2Error> {
        // 解析binary元素数量（字节数）
        let binary_count = Self::parse_item_long(bytes, length_bytes_count)?;

        // 每个binary元素占用1个字节
        let bytes_needed = binary_count;

        // 计算数据开始位置和总字节数
        let data_start = length_bytes_count as usize;
        let total_bytes_needed = data_start + bytes_needed;

        // 检查数据完整性
        if bytes.len() < total_bytes_needed {
            return Err(Secs2Error::IncompleteData {
                context: "binary",
                needed: total_bytes_needed,
                actual: bytes.len(),
            });
        }

        // 提取数据字节
        let data_bytes = &bytes[data_start..total_bytes_needed];

        // 直接复制原始二进制数据
        let binary_data: Vec<u8> = data_bytes.to_vec();

        // 确保解析的binary数量正确
        if binary_data.len() != binary_count {
            return Err(Secs2Error::CountMismatch {
                context: "binary",
                expected: binary_count,
                actual: binary_data.len(),
            });
        }

        // 返回结果
        Ok((Secs2::BINARY(binary_data), total_bytes_needed))
    }

    /// 解析BOOLEAN类型
    fn parse_boolean(
        bytes: &[u8],
        length_bytes_count: u8,
    ) -> Result<(Secs2, usize), Secs2Error> {
        // 解析boolean元素数量
        let boolean_count = Self::parse_item_long(bytes, length_bytes_count)?;

        // 每个boolean占用1个字节
        let bytes_needed = boolean_count;

        // 计算数据开始位置和总字节数
        let data_start = length_bytes_count as usize;
        let total_bytes_needed = data_start + bytes_needed;

        // 检查数据完整性
        if bytes.len() < total_bytes_needed {
            return Err(Secs2Error::IncompleteData {
                context: "boolean",
                needed: total_bytes_needed,
                actual: bytes.len(),
            });
        }

        // 提取数据字节
        let data_bytes = &bytes[data_start..total_bytes_needed];

        // 解析boolean值：每个字节对应一个boolean，0xFF=true，0x00=false
        // 注意：SECS-II标准规定非零即为真，这里放宽判断条件
        let booleans: Vec<bool> = data_bytes.iter().map(|&byte| byte != 0).collect();

        // 确保解析的boolean数量正确
        if booleans.len() != boolean_count {
            return Err(Secs2Error::CountMismatch {
                context: "boolean",
                expected: boolean_count,
                actual: booleans.len(),
            });
        }

        // 返回结果
        Ok((Secs2::BOOLEAN(booleans), total_bytes_needed))
    }

    /// 解析ASCII类型
    fn parse_ascii(
        bytes: &[u8],
        length_bytes_count: u8,
    ) -> Result<(Secs2, usize), Secs2Error> {
        // 解析字符数量
        let char_count = Self::parse_item_long(bytes, length_bytes_count)?;

        // 计算数据开始位置和需要的总字节数
        let data_start = length_bytes_count as usize;
        let total_bytes_needed = data_start + char_count;

        // 检查数据完整性
        if bytes.len() < total_bytes_needed {
            return Err(Secs2Error::IncompleteData {
                context: "ascii",
                needed: total_bytes_needed,
                actual: bytes.len(),
            });
        }

        // 提取ASCII字节并转换为字符串
        let ascii_bytes = &bytes[data_start..total_bytes_needed];
        // 使用lossy转换以避免panic，SECS-II ASCII应为7位，但容忍扩展ASCII
        let ascii_string = String::from_utf8_lossy(ascii_bytes).to_string();

        // 返回结果
        Ok((Secs2::ASCII(ascii_string), total_bytes_needed))
    }

    /// 解析CHARACTER类型
    fn parse_character(
        _bytes: &[u8],
        _length_bytes_count: u8,
    ) -> Result<(Secs2, usize), Secs2Error> {
        Err(Secs2Error::UnsupportedType { name: "Character" })
    }

    /// 泛型数值解析函数
    fn parse_numeric<T>(
        bytes: &[u8],
        length_bytes_count: u8,
        constructor: fn(Vec<T>) -> Secs2,
    ) -> Result<(Secs2, usize), Secs2Error>
    where
        T: SecsItemParser,
    {
        // 解析总字节数
        let total_bytes = Self::parse_item_long(bytes, length_bytes_count)?;
        let item_size = T::byte_size();

        // 检查对齐
        if total_bytes % item_size != 0 {
            return Err(Secs2Error::MisalignedData {
                total_bytes,
                item_size,
            });
        }
        let count = total_bytes / item_size;

        // 计算数据开始位置和总字节数
        let data_start = length_bytes_count as usize;
        let total_bytes_needed = data_start + total_bytes;

        // 检查数据完整性
        if bytes.len() < total_bytes_needed {
            return Err(Secs2Error::IncompleteData {
                context: "numeric",
                needed: total_bytes_needed,
                actual: bytes.len(),
            });
        }

        // 提取数据字节并解析
        let data_bytes = &bytes[data_start..total_bytes_needed];
        let mut values = Vec::with_capacity(count);

        for chunk in data_bytes.chunks_exact(item_size) {
            values.push(T::from_be_bytes_slice(chunk));
        }

        // 确保解析的数量正确
        if values.len() != count {
            return Err(Secs2Error::CountMismatch {
                context: "numeric",
                expected: count,
                actual: values.len(),
            });
        }

        // 返回结果
        Ok((constructor(values), total_bytes_needed))
    }
}

trait SecsItemParser {
    fn from_be_bytes_slice(bytes: &[u8]) -> Self;
    fn byte_size() -> usize;
}

macro_rules! impl_secs_item_parser {
    ($($t:ty),*) => {
        $(
            impl SecsItemParser for $t {
                fn from_be_bytes_slice(bytes: &[u8]) -> Self {
                    // 我们确定bytes长度正确，因为是chunks_exact产生的
                    let mut arr = [0u8; std::mem::size_of::<$t>()];
                    arr.copy_from_slice(bytes);
                    Self::from_be_bytes(arr)
                }
                fn byte_size() -> usize {
                    std::mem::size_of::<$t>()
                }
            }
        )*
    };
}

impl_secs_item_parser!(i8, u8, i16, i32, i64, u16, u32, u64, f32, f64);
