//! SML (SECS Message Language) 格式化器
//!
//! 将 `Secs2` 数据结构转换为 SML 文本格式。

use crate::secs2::Secs2;
use crate::hsms::message::{HsmsMessage, MessageType};

/// SML 格式化样式
#[derive(Debug, Clone, Copy)]
pub enum FormatStyle {
    /// 紧凑格式（单行）
    Compact,
    /// 美化格式（多行，带缩进）
    Pretty,
}

/// SML 格式化器
pub struct SmlFormatter {
    style: FormatStyle,
}

impl SmlFormatter {
    /// 创建新的格式化器
    pub fn new(style: FormatStyle) -> Self {
        Self { style }
    }

    /// 格式化 Secs2 数据为 SML 字符串
    pub fn format(&self, data: &Secs2) -> String {
        match self.style {
            FormatStyle::Compact => self.format_compact(data),
            FormatStyle::Pretty => self.format_pretty(data, 0),
        }
    }

    /// 格式化 HSMS 消息为 SML 字符串
    pub fn format_hsms(&self, msg: &HsmsMessage) -> String {
        let header_str = match msg.header.s_type {
            MessageType::Data => {
                let w_str = if msg.header.w_bit { " W" } else { "" };
                format!("S{}F{}{}", msg.header.stream, msg.header.function, w_str)
            }
            // 对于控制消息，显示类型
            msg_type => format!("{:?}", msg_type),
        };

        let body_str = self.format(&msg.body);
        
        if body_str.is_empty() {
            format!("{}.", header_str)
        } else {
            match self.style {
                FormatStyle::Compact => format!("{} {}.", header_str, body_str),
                FormatStyle::Pretty => format!("{}\n{}.", header_str, body_str),
            }
        }
    }

    /// 紧凑格式（单行）
    fn format_compact(&self, data: &Secs2) -> String {
        match data {
            Secs2::EMPTY => String::new(),

            Secs2::ASCII(s) => format!(r#"<A "{}">"#, escape_string(s)),

            Secs2::LIST(items) => {
                if items.is_empty() {
                    "<L>".to_string()
                } else {
                    let items_str: Vec<String> = items.iter()
                        .map(|item| self.format_compact(item))
                        .collect();
                    format!("<L {}>", items_str.join(" "))
                }
            }

            Secs2::BINARY(bytes) => {
                if bytes.is_empty() {
                    "<B>".to_string()
                } else {
                    let bytes_str: Vec<String> = bytes.iter()
                        .map(|b| format!("0x{:02X}", b))
                        .collect();
                    format!("<B {}>", bytes_str.join(" "))
                }
            }

            Secs2::BOOLEAN(bools) => {
                if bools.is_empty() {
                    "<Boolean>".to_string()
                } else {
                    let bools_str: Vec<&str> = bools.iter()
                        .map(|b| if *b { "T" } else { "F" })
                        .collect();
                    format!("<Boolean {}>", bools_str.join(" "))
                }
            }

            // 数值类型
            Secs2::U1(nums) => self.format_numeric("U1", nums),
            Secs2::U2(nums) => self.format_numeric("U2", nums),
            Secs2::U4(nums) => self.format_numeric("U4", nums),
            Secs2::U8(nums) => self.format_numeric("U8", nums),
            Secs2::I1(nums) => self.format_numeric("I1", nums),
            Secs2::I2(nums) => self.format_numeric("I2", nums),
            Secs2::I4(nums) => self.format_numeric("I4", nums),
            Secs2::I8(nums) => self.format_numeric("I8", nums),
            Secs2::D4(nums) => self.format_numeric("F4", nums),
            Secs2::D8(nums) => self.format_numeric("F8", nums),
        }
    }

    /// 美化格式（多行，带缩进）
    fn format_pretty(&self, data: &Secs2, depth: usize) -> String {
        let indent = "  ".repeat(depth);
        match data {
            Secs2::EMPTY => String::new(),

            Secs2::ASCII(s) => format!(r#"<A "{}">"#, escape_string(s)),

            Secs2::LIST(items) => {
                if items.is_empty() {
                    "<L>".to_string()
                } else {
                    let mut result = "<L\n".to_string();
                    for item in items {
                        let item_str = self.format_pretty(item, depth + 1);
                        result.push_str(&format!("{}  {}\n", indent, item_str));
                    }
                    result.push_str(&format!("{}>", indent));
                    result
                }
            }

            // 其他类型使用紧凑格式
            _ => self.format_compact(data),
        }
    }

    /// 格式化数值数组
    fn format_numeric<T: std::fmt::Display>(
        &self,
        type_name: &str,
        values: &[T],
    ) -> String {
        if values.is_empty() {
            format!("<{}>", type_name)
        } else {
            let values_str: Vec<String> = values.iter()
                .map(|v| format!("{}", v))
                .collect();
            format!("<{} {}>", type_name, values_str.join(" "))
        }
    }
}

/// 转义字符串中的特殊字符
fn escape_string(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

impl Default for SmlFormatter {
    fn default() -> Self {
        Self::new(FormatStyle::Compact)
    }
}

/// 便捷函数：将 Secs2 格式化为紧凑的 SML 字符串
pub fn to_sml_compact(data: &Secs2) -> String {
    SmlFormatter::new(FormatStyle::Compact).format(data)
}

/// 便捷函数：将 Secs2 格式化为美化的 SML 字符串
pub fn to_sml_secs(data: &Secs2) -> String {
    SmlFormatter::new(FormatStyle::Pretty).format(data)
}

/// 便捷函数：将 HSMS 消息格式化为美化的 SML 字符串
pub fn to_sml_hsms(msg: &HsmsMessage) -> String {
    SmlFormatter::new(FormatStyle::Pretty).format_hsms(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_ascii() {
        let data = Secs2::ASCII("Hello".to_string());
        assert_eq!(to_sml_compact(&data), r#"<A "Hello">"#);
    }

    #[test]
    fn test_format_empty() {
        let data = Secs2::EMPTY;
        assert_eq!(to_sml_compact(&data), "");
    }

    #[test]
    fn test_format_list_compact() {
        let data = Secs2::LIST(vec![
            Secs2::ASCII("MDLN".to_string()),
            Secs2::LIST(vec![Secs2::U1(vec![1]), Secs2::U2(vec![200])]),
        ]);
        let result = to_sml_compact(&data);
        assert!(result.contains("<L"));
        assert!(result.contains(r#"<A "MDLN">"#));
        assert!(result.contains("<U1 1>"));
        assert!(result.contains("<U2 200>"));
    }

    #[test]
    fn test_format_numeric() {
        let data = Secs2::U4(vec![100, 200]);
        assert_eq!(to_sml_compact(&data), "<U4 100 200>");
    }

    #[test]
    fn test_format_binary() {
        let data = Secs2::BINARY(vec![0x00, 0xFF]);
        assert_eq!(to_sml_compact(&data), "<B 0x00 0xFF>");
    }

    #[test]
    fn test_format_boolean() {
        let data = Secs2::BOOLEAN(vec![true, false]);
        assert_eq!(to_sml_compact(&data), "<Boolean T F>");
    }

    #[test]
    fn test_format_empty_list() {
        let data = Secs2::LIST(vec![]);
        assert_eq!(to_sml_compact(&data), "<L>");
    }

    #[test]
    fn test_format_escape_string() {
        let data = Secs2::ASCII("Hello\n\"World\"".to_string());
        assert_eq!(to_sml_compact(&data), r#"<A "Hello\n\"World\"">"#);
    }

    #[test]
    fn test_format_hsms_data() {
        let body = Secs2::LIST(vec![
            Secs2::ASCII("MDLN".to_string()),
            Secs2::ASCII("EP".to_string()),
        ]);
        // session_id: 0, stream: 1, function: 13, system_bytes: 0, body, w_bit: true
        let msg = HsmsMessage::build_data_message(0, 1, 13, 0, body, true);
        let sml = to_sml_hsms(&msg);
        assert!(sml.contains("S1F13 W"));
        assert!(sml.contains("<L"));
        assert!(sml.contains(r#"<A "MDLN">"#));
        assert!(sml.trim().ends_with("."));
    }

    #[test]
    fn test_format_hsms_control() {
        use crate::hsms::message::HsmsHeader;
        let header = HsmsHeader {
            session_id: 1,
            stream: 0,
            function: 0,
            p_type: 0,
            s_type: MessageType::SelectReq,
            system_bytes: 1,
            w_bit: false,
        };
        let msg = HsmsMessage {
            header,
            body: Secs2::EMPTY,
        };
        let sml = to_sml_hsms(&msg);
        assert_eq!(sml, "SelectReq.");
    }
}
