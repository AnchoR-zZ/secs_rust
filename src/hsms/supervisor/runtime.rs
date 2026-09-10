//! Owns the endpoint's single replaceable connection slot and TCP source.
//! Replacement requires an observed clean exit. Local invariant/resource faults
//! stop automatic recovery; accepted protocol commands never migrate to a new slot.

use super::{
    connection::{AttemptError, ConnectionSource, SourceError},
    session::{CleanupResult, SessionExit},
};
use crate::hsms::{
    generation::{
        driver::{CommandCompletion, SessionStateObserver},
        runtime::GenerationRuntime,
    },
    model::runtime::GenerationCloseReason,
    ConfigError, ConnectionGeneration, EndpointConfig,
};
use tokio::sync::watch;

/// Reasons an endpoint lifecycle operation cannot make progress normally.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SupervisorError {
    /// Startup failed before a running source was installed.
    #[error(transparent)]
    Start(#[from] SourceError),
    /// One candidate attempt failed, with T5 state retained by the source.
    #[error(transparent)]
    Attempt(#[from] AttemptError),
    /// Runtime construction rejected its configuration.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// An existing generation still owns the connection slot.
    #[error("HSMS generation still owns the endpoint slot")]
    Occupied,
    /// Endpoint intent is stopped and does not permit connection attempts.
    #[error("HSMS endpoint is stopped")]
    Stopped,
    /// A local fault or unproven cleanup blocks automatic recovery.
    #[error("HSMS endpoint requires fault recovery")]
    Faulted,
    /// The monotonic generation namespace cannot allocate another identity.
    #[error("HSMS generation identities exhausted")]
    GenerationExhausted,
    /// Resource recovery is forbidden while automatic running intent persists.
    #[error("stop the HSMS endpoint before retrying cleanup")]
    RecoveryRequiresStop,
}

/// Single-owner lifecycle coordinator above the connected-generation runtime.
pub(crate) struct ConnectionSupervisor<Observer, Completion> {
    /// Immutable validated endpoint policy shared by every generation.
    config: EndpointConfig,
    /// Listener or Active retry timing, retained while running intent persists.
    source: Option<ConnectionSource>,
    /// Current generation, including one awaiting cleanup proof.
    current: Option<GenerationRuntime<Observer, Completion>>,
    /// Next unique identity, absent after the final representable generation.
    next_generation: Option<u64>,
    /// Desired endpoint state; Stop prevents reconnection independently of cause.
    running: bool,
    /// Recovery latch for a local fault or poisoned cleanup.
    faulted: bool,
    /// Persistent tie preference between generation progress and excess candidates.
    prefer_generation: bool,
}

impl<Observer: SessionStateObserver, Completion: CommandCompletion>
    ConnectionSupervisor<Observer, Completion>
{
    /// Builds a stopped Supervisor without binding sockets or spawning tasks.
    pub(crate) fn new(config: EndpointConfig) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            config,
            source: None,
            current: None,
            next_generation: Some(1),
            running: false,
            faulted: false,
            prefer_generation: true,
        })
    }

    /// Establishes running intent; Passive acknowledgement follows successful bind.
    /// Starting a faulted or still-owned slot never silently resets cleanup state.
    pub(crate) async fn start(&mut self) -> Result<(), SupervisorError> {
        if self.faulted {
            return Err(SupervisorError::Faulted);
        }
        if self.running {
            return Ok(());
        }
        if self.current.is_some() {
            return Err(SupervisorError::Occupied);
        }
        let source = ConnectionSource::prepare(&self.config).await?;
        self.source = Some(source);
        self.running = true;
        Ok(())
    }

    /// Creates one generation only when running, fault-free and the slot is empty.
    /// Cancellation leaves it empty; failures retain the source's retry deadline.
    pub(crate) async fn connect(
        &mut self,
        observer: Observer,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<bool, SupervisorError> {
        if self.faulted {
            return Err(SupervisorError::Faulted);
        }
        if !self.running {
            return Err(SupervisorError::Stopped);
        }
        if self.current.is_some() {
            return Err(SupervisorError::Occupied);
        }
        let Some(id) = self.next_generation else {
            self.faulted = true;
            self.source.take();
            return Err(SupervisorError::GenerationExhausted);
        };
        let Some(stream) = self
            .source
            .as_mut()
            .ok_or(SupervisorError::Stopped)?
            .next(cancellation)
            .await?
        else {
            return Ok(false);
        };
        // Never reuse a candidate's identity, even if construction subsequently fails.
        self.next_generation = id.checked_add(1);
        match GenerationRuntime::from_stream(
            stream,
            ConnectionGeneration::new(id),
            &self.config,
            observer,
        ) {
            Ok(runtime) => {
                self.current = Some(runtime);
                Ok(true)
            }
            Err(error) => {
                self.faulted = true;
                self.source.take();
                Err(SupervisorError::Config(error))
            }
        }
    }

    /// Services one protocol round or rejects one excess Passive connection.
    /// A listener failure closes the source and drains the owned generation before
    /// latching its local fault; no resource cleanup is bypassed.
    pub(crate) async fn step_current(
        &mut self,
        capacity: impl Fn() -> Option<(usize, usize)>,
    ) -> std::io::Result<()> {
        let runtime = self.current.as_mut().expect("occupied generation required");
        let rejected = async {
            match self.source.as_ref() {
                Some(source) => source.reject_extra().await,
                None => std::future::pending().await,
            }
        };
        let result = match crate::hsms::scheduling::alternate(
            &mut self.prefer_generation,
            runtime.step_with_delivery_capacity(capacity),
            rejected,
        )
        .await
        {
            crate::hsms::scheduling::Selected::Left(_) => Ok(()),
            crate::hsms::scheduling::Selected::Right(result) => result,
        };
        if result.is_err() {
            self.source.take();
            runtime.begin_graceful_close(GenerationCloseReason::RuntimeInvariant);
        }
        result
    }

    /// Borrows the sole runtime for serialized protocol turns and routed commands.
    pub(crate) fn current_mut(&mut self) -> Option<&mut GenerationRuntime<Observer, Completion>> {
        self.current.as_mut()
    }

    /// Borrows the owned generation without changing admission or lifecycle state.
    pub(crate) fn current(&self) -> Option<&GenerationRuntime<Observer, Completion>> {
        self.current.as_ref()
    }

    /// Returns the logical endpoint's persistent running intent.
    pub(crate) const fn is_running(&self) -> bool {
        self.running
    }

    /// Returns whether automatic connection replacement is blocked by a fault.
    pub(crate) const fn is_faulted(&self) -> bool {
        self.faulted
    }

    /// Latches a source-level fatal failure when no generation owns the slot.
    pub(crate) fn fault_source(&mut self) {
        self.faulted = true;
        self.source.take();
    }

    /// Exposes a Passive endpoint's assigned port for startup snapshots.
    pub(crate) fn local_address(&self) -> std::io::Result<Option<std::net::SocketAddr>> {
        self.source
            .as_ref()
            .map_or(Ok(None), ConnectionSource::local_address)
    }

    /// Stops reconnection and begins finite graceful shutdown of any live slot.
    /// Dropping the source immediately releases the Passive listener.
    pub(crate) fn begin_stop(&mut self) {
        self.running = false;
        self.source.take();
        if let Some(runtime) = &mut self.current {
            runtime.begin_graceful_close(GenerationCloseReason::LocalStop);
        } else {
            self.faulted = false;
        }
    }

    /// Ends the current connection while retaining running intent and retry state.
    pub(crate) fn begin_disconnect(&mut self) {
        if let Some(runtime) = &mut self.current {
            runtime.begin_graceful_close(GenerationCloseReason::LocalDisconnect);
        }
    }

    /// Retries cleanup only under stopped intent, releasing the slot on new proof.
    /// Failure keeps ownership and fault state; successful recovery permits Start.
    pub(crate) async fn recover_stopped(&mut self) -> Result<Option<SessionExit>, SupervisorError> {
        if self.running {
            return Err(SupervisorError::RecoveryRequiresStop);
        }
        let Some(runtime) = &mut self.current else {
            self.faulted = false;
            return Ok(None);
        };
        if !runtime.driver().transport_closed() {
            return Err(SupervisorError::Occupied);
        }
        let exit = runtime.recover_cleanup().await;
        if exit.cleanup == CleanupResult::Clean {
            self.current.take();
            self.faulted = false;
        } else {
            self.faulted = true;
        }
        Ok(Some(exit))
    }

    /// Joins a closed slot before releasing it. Poison retains ownership and blocks
    /// replacement; clean local faults require an explicit Stop/Start recovery.
    pub(crate) async fn finish_generation(
        &mut self,
    ) -> Result<Option<SessionExit>, SupervisorError> {
        let Some(runtime) = &mut self.current else {
            return Ok(None);
        };
        if !runtime.driver().transport_closed() {
            return Err(SupervisorError::Occupied);
        }
        let exit = runtime.finish().await;
        if exit.cleanup != CleanupResult::Clean {
            self.faulted = true;
            self.source.take();
            return Ok(Some(exit));
        }
        self.current.take();
        if !self.running {
            self.faulted = false;
        } else if !recoverable(exit.reason) {
            self.faulted = true;
            self.source.take();
        }
        Ok(Some(exit))
    }
}

/// Whitelists recoverable peer/transport endings; local faults never spin retries.
fn recoverable(reason: GenerationCloseReason) -> bool {
    matches!(
        reason,
        GenerationCloseReason::LocalDisconnect
            | GenerationCloseReason::LocalSeparate
            | GenerationCloseReason::SeparateReceived
            | GenerationCloseReason::TransportLost
            | GenerationCloseReason::SystemBytesExhausted
            | GenerationCloseReason::CommunicationsTimeout(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::{
        generation::driver::DriverCommandResult, lifecycle::SessionState, model::runtime::MonoTime,
        SessionId,
    };
    use tokio::{net::TcpStream, time::Instant};

    /// Snapshot sink for lifecycle tests that inspect generation exit reports.
    struct Observer;
    impl SessionStateObserver for Observer {
        /// Accepts state transitions without retaining application-facing state.
        fn observe(&mut self, _: SessionState) {}
    }
    /// Internal-only completion sink; lifecycle tests submit no application work.
    struct Completion;
    impl CommandCompletion for Completion {
        /// Consumes the result once; no application receiver exists in these tests.
        fn complete(self, _: DriverCommandResult) {}
    }

    /// Starts a Passive Supervisor and connects one independent TCP peer.
    async fn connected() -> (ConnectionSupervisor<Observer, Completion>, TcpStream) {
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
        let mut supervisor = ConnectionSupervisor::new(config).unwrap();
        supervisor.start().await.unwrap();
        let peer = TcpStream::connect(supervisor.local_address().unwrap().unwrap())
            .await
            .unwrap();
        let (_cancel, mut signal) = watch::channel(false);
        assert!(supervisor.connect(Observer, &mut signal).await.unwrap());
        (supervisor, peer)
    }

    /// Live and closing ownership blocks replacement; clean Stop permits a fresh ID.
    #[tokio::test]
    async fn occupied_slot_requires_clean_exit_before_restart() {
        let (mut supervisor, _peer) = connected().await;
        let (_cancel, mut signal) = watch::channel(false);
        assert!(matches!(
            supervisor.connect(Observer, &mut signal).await,
            Err(SupervisorError::Occupied)
        ));
        assert!(matches!(
            supervisor.finish_generation().await,
            Err(SupervisorError::Occupied)
        ));
        supervisor.begin_stop();
        while supervisor.current_mut().unwrap().step().await {}
        let first = supervisor.finish_generation().await.unwrap().unwrap();
        assert_eq!(first.cleanup, CleanupResult::Clean);
        assert!(supervisor.current_mut().is_none());
        supervisor.start().await.unwrap();
        let _peer2 = TcpStream::connect(supervisor.local_address().unwrap().unwrap())
            .await
            .unwrap();
        assert!(supervisor.connect(Observer, &mut signal).await.unwrap());
        supervisor.begin_stop();
        while supervisor.current_mut().unwrap().step().await {}
        let second = supervisor.finish_generation().await.unwrap().unwrap();
        assert!(second.generation.get() > first.generation.get());
    }

    /// Disconnect preserves the bind address; only clean retirement permits rebind.
    #[tokio::test]
    async fn disconnect_retains_listener_and_replaces_only_after_clean_exit() {
        let (mut supervisor, _peer) = connected().await;
        let address = supervisor.local_address().unwrap().unwrap();
        supervisor.begin_disconnect();
        while supervisor.current_mut().unwrap().step().await {}
        let first = supervisor.finish_generation().await.unwrap().unwrap();
        assert_eq!(first.reason, GenerationCloseReason::LocalDisconnect);
        assert_eq!(first.cleanup, CleanupResult::Clean);
        assert_eq!(supervisor.local_address().unwrap(), Some(address));
        let (_cancel, mut signal) = watch::channel(false);
        let (connected, peer2) = tokio::join!(
            supervisor.connect(Observer, &mut signal),
            TcpStream::connect(address),
        );
        let _peer2 = peer2.unwrap();
        assert!(connected.unwrap());
        supervisor.begin_stop();
        while supervisor.current_mut().unwrap().step().await {}
        let second = supervisor.finish_generation().await.unwrap().unwrap();
        assert!(second.generation.get() > first.generation.get());
    }

    /// Poison cannot be erased by Stop/Start or a later attempted replacement.
    #[tokio::test]
    async fn unproven_cleanup_retains_slot_and_blocks_replacement() {
        let (mut supervisor, _peer) = connected().await;
        let exit = supervisor
            .current_mut()
            .unwrap()
            .close(GenerationCloseReason::LocalDisconnect, Instant::now())
            .await;
        assert!(matches!(exit.cleanup, CleanupResult::Poisoned(_)));
        let exit = supervisor.finish_generation().await.unwrap().unwrap();
        assert!(matches!(exit.cleanup, CleanupResult::Poisoned(_)));
        assert!(supervisor.current_mut().is_some());
        let (_cancel, mut signal) = watch::channel(false);
        assert!(matches!(
            supervisor.connect(Observer, &mut signal).await,
            Err(SupervisorError::Faulted)
        ));
        supervisor.begin_stop();
        assert!(matches!(
            supervisor.start().await,
            Err(SupervisorError::Faulted)
        ));
    }

    /// Recovery requires Stop and new join proof before releasing a poisoned slot.
    #[tokio::test]
    async fn stopped_recovery_reaps_old_tasks_before_allowing_start() {
        let (mut supervisor, _peer) = connected().await;
        supervisor
            .current_mut()
            .unwrap()
            .close(GenerationCloseReason::LocalDisconnect, Instant::now())
            .await;
        let original = supervisor.finish_generation().await.unwrap().unwrap();
        assert!(matches!(original.cleanup, CleanupResult::Poisoned(_)));
        assert!(matches!(
            supervisor.recover_stopped().await,
            Err(SupervisorError::RecoveryRequiresStop)
        ));
        supervisor.begin_stop();
        let recovered = supervisor.recover_stopped().await.unwrap().unwrap();
        assert_eq!(recovered.generation, original.generation);
        assert_eq!(recovered.cleanup, CleanupResult::Clean);
        assert!(supervisor.current_mut().is_none());
        supervisor.start().await.unwrap();
        let _peer2 = TcpStream::connect(supervisor.local_address().unwrap().unwrap())
            .await
            .unwrap();
        let (_cancel, mut signal) = watch::channel(false);
        assert!(supervisor.connect(Observer, &mut signal).await.unwrap());
        supervisor.begin_stop();
        while supervisor.current_mut().unwrap().step().await {}
        assert!(
            supervisor
                .finish_generation()
                .await
                .unwrap()
                .unwrap()
                .generation
                .get()
                > original.generation.get()
        );
    }

    /// Clean local faults require explicit Stop/Start; transport faults may recover.
    #[tokio::test]
    async fn local_fault_does_not_automatically_reconnect() {
        let (mut supervisor, _peer) = connected().await;
        supervisor.current_mut().unwrap().driver_mut().on_shutdown(
            GenerationCloseReason::RuntimeInvariant,
            None,
            MonoTime::ZERO,
        );
        assert_eq!(
            supervisor
                .finish_generation()
                .await
                .unwrap()
                .unwrap()
                .cleanup,
            CleanupResult::Clean
        );
        assert!(matches!(
            supervisor.start().await,
            Err(SupervisorError::Faulted)
        ));
        supervisor.begin_stop();
        supervisor.start().await.unwrap();
        assert!(recoverable(GenerationCloseReason::TransportLost));
        assert!(recoverable(GenerationCloseReason::SystemBytesExhausted));
        assert!(!recoverable(GenerationCloseReason::ApplicationBackpressure));
        assert!(!recoverable(GenerationCloseReason::ControlBackpressure));
        assert!(!recoverable(GenerationCloseReason::RuntimeInvariant));
        supervisor.begin_stop();
    }
}
