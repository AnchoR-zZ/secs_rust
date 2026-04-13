use crate::gem::state::comm_state::CommStateMachineConfig;
use crate::gem::state::control_state::StateMachineConfig;
use crate::hsms::config::HsmsConfig;

/// GEM 角色：设备端 (Equipment) 或 主机端 (Host)
#[derive(Debug, Clone, PartialEq, Default)]
pub enum GemRole {
    /// 设备端：Select 后主动发 S1F13，响应 Host 的 S1F1/S1F15/S1F17
    #[default]
    Equipment,
    /// 主机端：等待 Equipment 的 S1F13 并回复 S1F14，主动发 S1F1/S1F15/S1F17
    Host,
}

/// GEM 层配置
#[derive(Debug, Clone)]
pub struct GemConfig {
    pub role: GemRole,
    pub hsms_config: HsmsConfig,
    pub state_machine_config: StateMachineConfig,
    pub comm_state_machine_config: CommStateMachineConfig,
    pub mdln: String,
    pub softrev: String,
}

impl Default for GemConfig {
    fn default() -> Self {
        Self {
            role: GemRole::default(),
            hsms_config: HsmsConfig::default(),
            state_machine_config: StateMachineConfig::default(),
            comm_state_machine_config: CommStateMachineConfig::default(),
            mdln: "SECS-SIMULATOR".to_string(),
            softrev: "1.0.0".to_string(),
        }
    }
}

impl GemConfig {
    /// 创建 GEM 配置
    ///
    /// # 参数
    /// - `role`: 角色（Equipment/Host）
    /// - `hsms_config`: 底层 HSMS 配置
    /// - `state_machine_config`: GEM 状态机配置（可选，默认使用 `StateMachineConfig::default()`）
    /// - `mdln`: 设备型号名（可选，默认 "SECS-SIMULATOR"）
    /// - `softrev`: 软件版本（可选，默认 "1.0.0"）
    pub fn new(
        role: GemRole,
        hsms_config: HsmsConfig,
        state_machine_config: Option<StateMachineConfig>,
        mdln: Option<String>,
        softrev: Option<String>,
    ) -> Self {
        let defaults = Self::default();
        Self {
            role,
            hsms_config,
            state_machine_config: state_machine_config.unwrap_or(defaults.state_machine_config),
            comm_state_machine_config: defaults.comm_state_machine_config,
            mdln: mdln.unwrap_or(defaults.mdln),
            softrev: softrev.unwrap_or(defaults.softrev),
        }
    }
}
