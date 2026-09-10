//! Endpoint and session state snapshots used by lifecycle publication.

mod state;

pub use state::{
    ConnectionExitReport, EndpointPhase, EndpointStateSnapshot, GenerationSlotSnapshot,
    RunningIntent, SessionState,
};
