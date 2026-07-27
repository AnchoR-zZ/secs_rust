//! Inbound W=1 reply-contract and bounded capability-lifecycle boundary.
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
/// Bounded capability-ledger values used by later CoreResources assembly.
#[allow(unused_imports)]
pub(crate) use ledger::{
    AuthorizedAbandonReplyPlan, AuthorizedAbortReplyPlan, AuthorizedNormalReplyPlan,
    ReplyCapabilityLedger, ReplyCapabilityState, ReplyLedgerConfigError, ReplyPublicationDecision,
    ReplyPublicationTicket, ReplyReservation, ReplyReserveError, ReplyResetSummary,
    ReplyRevocationCommitError, ReplyRevocationPlan, ReplyRevocationTerminal,
    ReplyRevocationUnavailable, ReplyUseAuthorization, ReplyUseCommitError, ReplyUseKind,
    ReplyUsePlan, ReplyUseTerminal, ReplyUseUnavailable,
};
