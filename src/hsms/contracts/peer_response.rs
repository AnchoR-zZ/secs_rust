//! Narrow crate-internal façade for peer-response write coordination.
//!
//! The Control FSM uses these move-only authorities to bind one response
//! occurrence to its deferred state transition. Keeping this façade separate
//! prevents Core modules from depending on the complete write-contract
//! implementation or its lifecycle internals.

#[allow(unused_imports)]
pub(crate) use super::write::{
    ForeignPeerResponseCommit, PeerResponseCommit, PeerResponseCommitIssuer,
    PeerResponseCommitReceipt, PeerResponseIssueError, PendingPeerResponseWrite,
};
