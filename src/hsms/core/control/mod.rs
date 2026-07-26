//! Pure Select, Deselect, Linktest, Separate, T6, and T7 state decisions.
//!
//! The reducer owns only stable selection state, local selection overlays, the
//! peer-Deselect gate barrier, and the exact T7 registration. `HsmsCore`
//! remains responsible for identifiers, transaction and write registries, and
//! translating these decisions into ordered runtime effects.

mod fsm;

#[allow(unused_imports)]
pub(crate) use fsm::{
    CloseBarrier, ControlAction, ControlDecision, ControlFsm, ControlInvariantError,
    ControlTimeoutDecision, LocalControlPlan, LocalResponseDecision, MatchedControlResponse,
    OverlayTerminalDecision, PeerRequestDecision, PeerResponseCommit, PeerResponsePlan,
    SelectionOverlay,
};
