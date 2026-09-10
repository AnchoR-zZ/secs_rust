//! Runtime-neutral exit and cleanup evidence for one connection generation.
//! The concrete Supervisor retains ownership until this evidence permits release.

use crate::hsms::{model::runtime::GenerationCloseReason, ConnectionGeneration};

/// Reason cleanup could not prove that a generation released all resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CleanupPoison {
    /// At least one generation task failed to terminate within cleanup policy.
    TaskDidNotStop,
    /// Cleanup observed an impossible ownership or lifecycle state.
    InvariantViolation,
}

/// Proof result produced after all generation cleanup steps run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CleanupResult {
    /// Every owned task and transport resource was proven released.
    Clean,
    /// Cleanup failed for the attached stable reason.
    Poisoned(CleanupPoison),
}

/// Complete terminal report for one launched generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionExit {
    /// TCP incarnation whose SessionDriver exited.
    pub(crate) generation: ConnectionGeneration,
    /// Event that initiated or forced termination.
    pub(crate) reason: GenerationCloseReason,
    /// Proof of whether all owned resources were released.
    pub(crate) cleanup: CleanupResult,
}
