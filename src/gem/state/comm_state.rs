/*
 * COMMUNICATIONS STATE MACHINE DIAGRAM (SEMI E30 Table 3.2)
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

use std::fmt;
use std::time::Duration;

// ============================================================================
// State Enums
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum CommState {
    Disabled,
    Enabled(CommEnabledState),
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommEnabledState {
    NotCommunicating(CommConnectState),
    Communicating,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CommConnectState {
    EquipmentInitiated(EqInitState),
    HostInitiated,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EqInitState {
    WaitCra,
    WaitDelay,
}

// ============================================================================
// Event
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum CommEvent {
    OperatorEnable,
    OperatorDisable,

    T3Timeout,
    CommackRejected,
    ReceivedS1F14Accepted,

    CommDelayExpired,
    ReceivedAnyMessageInDelay,

    ReceivedS1F13,

    CommunicationFailure,
}

// ============================================================================
// Action
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub enum CommAction {
    SendS1F13,
    StartCommDelayTimer,
    StopCommDelayTimer,
    SendS1F14Accepted,
    ClearAllTimers,
    DequeueAllMessages,
}

// ============================================================================
// Config
// ============================================================================

#[derive(Debug, Clone, PartialEq)]
pub struct CommStateMachineConfig {
    pub comm_delay: Duration,
    pub connect_mode: CommConnectMode,
    pub initial_state: CommInitialState,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommConnectMode {
    EquipmentInitiated,
    HostInitiated,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CommInitialState {
    Disabled,
    Enabled,
}

impl Default for CommStateMachineConfig {
    fn default() -> Self {
        Self {
            comm_delay: Duration::from_secs(10),
            connect_mode: CommConnectMode::EquipmentInitiated,
            initial_state: CommInitialState::Disabled,
        }
    }
}

// ============================================================================
// CommControl — Wrapper
// ============================================================================

pub struct CommControl {
    pub state: CommState,
    pub config: CommStateMachineConfig,
}

impl CommControl {
    pub fn new(config: CommStateMachineConfig) -> Self {
        let state = match config.initial_state {
            CommInitialState::Disabled => CommState::Disabled,
            CommInitialState::Enabled => {
                let connect = match config.connect_mode {
                    CommConnectMode::EquipmentInitiated => {
                        CommConnectState::EquipmentInitiated(EqInitState::WaitCra)
                    }
                    CommConnectMode::HostInitiated => CommConnectState::HostInitiated,
                };
                CommState::Enabled(CommEnabledState::NotCommunicating(connect))
            }
        };

        let mut control = Self { state, config };
        let actions = control.entry_actions();
        let _ = actions;
        control
    }

    pub fn handle_event(&mut self, event: CommEvent) -> Vec<CommAction> {
        let (new_state, actions) = self.state.on_event(event, &self.config);
        self.state = new_state;
        let mut all_actions = actions;
        let entry = self.entry_actions();
        all_actions.extend(entry);
        all_actions
    }

    fn entry_actions(&mut self) -> Vec<CommAction> {
        match &mut self.state {
            CommState::Enabled(CommEnabledState::NotCommunicating(
                CommConnectState::EquipmentInitiated(EqInitState::WaitCra),
            )) => {
                if let CommState::Enabled(CommEnabledState::NotCommunicating(
                    CommConnectState::EquipmentInitiated(ref mut eq),
                )) = self.state
                {
                    let _ = eq;
                }
                vec![CommAction::SendS1F13]
            }
            _ => vec![],
        }
    }

    pub fn is_communicating(&self) -> bool {
        matches!(
            self.state,
            CommState::Enabled(CommEnabledState::Communicating)
        )
    }

    pub fn is_enabled(&self) -> bool {
        matches!(self.state, CommState::Enabled(_))
    }
}

// ============================================================================
// State Transitions
// ============================================================================

impl CommState {
    pub fn on_event(
        &self,
        event: CommEvent,
        config: &CommStateMachineConfig,
    ) -> (CommState, Vec<CommAction>) {
        match self {
            CommState::Disabled => match event {
                CommEvent::OperatorEnable => {
                    let connect = match config.connect_mode {
                        CommConnectMode::EquipmentInitiated => {
                            CommConnectState::EquipmentInitiated(EqInitState::WaitCra)
                        }
                        CommConnectMode::HostInitiated => CommConnectState::HostInitiated,
                    };
                    let new_state = CommState::Enabled(CommEnabledState::NotCommunicating(connect));
                    let actions = match config.connect_mode {
                        CommConnectMode::EquipmentInitiated => vec![CommAction::SendS1F13],
                        CommConnectMode::HostInitiated => vec![],
                    };
                    (new_state, actions)
                }
                _ => (self.clone(), vec![]),
            },

            CommState::Enabled(enabled) => match event {
                CommEvent::OperatorDisable => (
                    CommState::Disabled,
                    vec![CommAction::StopCommDelayTimer, CommAction::ClearAllTimers],
                ),

                _ => {
                    let (new_enabled, actions) = enabled.on_event(event, config);
                    (CommState::Enabled(new_enabled), actions)
                }
            },
        }
    }
}

impl CommEnabledState {
    pub fn on_event(
        &self,
        event: CommEvent,
        config: &CommStateMachineConfig,
    ) -> (CommEnabledState, Vec<CommAction>) {
        match self {
            CommEnabledState::NotCommunicating(connect) => {
                let (new_connect, actions) = connect.on_event(event, config);
                match new_connect {
                    Some(c) => (CommEnabledState::NotCommunicating(c), actions),
                    None => (CommEnabledState::Communicating, actions),
                }
            }

            CommEnabledState::Communicating => match event {
                CommEvent::CommunicationFailure => {
                    let connect = match config.connect_mode {
                        CommConnectMode::EquipmentInitiated => {
                            CommConnectState::EquipmentInitiated(EqInitState::WaitCra)
                        }
                        CommConnectMode::HostInitiated => CommConnectState::HostInitiated,
                    };
                    let actions = match config.connect_mode {
                        CommConnectMode::EquipmentInitiated => vec![
                            CommAction::ClearAllTimers,
                            CommAction::DequeueAllMessages,
                            CommAction::SendS1F13,
                        ],
                        CommConnectMode::HostInitiated => {
                            vec![CommAction::ClearAllTimers, CommAction::DequeueAllMessages]
                        }
                    };
                    (CommEnabledState::NotCommunicating(connect), actions)
                }
                _ => (self.clone(), vec![]),
            },
        }
    }
}

impl CommConnectState {
    /// Returns Some(new_connect_state) to stay in NotCommunicating,
    /// or None to transition to Communicating.
    fn on_event(
        &self,
        event: CommEvent,
        _config: &CommStateMachineConfig,
    ) -> (Option<CommConnectState>, Vec<CommAction>) {
        match self {
            CommConnectState::EquipmentInitiated(eq) => {
                let (new_eq, actions) = eq.on_event(event);
                match new_eq {
                    EqTransition::Stay(eq) => {
                        (Some(CommConnectState::EquipmentInitiated(eq)), actions)
                    }
                    EqTransition::ToCommunicating => (None, actions),
                }
            }

            CommConnectState::HostInitiated => match event {
                CommEvent::ReceivedS1F13 => (None, vec![CommAction::SendS1F14Accepted]),
                _ => (Some(self.clone()), vec![]),
            },
        }
    }
}

enum EqTransition {
    Stay(EqInitState),
    ToCommunicating,
}

impl EqInitState {
    fn on_event(&self, event: CommEvent) -> (EqTransition, Vec<CommAction>) {
        match self {
            EqInitState::WaitCra => match event {
                CommEvent::T3Timeout | CommEvent::CommackRejected => (
                    EqTransition::Stay(EqInitState::WaitDelay),
                    vec![
                        CommAction::StartCommDelayTimer,
                        CommAction::DequeueAllMessages,
                    ],
                ),
                CommEvent::ReceivedS1F14Accepted => (EqTransition::ToCommunicating, vec![]),
                _ => (EqTransition::Stay(self.clone()), vec![]),
            },

            EqInitState::WaitDelay => match event {
                CommEvent::CommDelayExpired => (
                    EqTransition::Stay(EqInitState::WaitCra),
                    vec![CommAction::SendS1F13],
                ),
                CommEvent::ReceivedAnyMessageInDelay => (
                    EqTransition::Stay(EqInitState::WaitCra),
                    vec![CommAction::StopCommDelayTimer, CommAction::SendS1F13],
                ),
                _ => (EqTransition::Stay(self.clone()), vec![]),
            },
        }
    }
}

// ============================================================================
// Display
// ============================================================================

impl fmt::Display for CommState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommState::Disabled => write!(f, "DISABLED"),
            CommState::Enabled(inner) => write!(f, "ENABLED/{}", inner),
        }
    }
}

impl fmt::Display for CommEnabledState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommEnabledState::NotCommunicating(inner) => {
                write!(f, "NOT_COMMUNICATING/{}", inner)
            }
            CommEnabledState::Communicating => write!(f, "COMMUNICATING"),
        }
    }
}

impl fmt::Display for CommConnectState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommConnectState::EquipmentInitiated(eq) => {
                write!(f, "EQ_INIT/{}", eq)
            }
            CommConnectState::HostInitiated => write!(f, "HOST_INIT"),
        }
    }
}

impl fmt::Display for EqInitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EqInitState::WaitCra => write!(f, "WAIT_CRA"),
            EqInitState::WaitDelay => write!(f, "WAIT_DELAY"),
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn eq_config() -> CommStateMachineConfig {
        CommStateMachineConfig {
            connect_mode: CommConnectMode::EquipmentInitiated,
            ..CommStateMachineConfig::default()
        }
    }

    fn host_config() -> CommStateMachineConfig {
        CommStateMachineConfig {
            connect_mode: CommConnectMode::HostInitiated,
            ..CommStateMachineConfig::default()
        }
    }

    fn wait_cra_eq() -> CommState {
        CommState::Enabled(CommEnabledState::NotCommunicating(
            CommConnectState::EquipmentInitiated(EqInitState::WaitCra),
        ))
    }

    fn wait_delay_eq() -> CommState {
        CommState::Enabled(CommEnabledState::NotCommunicating(
            CommConnectState::EquipmentInitiated(EqInitState::WaitDelay),
        ))
    }

    fn communicating() -> CommState {
        CommState::Enabled(CommEnabledState::Communicating)
    }

    fn host_init() -> CommState {
        CommState::Enabled(CommEnabledState::NotCommunicating(
            CommConnectState::HostInitiated,
        ))
    }

    // ========================================================================
    // #1: Initial state
    // ========================================================================

    #[test]
    fn test_initial_disabled() {
        let config = CommStateMachineConfig {
            initial_state: CommInitialState::Disabled,
            ..eq_config()
        };
        let control = CommControl::new(config);
        assert_eq!(control.state, CommState::Disabled);
    }

    #[test]
    fn test_initial_enabled_eq() {
        let config = CommStateMachineConfig {
            initial_state: CommInitialState::Enabled,
            ..eq_config()
        };
        let control = CommControl::new(config);
        assert_eq!(control.state, wait_cra_eq());
    }

    #[test]
    fn test_initial_enabled_host() {
        let config = CommStateMachineConfig {
            initial_state: CommInitialState::Enabled,
            ..host_config()
        };
        let control = CommControl::new(config);
        assert_eq!(control.state, host_init());
    }

    // ========================================================================
    // #2: DISABLED → ENABLED
    // ========================================================================

    #[test]
    fn test_transition_2_disabled_to_enabled_eq() {
        let c = eq_config();
        let s = CommState::Disabled;
        let (new_s, actions) = s.on_event(CommEvent::OperatorEnable, &c);
        assert_eq!(new_s, wait_cra_eq());
        assert_eq!(actions, vec![CommAction::SendS1F13]);
    }

    #[test]
    fn test_transition_2_disabled_to_enabled_host() {
        let c = host_config();
        let s = CommState::Disabled;
        let (new_s, actions) = s.on_event(CommEvent::OperatorEnable, &c);
        assert_eq!(new_s, host_init());
        assert!(actions.is_empty());
    }

    // ========================================================================
    // #3: ENABLED → DISABLED
    // ========================================================================

    #[test]
    fn test_transition_3_enabled_to_disabled() {
        let c = eq_config();
        let s = wait_cra_eq();
        let (new_s, actions) = s.on_event(CommEvent::OperatorDisable, &c);
        assert_eq!(new_s, CommState::Disabled);
        assert_eq!(
            actions,
            vec![CommAction::StopCommDelayTimer, CommAction::ClearAllTimers,]
        );
    }

    #[test]
    fn test_transition_3_communicating_to_disabled() {
        let c = eq_config();
        let s = communicating();
        let (new_s, actions) = s.on_event(CommEvent::OperatorDisable, &c);
        assert_eq!(new_s, CommState::Disabled);
        assert_eq!(
            actions,
            vec![CommAction::StopCommDelayTimer, CommAction::ClearAllTimers,]
        );
    }

    // ========================================================================
    // #5: Entry to EQ-Init → WaitCra (implicit via #2)
    // ========================================================================

    #[test]
    fn test_transition_5_implicit_send_s1f13() {
        let c = eq_config();
        let s = CommState::Disabled;
        let (_, actions) = s.on_event(CommEvent::OperatorEnable, &c);
        assert!(actions.contains(&CommAction::SendS1F13));
    }

    // ========================================================================
    // #6: WAIT_CRA → WAIT_DELAY
    // ========================================================================

    #[test]
    fn test_transition_6_t3_timeout() {
        let c = eq_config();
        let s = wait_cra_eq();
        let (new_s, actions) = s.on_event(CommEvent::T3Timeout, &c);
        assert_eq!(new_s, wait_delay_eq());
        assert_eq!(
            actions,
            vec![
                CommAction::StartCommDelayTimer,
                CommAction::DequeueAllMessages,
            ]
        );
    }

    #[test]
    fn test_transition_6_commack_rejected() {
        let c = eq_config();
        let s = wait_cra_eq();
        let (new_s, actions) = s.on_event(CommEvent::CommackRejected, &c);
        assert_eq!(new_s, wait_delay_eq());
        assert_eq!(
            actions,
            vec![
                CommAction::StartCommDelayTimer,
                CommAction::DequeueAllMessages,
            ]
        );
    }

    // ========================================================================
    // #7: WAIT_DELAY → WAIT_CRA (CommDelay expired)
    // ========================================================================

    #[test]
    fn test_transition_7_comm_delay_expired() {
        let c = eq_config();
        let s = wait_delay_eq();
        let (new_s, actions) = s.on_event(CommEvent::CommDelayExpired, &c);
        assert_eq!(new_s, wait_cra_eq());
        assert_eq!(actions, vec![CommAction::SendS1F13]);
    }

    // ========================================================================
    // #8: WAIT_DELAY → WAIT_CRA (Received any message)
    // ========================================================================

    #[test]
    fn test_transition_8_received_any_message() {
        let c = eq_config();
        let s = wait_delay_eq();
        let (new_s, actions) = s.on_event(CommEvent::ReceivedAnyMessageInDelay, &c);
        assert_eq!(new_s, wait_cra_eq());
        assert_eq!(
            actions,
            vec![CommAction::StopCommDelayTimer, CommAction::SendS1F13,]
        );
    }

    // ========================================================================
    // #9: WAIT_CRA → COMMUNICATING
    // ========================================================================

    #[test]
    fn test_transition_9_s1f14_accepted() {
        let c = eq_config();
        let s = wait_cra_eq();
        let (new_s, actions) = s.on_event(CommEvent::ReceivedS1F14Accepted, &c);
        assert_eq!(new_s, communicating());
        assert!(actions.is_empty());
    }

    // ========================================================================
    // #10: Entry to Host-Init → HostInitiated (implicit via #2)
    // ========================================================================

    #[test]
    fn test_transition_10_implicit_host_init() {
        let c = host_config();
        let s = CommState::Disabled;
        let (new_s, actions) = s.on_event(CommEvent::OperatorEnable, &c);
        assert_eq!(new_s, host_init());
        assert!(actions.is_empty());
    }

    // ========================================================================
    // #14: COMMUNICATING → NOT_COMMUNICATING
    // ========================================================================

    #[test]
    fn test_transition_14_eq_mode() {
        let c = eq_config();
        let s = communicating();
        let (new_s, actions) = s.on_event(CommEvent::CommunicationFailure, &c);
        assert_eq!(new_s, wait_cra_eq());
        assert_eq!(
            actions,
            vec![
                CommAction::ClearAllTimers,
                CommAction::DequeueAllMessages,
                CommAction::SendS1F13,
            ]
        );
    }

    #[test]
    fn test_transition_14_host_mode() {
        let c = host_config();
        let s = communicating();
        let (new_s, actions) = s.on_event(CommEvent::CommunicationFailure, &c);
        assert_eq!(new_s, host_init());
        assert_eq!(
            actions,
            vec![CommAction::ClearAllTimers, CommAction::DequeueAllMessages,]
        );
    }

    // ========================================================================
    // #15: HOST_INIT → COMMUNICATING
    // ========================================================================

    #[test]
    fn test_transition_15_received_s1f13() {
        let c = host_config();
        let s = host_init();
        let (new_s, actions) = s.on_event(CommEvent::ReceivedS1F13, &c);
        assert_eq!(new_s, communicating());
        assert_eq!(actions, vec![CommAction::SendS1F14Accepted]);
    }

    // ========================================================================
    // Full flow: Equipment-Initiated
    // ========================================================================

    #[test]
    fn test_full_eq_flow() {
        let c = eq_config();
        let s = CommState::Disabled;

        // #2: → Enabled/WaitCra
        let (s, actions) = s.on_event(CommEvent::OperatorEnable, &c);
        assert_eq!(s, wait_cra_eq());
        assert_eq!(actions, vec![CommAction::SendS1F13]);

        // #6: T3 timeout → WaitDelay
        let (s, actions) = s.on_event(CommEvent::T3Timeout, &c);
        assert_eq!(s, wait_delay_eq());
        assert!(actions.contains(&CommAction::StartCommDelayTimer));

        // #7: CommDelay expired → WaitCra
        let (s, actions) = s.on_event(CommEvent::CommDelayExpired, &c);
        assert_eq!(s, wait_cra_eq());
        assert_eq!(actions, vec![CommAction::SendS1F13]);

        // #9: S1F14 accepted → Communicating
        let (s, actions) = s.on_event(CommEvent::ReceivedS1F14Accepted, &c);
        assert_eq!(s, communicating());
        assert!(actions.is_empty());

        // #14: Communication failure → NotCommunicating
        let (s, actions) = s.on_event(CommEvent::CommunicationFailure, &c);
        assert_eq!(s, wait_cra_eq());
        assert!(actions.contains(&CommAction::ClearAllTimers));

        // #3: → Disabled
        let (s, actions) = s.on_event(CommEvent::OperatorDisable, &c);
        assert_eq!(s, CommState::Disabled);
        assert!(actions.contains(&CommAction::ClearAllTimers));
    }

    // ========================================================================
    // Full flow: Host-Initiated
    // ========================================================================

    #[test]
    fn test_full_host_flow() {
        let c = host_config();
        let s = CommState::Disabled;

        // #2: → HostInitiated
        let (s, _) = s.on_event(CommEvent::OperatorEnable, &c);
        assert_eq!(s, host_init());

        // #15: Received S1F13 → Communicating
        let (s, actions) = s.on_event(CommEvent::ReceivedS1F13, &c);
        assert_eq!(s, communicating());
        assert_eq!(actions, vec![CommAction::SendS1F14Accepted]);

        // #14: failure → HostInitiated
        let (s, _) = s.on_event(CommEvent::CommunicationFailure, &c);
        assert_eq!(s, host_init());
    }

    // ========================================================================
    // Ignored events
    // ========================================================================

    #[test]
    fn test_disabled_ignores_all_except_enable() {
        let c = eq_config();
        let s = CommState::Disabled;
        let events = vec![
            CommEvent::T3Timeout,
            CommEvent::CommackRejected,
            CommEvent::ReceivedS1F14Accepted,
            CommEvent::CommDelayExpired,
            CommEvent::ReceivedAnyMessageInDelay,
            CommEvent::ReceivedS1F13,
            CommEvent::CommunicationFailure,
            CommEvent::OperatorDisable,
        ];
        for event in events {
            let (new_s, actions) = s.on_event(event, &c);
            assert_eq!(new_s, CommState::Disabled);
            assert!(actions.is_empty());
        }
    }

    #[test]
    fn test_communicating_ignores_irrelevant_events() {
        let c = eq_config();
        let s = communicating();
        let events = vec![
            CommEvent::T3Timeout,
            CommEvent::CommackRejected,
            CommEvent::ReceivedS1F14Accepted,
            CommEvent::CommDelayExpired,
            CommEvent::ReceivedAnyMessageInDelay,
            CommEvent::ReceivedS1F13,
            CommEvent::OperatorEnable,
        ];
        for event in events {
            let (new_s, actions) = s.on_event(event.clone(), &c);
            assert_eq!(new_s, communicating());
            assert!(actions.is_empty());
        }
    }

    // ========================================================================
    // CommControl wrapper
    // ========================================================================

    #[test]
    fn test_comm_control_full_cycle() {
        let mut control = CommControl::new(eq_config());
        assert_eq!(control.state, CommState::Disabled);
        assert!(!control.is_communicating());
        assert!(!control.is_enabled());

        let actions = control.handle_event(CommEvent::OperatorEnable);
        assert!(control.is_enabled());
        assert!(!control.is_communicating());
        assert!(actions.contains(&CommAction::SendS1F13));

        let actions = control.handle_event(CommEvent::ReceivedS1F14Accepted);
        assert!(control.is_communicating());
        assert!(actions.is_empty());

        let actions = control.handle_event(CommEvent::CommunicationFailure);
        assert!(!control.is_communicating());
        assert!(actions.contains(&CommAction::SendS1F13));
    }

    #[test]
    fn test_comm_control_retry_cycle() {
        let mut control = CommControl::new(eq_config());

        control.handle_event(CommEvent::OperatorEnable);

        // Fail → WaitDelay
        let actions = control.handle_event(CommEvent::T3Timeout);
        assert!(actions.contains(&CommAction::StartCommDelayTimer));

        // WaitDelay → WaitCra (retry)
        let actions = control.handle_event(CommEvent::CommDelayExpired);
        assert!(actions.contains(&CommAction::SendS1F13));

        // Fail again → WaitDelay
        let actions = control.handle_event(CommEvent::CommackRejected);
        assert!(actions.contains(&CommAction::StartCommDelayTimer));

        // Message during delay → immediate retry
        let actions = control.handle_event(CommEvent::ReceivedAnyMessageInDelay);
        assert!(actions.contains(&CommAction::StopCommDelayTimer));
        assert!(actions.contains(&CommAction::SendS1F13));

        // Finally succeed
        let actions = control.handle_event(CommEvent::ReceivedS1F14Accepted);
        assert!(actions.is_empty());
        assert!(control.is_communicating());
    }

    // ========================================================================
    // Display
    // ========================================================================

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", CommState::Disabled), "DISABLED");
        assert_eq!(
            format!("{}", wait_cra_eq()),
            "ENABLED/NOT_COMMUNICATING/EQ_INIT/WAIT_CRA"
        );
        assert_eq!(
            format!("{}", wait_delay_eq()),
            "ENABLED/NOT_COMMUNICATING/EQ_INIT/WAIT_DELAY"
        );
        assert_eq!(format!("{}", communicating()), "ENABLED/COMMUNICATING");
        assert_eq!(
            format!("{}", host_init()),
            "ENABLED/NOT_COMMUNICATING/HOST_INIT"
        );
    }
}
