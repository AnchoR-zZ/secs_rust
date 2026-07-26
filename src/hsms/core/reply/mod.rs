//! Inbound W=1 reply-capability contracts and future bounded ledger boundary.
//!
//! Header correlation and the admission hint remain crate-private while the
//! public token exposes only opaque capability metadata.

mod contract;
mod ledger;

/// Frozen reply contracts re-exported for later ledger and reducer tasks.
#[allow(unused_imports)]
pub(crate) use contract::{
    NormalSecondaryUnavailable, ReplyCapabilityMode, ReplyContract, ReplyContractError,
};
