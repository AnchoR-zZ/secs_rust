//! GEM 相关 SECS-II 消息构造辅助函数
//!
//! 提供 S1F0 ~ S1F18 的消息构造方法，供 GemSession 自动处理使用。

use crate::hsms::message::HsmsMessage;
use crate::secs2::Secs2;
use crate::util::next_system_bytes;

// ============================================================================
// S1F0 — Abort Transaction
// ============================================================================

/// 构造 S1F0 Abort Transaction
pub fn build_s1f0(session_id: u16) -> HsmsMessage {
    HsmsMessage::build_unidirectional_data_message(
        session_id, 1, 0,
        next_system_bytes(),
        Secs2::EMPTY,
    )
}

// ============================================================================
// S1F1 / S1F2 — Are You There
// ============================================================================

/// 构造 S1F1 Are You There Request (W-bit = true)
pub fn build_s1f1(session_id: u16) -> HsmsMessage {
    HsmsMessage::build_request_data_message(
        session_id, 1, 1,
        next_system_bytes(),
        Secs2::EMPTY,
    )
}

/// 构造 S1F2 On Line Data (回复)
/// - Equipment 回复: L:2 { A:mdln, A:softrev }
/// - Host 回复: L:0
pub fn build_s1f2_reply(req: &HsmsMessage, mdln: &str, softrev: &str) -> HsmsMessage {
    let body = if mdln.is_empty() && softrev.is_empty() {
        Secs2::LIST(vec![])
    } else {
        Secs2::LIST(vec![
            Secs2::ASCII(mdln.to_string()),
            Secs2::ASCII(softrev.to_string()),
        ])
    };
    req.build_reply_message(1, 2, body)
}

// ============================================================================
// S1F13 / S1F14 — Establish Communication
// ============================================================================

/// 构造 S1F13 Establish Communication Request (W-bit = true)
/// body = L:2 { A:mdln, A:softrev } 或 L:0
pub fn build_s1f13(session_id: u16, mdln: &str, softrev: &str) -> HsmsMessage {
    let body = if mdln.is_empty() && softrev.is_empty() {
        Secs2::LIST(vec![])
    } else {
        Secs2::LIST(vec![
            Secs2::ASCII(mdln.to_string()),
            Secs2::ASCII(softrev.to_string()),
        ])
    };
    HsmsMessage::build_request_data_message(
        session_id, 1, 13,
        next_system_bytes(),
        body,
    )
}

/// 构造 S1F14 Establish Communication Acknowledge (回复)
/// body = L:2 { B:commack, L:2 { A:mdln, A:softrev } }
/// commack: 0 = accepted, 1 = denied (try again), 2 = already established
pub fn build_s1f14_reply(req: &HsmsMessage, commack: u8, mdln: &str, softrev: &str) -> HsmsMessage {
    let body = Secs2::LIST(vec![
        Secs2::BINARY(vec![commack]),
        if mdln.is_empty() && softrev.is_empty() {
            Secs2::LIST(vec![])
        } else {
            Secs2::LIST(vec![
                Secs2::ASCII(mdln.to_string()),
                Secs2::ASCII(softrev.to_string()),
            ])
        },
    ]);
    req.build_reply_message(1, 14, body)
}

// ============================================================================
// S1F15 / S1F16 — Request OFF-LINE
// ============================================================================

/// 构造 S1F15 Request OFF-LINE (W-bit = true)
pub fn build_s1f15(session_id: u16) -> HsmsMessage {
    HsmsMessage::build_request_data_message(
        session_id, 1, 15,
        next_system_bytes(),
        Secs2::EMPTY,
    )
}

/// 构造 S1F16 OFF-LINE Acknowledge (回复)
/// body = B:oflack (0 = accepted)
pub fn build_s1f16_reply(req: &HsmsMessage, oflack: u8) -> HsmsMessage {
    req.build_reply_message(1, 16, Secs2::BINARY(vec![oflack]))
}

// ============================================================================
// S1F17 / S1F18 — Request ON-LINE
// ============================================================================

/// 构造 S1F17 Request ON-LINE (W-bit = true)
pub fn build_s1f17(session_id: u16) -> HsmsMessage {
    HsmsMessage::build_request_data_message(
        session_id, 1, 17,
        next_system_bytes(),
        Secs2::EMPTY,
    )
}

/// 构造 S1F18 ON-LINE Acknowledge (回复)
/// body = B:onlack (0 = accepted, 1 = refused, 2 = already online)
pub fn build_s1f18_reply(req: &HsmsMessage, onlack: u8) -> HsmsMessage {
    req.build_reply_message(1, 18, Secs2::BINARY(vec![onlack]))
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_s1f1() {
        let msg = build_s1f1(100);
        assert_eq!(msg.header.stream, 1);
        assert_eq!(msg.header.function, 1);
        assert_eq!(msg.header.session_id, 100);
        assert!(msg.header.w_bit);
        assert_eq!(msg.body, Secs2::EMPTY);
    }

    #[test]
    fn test_build_s1f2_reply_with_data() {
        let req = build_s1f1(100);
        let reply = build_s1f2_reply(&req, "MODEL", "1.0.0");
        assert_eq!(reply.header.stream, 1);
        assert_eq!(reply.header.function, 2);
        assert_eq!(reply.header.system_bytes, req.header.system_bytes);
        assert!(!reply.header.w_bit);
        match &reply.body {
            Secs2::LIST(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Secs2::ASCII("MODEL".into()));
                assert_eq!(items[1], Secs2::ASCII("1.0.0".into()));
            }
            _ => panic!("expected LIST"),
        }
    }

    #[test]
    fn test_build_s1f2_reply_empty() {
        let req = build_s1f1(100);
        let reply = build_s1f2_reply(&req, "", "");
        assert_eq!(reply.header.function, 2);
        match &reply.body {
            Secs2::LIST(items) => assert!(items.is_empty()),
            _ => panic!("expected empty LIST"),
        }
    }

    #[test]
    fn test_build_s1f13() {
        let msg = build_s1f13(200, "EQ1", "2.0");
        assert_eq!(msg.header.stream, 1);
        assert_eq!(msg.header.function, 13);
        assert!(msg.header.w_bit);
        match &msg.body {
            Secs2::LIST(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Secs2::ASCII("EQ1".into()));
                assert_eq!(items[1], Secs2::ASCII("2.0".into()));
            }
            _ => panic!("expected LIST"),
        }
    }

    #[test]
    fn test_build_s1f14_reply() {
        let req = build_s1f13(200, "", "");
        let reply = build_s1f14_reply(&req, 0, "HOST", "3.0");
        assert_eq!(reply.header.stream, 1);
        assert_eq!(reply.header.function, 14);
        assert!(!reply.header.w_bit);
        match &reply.body {
            Secs2::LIST(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], Secs2::BINARY(vec![0]));
                // 第二项是嵌套 LIST
                match &items[1] {
                    Secs2::LIST(inner) => {
                        assert_eq!(inner.len(), 2);
                    }
                    _ => panic!("expected nested LIST"),
                }
            }
            _ => panic!("expected LIST"),
        }
    }

    #[test]
    fn test_build_s1f15() {
        let msg = build_s1f15(300);
        assert_eq!(msg.header.stream, 1);
        assert_eq!(msg.header.function, 15);
        assert!(msg.header.w_bit);
    }

    #[test]
    fn test_build_s1f16_reply() {
        let req = build_s1f15(300);
        let reply = build_s1f16_reply(&req, 0);
        assert_eq!(reply.header.function, 16);
        assert_eq!(reply.body, Secs2::BINARY(vec![0]));
    }

    #[test]
    fn test_build_s1f17() {
        let msg = build_s1f17(400);
        assert_eq!(msg.header.stream, 1);
        assert_eq!(msg.header.function, 17);
        assert!(msg.header.w_bit);
    }

    #[test]
    fn test_build_s1f18_reply() {
        let req = build_s1f17(400);
        let reply = build_s1f18_reply(&req, 0);
        assert_eq!(reply.header.function, 18);
        assert_eq!(reply.body, Secs2::BINARY(vec![0]));
    }

    #[test]
    fn test_build_s1f0() {
        let msg = build_s1f0(500);
        assert_eq!(msg.header.stream, 1);
        assert_eq!(msg.header.function, 0);
        assert!(!msg.header.w_bit);
    }
}
