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
#[allow(unused_imports)]
pub(crate) use registry::{
    CloseDecision, CloseOperation, CollisionSource, CommitDecision, ControlCollision, ControlKind,
    ControlTakeDecision, ExpiryDecision, FinishDecision, InboundDataDecision, MarkVisibleDecision,
    OneWayKind, OperationClass, OperationVisibility, RegistryBuildError, ReserveError,
    ReservedControl, ReservedOneWay, ReservedRequest, TombstoneArrival, TombstoneCategory,
    TransactionRegistry,
};
