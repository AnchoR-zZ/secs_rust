//! Private Reply child of the publication resource subaggregate.
//!
//! The child owns pending and available capability state. Only the parent
//! publication facade can reach its ledger types or coordinate mutation.

mod ledger;

/// Selected-reset versus generation-end scope shared by clear requests and receipts.
pub(super) use super::authority::PublicationClearScope as ReplyClearScope;

/// Exposes bounded capability-ledger values only to the parent publication facade.
#[allow(unused_imports)]
pub(super) use ledger::{
    AuthorizedAbandonReplyPlan, AuthorizedAbortReplyPlan, AuthorizedNormalReplyPlan,
    ReplyCapabilityLedger, ReplyCapabilityState, ReplyClearCommitError, ReplyClearCommitFailure,
    ReplyClearPrepareError, ReplyClearPrepareFailure, ReplyClearReceipt, ReplyClearRequest,
    ReplyClearValidationError, ReplyClearValidationFailure, ReplyGenerationEndCommit,
    ReplyGenerationEndPreparation, ReplyLedgerConfigError, ReplyLedgerIdentity,
    ReplyPublicationDecision, ReplyPublicationTicket, ReplyPublicationTicketIdentity,
    ReplyReservation, ReplyReserveError, ReplyResetSummary, ReplyRevocationCommitError,
    ReplyRevocationPlan, ReplyRevocationTerminal, ReplyRevocationUnavailable,
    ReplySelectedResetCommit, ReplySelectedResetPreparation, ReplyUseCommitFailure, ReplyUsePlan,
};
