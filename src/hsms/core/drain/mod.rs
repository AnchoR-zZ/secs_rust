//! Generation-close barriers resolved from semantic operations to exact writes.
//!
//! `ControlFsm` names a local Separate operation. `HsmsCore` must resolve that
//! operation through WriteLedger once, freeze the exact `WriteId`, and retain
//! this value until only that write's terminal outcome releases transport close.

use crate::hsms::model::{
    ids::{OperationId, WriteId},
    runtime::GenerationCloseReason,
};

mod coordinator;

/// Exact writer boundary retained while local Separate drains.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WriteBarrier {
    /// Local Separate operation that requested generation close.
    operation_id: OperationId,
    /// Exact outbound write whose terminal result releases close.
    write_id: WriteId,
}

impl WriteBarrier {
    /// Creates an already-validated operation-to-write close barrier.
    ///
    /// Core may call this only after WriteLedger proves the active mapping.
    pub(crate) const fn new(operation_id: OperationId, write_id: WriteId) -> Self {
        Self {
            operation_id,
            write_id,
        }
    }

    /// Returns the semantic local Separate operation.
    pub(crate) const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Returns the exact write whose terminal result releases close.
    pub(crate) const fn write_id(self) -> WriteId {
        self.write_id
    }
}

/// Runtime-resolved boundary controlling when transport close may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ResolvedCloseBarrier {
    /// No outbound write must finish before transport close.
    Immediate,
    /// Close waits for one exact local Separate write terminal.
    AfterWrite(
        /// Frozen operation-to-write mapping.
        WriteBarrier,
    ),
}

/// First-writer-wins generation-close request retained by DrainCoordinator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DrainRequest {
    /// Stable first reason retained through the unique close request.
    reason: GenerationCloseReason,
    /// Immediate or exact-write transport-close boundary.
    barrier: ResolvedCloseBarrier,
}

impl DrainRequest {
    /// Creates a resolved, immutable generation-close request.
    pub(crate) const fn new(reason: GenerationCloseReason, barrier: ResolvedCloseBarrier) -> Self {
        Self { reason, barrier }
    }

    /// Returns the first stable reason for generation close.
    pub(crate) const fn reason(self) -> GenerationCloseReason {
        self.reason
    }

    /// Returns the exact resolved transport-close boundary.
    pub(crate) const fn barrier(self) -> ResolvedCloseBarrier {
        self.barrier
    }
}

#[cfg(test)]
mod tests {
    use crate::hsms::{
        core::drain::{DrainRequest, ResolvedCloseBarrier, WriteBarrier},
        model::{
            ids::{OperationId, WriteId},
            runtime::GenerationCloseReason,
        },
    };

    /// Confirms a local Separate close request retains the exact semantic and
    /// writer identities required to resist premature operation completion.
    #[test]
    fn resolved_separate_barrier_preserves_exact_write_identity() {
        let barrier = WriteBarrier::new(OperationId::new(5), WriteId::new(7));
        let request = DrainRequest::new(
            GenerationCloseReason::LocalSeparate,
            ResolvedCloseBarrier::AfterWrite(barrier),
        );

        assert_eq!(request.reason(), GenerationCloseReason::LocalSeparate);
        assert_eq!(barrier.operation_id(), OperationId::new(5));
        assert_eq!(barrier.write_id(), WriteId::new(7));
        assert_eq!(request.barrier(), ResolvedCloseBarrier::AfterWrite(barrier));
    }
}
