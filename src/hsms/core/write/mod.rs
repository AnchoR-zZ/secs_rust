//! Pure contracts and lifecycle state for generation-local outbound writes.
//!
//! The contract is frozen independently of the later bounded `WriteLedger`
//! implementation so Core and runtime coordination share one fence vocabulary.

mod contract;
mod ledger;

/// Frozen write contracts re-exported for later ledger and reducer tasks.
#[allow(unused_imports)]
pub(crate) use contract::{
    BeginWriteHook, FenceResolution, WritePhase, WriteSpec, WriteTerminalOutcome,
};
