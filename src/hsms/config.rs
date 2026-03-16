use std::time::Duration;

#[derive(Debug, Clone)]
pub struct HsmsConfig {
    pub session_id: u16,
    pub ip: String,
    pub port: u16,
    pub mode: ConnectionMode,
    pub connect_timeout: Duration, // TCP连接超时
    pub t3: Duration,              // Reply Timeout
    pub t5: Duration,              // Connect Separation Timeout (Active重连间隔)
    pub t6: Duration,              // Control Transaction Timeout
    pub t7: Duration,              // Not Selected Timeout (TCP建立后多久没收到Select则断开)
    pub t8: Duration,              // Inter-character Timeout (通常由TCP栈处理，应用层可忽略)
    pub linktest: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionMode {
    Active,  // 主动连接
    Passive, // 被动监听
}

impl Default for HsmsConfig {
    fn default() -> Self {
        Self {
            session_id: 0,
            ip: "0.0.0.0".to_string(),
            port: 5000,
            mode: ConnectionMode::Passive,
            connect_timeout: Duration::from_secs(10),
            t3: Duration::from_secs(5),
            t5: Duration::from_secs(10),
            t6: Duration::from_secs(5),
            t7: Duration::from_secs(10),
            t8: Duration::from_secs(5),
            linktest: Duration::from_secs(30),
        }
    }
}
