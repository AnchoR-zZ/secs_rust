//! Immutable values exchanged with the future generation-local WriteLedger.
//!
//! These values describe reservation, BeginWrite hooks, fence resolution, and
//! terminal outcomes without fixing the ledger's maps or state representation.

use crate::hsms::{
    contracts::{PeerResponseCommit, ScheduleFailure, WriteClass},
    model::{
        ids::{OperationId, WriteId},
        runtime::WriteResult,
    },
};

/// Deferred semantic transition attached to one exact writer fence.
///
/// Core may consume the hook only while atomically resolving the matching
/// `BeginWrite` fence to [`FenceResolution::Proceed`]. If scheduling fails, the
/// fence is aborted, or hook commit fails, WriteLedger returns the uncommitted
/// hook in [`WriteTerminalOutcome`]; Core must then begin immediate generation
/// close before processing another input. This prevents a mandatory peer
/// response from leaving protocol state half-transitioned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BeginWriteHook {
    /// Commit a typed peer control response before authorizing its bytes.
    PeerResponse(
        /// Selection transition retained until the response reaches its fence.
        PeerResponseCommit,
    ),
}

/// Immutable description used to reserve one outbound frame write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriteSpec {
    /// Core-assigned identity of the outbound frame.
    write_id: WriteId,
    /// Core operation that owns this frame.
    operation_id: OperationId,
    /// Independently bounded scheduler lane occupied by the frame.
    class: WriteClass,
    /// Optional semantic work deferred until the exact BeginWrite fence.
    begin_hook: Option<BeginWriteHook>,
}

impl WriteSpec {
    /// Creates a complete immutable write reservation description.
    pub(crate) const fn new(
        write_id: WriteId,
        operation_id: OperationId,
        class: WriteClass,
        begin_hook: Option<BeginWriteHook>,
    ) -> Self {
        Self {
            write_id,
            operation_id,
            class,
            begin_hook,
        }
    }

    /// Returns the Core-assigned write identity.
    pub(crate) const fn write_id(self) -> WriteId {
        self.write_id
    }

    /// Returns the Core operation that owns this write.
    pub(crate) const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Returns the independently bounded scheduler lane.
    pub(crate) const fn class(self) -> WriteClass {
        self.class
    }

    /// Returns semantic work deferred until the exact BeginWrite fence.
    pub(crate) const fn begin_hook(self) -> Option<BeginWriteHook> {
        self.begin_hook
    }
}

/// Resolution applied exactly once to a validated BeginWrite fence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FenceResolution {
    /// Authorize transport access after all semantic prerequisites commit.
    Proceed,
    /// Preserve definite non-visibility and terminate the write as cancelled.
    Abort,
}

/// Stable diagnostic phase exposed by WriteLedger decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WritePhase {
    /// Core registered the write and is waiting for scheduler completion.
    Scheduling,
    /// Scheduler assigned a wire position not yet presented at the writer fence.
    Queued,
    /// Writer is stopped at the exact BeginWrite fence.
    Fenced,
    /// Core authorized transport visibility.
    Proceeded,
    /// Core rejected transport visibility and awaits cancellation completion.
    Aborting,
}

/// Terminal source consumed exactly once by Core orchestration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WriteTerminalOutcome {
    /// Scheduler rejected the write before assigning a wire position.
    ScheduleFailed {
        /// Stable scheduling failure returned by runtime.
        failure: ScheduleFailure,
        /// Deferred transition that never reached its exact writer fence.
        ///
        /// A present hook requires immediate generation close.
        uncommitted_hook: Option<BeginWriteHook>,
    },
    /// A resolved writer fence reached its final visibility outcome.
    Finished {
        /// Committed, definitely-not-written, or indeterminate result.
        result: WriteResult,
        /// Deferred transition not committed before the write terminated.
        ///
        /// This is `None` after a valid Proceed. A present hook after Abort or
        /// an invariant failure requires immediate generation close.
        uncommitted_hook: Option<BeginWriteHook>,
    },
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        contracts::{PeerResponseCommit, ScheduleFailure, WriteClass},
        core::write::{BeginWriteHook, WriteSpec, WriteTerminalOutcome},
        model::ids::{OperationId, WriteId},
    };

    /// Confirms an immutable write specification preserves independent write,
    /// operation, lane, and optional hook fields.
    #[test]
    fn write_spec_preserves_reservation_contract() {
        let spec = WriteSpec::new(
            WriteId::new(5),
            OperationId::new(7),
            WriteClass::Critical,
            None,
        );

        assert_eq!(spec.write_id(), WriteId::new(5));
        assert_eq!(spec.operation_id(), OperationId::new(7));
        assert_eq!(spec.class(), WriteClass::Critical);
        assert!(spec.begin_hook().is_none());
    }

    /// Confirms a pre-fence terminal result retains its deferred semantic hook
    /// so Core cannot silently continue in a half-transitioned protocol state.
    #[test]
    fn schedule_failure_returns_uncommitted_begin_write_hook() {
        let hook = BeginWriteHook::PeerResponse(PeerResponseCommit::None);
        let outcome = WriteTerminalOutcome::ScheduleFailed {
            failure: ScheduleFailure::CapacityExhausted,
            uncommitted_hook: Some(hook),
        };

        assert_eq!(
            outcome,
            WriteTerminalOutcome::ScheduleFailed {
                failure: ScheduleFailure::CapacityExhausted,
                uncommitted_hook: Some(hook),
            }
        );
    }
}
