//! Shared leaf values for scheduling, writer fences, and control-response commits.
//!
//! These data-only enums break dependency cycles between the control FSM,
//! WriteLedger contracts, Core I/O, and the future generation scheduler.

/// Scheduler lane selected for an outbound frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WriteClass {
    /// Reserved lane for control and termination-critical traffic.
    Critical,
    /// Bounded lane gated by the Core's Selected state.
    Data,
}

/// Scheduler gate state applied synchronously to outbound Data scheduling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DataGateState {
    /// Allow newly scheduled Data frames to enter the bounded Data lane.
    Open,
    /// Reject newly scheduled Data frames while preserving critical traffic.
    Closed,
}

/// Stable reasons why the scheduler could not reserve a wire position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScheduleFailure {
    /// The selected scheduler lane had no remaining bounded capacity.
    CapacityExhausted,
    /// Data scheduling was rejected because the Data Gate was closed.
    DataGateClosed,
    /// The generation scheduler had already stopped accepting new work.
    SchedulerStopped,
}

/// Selection transition committed only at the exact peer-response write fence.
#[must_use = "peer response commit tokens must be retained through BeginWrite"]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PeerResponseCommit {
    /// The response fence carries no stable selection transition.
    None,
    /// A successful `Select.rsp` fence may commit selection.
    SelectAccepted,
    /// A successful `Deselect.rsp` fence commits the accepted peer downgrade.
    DeselectAccepted,
}
