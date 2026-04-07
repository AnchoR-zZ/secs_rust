/*
 * COMMUNICATIONS STATE MACHINE DIAGRAM
 *
stateDiagram-v2
    [*] --> DISABLED: 1 (Power-up)
    [*] --> ENABLED: 1 (Power-up)

    state ENABLED {
        [*] --> NOT_COMMUNICATING: 4
        
        state NOT_COMMUNICATING {
            direction LR
            
            state "EQUIPMENT-INITIATED CONNECT" as EQ_INIT {
                [*] --> WAIT_CRA: 5
                WAIT_CRA --> WAIT_DELAY: 6
                WAIT_DELAY --> WAIT_CRA: 7, 8
            }
            
            state "HOST-INITIATED CONNECT" as HOST_INIT {
                [*] --> WAIT_CR_FROM_HOST: 10
            }
        }

        NOT_COMMUNICATING --> COMMUNICATING: 9, 15
        COMMUNICATING --> NOT_COMMUNICATING: 14
    }

    DISABLED --> ENABLED: 2
    ENABLED --> DISABLED: 3
 *
 * COMMUNICATIONS STATE TRANSITION TABLE (Strictly aligned with SEMI E30 Table 3.2)
 *
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | #  | Current State             | Trigger                                    | New State             | Action                                           | Comments                                         |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 1  | (Entry)                   | System initialization.                     | System Default        | None                                             | Default may be set to DISABLED or ENABLED.       |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 2  | DISABLED                  | Operator switches to ENABLED.              | ENABLED               | None                                             | SECS-II communications are enabled.              |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 3  | ENABLED                   | Operator switches to DISABLED.             | DISABLED              | None                                             | SECS-II communications are prohibited.           |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 4  | (Entry to ENABLED)        | Any entry to ENABLED state.                | NOT COMMUNICATING     | Init internal variables.                         | From init or operator switch.                    |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 5  | (Entry to EQ-INITIATED)   | Any entry to NOT COMMUNICATING.            | WAIT CRA              | Send S1,F13. Set CommDelay "expired".            | Begin establish communications attempt.          |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 6  | WAIT CRA                  | Connection transaction failure (T3 timeout | WAIT DELAY            | Init CommDelay timer. Dequeue all messages.      | Dequeued messages may be placed in spool buffer. |
 * |    |                           | or S1,F14 with COMMACK != 0).              |                       |                                                  |                                                  |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 7  | WAIT DELAY                | CommDelay timer expired.                   | WAIT CRA              | Send S1,F13.                                     | Retry sending establish request.                 |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 8  | WAIT DELAY                | Received any message from host.            | WAIT CRA              | Discard message. No reply. Set timer "expired".  | Send S1,F13 immediately.                         |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 9  | WAIT CRA                  | Received expected S1,F14 (COMMACK = 0).    | COMMUNICATING         | None                                             | Communications are established.                  |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 10 | (Entry to HOST-INITIATED) | Any entry to NOT COMMUNICATING.            | WAIT CR FROM HOST     | None                                             | Wait for S1,F13 from Host.                       |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 14 | COMMUNICATING             | Communication failure.                     | NOT COMMUNICATING     | Dequeue all messages. Clear all internal timers. | Timers include T3, T5, T6, T7, T8.               |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 * | 15 | WAIT CR FROM HOST         | Received S1,F13.                           | COMMUNICATING         | Send S1,F14 (COMMACK = 0).                       | Communications are established.                  |
 * +----+---------------------------+--------------------------------------------+-----------------------+--------------------------------------------------+--------------------------------------------------+
 */

/*
 * CONTROL STATE MACHINE DIAGRAM
stateDiagram-v2
    [*] --> CONTROL: 1
    
    state CONTROL {
        [*] --> OFF_LINE: 2
        
        state OFF_LINE {
            [*] --> EQUIPMENT_OFF_LINE
            EQUIPMENT_OFF_LINE --> ATTEMPT_ON_LINE : 3
            ATTEMPT_ON_LINE --> EQUIPMENT_OFF_LINE : 4
            ATTEMPT_ON_LINE --> HOST_OFF_LINE : 4
            HOST_OFF_LINE --> EQUIPMENT_OFF_LINE : 12
        }

        state ON_LINE {
            [*] --> LOCAL: 7
            [*] --> REMOTE: 7
            
            LOCAL --> REMOTE : 8
            REMOTE --> LOCAL : 9
        }

        OFF_LINE --> ON_LINE : 5/11
        ON_LINE --> OFF_LINE : 6/10
    }
 *
 * CONTROL STATE TRANSITION TABLE
 *
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | #  | Current State      | Trigger                          | New State                 | Action | Comments                                                   |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 1  | (Undefined)        | Entry into CONTROL state         | CONTROL                   | None   | Equipment may be configured to default to ON-LINE or       |
 * |    |                    | (system initialization).         | (Substate conditional     |        | OFF-LINE. (See NOTE 1.)                                    |
 * |    |                    |                                  | on configuration).        |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 2  | (Undefined)        | Entry into OFF-LINE state.       | OFF-LINE                  | None   | Equipment may be configured to default to any substate     |
 * |    |                    |                                  | (Substate conditional     |        | of OFF-LINE.                                               |
 * |    |                    |                                  | on configuration.)        |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 3  | EQUIPMENT          | Operator actuates ON-LINE        | ATTEMPT ON-LINE           | None   | Note that an S1,F1 is sent whenever ATTEMPT ON-LINE is     |
 * |    | OFF-LINE           | switch.                          |                           |        | activated.                                                 |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 4  | ATTEMPT ON-LINE    | S1,F0.                           | New state conditional     | None   | This may be due to a communication failure (See NOTE 2),   |
 * |    |                    |                                  | on configuration.         |        | reply timeout, or receipt of S1,F0. Configuration may be   |
 * |    |                    |                                  |                           |        | set to EQUIPMENT OFF-LINE or HOST OFF-LINE.                |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 5  | ATTEMPT ON-LINE    | Equipment receives expected      | ON-LINE                   | None   | Host is notified of transition to ON-LINE at transition 7. |
 * |    |                    | S1,F2 message from the host.     |                           |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 6  | ON-LINE            | Operator actuates OFF-LINE       | EQUIPMENT OFF-LINE        | None   | "Equipment OFF-LINE" event occurs. (See NOTE 3.) Event     |
 * |    |                    | switch.                          |                           |        | reply will be discarded while OFF-LINE is active.          |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 7  | (Undefined)        | Entry to ON-LINE state.          | ON-LINE                   | None   | "Control State LOCAL" or "Control State REMOTE" event      |
 * |    |                    |                                  | (Substate conditional     |        | occurs. Event reported based on actual ON-LINE substate    |
 * |    |                    |                                  | on REMOTE/LOCAL           |        | activated.                                                 |
 * |    |                    |                                  | switch setting.)          |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 8  | LOCAL              | Operator sets front panel        | REMOTE                    | None   | "Control State REMOTE" event occurs.                       |
 * |    |                    | switch to REMOTE.                |                           |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 9  | REMOTE             | Operator sets front panel        | LOCAL                     | None   | "Control State LOCAL" event occurs.                        |
 * |    |                    | switch to LOCAL.                 |                           |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 10 | ON-LINE            | Equipment accepts "Set           | HOST OFF-LINE             | None   | "Equipment OFF-LINE" event occurs.                         |
 * |    |                    | OFF-LINE" message from host      |                           |        |                                                            |
 * |    |                    | (S1,F15).                        |                           |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 11 | HOST OFF-LINE      | Equipment accepts host request   | ON-LINE                   | None   | Host is notified to transition to ON-LINE at transition 7. |
 * |    |                    | to go ON-LINE (S1,F17).          |                           |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
 * | 12 | HOST OFF-LINE      | Operator actuates OFF-LINE       | EQUIPMENT OFF-LINE        | None   | "Equipment OFF-LINE" event occurs.                         |
 * |    |                    | switch.                          |                           |        |                                                            |
 * +----+--------------------+----------------------------------+---------------------------+--------+------------------------------------------------------------+
*/

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceState {
    NotConnected,
    NotSelected,
    Selected(GemState),
}

/// 自定义序列化：将 DeviceState 扁平化为字符串
///
/// 前端收到的始终是简单字符串：
/// `"NotConnected"` | `"NotSelected"` | `"EquipmentOffLine"` |
/// `"HostOffline"` | `"AttemptOnLine"` | `"OnlineLocal"` | `"OnlineRemote"`
impl serde::Serialize for DeviceState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let s = match self {
            DeviceState::NotConnected => "NotConnected",
            DeviceState::NotSelected => "NotSelected",
            DeviceState::Selected(gem) => match gem {
                GemState::OffLineState(off) => match off {
                    GemOfflineState::EquipmentOffLine => "EquipmentOffLine",
                    GemOfflineState::HostOffline => "HostOffline",
                    GemOfflineState::AttemptOnLine => "AttemptOnLine",
                },
                GemState::OnlineState(on) => match on {
                    GemOnlineState::Local => "OnlineLocal",
                    GemOnlineState::Remote => "OnlineRemote",
                },
            },
        };
        serializer.serialize_str(s)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum GemState {
    OffLineState(GemOfflineState),
    OnlineState(GemOnlineState),
}

#[derive(Debug, Clone, PartialEq)]
pub enum GemOnlineState {
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GemOfflineState {
    EquipmentOffLine,
    HostOffline,
    AttemptOnLine,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StateEvent {
    // Secs2 Level Events
    SocketConnectedEvent,
    SocketDisconnectedEvent,
    SelectEvent,
    DisSelectEvent,

    // HSMS Level Events - Offline State Events
    OperatorActuatesOnlineEvent, // optional 3  Operator actuates ON-LINE switch.
    ReceivedS1F0Event, // optional 4  Remote side sends S1F0 (ON-LINE)
    ReceivedS1F1ReplyTimeoutEvent, // optrional 4 No reply to S1F1 within timeout period
    ReceivedS1F2Event, // optional 5 Equipment receives expected  S1,F2 message from the host
    ReceivedS1F15Event, // optional 10 Equipment accepts "Set OFF-LINE" message from host (S1,F15)
    ReceivedS1F17Event, // optional 11 Equipment accepts host request to go ON-LINE (S1,F17)

    OperatorActuatesOfflineEvent, // optional 6 and 12 Operator actuates OFF-LINE switch.
    
    // HSMS Level Events - Online State Events
    OperatorSetsRemoteEvent,  // 新增
    OperatorSetsLocalEvent,   // 新增
}

// ============================================================================
// 配置 — 控制条件转换 (#1, #2, #4, #7)
// ============================================================================

/// 条件转换的目标状态配置
#[derive(Debug, Clone, PartialEq)]
pub struct StateMachineConfig {
    /// 转换 #1: 系统初始化后进入控制状态时的默认选择
    pub initial_control_state: InitialControlOption,
    /// 转换 #2: 进入 OFF-LINE 时的默认子状态
    pub initial_offline_substate: GemOfflineState,
    /// 转换 #4: AttemptOnLine 失败后的目标状态
    pub attempt_online_fail_target: AttemptOnlineFailTarget,
    /// 转换 #7: 进入 ON-LINE 时的默认子状态
    pub default_online_substate: GemOnlineState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InitialControlOption {
    OffLine,
    OnLine,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AttemptOnlineFailTarget {
    EquipmentOffLine,
    HostOffline,
}

impl Default for StateMachineConfig {
    fn default() -> Self {
        Self {
            initial_control_state: InitialControlOption::OffLine,
            initial_offline_substate: GemOfflineState::EquipmentOffLine,
            attempt_online_fail_target: AttemptOnlineFailTarget::EquipmentOffLine,
            default_online_substate: GemOnlineState::Local,
        }
    }
}

/// 包装类：将状态与配置绑定，提供更简便的 handle_event 调用方法
pub struct GemControl {
    pub state: DeviceState,
    pub config: StateMachineConfig,
}

impl GemControl {
    pub fn new(config: StateMachineConfig) -> Self {
        Self {
            state: DeviceState::NotConnected,
            config,
        }
    }

    /// 核心调用方法：无需手动传入 config
    pub fn handle_event(&mut self, event: StateEvent) {
        self.state = self.state.on_event(event, &self.config);
    }
}

// ============================================================================
// DeviceState — 设备完整通信状态（含 HSMS + GEM 子状态）
// ============================================================================

impl DeviceState {
    pub fn on_event(&self, event: StateEvent, config: &StateMachineConfig) -> DeviceState {
        match self {
            // --- NotConnected ---
            DeviceState::NotConnected => match event {
                StateEvent::SocketConnectedEvent => DeviceState::NotSelected,
                _ => self.clone(),
            },

            // --- NotSelected ---
            DeviceState::NotSelected => match event {
                StateEvent::SocketDisconnectedEvent => DeviceState::NotConnected,
                // 转换 #1/#2: Select 后进入 GEM 控制状态
                StateEvent::SelectEvent => {
                    let gem_state = match config.initial_control_state {
                        InitialControlOption::OffLine => {
                            GemState::OffLineState(config.initial_offline_substate.clone())
                        }
                        InitialControlOption::OnLine => {
                            GemState::OffLineState(GemOfflineState::AttemptOnLine)
                        }
                    };
                    DeviceState::Selected(gem_state)
                }
                _ => self.clone(),
            },

            // --- Selected: 委托给 GemState ---
            DeviceState::Selected(gem) => match event {
                StateEvent::SocketDisconnectedEvent => DeviceState::NotConnected,
                StateEvent::DisSelectEvent => DeviceState::NotSelected,
                _ => DeviceState::Selected(gem.on_event(event, config)),
            },
        }
    }

    /// 是否已建立 TCP 连接
    pub fn is_connected(&self) -> bool {
        !matches!(self, DeviceState::NotConnected)
    }

    /// 是否处于 Selected 状态（可以进行 GEM 操作）
    pub fn is_selected(&self) -> bool {
        matches!(self, DeviceState::Selected(_))
    }

    /// 是否处于 ON-LINE 状态
    pub fn is_online(&self) -> bool {
        matches!(self, DeviceState::Selected(GemState::OnlineState(_)))
    }

    /// 是否处于 OFF-LINE 状态
    pub fn is_offline(&self) -> bool {
        matches!(self, DeviceState::Selected(GemState::OffLineState(_)))
    }
}

// ============================================================================
// GemState — GEM 控制状态机转换 (#3 ~ #12)
// ============================================================================

impl GemState {
    pub fn on_event(&self, event: StateEvent, config: &StateMachineConfig) -> GemState {
        match self {
            // ========================
            // OFF-LINE 子状态
            // ========================
            GemState::OffLineState(offline) => match offline {
                // --- EquipmentOffLine ---
                GemOfflineState::EquipmentOffLine => match event {
                    // #3: 操作员按下 ON-LINE → AttemptOnLine (应发送 S1,F1)
                    StateEvent::OperatorActuatesOnlineEvent => {
                        GemState::OffLineState(GemOfflineState::AttemptOnLine)
                    }
                    _ => self.clone(),
                },

                // --- AttemptOnLine ---
                GemOfflineState::AttemptOnLine => match event {
                    // #4: S1,F0 或超时 → 根据配置回退
                    StateEvent::ReceivedS1F0Event
                    | StateEvent::ReceivedS1F1ReplyTimeoutEvent => {
                        let target = match config.attempt_online_fail_target {
                            AttemptOnlineFailTarget::EquipmentOffLine => {
                                GemOfflineState::EquipmentOffLine
                            }
                            AttemptOnlineFailTarget::HostOffline => GemOfflineState::HostOffline,
                        };
                        GemState::OffLineState(target)
                    }
                    // #5: 收到 S1,F2 → ON-LINE (子状态由配置 #7 决定)
                    StateEvent::ReceivedS1F2Event => {
                        GemState::OnlineState(config.default_online_substate.clone())
                    }
                    _ => self.clone(),
                },

                // --- HostOffline ---
                GemOfflineState::HostOffline => match event {
                    // #11: 收到 S1,F17 → ON-LINE
                    StateEvent::ReceivedS1F17Event => {
                        GemState::OnlineState(config.default_online_substate.clone())
                    }
                    // #12: 操作员按下 OFF-LINE → EquipmentOffLine
                    StateEvent::OperatorActuatesOfflineEvent => {
                        GemState::OffLineState(GemOfflineState::EquipmentOffLine)
                    }
                    _ => self.clone(),
                },
            },

            // ========================
            // ON-LINE 子状态
            // ========================
            GemState::OnlineState(online) => match event {
                // #6: 操作员按下 OFF-LINE → EquipmentOffLine
                StateEvent::OperatorActuatesOfflineEvent => {
                    GemState::OffLineState(GemOfflineState::EquipmentOffLine)
                }
                // #10: 收到 S1,F15 → HostOffline
                StateEvent::ReceivedS1F15Event => {
                    GemState::OffLineState(GemOfflineState::HostOffline)
                }
                // #8: LOCAL → REMOTE
                StateEvent::OperatorSetsRemoteEvent => match online {
                    GemOnlineState::Local => GemState::OnlineState(GemOnlineState::Remote),
                    _ => self.clone(),
                },
                // #9: REMOTE → LOCAL
                StateEvent::OperatorSetsLocalEvent => match online {
                    GemOnlineState::Remote => GemState::OnlineState(GemOnlineState::Local),
                    _ => self.clone(),
                },
                _ => self.clone(),
            },
        }
    }

    pub fn is_online(&self) -> bool {
        matches!(self, GemState::OnlineState(_))
    }

    pub fn is_offline(&self) -> bool {
        matches!(self, GemState::OffLineState(_))
    }

    pub fn is_local(&self) -> bool {
        matches!(self, GemState::OnlineState(GemOnlineState::Local))
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, GemState::OnlineState(GemOnlineState::Remote))
    }
}

// ============================================================================
// Display 实现
// ============================================================================

impl std::fmt::Display for DeviceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceState::NotConnected => write!(f, "Not Connected"),
            DeviceState::NotSelected => write!(f, "Not Selected"),
            DeviceState::Selected(gem) => write!(f, "Selected({})", gem),
        }
    }
}

impl std::fmt::Display for GemState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GemState::OffLineState(s) => write!(f, "OFF-LINE/{}", s),
            GemState::OnlineState(s) => write!(f, "ON-LINE/{}", s),
        }
    }
}

impl std::fmt::Display for GemOfflineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GemOfflineState::EquipmentOffLine => write!(f, "Equipment OFF-LINE"),
            GemOfflineState::HostOffline => write!(f, "Host OFF-LINE"),
            GemOfflineState::AttemptOnLine => write!(f, "Attempt ON-LINE"),
        }
    }
}

impl std::fmt::Display for GemOnlineState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GemOnlineState::Local => write!(f, "LOCAL"),
            GemOnlineState::Remote => write!(f, "REMOTE"),
        }
    }
}

// ============================================================================
// 单元测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> StateMachineConfig {
        StateMachineConfig::default()
    }

    #[test]
    fn test_gem_control_wrapper() {
        let mut control = GemControl::new(cfg());
        assert_eq!(control.state, DeviceState::NotConnected);
        assert!(!control.state.is_connected());

        control.handle_event(StateEvent::SocketConnectedEvent);
        assert_eq!(control.state, DeviceState::NotSelected);
        assert!(control.state.is_connected());

        control.handle_event(StateEvent::SelectEvent);
        assert!(control.state.is_selected());
        assert!(control.state.is_offline());
    }

    #[test]
    fn test_initial_config_online() {
        let config = StateMachineConfig {
            initial_control_state: InitialControlOption::OnLine,
            ..cfg()
        };
        let mut control = GemControl::new(config);
        
        control.handle_event(StateEvent::SocketConnectedEvent);
        control.handle_event(StateEvent::SelectEvent);

        // 配置为 OnLine 时，Select 后应直接进入 AttemptOnLine
        assert_eq!(
            control.state,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::AttemptOnLine))
        );
    }

    #[test]
    fn test_initial_config_host_offline() {
        let config = StateMachineConfig {
            initial_offline_substate: GemOfflineState::HostOffline,
            ..cfg()
        };
        let mut control = GemControl::new(config);
        
        control.handle_event(StateEvent::SocketConnectedEvent);
        control.handle_event(StateEvent::SelectEvent);

        // 配置初始离线子状态为 HostOffline 时
        assert_eq!(
            control.state,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::HostOffline))
        );
    }

    #[test]
    fn test_query_helpers() {
        let s = DeviceState::Selected(GemState::OnlineState(GemOnlineState::Local));
        
        assert!(s.is_connected());
        assert!(s.is_selected());
        assert!(s.is_online());
        assert!(!s.is_offline());

        if let DeviceState::Selected(gem) = s {
            assert!(gem.is_online());
            assert!(gem.is_local());
            assert!(!gem.is_remote());
        }
    }

    #[test]
    fn test_full_online_flow() {
        let c = cfg();
        let s = DeviceState::NotConnected;

        // Socket 连接
        let s = s.on_event(StateEvent::SocketConnectedEvent, &c);
        assert_eq!(s, DeviceState::NotSelected);

        // Select → EquipmentOffLine
        let s = s.on_event(StateEvent::SelectEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::EquipmentOffLine))
        );

        // #3: → AttemptOnLine
        let s = s.on_event(StateEvent::OperatorActuatesOnlineEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::AttemptOnLine))
        );

        // #5: → ON-LINE/Local
        let s = s.on_event(StateEvent::ReceivedS1F2Event, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OnlineState(GemOnlineState::Local))
        );

        // #8: → Remote
        let s = s.on_event(StateEvent::OperatorSetsRemoteEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OnlineState(GemOnlineState::Remote))
        );

        // #9: → Local
        let s = s.on_event(StateEvent::OperatorSetsLocalEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OnlineState(GemOnlineState::Local))
        );
    }

    #[test]
    fn test_transition_4_fail_to_equipment_offline() {
        let c = cfg(); // 默认: fail → EquipmentOffLine
        let s = DeviceState::Selected(GemState::OffLineState(GemOfflineState::AttemptOnLine));

        let s = s.on_event(StateEvent::ReceivedS1F0Event, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::EquipmentOffLine))
        );
    }

    #[test]
    fn test_transition_4_fail_to_host_offline() {
        let c = StateMachineConfig {
            attempt_online_fail_target: AttemptOnlineFailTarget::HostOffline,
            ..cfg()
        };
        let s = DeviceState::Selected(GemState::OffLineState(GemOfflineState::AttemptOnLine));

        let s = s.on_event(StateEvent::ReceivedS1F1ReplyTimeoutEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::HostOffline))
        );
    }

    #[test]
    fn test_transition_6_online_to_equipment_offline() {
        let c = cfg();
        let s = DeviceState::Selected(GemState::OnlineState(GemOnlineState::Local));

        let s = s.on_event(StateEvent::OperatorActuatesOfflineEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::EquipmentOffLine))
        );
    }

    #[test]
    fn test_transition_10_s1f15_to_host_offline() {
        let c = cfg();
        let s = DeviceState::Selected(GemState::OnlineState(GemOnlineState::Remote));

        let s = s.on_event(StateEvent::ReceivedS1F15Event, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::HostOffline))
        );
    }

    #[test]
    fn test_transition_11_host_offline_to_online() {
        let c = cfg();
        let s = DeviceState::Selected(GemState::OffLineState(GemOfflineState::HostOffline));

        let s = s.on_event(StateEvent::ReceivedS1F17Event, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OnlineState(GemOnlineState::Local))
        );
    }

    #[test]
    fn test_transition_12_host_offline_to_equipment_offline() {
        let c = cfg();
        let s = DeviceState::Selected(GemState::OffLineState(GemOfflineState::HostOffline));

        let s = s.on_event(StateEvent::OperatorActuatesOfflineEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::EquipmentOffLine))
        );
    }

    #[test]
    fn test_disconnect_from_selected() {
        let c = cfg();
        let s = DeviceState::Selected(GemState::OnlineState(GemOnlineState::Remote));

        let s = s.on_event(StateEvent::SocketDisconnectedEvent, &c);
        assert_eq!(s, DeviceState::NotConnected);
    }

    #[test]
    fn test_deselect() {
        let c = cfg();
        let s = DeviceState::Selected(GemState::OnlineState(GemOnlineState::Local));

        let s = s.on_event(StateEvent::DisSelectEvent, &c);
        assert_eq!(s, DeviceState::NotSelected);
    }

    #[test]
    fn test_ignored_event_returns_same_state() {
        let c = cfg();
        // NotConnected 忽略 SelectEvent
        let s = DeviceState::NotConnected;
        assert_eq!(s.on_event(StateEvent::SelectEvent, &c), DeviceState::NotConnected);

        // EquipmentOffLine 忽略 S1F2
        let s = DeviceState::Selected(GemState::OffLineState(GemOfflineState::EquipmentOffLine));
        assert_eq!(
            s.on_event(StateEvent::ReceivedS1F2Event, &c),
            DeviceState::Selected(GemState::OffLineState(GemOfflineState::EquipmentOffLine))
        );
    }

    #[test]
    fn test_display() {
        assert_eq!(
            format!("{}", DeviceState::NotConnected),
            "Not Connected"
        );
        assert_eq!(
            format!(
                "{}",
                DeviceState::Selected(GemState::OnlineState(GemOnlineState::Local))
            ),
            "Selected(ON-LINE/LOCAL)"
        );
        assert_eq!(
            format!(
                "{}",
                DeviceState::Selected(GemState::OffLineState(GemOfflineState::HostOffline))
            ),
            "Selected(OFF-LINE/Host OFF-LINE)"
        );
    }
}