//! All-or-nothing endpoint command admission boundary.
//!
//! Wave 2A implements ProducerLease, ReservationBundle, generation
//! revalidation and CompletionGuard.

#![allow(dead_code)]

use crate::hsms::{model::ids::LifecycleSequence, ConnectionGeneration};

/// Generation-conditioned snapshot captured before capacity reservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AdmissionSnapshot {
    /// Lifecycle revision observed before reserving bounded resources.
    pub(crate) lifecycle_sequence: LifecycleSequence,
    /// Open TCP incarnation to which the prospective command is conditioned.
    pub(crate) generation: ConnectionGeneration,
}
