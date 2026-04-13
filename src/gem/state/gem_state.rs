use super::comm_state::{CommAction, CommEvent, CommState, CommStateMachineConfig};
use super::control_state::{
    ControlState, GemOfflineState, InitialControlOption, StateEvent, StateMachineConfig,
};

#[derive(Debug, Clone, PartialEq)]
pub enum DeviceState {
    NotConnected,
    NotSelected,
    Selected {
        comm_state: CommState,
        control_state: ControlState,
    },
}

impl serde::Serialize for DeviceState {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        match self {
            DeviceState::NotConnected => serializer.serialize_str("NotConnected"),
            DeviceState::NotSelected => serializer.serialize_str("NotSelected"),
            DeviceState::Selected {
                comm_state,
                control_state,
            } => {
                let mut s = serializer.serialize_struct("Selected", 2)?;
                s.serialize_field("commState", &comm_state)?;
                s.serialize_field("controlState", &control_state)?;
                s.end()
            }
        }
    }
}

impl DeviceState {
    pub fn on_control_event(&self, event: StateEvent, config: &StateMachineConfig) -> DeviceState {
        match self {
            DeviceState::NotConnected => match event {
                StateEvent::SocketConnectedEvent => DeviceState::NotSelected,
                _ => self.clone(),
            },

            DeviceState::NotSelected => match event {
                StateEvent::SocketDisconnectedEvent => DeviceState::NotConnected,
                StateEvent::SelectEvent => {
                    let control_state = match config.initial_control_state {
                        InitialControlOption::OffLine => {
                            ControlState::OffLineState(config.initial_offline_substate.clone())
                        }
                        InitialControlOption::OnLine => {
                            ControlState::OffLineState(GemOfflineState::AttemptOnLine)
                        }
                    };
                    DeviceState::Selected {
                        comm_state: CommState::Disabled,
                        control_state,
                    }
                }
                _ => self.clone(),
            },

            DeviceState::Selected {
                comm_state,
                control_state,
            } => match event {
                StateEvent::SocketDisconnectedEvent => DeviceState::NotConnected,
                StateEvent::DisSelectEvent => DeviceState::NotSelected,
                _ => DeviceState::Selected {
                    comm_state: comm_state.clone(),
                    control_state: control_state.on_event(event, config),
                },
            },
        }
    }

    pub fn on_comm_event(
        &self,
        event: CommEvent,
        config: &CommStateMachineConfig,
    ) -> (DeviceState, Vec<CommAction>) {
        match self {
            DeviceState::Selected {
                comm_state,
                control_state,
            } => {
                let (new_comm, actions) = comm_state.on_event(event, config);
                (
                    DeviceState::Selected {
                        comm_state: new_comm,
                        control_state: control_state.clone(),
                    },
                    actions,
                )
            }
            _ => (self.clone(), vec![]),
        }
    }

    pub fn comm_state(&self) -> &CommState {
        match self {
            DeviceState::Selected { comm_state, .. } => comm_state,
            _ => &CommState::Disabled,
        }
    }

    pub fn control_state(&self) -> Option<&ControlState> {
        match self {
            DeviceState::Selected { control_state, .. } => Some(control_state),
            _ => None,
        }
    }

    pub fn is_connected(&self) -> bool {
        !matches!(self, DeviceState::NotConnected)
    }

    pub fn is_selected(&self) -> bool {
        matches!(self, DeviceState::Selected { .. })
    }

    pub fn is_communicating(&self) -> bool {
        matches!(
            self,
            DeviceState::Selected {
                comm_state: CommState::Enabled(super::comm_state::CommEnabledState::Communicating),
                ..
            }
        )
    }

    pub fn is_online(&self) -> bool {
        matches!(
            self,
            DeviceState::Selected {
                control_state: ControlState::OnlineState(_),
                ..
            }
        )
    }

    pub fn is_offline(&self) -> bool {
        matches!(
            self,
            DeviceState::Selected {
                control_state: ControlState::OffLineState(_),
                ..
            }
        )
    }
}

impl std::fmt::Display for DeviceState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceState::NotConnected => write!(f, "Not Connected"),
            DeviceState::NotSelected => write!(f, "Not Selected"),
            DeviceState::Selected {
                comm_state,
                control_state,
            } => {
                write!(
                    f,
                    "Selected(Comm={}, Control={})",
                    comm_state, control_state
                )
            }
        }
    }
}
