//! Port contracts between `ConnectionSupervisor` and one `SessionDriver` run.
//!
//! The supervisor launches a generation, retains a non-blocking shutdown
//! control, and awaits a runtime-neutral exit future that proves cleanup.

use std::future::Future;

use crate::hsms::ConnectionGeneration;

/// Reason cleanup could not prove that a generation released all resources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CleanupPoison {
    /// At least one generation task failed to terminate within cleanup policy.
    TaskDidNotStop,
    /// The TCP transport or one of its owned halves remained live.
    TransportNotReleased,
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

/// Terminal reason reported by one generation-scoped SessionDriver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionExitReason {
    /// The logical endpoint was stopped locally.
    LocalStop,
    /// The application requested disconnection of this generation.
    LocalDisconnect,
    /// The peer sent `Separate.req`.
    SeparateReceived,
    /// The TCP transport ended or became unusable.
    TransportLost,
    /// Protocol invariants required termination.
    ProtocolViolation,
    /// Reliable application event delivery could not accept more data.
    ApplicationBackpressure,
}

/// Complete terminal report for one launched generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SessionExit {
    /// TCP incarnation whose SessionDriver exited.
    pub(crate) generation: ConnectionGeneration,
    /// Event that initiated or forced termination.
    pub(crate) reason: SessionExitReason,
    /// Proof of whether all owned resources were released.
    pub(crate) cleanup: CleanupResult,
}

/// Why the endpoint asks an open generation to begin shutdown. The precise
/// drain membership and deadline semantics remain deferred to the Drain
/// review; this value only carries the initiating intent across the port.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionShutdownRequest {
    /// Begin shutdown because the logical endpoint is stopping.
    EndpointStopping,
    /// End only the current generation while endpoint supervision continues.
    DisconnectRequested,
}

/// Failure to deliver a shutdown request to a launched session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionControlError {
    /// SessionDriver already produced its terminal exit.
    AlreadyExited,
}

/// Non-blocking control side of one launched SessionDriver.
pub(crate) trait SessionControl: Send + Sync + 'static {
    /// Requests non-blocking shutdown for the supplied endpoint `request`.
    ///
    /// Returns `Ok(())` when SessionDriver accepted the signal, or
    /// [`SessionControlError::AlreadyExited`] when no live session remains.
    fn request_shutdown(&self, request: SessionShutdownRequest) -> Result<(), SessionControlError>;
}

/// The two independently owned sides returned when a generation is launched.
pub(crate) struct LaunchedSession<Control, Exit> {
    /// Non-blocking handle retained by `ConnectionSupervisor` for shutdown.
    control: Control,
    /// Future awaited by the supervisor to obtain the terminal cleanup report.
    exit: Exit,
}

impl<Control, Exit> LaunchedSession<Control, Exit> {
    /// Combines independently owned `control` and `exit` sides returned by a
    /// successful SessionDriver launch.
    pub(crate) const fn new(control: Control, exit: Exit) -> Self {
        Self { control, exit }
    }

    /// Consumes the wrapper and returns `(control, exit)` for independent
    /// storage and polling by `ConnectionSupervisor`.
    pub(crate) fn into_parts(self) -> (Control, Exit) {
        (self.control, self.exit)
    }
}

/// Factory port used by ConnectionSupervisor and replaceable by a fake in
/// lifecycle tests. A concrete candidate may own a connected TCP stream.
pub(crate) trait SessionLauncher<Candidate> {
    /// Non-blocking shutdown-control implementation for the launched session.
    type Control: SessionControl;
    /// Runtime-neutral future resolving to the generation's terminal report.
    type Exit: Future<Output = SessionExit> + Send + 'static;
    /// Immediate launch failure returned before a SessionDriver run exists.
    type Error;

    /// Starts the SessionDriver for `candidate` under `generation` identity.
    ///
    /// Returns separate control and exit sides on success, or `Self::Error`
    /// when the candidate cannot be turned into a running generation.
    fn launch(
        &self,
        generation: ConnectionGeneration,
        candidate: Candidate,
    ) -> Result<LaunchedSession<Self::Control, Self::Exit>, Self::Error>;
}
