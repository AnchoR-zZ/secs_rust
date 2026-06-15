use crate::hsms::HsmsError;
use crate::secs2::Secs2;
use std::io::{Error, ErrorKind};
use serde::Serialize;
use tokio_util::{
    bytes::Buf,
    bytes::BytesMut,
    codec::{Decoder, Encoder},
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HsmsMessage {
    pub header: HsmsHeader,
    pub body: Secs2,
}

impl HsmsMessage {
    pub fn select_req(session_id: u16, system_bytes: u32) -> Self{
        let header = HsmsHeader {
            session_id,
            stream: 0,
            function: 0,
            p_type: 0,
            s_type: MessageType::SelectReq,
            system_bytes,
            w_bit: false,
        };

        HsmsMessage {
            header,
            body: crate::secs2::Secs2::EMPTY,
        }
    }

    pub fn select_rsp(req: &HsmsMessage, status: u8) -> Self {
        let header = HsmsHeader {
            session_id: req.header.session_id,
            stream: 0,
            function: status,
            p_type: 0,
            s_type: MessageType::SelectRsp,
            system_bytes: req.header.system_bytes,
            w_bit: false,
        };

        HsmsMessage {
            header,
            body: crate::secs2::Secs2::EMPTY,
        }
    }

    pub fn deselect_req(session_id: u16, system_bytes: u32) -> Self {
        let header = HsmsHeader {
            session_id,
            stream: 0,
            function: 0,
            p_type: 0,
            s_type: MessageType::DeselectReq,
            system_bytes,
            w_bit: false,
        };

        HsmsMessage {
            header,
            body: crate::secs2::Secs2::EMPTY,
        }
    }

    pub fn deselect_rsp(req: &HsmsMessage, status: u8) -> Self {
        let header = HsmsHeader {
            session_id: req.header.session_id,
            stream: 0,
            function: status,
            p_type: 0,
            s_type: MessageType::DeselectRsp,
            system_bytes: req.header.system_bytes,
            w_bit: false,
        };

        HsmsMessage {
            header,
            body: crate::secs2::Secs2::EMPTY,
        }
    }

    pub fn linktest_req(system_bytes: u32) -> Self {
        let header = HsmsHeader {
            session_id: 0xFFFF,
            stream: 0,
            function: 0,
            p_type: 0,
            s_type: MessageType::LinktestReq,
            system_bytes,
            w_bit: false,
        };

        HsmsMessage {
            header,
            body: crate::secs2::Secs2::EMPTY,
        }
    }

    pub fn linktest_rsp(req: &HsmsMessage) -> Self {
        let header = HsmsHeader {
            session_id: 0xFFFF,
            stream: 0,
            function: 0,
            p_type: 0,
            s_type: MessageType::LinktestRsp,
            system_bytes: req.header.system_bytes,
            w_bit: false,
        };

        HsmsMessage {
            header,
            body: crate::secs2::Secs2::EMPTY,
        }
    }

    pub fn separate_req(session_id: u16, system_bytes: u32) -> Self {
        let header = HsmsHeader {
            session_id,
            stream: 0,
            function: 0,
            p_type: 0,
            s_type: MessageType::SeparateReq,
            system_bytes,
            w_bit: false,
        };

        HsmsMessage {
            header,
            body: crate::secs2::Secs2::EMPTY,
        }
    }

    pub fn new_reject(rejected_message: &HsmsMessage, reason: u8) -> Self {
        let header = HsmsHeader {
            session_id: rejected_message.header.session_id,
            stream: rejected_message.header.s_type as u8,
            function: reason,
            p_type: 0,
            s_type: MessageType::RejectReq,
            system_bytes: rejected_message.header.system_bytes,
            w_bit: false,
        };

        HsmsMessage {
            header,
            body: crate::secs2::Secs2::EMPTY,
        }
    }

    pub fn build_data_message(
        session_id: u16,
        stream: u8,
        function: u8,
        system_bytes: u32,
        body: Secs2,
        w_bit: bool, // 新增参数表示是否期望回复
    ) -> Self {
        let header = HsmsHeader {
            session_id,
            stream: stream & 0x7F, // 确保不超过7位
            function,
            p_type: 0, // SECS-II 标准消息
            s_type: MessageType::Data,
            system_bytes,
            w_bit,
        };

        HsmsMessage { header, body }
    }

    // 便捷方法：不需要回复的数据消息
    pub fn build_unidirectional_data_message(
        session_id: u16,
        stream: u8,
        function: u8,
        system_bytes: u32,
        body: Secs2,
    ) -> Self {
        Self::build_data_message(session_id, stream, function, system_bytes, body, false)
    }

    // 便捷方法：需要回复的数据消息
    pub fn build_request_data_message(
        session_id: u16,
        stream: u8,
        function: u8,
        system_bytes: u32,
        body: Secs2,
    ) -> Self {
        Self::build_data_message(session_id, stream, function, system_bytes, body, true)
    }

    pub fn build_reply_message(
        &self,
        stream: u8,
        function: u8,
        body: Secs2
    ) -> Self {
        let header = HsmsHeader {
            session_id: self.header.session_id,
            stream,
            function,
            p_type: self.header.p_type,
            s_type: MessageType::Data,
            system_bytes: self.header.system_bytes, 
            w_bit: false,
        };

        HsmsMessage { header, body }
    }
}

/// 单条 HSMS 消息的默认长度上限（含 10 字节 header，不含 4 字节 length 前缀）。
/// 16 MiB 远超任何真实 SECS-II 消息（单 item 的 length 字段本身最多 3 字节即 16 MiB），
/// 用于防御 corrupted length 前缀（对端 bug / 线缆噪声 / fuzz）导致的内存放大。
const DEFAULT_MAX_MSG_LEN: usize = 1 << 24; // 16 MiB

/// HSMS 消息编解码器。
///
/// `max_msg_len` 为单条消息（header + body）的字节上限；为 0 时使用内部默认 16 MiB。
/// 超过上限的 length 前缀会在首次 decode 即被拒绝（返回 `InvalidData`），
/// 避免读缓冲随对端慢速灌入的数据单调增长直至 OOM。
pub struct HsmsMessageCodec {
    max_msg_len: usize,
}

impl Default for HsmsMessageCodec {
    fn default() -> Self {
        Self {
            max_msg_len: DEFAULT_MAX_MSG_LEN,
        }
    }
}

impl HsmsMessageCodec {
    /// 构造指定上限的 codec；`max_msg_len` 为 0 时使用默认 16 MiB。
    pub fn new(max_msg_len: usize) -> Self {
        Self {
            max_msg_len: if max_msg_len == 0 {
                DEFAULT_MAX_MSG_LEN
            } else {
                max_msg_len
            },
        }
    }

    #[inline]
    fn effective_max(&self) -> usize {
        if self.max_msg_len == 0 {
            DEFAULT_MAX_MSG_LEN
        } else {
            self.max_msg_len
        }
    }
}

impl Decoder for HsmsMessageCodec {
    type Item = HsmsMessage;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // 1. 检查 src 长度是否 >= 4 (Length Bytes)
        if src.len() < 4 {
            return Ok(None);
        }

        // 2. 读取前 4 字节解析为 msg_len (大端序)
        let msg_len = ((src[0] as u32) << 24)
            | ((src[1] as u32) << 16)
            | ((src[2] as u32) << 8)
            | (src[3] as u32);

        // 2.1 长度上限校验：超限立刻拒绝，避免缓冲随慢速灌入数据无限增长
        let max = self.effective_max();
        if msg_len as usize > max {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("消息长度 {} 超过上限 {}", msg_len, max),
            ));
        }

        // 3. 检查 src 剩余长度是否 >= msg_len
        if src.len() < 4 + msg_len as usize {
            return Ok(None);
        }

        // 4. split_to(4 + msg_len) 取出完整消息数据
        let mut msg_data = src.split_to(4 + msg_len as usize);

        // 丢弃长度字段，保留消息头和消息体
        msg_data.advance(4);

        // 5. 解析 Header (10 bytes)
        if msg_data.len() < 10 {
            return Err(Error::new(ErrorKind::UnexpectedEof, "消息头数据不足"));
        }

        let header = HsmsHeader::decode(&msg_data[..10])
            .map_err(|e| Error::new(ErrorKind::InvalidData, format!("解析HSMS头部失败: {}", e)))?;

        // 6. 解析 Body (SECS-II)
        msg_data.advance(10); // 跳过头部的10字节
        let body = Secs2::decode(&msg_data)
            .map_err(|e| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!("解析SECS-II消息体失败: {}", e),
                )
            })?
            .ok_or_else(|| Error::new(ErrorKind::InvalidData, "SECS-II消息体解码失败"))?;

        // 7. 返回 Ok(Some(msg))
        Ok(Some(HsmsMessage { header, body }))
    }
}

impl Encoder<HsmsMessage> for HsmsMessageCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: HsmsMessage, dst: &mut BytesMut) -> Result<(), Self::Error> {
        // 1. 编码SECS-II消息体
        let body_bytes = crate::secs2::encode(&item.body).map_err(|e| {
            Error::new(
                ErrorKind::InvalidData,
                format!("编码SECS-II消息体失败: {}", e),
            )
        })?;

        // 2. 计算长度
        let body_length = body_bytes.len();
        let total_length = 10 + body_length; // 10字节头部 + 消息体长度

        // 3. 预分配足够空间，避免多次扩容 (性能优化)
        let total_size = 4 + 10 + body_length; // 4字节长度字段 + 10字节头部 + 消息体长度
        dst.reserve(total_size);

        // 4. 写入4字节总长度（大端序）
        dst.extend_from_slice(&(total_length as u32).to_be_bytes());

        // 5. 写入10字节HSMS头部
        let header_bytes = item.header.encode();
        dst.extend_from_slice(&header_bytes);

        // 6. 写入SECS-II消息体
        dst.extend_from_slice(&body_bytes);

        Ok(())
    }
}

// HSMS 消息类型 (Header Byte 5)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub enum MessageType {
    Data = 0,
    SelectReq = 1,
    SelectRsp = 2,
    DeselectReq = 3,
    DeselectRsp = 4,
    LinktestReq = 5,
    LinktestRsp = 6,
    RejectReq = 7,
    SeparateReq = 9,
}

// HSMS 消息头 (10 Bytes)
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HsmsHeader {
    pub session_id: u16,
    pub stream: u8,
    pub function: u8,
    pub p_type: u8, // Usually 0 for SECS-II
    pub s_type: MessageType,
    pub system_bytes: u32, // Transaction ID
    pub w_bit: bool,       // W-Bit (Need Reply)
}

impl HsmsHeader {
    /// 编码HSMS头部为10字节数组（大端序）
    pub fn encode(&self) -> [u8; 10] {
        let mut header = [0u8; 10];

        // Session ID (bytes 0-1, big-endian)
        header[0] = (self.session_id >> 8) as u8;
        header[1] = self.session_id as u8;

        if self.s_type == MessageType::Data {
            let stream_with_w = (self.w_bit as u8) << 7 | (self.stream & 0x7F);
            header[2] = stream_with_w;
        } else if self.s_type == MessageType::RejectReq {
            header[2] = self.stream & 0x7F;
        } else {
            header[2] = 0;
        }

        // Function (byte 3)
        header[3] = self.function;

        // P-Type (byte 4)
        header[4] = self.p_type;

        // S-Type (byte 5)
        header[5] = self.s_type as u8;

        // System Bytes (bytes 6-9, big-endian)
        header[6] = (self.system_bytes >> 24) as u8;
        header[7] = (self.system_bytes >> 16) as u8;
        header[8] = (self.system_bytes >> 8) as u8;
        header[9] = self.system_bytes as u8;

        header
    }

    /// 从字节数组解码HSMS头部
    pub fn decode(bytes: &[u8]) -> Result<Self, HsmsError> {
        if bytes.len() < 10 {
            return Err(HsmsError::Protocol {
                message: "头部数据长度不足10字节".to_string(),
            });
        }

        let session_id = ((bytes[0] as u16) << 8) | (bytes[1] as u16);
        let p_type = bytes[4];

        // 先解析 SType 来确定如何处理字节 2 和 3
        let s_type = match bytes[5] {
            0 => MessageType::Data,
            1 => MessageType::SelectReq,
            2 => MessageType::SelectRsp,
            3 => MessageType::DeselectReq,
            4 => MessageType::DeselectRsp,
            5 => MessageType::LinktestReq,
            6 => MessageType::LinktestRsp,
            7 => MessageType::RejectReq,
            9 => MessageType::SeparateReq,
            _ => {
                return Err(HsmsError::Protocol {
                    message: format!("无效的消息类型: {}", bytes[5]),
                });
            }
        };

        let (stream, function, w_bit) = if s_type == MessageType::Data && p_type == 0 {
            let stream_with_w = bytes[2];
            let w_bit = (stream_with_w & 0x80) != 0;
            let stream = stream_with_w & 0x7F;
            let function = bytes[3];
            (stream, function, w_bit)
        } else if s_type == MessageType::RejectReq && p_type == 0 {
            let stream = bytes[2] & 0x7F;
            let function = bytes[3];
            (stream, function, false)
        } else {
            (0, bytes[3], false)
        };

        let system_bytes = ((bytes[6] as u32) << 24)
            | ((bytes[7] as u32) << 16)
            | ((bytes[8] as u32) << 8)
            | (bytes[9] as u32);

        Ok(HsmsHeader {
            session_id,
            stream,
            function,
            p_type,
            s_type,
            system_bytes,
            w_bit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::bytes::BytesMut;

    #[test]
    fn test_decode_given_binary() {
        // 完整二进制数据: 00 00 00 11 00 00 01 01 00 00 00 00 00 01 41 05 48 45 4C 4C 4F
        // 注意: 字节 2 = 0x01 表示 Stream=1, W-bit=0 (不期望回复的数据消息)
        let binary_data = [
            0x00, 0x00, 0x00, 0x11, // 总长度: 17字节
            0x00, 0x00, // Session ID: 0x0000
            0x01, // Stream=1, W-bit=0 (0x01 & 0x7F = 1, W-bit = 0)
            0x01, // Function: 1
            0x00, // P-Type: 0
            0x00, // S-Type: 0 (Data message)
            0x00, 0x00, 0x00, 0x01, // System Bytes: 0x00000001
            0x41, 0x05, // 消息体部分
            0x48, 0x45, 0x4C, 0x4C, 0x4F, // "HELLO" ASCII数据
        ];

        let mut src = BytesMut::from(&binary_data[..]);
        let mut codec = HsmsMessageCodec::default();

        let result = codec.decode(&mut src);

        assert!(result.is_ok());
        let message = result.unwrap().unwrap();

        // 验证头部
        assert_eq!(message.header.session_id, 0); // 0x0000
        assert_eq!(message.header.stream, 1); // Stream = 0x01 & 0x7F = 1
        assert!(!message.header.w_bit); // W-bit = (0x01 & 0x80) != 0 = false
        assert_eq!(message.header.function, 1); // 0x01
        assert_eq!(message.header.p_type, 0); // 0x00
        assert_eq!(message.header.s_type, MessageType::Data); // 0x00
        assert_eq!(message.header.system_bytes, 1); // 0x00000001

        // 验证消息体
        match &message.body {
            Secs2::ASCII(s) => assert_eq!(s, "HELLO"),
            _ => panic!("Expected ASCII SECS-II type"),
        }
    }

    #[test]
    fn test_encode_known_message() {
        // 构建已知的HSMS消息
        let original_message = HsmsMessage {
            header: HsmsHeader {
                session_id: 0,
                stream: 1,
                function: 1,
                p_type: 0,
                s_type: MessageType::Data,
                system_bytes: 16645,
                w_bit: false, // 期望回复的主消息
            },
            body: Secs2::ASCII("HELLO".to_string()),
        };

        let mut dst = BytesMut::new();
        let mut codec = HsmsMessageCodec::default();

        let result = codec.encode(original_message.clone(), &mut dst);
        assert!(result.is_ok());

        // 验证编码结果
        let expected_bytes = [
            0x00, 0x00, 0x00, 0x11, // 总长度: 17字节
            0x00, 0x00, // Session ID: 0x0000
            0x01, // Stream: 1
            0x01, // Function: 1
            0x00, // P-Type: 0
            0x00, // S-Type: 0
            0x00, 0x00, 0x41, 0x05, // System Bytes: 16645 (0x4105)
            0x41, 0x05, // 消息体部分
            0x48, 0x45, 0x4C, 0x4C, 0x4F, // "HELLO" ASCII数据
        ];

        assert_eq!(&dst[..], &expected_bytes[..]);
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        // 构建测试消息
        let original_message = HsmsMessage {
            header: HsmsHeader {
                session_id: 65535,                // 0xFFFF
                stream: 0,                        // 控制消息中stream无意义
                function: 0,                      // LinktestReq 中为 0
                p_type: 0,                        // SECS-II
                s_type: MessageType::LinktestReq, // 5
                system_bytes: 4294967295,         // 0xFFFFFFFF
                w_bit: false,                     // 控制消息不使用 w_bit
            },
            body: Secs2::ASCII("ROUNDTRIP TEST".to_string()),
        };

        // 编码
        let mut buffer = BytesMut::new();
        let mut codec = HsmsMessageCodec::default();
        codec.encode(original_message.clone(), &mut buffer).unwrap();

        // 解码
        let decoded_message = codec.decode(&mut buffer).unwrap().unwrap();

        // 验证往返一致性
        assert_eq!(
            original_message.header.session_id,
            decoded_message.header.session_id
        );
        assert_eq!(
            original_message.header.stream,
            decoded_message.header.stream
        );
        assert_eq!(
            original_message.header.function,
            decoded_message.header.function
        );
        assert_eq!(
            original_message.header.p_type,
            decoded_message.header.p_type
        );
        assert_eq!(
            original_message.header.s_type,
            decoded_message.header.s_type
        );
        assert_eq!(
            original_message.header.system_bytes,
            decoded_message.header.system_bytes
        );

        // 验证消息体
        match (&original_message.body, &decoded_message.body) {
            (Secs2::ASCII(orig), Secs2::ASCII(decoded)) => assert_eq!(orig, decoded),
            _ => panic!("Message body types don't match"),
        }
    }

    #[test]
    fn test_encode_reject_message_header() {
        let header = HsmsHeader {
            session_id: 0xFFFF,
            stream: 3,
            function: 4,
            p_type: 0,
            s_type: MessageType::RejectReq,
            system_bytes: 17,
            w_bit: false,
        };

        let message = HsmsMessage {
            header,
            body: Secs2::EMPTY,
        };

        let mut dst = BytesMut::new();
        let mut codec = HsmsMessageCodec::default();
        codec.encode(message, &mut dst).unwrap();

        let expected_bytes = [
            0x00, 0x00, 0x00, 0x0A, 0xFF, 0xFF, 0x03, 0x04, 0x00, 0x07, 0x00, 0x00, 0x00, 0x11,
        ];

        assert_eq!(&dst[..], &expected_bytes[..]);
    }

    #[test]
    fn test_decode_incomplete_data() {
        // 测试数据不足的情况
        let incomplete_data = [0x00, 0x00]; // 只有2字节，不足4字节长度字段

        let mut src = BytesMut::from(&incomplete_data[..]);
        let mut codec = HsmsMessageCodec::default();

        let result = codec.decode(&mut src);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none()); // 应该返回None，表示数据不完整
    }

    #[test]
    fn test_decode_partial_message() {
        // 测试消息不完整的情况
        let partial_data = [
            0x00, 0x00, 0x00, 0x10, // 长度字段表示需要16字节数据
            0x00, 0x00, 0x01, 0x01, // 但只有4字节头部数据
        ];

        let mut src = BytesMut::from(&partial_data[..]);
        let mut codec = HsmsMessageCodec::default();

        let result = codec.decode(&mut src);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none()); // 应该返回None，表示消息不完整
    }

    #[test]
    fn test_decode_rejects_oversized_length() {
        // 长度前缀声称 0xFFFFFFFF (~4GB)，但实际只发了 4 字节前缀 + 少量 body。
        // 期望：首次 decode 立即返回 InvalidData，而非 Ok(None) 慢慢攒数据到 OOM。
        let oversized_data = [
            0xFF, 0xFF, 0xFF, 0xFF, // 伪造的长度前缀
            0x00, 0x00, 0x01, 0x01, // 少量后续字节（远不足以填满声称的长度）
        ];

        let mut src = BytesMut::from(&oversized_data[..]);
        let mut codec = HsmsMessageCodec::default();

        let result = codec.decode(&mut src);
        assert!(result.is_err(), "超限长度前缀应被拒绝");
        let err = result.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn test_decode_respects_configured_max_msg_len() {
        // 配置一个很小的上限 (64)，构造一条合法但长度刚好超限的消息前缀。
        // 0x00 0x00 0x00 0x41 = 65，刚好超过上限 64。
        let data = [0x00, 0x00, 0x00, 0x41];

        let mut src = BytesMut::from(&data[..]);
        let mut codec = HsmsMessageCodec::new(64);

        let result = codec.decode(&mut src);
        assert!(result.is_err(), "超过自定义上限的长度应被拒绝");
        assert_eq!(
            result.unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn test_decode_allows_message_within_max_msg_len() {
        // 长度 17，在默认上限内，且数据完整 —— 应返回 Ok(None) 表示数据不完整，
        // 而非 Err（验证上限校验没有误伤合法长度）。
        let data = [
            0x00, 0x00, 0x00, 0x11, // 长度 17
            0x00, 0x00, 0x01, 0x01, // 仅 4 字节，不够 17
        ];

        let mut src = BytesMut::from(&data[..]);
        let mut codec = HsmsMessageCodec::new(64); // 上限 64 > 17，合法

        let result = codec.decode(&mut src);
        assert!(result.is_ok());
        assert!(result.unwrap().is_none()); // 数据不完整，但长度合法
    }
}
