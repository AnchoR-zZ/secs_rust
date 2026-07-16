//! Immutable endpoint and session state exposed to application observers.
//!
//! The future `LifecycleCell` owns mutation and linearization. This file only
//! defines copyable snapshots and state vocabulary shared across boundaries.

use crate::hsms::{model::ids::LifecycleSequence, ConnectionGeneration};

/// What the caller wants the long-lived endpoint to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunningIntent {
    /// The application wants the logical endpoint fully stopped.
    Stopped,
    /// The application wants the endpoint supervising a connection.
    Running,
}

/// Coarse endpoint lifecycle phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointPhase {
    /// No generation is open and cleanup has proven all resources released.
    StoppedClean,
    /// Startup or initial connection supervision is being established.
    Starting,
    /// The endpoint supervisor is active, with or without a current connection.
    Running,
    /// Admission is closed while one generation is being drained and cleaned.
    Draining,
    /// Cleanup could not prove safety; explicit stop/restart recovery is required.
    Faulted,
}

/// Snapshot of the single generation slot owned by the endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GenerationSlotSnapshot {
    /// The endpoint currently owns no TCP generation.
    None,
    /// The identified generation is open for admitted work.
    Open(ConnectionGeneration),
    /// The identified generation exists but its admission gate is closed.
    Draining(ConnectionGeneration),
}

/// HSMS-SS selection state inside one TCP generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// TCP is established but the HSMS session is not selected.
    NotSelected,
    /// `Select.req` is pending or passive selection is being processed.
    Selecting,
    /// The session may exchange HSMS Data messages.
    Selected,
    /// A Deselect control transaction is in progress.
    Deselecting,
    /// The session has entered protocol/runtime shutdown.
    Closing,
    /// This generation can no longer process protocol work.
    Closed,
}

/// Read-only endpoint lifecycle state published to applications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EndpointStateSnapshot {
    /// Latest application running intent committed by `LifecycleCell`.
    desired: RunningIntent,
    /// Coarse lifecycle phase committed at the same sequence.
    phase: EndpointPhase,
    /// State of the endpoint's single replaceable generation slot.
    generation: GenerationSlotSnapshot,
    /// Monotonic lifecycle revision exposed as a primitive public value.
    sequence: u64,
    /// Selection state of the current generation, if one exists.
    session: Option<SessionState>,
}

impl EndpointStateSnapshot {
    /// Returns the initial snapshot for a clean endpoint that has never started.
    #[must_use]
    pub const fn stopped_clean() -> Self {
        Self {
            desired: RunningIntent::Stopped,
            phase: EndpointPhase::StoppedClean,
            generation: GenerationSlotSnapshot::None,
            sequence: 0,
            session: None,
        }
    }

    #[allow(dead_code)]
    /// Builds a snapshot from one atomically observed lifecycle revision.
    ///
    /// `desired`, `phase`, `generation`, and `session` must all describe the
    /// state committed at `sequence`; only `LifecycleCell` may call this helper.
    pub(crate) const fn new(
        desired: RunningIntent,
        phase: EndpointPhase,
        generation: GenerationSlotSnapshot,
        sequence: LifecycleSequence,
        session: Option<SessionState>,
    ) -> Self {
        Self {
            desired,
            phase,
            generation,
            sequence: sequence.get(),
            session,
        }
    }

    #[must_use]
    /// Returns the application's latest running intent.
    pub const fn desired(self) -> RunningIntent {
        self.desired
    }

    #[must_use]
    /// Returns the endpoint's coarse lifecycle phase.
    pub const fn phase(self) -> EndpointPhase {
        self.phase
    }

    #[must_use]
    /// Returns the current state of the replaceable generation slot.
    pub const fn generation(self) -> GenerationSlotSnapshot {
        self.generation
    }

    #[must_use]
    /// Returns the monotonic lifecycle revision represented by this snapshot.
    pub const fn sequence(self) -> u64 {
        self.sequence
    }

    #[must_use]
    /// Returns the current generation's session state, if a generation exists.
    pub const fn session(self) -> Option<SessionState> {
        self.session
    }
}

impl Default for EndpointStateSnapshot {
    /// Returns the same initial clean state as [`Self::stopped_clean`].
    fn default() -> Self {
        Self::stopped_clean()
    }
}
