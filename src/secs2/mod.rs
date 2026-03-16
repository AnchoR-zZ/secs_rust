//! SECS-II 协议解析器
//!
//! 提供完整的SECS-II数据类型解析功能，支持所有SECS-II标准数据类型。

mod encoder;
mod parser;
mod types;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Secs2Error {
    #[error("SECS2_E2001 invalid format code: {code}")]
    InvalidFormatCode { code: u8 },
    #[error("SECS2_E2002 invalid length bytes count: {count}")]
    InvalidLengthBytesCount { count: u8 },
    #[error("SECS2_E2003 incomplete data in {context}: need {needed}, got {actual}")]
    IncompleteData {
        context: &'static str,
        needed: usize,
        actual: usize,
    },
    #[error("SECS2_E2004 misaligned data: total {total_bytes}, item {item_size}")]
    MisalignedData { total_bytes: usize, item_size: usize },
    #[error("SECS2_E2005 count mismatch in {context}: expected {expected}, got {actual}")]
    CountMismatch {
        context: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("SECS2_E2006 unsupported type: {name}")]
    UnsupportedType { name: &'static str },
    #[error("SECS2_E2007 data length too large: {length}")]
    LengthTooLarge { length: usize },
    #[error("SECS2_E2008 invalid data: {message}")]
    InvalidData { message: String },
}

// 导出主要类型和函数
pub use encoder::encode;
pub use Secs2Error as Error;
pub use types::Secs2;

// 导入测试模块
#[cfg(test)]
mod tests;
