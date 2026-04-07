
use super::control_state::{
    GemOfflineState, GemOnlineState, GemState, InitialControlOption, StateEvent, StateMachineConfig,
};

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceState {
    NotConnected,
    NotSelected,
    Selected(GemState),
}

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

    pub fn is_connected(&self) -> bool {
        !matches!(self, DeviceState::NotConnected)
    }

    pub fn is_selected(&self) -> bool {
        matches!(self, DeviceState::Selected(_))
    }

    pub fn is_online(&self) -> bool {
        matches!(self, DeviceState::Selected(GemState::OnlineState(_)))
    }

    pub fn is_offline(&self) -> bool {
        matches!(self, DeviceState::Selected(GemState::OffLineState(_)))
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
