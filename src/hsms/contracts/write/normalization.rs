//! Conservative normalization of raw writer callbacks.
//!
//! This module keeps runtime observations distinct from the effective result
//! Core is allowed to use, while retaining every lifecycle contradiction as a
//! structured `WriteRuntimeViolation`.

use crate::hsms::model::runtime::{
    EffectiveWriteResult, TransportFaultKind, WriteResult, WriteRuntimeViolation,
};

/// Observable lifecycle phase of one registered write.
///
/// This enum is data rather than permission and may therefore remain `Copy`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WritePhase {
    /// Core emitted scheduling work and awaits admission acknowledgement.
    Scheduling,
    /// Runtime admitted the write but has not raised the BeginWrite fence.
    Queued,
    /// BeginWrite arrived and its semantic hook remains unresolved.
    Fenced,
    /// Core resolved the fence by allowing the writer to proceed.
    Proceeded,
    /// Core resolved the fence by requesting cancellation.
    Aborting,
}

/// Normalizes one raw runtime result without erasing contract contradictions.
///
/// `phase` is the ledger phase at callback time, `observed_may_be_visible`
/// records whether the runtime previously raised the visibility watermark, and
/// `raw` is the unchanged runtime callback. The return value contains the
/// conservative effective result plus an optional violation that must be
/// retained in the terminal receipt.
///
/// `TransportFaultKind::Cancelled` may be produced either by an exact
/// `AbortWrite` permission or by generation-wide runtime cancellation. This
/// normalizer treats it only as evidence that an already-aborting write ended
/// without bytes; the surrounding WriteLedger and close coordinator remain
/// responsible for validating why cancellation was active.
pub(crate) fn normalize_runtime_result(
    phase: WritePhase,
    observed_may_be_visible: bool,
    raw: &WriteResult,
) -> (EffectiveWriteResult, Option<WriteRuntimeViolation>) {
    let effective = match raw {
        WriteResult::Committed => EffectiveWriteResult::Committed,
        WriteResult::NotWritten(fault) if observed_may_be_visible => {
            EffectiveWriteResult::Indeterminate(fault.clone())
        }
        WriteResult::NotWritten(fault) => EffectiveWriteResult::NotWritten(fault.clone()),
        WriteResult::Indeterminate(fault) => EffectiveWriteResult::Indeterminate(fault.clone()),
    };

    let violation = match phase {
        WritePhase::Scheduling => Some(WriteRuntimeViolation::TerminalWhileScheduling),
        WritePhase::Queued => Some(WriteRuntimeViolation::TerminalWhileQueued),
        WritePhase::Fenced => Some(WriteRuntimeViolation::TerminalWithUnresolvedFence),
        WritePhase::Proceeded => match raw {
            WriteResult::NotWritten(_) if observed_may_be_visible => {
                Some(WriteRuntimeViolation::NotWrittenAfterMayBeVisible)
            }
            _ => None,
        },
        WritePhase::Aborting => match raw {
            WriteResult::NotWritten(fault) if observed_may_be_visible => {
                Some(WriteRuntimeViolation::AbortingNotWrittenAfterMayBeVisible {
                    actual: fault.kind,
                })
            }
            WriteResult::NotWritten(fault) if fault.kind == TransportFaultKind::Cancelled => None,
            WriteResult::NotWritten(fault) => {
                Some(WriteRuntimeViolation::AbortingNotCancelled { actual: fault.kind })
            }
            WriteResult::Committed => Some(WriteRuntimeViolation::AbortingCommitted),
            WriteResult::Indeterminate(_) => Some(WriteRuntimeViolation::AbortingIndeterminate),
        },
    };

    (effective, violation)
}

#[cfg(test)]
mod tests {
    use crate::hsms::model::runtime::{
        EffectiveWriteResult, TransportFault, TransportFaultKind, WriteResult,
        WriteRuntimeViolation,
    };

    use super::{normalize_runtime_result, WritePhase};

    /// Converts contradictory post-visibility `NotWritten` into indeterminate.
    #[test]
    fn visible_not_written_is_conservative_and_retains_violation() {
        let raw = WriteResult::NotWritten(TransportFault {
            kind: TransportFaultKind::BrokenPipe,
            context: "writer callback",
        });
        let (effective, violation) = normalize_runtime_result(WritePhase::Proceeded, true, &raw);
        assert!(matches!(effective, EffectiveWriteResult::Indeterminate(_)));
        assert_eq!(
            violation,
            Some(WriteRuntimeViolation::NotWrittenAfterMayBeVisible)
        );
    }

    /// Accepts only definite cancellation as a normal aborting completion.
    #[test]
    fn aborting_accepts_only_cancelled_not_written() {
        let cancelled = WriteResult::NotWritten(TransportFault {
            kind: TransportFaultKind::Cancelled,
            context: "cancel acknowledgement",
        });
        let (_, violation) = normalize_runtime_result(WritePhase::Aborting, false, &cancelled);
        assert_eq!(violation, None);

        let broken_pipe = WriteResult::NotWritten(TransportFault {
            kind: TransportFaultKind::BrokenPipe,
            context: "cancel acknowledgement",
        });
        let (_, violation) = normalize_runtime_result(WritePhase::Aborting, false, &broken_pipe);
        assert_eq!(
            violation,
            Some(WriteRuntimeViolation::AbortingNotCancelled {
                actual: TransportFaultKind::BrokenPipe,
            })
        );
    }

    /// Retains a violation when terminal completion bypasses an unresolved fence.
    #[test]
    fn unresolved_fence_terminal_is_never_silently_normalized() {
        let raw = WriteResult::Committed;
        let (effective, violation) = normalize_runtime_result(WritePhase::Fenced, false, &raw);
        assert_eq!(effective, EffectiveWriteResult::Committed);
        assert_eq!(
            violation,
            Some(WriteRuntimeViolation::TerminalWithUnresolvedFence)
        );
    }

    /// Allows a proceeded writer to prove no bytes were written before visibility.
    #[test]
    fn proceeded_before_visibility_accepts_definite_not_written() {
        let raw = WriteResult::NotWritten(TransportFault {
            kind: TransportFaultKind::BrokenPipe,
            context: "failed before first byte",
        });
        let (effective, violation) = normalize_runtime_result(WritePhase::Proceeded, false, &raw);
        assert!(matches!(effective, EffectiveWriteResult::NotWritten(_)));
        assert_eq!(violation, None);
    }

    /// Marks every non-cancellation terminal result during abort as a violation.
    #[test]
    fn aborting_committed_and_indeterminate_are_violations() {
        let (_, committed_violation) =
            normalize_runtime_result(WritePhase::Aborting, false, &WriteResult::Committed);
        assert_eq!(
            committed_violation,
            Some(WriteRuntimeViolation::AbortingCommitted)
        );

        let indeterminate = WriteResult::Indeterminate(TransportFault {
            kind: TransportFaultKind::ConnectionReset,
            context: "cancel race",
        });
        let (_, indeterminate_violation) =
            normalize_runtime_result(WritePhase::Aborting, false, &indeterminate);
        assert_eq!(
            indeterminate_violation,
            Some(WriteRuntimeViolation::AbortingIndeterminate)
        );
    }

    /// Distinguishes terminal callbacks that arrive in each premature phase.
    #[test]
    fn premature_terminal_phases_keep_exact_violation_kind() {
        for (phase, expected) in [
            (
                WritePhase::Scheduling,
                WriteRuntimeViolation::TerminalWhileScheduling,
            ),
            (
                WritePhase::Queued,
                WriteRuntimeViolation::TerminalWhileQueued,
            ),
            (
                WritePhase::Fenced,
                WriteRuntimeViolation::TerminalWithUnresolvedFence,
            ),
        ] {
            let (_, violation) = normalize_runtime_result(phase, false, &WriteResult::Committed);
            assert_eq!(violation, Some(expected));
        }
    }
}
