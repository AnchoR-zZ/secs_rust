//! Bounded semantic operation ownership and outbound Reject correlation.
//!
//! `OperationLedger` is the sole first-terminal-wins and command-completion
//! authority. Its private correlation child supplies global unique Reject
//! attribution without owning transaction or write implementations.

mod correlation;
mod ledger;

/// Operation contracts and decisions used by later CoreResources integration.
#[allow(unused_imports)]
pub(crate) use ledger::{
    ActiveOperationWrite, ActiveWriteReleaseDecision, CompletionTarget, LifecycleOperationDecision,
    LifecycleTerminalStatus, OperationClaimCause, OperationCloseDecision, OperationLedger,
    OperationLedgerBuildError, OperationPurpose, OperationRegisterError,
    OperationRegistrationRejection, OperationRegistrationToken, OperationRetention, OperationScope,
    OperationSessionResetDecision, OperationSnapshot, OperationSpec, OperationSpecError,
    OperationTerminalCause, OperationVisibilityDecision, PeerRejectOperationCommit,
    RegistryReleaseDecision, RejectCommitToken, RejectDiscoveryDecision, RejectTokenInvalidity,
    RejectValidationDecision, TerminalClaimDecision, TerminalCorrelationRetention,
    ValidatedRejectCommitToken,
};
