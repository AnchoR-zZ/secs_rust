//! Endpoint and session state snapshots.
//!
//! The mutable `LifecycleCell` is implemented in Wave 2A. Wave 0 freezes only
//! the externally observable values and legal vocabulary.

mod state;

pub use state::{
    EndpointPhase, EndpointStateSnapshot, GenerationSlotSnapshot, RunningIntent, SessionState,
};
