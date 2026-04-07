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
pub enum ControlState {
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
    OperatorActuatesOnlineEvent,
    ReceivedS1F0Event,
    ReceivedS1F1ReplyTimeoutEvent,
    ReceivedS1F2Event,
    ReceivedS1F15Event,
    ReceivedS1F17Event,

    OperatorActuatesOfflineEvent,

    // HSMS Level Events - Online State Events
    OperatorSetsRemoteEvent,
    OperatorSetsLocalEvent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StateMachineConfig {
    pub initial_control_state: InitialControlOption,
    pub initial_offline_substate: GemOfflineState,
    pub attempt_online_fail_target: AttemptOnlineFailTarget,
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

pub struct GemControl {
    pub state: super::gem_state::DeviceState,
    pub config: StateMachineConfig,
}

impl GemControl {
    pub fn new(config: StateMachineConfig) -> Self {
        Self {
            state: super::gem_state::DeviceState::NotConnected,
            config,
        }
    }

    pub fn handle_event(&mut self, event: StateEvent) {
        self.state = self.state.on_event(event, &self.config);
    }
}

// ============================================================================
// GemState — GEM 控制状态机转换 (#3 ~ #12)
// ============================================================================

impl ControlState {
    pub fn on_event(&self, event: StateEvent, config: &StateMachineConfig) -> ControlState {
        match self {
            ControlState::OffLineState(offline) => match offline {
                GemOfflineState::EquipmentOffLine => match event {
                    StateEvent::OperatorActuatesOnlineEvent => {
                        ControlState::OffLineState(GemOfflineState::AttemptOnLine)
                    }
                    _ => self.clone(),
                },

                GemOfflineState::AttemptOnLine => match event {
                    StateEvent::ReceivedS1F0Event | StateEvent::ReceivedS1F1ReplyTimeoutEvent => {
                        let target = match config.attempt_online_fail_target {
                            AttemptOnlineFailTarget::EquipmentOffLine => {
                                GemOfflineState::EquipmentOffLine
                            }
                            AttemptOnlineFailTarget::HostOffline => GemOfflineState::HostOffline,
                        };
                        ControlState::OffLineState(target)
                    }
                    StateEvent::ReceivedS1F2Event => {
                        ControlState::OnlineState(config.default_online_substate.clone())
                    }
                    _ => self.clone(),
                },

                GemOfflineState::HostOffline => match event {
                    StateEvent::ReceivedS1F17Event => {
                        ControlState::OnlineState(config.default_online_substate.clone())
                    }
                    StateEvent::OperatorActuatesOfflineEvent => {
                        ControlState::OffLineState(GemOfflineState::EquipmentOffLine)
                    }
                    _ => self.clone(),
                },
            },

            ControlState::OnlineState(online) => match event {
                StateEvent::OperatorActuatesOfflineEvent => {
                    ControlState::OffLineState(GemOfflineState::EquipmentOffLine)
                }
                StateEvent::ReceivedS1F15Event => {
                    ControlState::OffLineState(GemOfflineState::HostOffline)
                }
                StateEvent::OperatorSetsRemoteEvent => match online {
                    GemOnlineState::Local => ControlState::OnlineState(GemOnlineState::Remote),
                    _ => self.clone(),
                },
                StateEvent::OperatorSetsLocalEvent => match online {
                    GemOnlineState::Remote => ControlState::OnlineState(GemOnlineState::Local),
                    _ => self.clone(),
                },
                _ => self.clone(),
            },
        }
    }

    pub fn is_online(&self) -> bool {
        matches!(self, ControlState::OnlineState(_))
    }

    pub fn is_offline(&self) -> bool {
        matches!(self, ControlState::OffLineState(_))
    }

    pub fn is_local(&self) -> bool {
        matches!(self, ControlState::OnlineState(GemOnlineState::Local))
    }

    pub fn is_remote(&self) -> bool {
        matches!(self, ControlState::OnlineState(GemOnlineState::Remote))
    }
}

// ============================================================================
// Display 实现
// ============================================================================

impl std::fmt::Display for ControlState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ControlState::OffLineState(s) => write!(f, "OFF-LINE/{}", s),
            ControlState::OnlineState(s) => write!(f, "ON-LINE/{}", s),
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
    use super::super::gem_state::DeviceState;
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

        assert_eq!(
            control.state,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::AttemptOnLine))
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

        assert_eq!(
            control.state,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::HostOffline))
        );
    }

    #[test]
    fn test_query_helpers() {
        let s = DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Local));

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

        let s = s.on_event(StateEvent::SocketConnectedEvent, &c);
        assert_eq!(s, DeviceState::NotSelected);

        let s = s.on_event(StateEvent::SelectEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::EquipmentOffLine))
        );

        let s = s.on_event(StateEvent::OperatorActuatesOnlineEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::AttemptOnLine))
        );

        let s = s.on_event(StateEvent::ReceivedS1F2Event, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Local))
        );

        let s = s.on_event(StateEvent::OperatorSetsRemoteEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Remote))
        );

        let s = s.on_event(StateEvent::OperatorSetsLocalEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Local))
        );
    }

    #[test]
    fn test_transition_4_fail_to_equipment_offline() {
        let c = cfg();
        let s = DeviceState::Selected(ControlState::OffLineState(GemOfflineState::AttemptOnLine));

        let s = s.on_event(StateEvent::ReceivedS1F0Event, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::EquipmentOffLine))
        );
    }

    #[test]
    fn test_transition_4_fail_to_host_offline() {
        let c = StateMachineConfig {
            attempt_online_fail_target: AttemptOnlineFailTarget::HostOffline,
            ..cfg()
        };
        let s = DeviceState::Selected(ControlState::OffLineState(GemOfflineState::AttemptOnLine));

        let s = s.on_event(StateEvent::ReceivedS1F1ReplyTimeoutEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::HostOffline))
        );
    }

    #[test]
    fn test_transition_6_online_to_equipment_offline() {
        let c = cfg();
        let s = DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Local));

        let s = s.on_event(StateEvent::OperatorActuatesOfflineEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::EquipmentOffLine))
        );
    }

    #[test]
    fn test_transition_10_s1f15_to_host_offline() {
        let c = cfg();
        let s = DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Remote));

        let s = s.on_event(StateEvent::ReceivedS1F15Event, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::HostOffline))
        );
    }

    #[test]
    fn test_transition_11_host_offline_to_online() {
        let c = cfg();
        let s = DeviceState::Selected(ControlState::OffLineState(GemOfflineState::HostOffline));

        let s = s.on_event(StateEvent::ReceivedS1F17Event, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Local))
        );
    }

    #[test]
    fn test_transition_12_host_offline_to_equipment_offline() {
        let c = cfg();
        let s = DeviceState::Selected(ControlState::OffLineState(GemOfflineState::HostOffline));

        let s = s.on_event(StateEvent::OperatorActuatesOfflineEvent, &c);
        assert_eq!(
            s,
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::EquipmentOffLine))
        );
    }

    #[test]
    fn test_disconnect_from_selected() {
        let c = cfg();
        let s = DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Remote));

        let s = s.on_event(StateEvent::SocketDisconnectedEvent, &c);
        assert_eq!(s, DeviceState::NotConnected);
    }

    #[test]
    fn test_deselect() {
        let c = cfg();
        let s = DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Local));

        let s = s.on_event(StateEvent::DisSelectEvent, &c);
        assert_eq!(s, DeviceState::NotSelected);
    }

    #[test]
    fn test_ignored_event_returns_same_state() {
        let c = cfg();
        let s = DeviceState::NotConnected;
        assert_eq!(
            s.on_event(StateEvent::SelectEvent, &c),
            DeviceState::NotConnected
        );

        let s = DeviceState::Selected(ControlState::OffLineState(GemOfflineState::EquipmentOffLine));
        assert_eq!(
            s.on_event(StateEvent::ReceivedS1F2Event, &c),
            DeviceState::Selected(ControlState::OffLineState(GemOfflineState::EquipmentOffLine))
        );
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", DeviceState::NotConnected), "Not Connected");
        assert_eq!(
            format!(
                "{}",
                DeviceState::Selected(ControlState::OnlineState(GemOnlineState::Local))
            ),
            "Selected(ON-LINE/LOCAL)"
        );
        assert_eq!(
            format!(
                "{}",
                DeviceState::Selected(ControlState::OffLineState(GemOfflineState::HostOffline))
            ),
            "Selected(OFF-LINE/Host OFF-LINE)"
        );
    }
}
