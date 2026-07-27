//! Generation-scoped System Bytes allocation, pending operation ownership, T3
//! and T6 correlation, tombstones, and compiled response matching.
//!
//! The registry is a single-threaded Sans-I/O component. It returns structured
//! protocol decisions and never executes timers, command completions, or Core
//! effects itself.

mod allocator;
mod matcher;
mod registry;

#[allow(unused_imports)]
pub(crate) use matcher::{MatcherBuildError, MatcherDecision, MismatchField, ResponseMatcher};
pub(crate) use registry::PeerRejectRegistryRelease;
#[allow(unused_imports)]
pub(crate) use registry::{
    CloseDecision, CloseOperation, CollisionSource, CommitDecision, ControlCollision,
    ControlCorrelation, ControlKind, ControlTakeDecision, ExpiryDecision, FinishDecision,
    InboundDataDecision, MarkVisibleDecision, OneWayKind, OperationClass, OperationDisposition,
    OperationVisibility, PeerRejectFinishDecision, PrematureDataMatch, RegistryBuildError,
    RegistryOperationState, ReserveError, ReservedControl, ReservedOneWay, ReservedRequest,
    SessionResetDecision, SessionResetOperation, TombstoneArrival, TombstoneCategory,
    TransactionRegistry,
};

#[cfg(test)]
mod reexport_tests {
    use super::{PeerRejectFinishDecision, PrematureDataMatch, TombstoneCategory};

    /// Proves sibling Core modules can name both registry decisions through the
    /// transaction boundary without reaching into the private registry module.
    #[test]
    fn registry_decision_reexports_are_sibling_reachable() {
        assert!(matches!(
            PrematureDataMatch::Secondary,
            PrematureDataMatch::Secondary
        ));
        assert!(matches!(
            PeerRejectFinishDecision::AlreadyTerminal {
                category: TombstoneCategory::PeerRejected,
            },
            PeerRejectFinishDecision::AlreadyTerminal {
                category: TombstoneCategory::PeerRejected,
            }
        ));
    }
}
