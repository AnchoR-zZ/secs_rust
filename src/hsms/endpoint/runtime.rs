//! Autonomous endpoint lifecycle loop above the single-generation Supervisor.
//! It owns no application sender, so final-handle drop initiates bounded Stop.
//! Lifecycle acknowledgements follow coherent snapshot publication and cleanup.

use super::{EndpointError, LifecycleCommand, LifecycleIntent, LifecycleReceipt, StartReceipt};
use crate::hsms::{
    generation::driver::{CommandCompletion, DriverCommandResult, SessionStateObserver},
    model::ids::LifecycleSequence,
    supervisor::{
        connection::{AttemptError, SourceError},
        runtime::{ConnectionSupervisor, SupervisorError},
        session::CleanupResult,
    },
    ConfigError, ConnectionMode, EndpointConfig, EndpointPhase, EndpointStateSnapshot,
    GenerationSlotSnapshot, RunningIntent, SessionState,
};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

/// A protocol result deferred until its enclosing Driver round is published.
struct PublishedCompletion {
    /// Original bounded endpoint request.
    command: LifecycleCommand,
    /// Terminal result awaiting coherent public state publication.
    result: Result<LifecycleReceipt, EndpointError>,
}

/// Session state remains in Driver until the enclosing round publishes a snapshot.
struct Observer(
    /// Bounded diagnostic publisher that cannot retain application command handles.
    super::diagnostics::DiagnosticPublisher,
);
impl SessionStateObserver for Observer {
    /// Receives committed state; publication occurs after the complete Driver batch.
    fn observe(&mut self, _: SessionState) {}
    /// Publishes the Core's safe attribution result without blocking protocol work.
    fn notice(
        &mut self,
        generation: crate::hsms::ConnectionGeneration,
        notice: crate::hsms::ProtocolNotice,
    ) {
        self.0
            .publish(super::DiagnosticEvent::Protocol { generation, notice });
    }
}

/// Application control completion deferred until coherent state publication.
struct Completion(
    /// Original endpoint request, including its completion and count reservation.
    LifecycleCommand,
    /// Shared completion outbox bounded by the retained command reservations.
    Arc<Mutex<Vec<PublishedCompletion>>>,
);
impl CommandCompletion for Completion {
    /// Retains a protocol result until the enclosing Driver round is published.
    fn complete(self, result: DriverCommandResult) {
        let result = match result {
            DriverCommandResult::PrimaryRejected { message, error, .. } => {
                Ok(LifecycleReceipt::RejectedPrimary {
                    message,
                    error: EndpointError::Operation(error),
                })
            }
            DriverCommandResult::Control(result) => result
                .map(|()| LifecycleReceipt::Closed)
                .map_err(EndpointError::Operation),
            DriverCommandResult::Send(result) => result
                .map(LifecycleReceipt::Sent)
                .map_err(EndpointError::Operation),
            DriverCommandResult::Request(result) => result
                .map(LifecycleReceipt::Secondary)
                .map_err(EndpointError::Operation),
            DriverCommandResult::ReplyRejected {
                intent,
                token,
                body,
                error,
            } => Ok(LifecycleReceipt::RejectedReply {
                intent,
                token,
                body,
                error: EndpointError::Operation(error),
            }),
        };
        self.1
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(PublishedCompletion {
                command: self.0,
                result,
            });
    }
}

/// Explicitly owned endpoint execution future, driven by `run` inside Tokio.
pub struct HsmsRuntime {
    /// Sole owner of listener, retry timing, generation and cleanup state.
    supervisor: ConnectionSupervisor<Observer, Completion>,
    /// Bounded lifecycle queue; closure identifies final application handle drop.
    commands: mpsc::Receiver<LifecycleCommand>,
    /// Latest coherent public state, retained independently by subscribers.
    state: watch::Sender<EndpointStateSnapshot>,
    /// Close acknowledgements awaiting the current generation's cleanup barrier.
    pending: Vec<LifecycleCommand>,
    /// Active may retry transport failures; Passive accept failures latch a fault.
    active: bool,
    /// Final handle drop has requested irreversible exit from this runtime future.
    quitting: bool,
    /// Persistent tie preference between public commands and supervised I/O.
    prefer_commands: bool,
    /// Protocol completions released only after coherent snapshot publication.
    completions: Arc<Mutex<Vec<PublishedCompletion>>>,
    /// Bounded application event channels and queued Primary byte accounting.
    delivery: super::receiver::Delivery,
    /// Best-effort lifecycle/protocol observations with independent loss accounting.
    diagnostics: super::diagnostics::DiagnosticPublisher,
    /// Last generation exit and cleanup report, retained independently of diagnostics.
    last_exit: Option<crate::hsms::ConnectionExitReport>,
}

impl HsmsRuntime {
    /// Constructs the runtime without starting I/O, listeners or tasks.
    pub(super) fn new(
        config: EndpointConfig,
        commands: mpsc::Receiver<LifecycleCommand>,
        state: watch::Sender<EndpointStateSnapshot>,
        delivery: super::receiver::Delivery,
        diagnostics: super::diagnostics::DiagnosticPublisher,
    ) -> Result<Self, ConfigError> {
        let active = config.mode() == ConnectionMode::Active;
        Ok(Self {
            supervisor: ConnectionSupervisor::new(config)?,
            commands,
            state,
            pending: Vec::new(),
            active,
            quitting: false,
            prefer_commands: true,
            completions: Arc::new(Mutex::new(Vec::new())),
            delivery,
            diagnostics,
            last_exit: None,
        })
    }

    /// Runs until all handles are dropped, keeping Stop/Start reusable while owned.
    /// The caller may spawn this future. Dropping it aborts owned transport tasks;
    /// awaiting successful completion confirms the final clean shutdown.
    pub async fn run(mut self) -> Result<(), EndpointError> {
        let (_cancel, mut signal) = watch::channel(false);
        loop {
            self.publish()?;
            if !self.supervisor.is_faulted()
                && self
                    .supervisor
                    .current()
                    .is_some_and(|runtime| runtime.driver().transport_closed())
            {
                let exit = self
                    .supervisor
                    .finish_generation()
                    .await
                    .map_err(map_error)?
                    .expect("closed generation retained");
                self.record_exit(&exit);
                self.publish()?;
                let clean = exit.cleanup == CleanupResult::Clean;
                self.diagnostics
                    .publish(super::DiagnosticEvent::ConnectionClosed {
                        generation: exit.generation,
                        reason: super::diagnostics::close_reason(exit.reason),
                        clean,
                    });
                for command in self.pending.drain(..) {
                    command.complete(if clean {
                        Ok(LifecycleReceipt::Closed)
                    } else {
                        Err(EndpointError::CleanupUnproven)
                    });
                }
                continue;
            }
            if self.quitting {
                if self.supervisor.is_faulted() {
                    return Err(EndpointError::CleanupUnproven);
                }
                if self.supervisor.current().is_none() {
                    return Ok(());
                }
            }
            if self.supervisor.is_faulted()
                || (!self.supervisor.is_running() && self.supervisor.current().is_none())
            {
                let command = self.commands.recv().await;
                self.receive(command).await?;
            } else if self.supervisor.current().is_some() {
                let delivery = &self.delivery;
                let command = async {
                    if self.quitting {
                        std::future::pending().await
                    } else {
                        self.commands.recv().await
                    }
                };
                match crate::hsms::scheduling::alternate(
                    &mut self.prefer_commands,
                    command,
                    self.supervisor.step_current(|| Some(delivery.capacity())),
                )
                .await
                {
                    crate::hsms::scheduling::Selected::Left(command) => {
                        self.receive(command).await?
                    }
                    crate::hsms::scheduling::Selected::Right(result) => {
                        if let Err(error) = result {
                            self.diagnostics.publish(
                                super::DiagnosticEvent::ConnectionAttemptFailed {
                                    message: error.to_string(),
                                },
                            );
                        }
                    }
                }
            } else {
                match crate::hsms::scheduling::alternate(
                    &mut self.prefer_commands,
                    self.commands.recv(),
                    self.supervisor
                        .connect(Observer(self.diagnostics.clone()), &mut signal),
                )
                .await
                {
                    crate::hsms::scheduling::Selected::Left(command) => {
                        self.receive(command).await?
                    }
                    crate::hsms::scheduling::Selected::Right(connected) => {
                        if let Err(error) = &connected {
                            self.diagnostics.publish(
                                super::DiagnosticEvent::ConnectionAttemptFailed {
                                    message: error.to_string(),
                                },
                            );
                        }
                        match connected {
                            Ok(_) => {}
                            Err(SupervisorError::Attempt(
                                AttemptError::Transport(_) | AttemptError::Timeout,
                            )) if self.active => {}
                            Err(_) => self.supervisor.fault_source(),
                        }
                    }
                }
            }
        }
    }

    /// Applies a lifecycle request or final-sender closure at a serialized boundary.
    async fn receive(&mut self, command: Option<LifecycleCommand>) -> Result<(), EndpointError> {
        if let Some(runtime) = self.supervisor.current_mut() {
            runtime.process_ready_priority().await;
        }
        let Some(mut command) = command else {
            self.quitting = true;
            self.supervisor.begin_stop();
            return Ok(());
        };
        let intent = std::mem::replace(&mut command.intent, LifecycleIntent::Start);
        match intent {
            LifecycleIntent::Reply {
                generation,
                intent,
                token,
                body,
            } => {
                use crate::hsms::generation::runtime::RuntimeCompletion;
                let error = if self.supervisor.is_faulted() {
                    Some(EndpointError::Faulted)
                } else if self
                    .supervisor
                    .current()
                    .map(|runtime| runtime.generation())
                    != Some(generation)
                {
                    Some(EndpointError::StaleConnectionGeneration)
                } else {
                    None
                };
                if let Some(error) = error {
                    command.complete(Ok(LifecycleReceipt::RejectedReply {
                        intent,
                        token,
                        body,
                        error,
                    }));
                } else {
                    let completion = RuntimeCompletion::Application(Completion(
                        command,
                        self.completions.clone(),
                    ));
                    if let Err((completion, result)) = self
                        .supervisor
                        .current_mut()
                        .expect("route checked")
                        .driver_mut()
                        .try_accept_reply(intent, token, body, completion)
                    {
                        completion.complete(result);
                    }
                }
            }
            LifecycleIntent::Data {
                generation,
                message,
                reply_expected,
            } => {
                use crate::hsms::generation::{
                    driver::DataAdmissionErrorKind, runtime::RuntimeCompletion,
                };
                let error = if self.supervisor.is_faulted() {
                    Some(EndpointError::Faulted)
                } else if self
                    .supervisor
                    .current()
                    .map(|runtime| runtime.generation())
                    != Some(generation)
                {
                    Some(EndpointError::StaleConnectionGeneration)
                } else {
                    None
                };
                if let Some(error) = error {
                    command.complete(Ok(LifecycleReceipt::RejectedPrimary { message, error }));
                } else {
                    let driver = self
                        .supervisor
                        .current_mut()
                        .expect("route checked")
                        .driver_mut();
                    let completion = RuntimeCompletion::Application(Completion(
                        command,
                        self.completions.clone(),
                    ));
                    let admitted = if reply_expected {
                        driver.try_accept_request(message, completion)
                    } else {
                        driver.try_accept_send(message, completion)
                    };
                    if let Err(rejection) = admitted {
                        let error = match rejection.kind() {
                            DataAdmissionErrorKind::Full => EndpointError::Backpressure,
                            DataAdmissionErrorKind::Draining => EndpointError::Draining,
                            DataAdmissionErrorKind::Closing => EndpointError::Operation(
                                crate::hsms::OperationError::ConnectionLost,
                            ),
                            DataAdmissionErrorKind::Invalid(error) => {
                                EndpointError::Operation(error)
                            }
                        };
                        let (message, completion) = rejection.into_parts();
                        let RuntimeCompletion::Application(completion) = completion else {
                            unreachable!("public Data completion")
                        };
                        self.completions
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner())
                            .push(PublishedCompletion {
                                command: completion.0,
                                result: Ok(LifecycleReceipt::RejectedPrimary { message, error }),
                            });
                    }
                }
            }
            LifecycleIntent::Control(generation, intent) => {
                use crate::hsms::generation::{
                    driver::ControlAdmissionErrorKind, runtime::RuntimeCompletion,
                };
                if self.supervisor.is_faulted() {
                    command.complete(Err(EndpointError::Faulted));
                } else if let Some(runtime) = self.supervisor.current_mut() {
                    if runtime.generation() != generation {
                        command.complete(Err(EndpointError::StaleConnectionGeneration));
                    } else if let Err(error) = runtime.driver_mut().try_accept_control(
                        intent,
                        RuntimeCompletion::Application(Completion(
                            command,
                            self.completions.clone(),
                        )),
                    ) {
                        let reason = match error.kind() {
                            ControlAdmissionErrorKind::Full => {
                                crate::hsms::OperationError::Backpressure
                            }
                            ControlAdmissionErrorKind::Draining => {
                                crate::hsms::OperationError::Draining
                            }
                            _ => crate::hsms::OperationError::ConnectionLost,
                        };
                        let (_, completion) = error.into_parts();
                        completion.complete(DriverCommandResult::Control(Err(reason)));
                    }
                } else {
                    command.complete(Err(EndpointError::StaleConnectionGeneration));
                }
            }
            LifecycleIntent::Start => {
                let result = if !self.pending.is_empty() {
                    Err(EndpointError::Draining)
                } else {
                    self.supervisor
                        .start()
                        .await
                        .map_err(map_error)
                        .and_then(|()| {
                            self.supervisor
                                .local_address()
                                .map(|local_address| {
                                    LifecycleReceipt::Started(StartReceipt { local_address })
                                })
                                .map_err(io_error)
                        })
                };
                self.publish()?;
                command.complete(result);
            }
            LifecycleIntent::Stop => {
                let recovering = self.supervisor.is_faulted();
                self.supervisor.begin_stop();
                if recovering {
                    let result = self
                        .supervisor
                        .recover_stopped()
                        .await
                        .map_err(map_error)
                        .and_then(|exit| {
                            if let Some(exit) = exit {
                                self.record_exit(&exit);
                                if exit.cleanup != CleanupResult::Clean {
                                    return Err(EndpointError::CleanupUnproven);
                                }
                            }
                            Ok(LifecycleReceipt::Closed)
                        });
                    self.publish()?;
                    command.complete(result);
                } else if self.supervisor.current().is_none() {
                    self.publish()?;
                    command.complete(Ok(LifecycleReceipt::Closed));
                } else {
                    self.pending.push(command);
                }
            }
            LifecycleIntent::Disconnect(generation) => {
                let current = self
                    .supervisor
                    .current()
                    .map(|runtime| runtime.generation());
                if generation.is_none() {
                    command.complete(Ok(LifecycleReceipt::Closed));
                } else if generation != current {
                    command.complete(Err(EndpointError::StaleConnectionGeneration));
                } else if self.supervisor.is_faulted() {
                    command.complete(Err(EndpointError::Faulted));
                } else {
                    self.supervisor.begin_disconnect();
                    self.pending.push(command);
                }
            }
        }
        Ok(())
    }

    /// Publishes the complete state before waking any protocol command waiter.
    fn publish(&mut self) -> Result<(), EndpointError> {
        if let Some(runtime) = self.supervisor.current_mut() {
            let mut failed = false;
            while let Some(primary) = runtime.driver_mut().take_inbound() {
                if self.delivery.primary(primary).is_err() {
                    failed = true;
                    break;
                }
            }
            while let Some(error) = runtime.driver_mut().take_protocol_error() {
                if self.delivery.error(error).is_err() {
                    failed = true;
                    break;
                }
            }
            if failed {
                runtime.force_close(
                    crate::hsms::model::runtime::GenerationCloseReason::ApplicationBackpressure,
                );
            }
        }
        self.publish_state()?;
        let completed = std::mem::take(
            &mut *self
                .completions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        for completion in completed {
            completion.command.complete(completion.result);
        }
        Ok(())
    }

    /// Retains the latest cleanup attempt without depending on event capacity.
    fn record_exit(&mut self, exit: &crate::hsms::supervisor::session::SessionExit) {
        self.last_exit = Some(crate::hsms::ConnectionExitReport::new(
            exit.generation,
            super::diagnostics::close_reason(exit.reason),
            exit.cleanup == CleanupResult::Clean,
        ));
    }

    /// Publishes one complete state revision only when observable fields change.
    fn publish_state(&mut self) -> Result<(), EndpointError> {
        let desired = if self.supervisor.is_running() {
            RunningIntent::Running
        } else {
            RunningIntent::Stopped
        };
        let current = self.supervisor.current();
        let draining = current.is_some_and(|runtime| runtime.is_draining());
        let phase = if self.supervisor.is_faulted() {
            EndpointPhase::Faulted
        } else if draining {
            EndpointPhase::Draining
        } else if self.supervisor.is_running() {
            EndpointPhase::Running
        } else {
            EndpointPhase::StoppedClean
        };
        let generation = current.map_or(GenerationSlotSnapshot::None, |runtime| {
            if draining {
                GenerationSlotSnapshot::Draining(runtime.generation())
            } else {
                GenerationSlotSnapshot::Open(runtime.generation())
            }
        });
        let session = current.and_then(|runtime| runtime.driver().state());
        let previous = *self.state.borrow();
        if (
            previous.desired(),
            previous.phase(),
            previous.generation(),
            previous.session(),
            previous.last_exit(),
        ) == (desired, phase, generation, session, self.last_exit)
        {
            return Ok(());
        }
        let sequence = previous
            .sequence()
            .checked_add(1)
            .ok_or(EndpointError::IdentifierExhausted)?;
        self.state.send_replace(
            EndpointStateSnapshot::new(
                desired,
                phase,
                generation,
                LifecycleSequence::new(sequence),
                session,
            )
            .with_last_exit(self.last_exit),
        );
        Ok(())
    }
}

impl Drop for HsmsRuntime {
    /// Closes request admission and leaves a conservative snapshot if aborted.
    fn drop(&mut self) {
        self.commands.close();
        if self.supervisor.current().is_some() {
            let previous = *self.state.borrow();
            self.state.send_replace(
                EndpointStateSnapshot::new(
                    RunningIntent::Stopped,
                    EndpointPhase::Faulted,
                    previous.generation(),
                    LifecycleSequence::new(previous.sequence().saturating_add(1)),
                    Some(SessionState::Closed),
                )
                .with_last_exit(self.last_exit),
            );
        }
        while let Ok(mut command) = self.commands.try_recv() {
            let intent = std::mem::replace(&mut command.intent, LifecycleIntent::Start);
            let result = match intent {
                LifecycleIntent::Reply {
                    intent,
                    token,
                    body,
                    ..
                } => Ok(LifecycleReceipt::RejectedReply {
                    intent,
                    token,
                    body,
                    error: EndpointError::RuntimeStopped,
                }),
                LifecycleIntent::Data { message, .. } => Ok(LifecycleReceipt::RejectedPrimary {
                    message,
                    error: EndpointError::RuntimeStopped,
                }),
                _ => Err(EndpointError::RuntimeStopped),
            };
            command.complete(result);
        }
        // Actual completed facts survive runtime cancellation; unresolved Driver
        // work retains its conservative RuntimeStopped receiver-closure result.
        let completed = std::mem::take(
            &mut *self
                .completions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()),
        );
        for completion in completed {
            completion.command.complete(completion.result);
        }
    }
}

/// Preserves the operating system's portable category and diagnostic text.
fn io_error(error: std::io::Error) -> EndpointError {
    EndpointError::Io {
        kind: error.kind(),
        message: error.to_string(),
    }
}

/// Maps internal lifecycle failures without leaking runtime-owned resources.
fn map_error(error: SupervisorError) -> EndpointError {
    match error {
        SupervisorError::Start(SourceError::Bind(error))
        | SupervisorError::Attempt(AttemptError::Transport(error)) => io_error(error),
        SupervisorError::Occupied => EndpointError::Draining,
        SupervisorError::GenerationExhausted => EndpointError::IdentifierExhausted,
        _ => EndpointError::Faulted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::{
        model::runtime::{CommunicationsTimeoutKind, GenerationCloseReason},
        HsmsEndpoint, SessionId,
    };
    use std::{future::Future, task::Poll};

    /// Poison and its later explicit recovery are retained in the latest snapshot.
    #[tokio::test]
    async fn last_exit_retains_poison_and_updates_after_explicit_recovery() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let config =
                EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
            let (handle, mut runtime) = HsmsEndpoint::build(config).unwrap();
            runtime.supervisor.start().await.unwrap();
            let _peer = tokio::net::TcpStream::connect(
                runtime.supervisor.local_address().unwrap().unwrap(),
            )
            .await
            .unwrap();
            let (_cancel, mut signal) = watch::channel(false);
            runtime
                .supervisor
                .connect(Observer(runtime.diagnostics.clone()), &mut signal)
                .await
                .unwrap();
            let exit = runtime
                .supervisor
                .current_mut()
                .unwrap()
                .close(
                    GenerationCloseReason::LocalDisconnect,
                    tokio::time::Instant::now(),
                )
                .await;
            assert!(matches!(exit.cleanup, CleanupResult::Poisoned(_)));
            let mut states = handle.subscribe();
            let task = tokio::spawn(runtime.run());
            while states.borrow_and_update().phase() != EndpointPhase::Faulted {
                states.changed().await.unwrap();
            }
            let poisoned = handle.snapshot().last_exit().unwrap();
            assert_eq!(poisoned.generation(), exit.generation);
            assert!(!poisoned.clean());
            handle.stop().await.unwrap();
            let recovered = handle.snapshot().last_exit().unwrap();
            assert_eq!(recovered.generation(), poisoned.generation());
            assert_eq!(recovered.reason(), poisoned.reason());
            assert!(recovered.clean());
            assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
            drop(handle);
            task.await.unwrap().unwrap();
        })
        .await
        .unwrap();
    }

    /// Rebind failure after proven cleanup is observable and requires explicit recovery.
    #[tokio::test]
    async fn passive_rebind_failure_faults_after_old_generation_is_clean() {
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let config = EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
            let (handle, mut runtime) = HsmsEndpoint::build(config).unwrap();
            let mut diagnostics = handle.take_diagnostics().unwrap();
            runtime.supervisor.start().await.unwrap();
            let address = runtime.supervisor.local_address().unwrap().unwrap();
            let peer = tokio::net::TcpStream::connect(address).await.unwrap();
            let (_cancel, mut signal) = watch::channel(false);
            runtime.supervisor.connect(Observer(runtime.diagnostics.clone()), &mut signal).await.unwrap();
            runtime.supervisor.begin_disconnect();
            while runtime.supervisor.current_mut().unwrap().step().await {}
            let exit = runtime.supervisor.finish_generation().await.unwrap().unwrap();
            assert_eq!(exit.cleanup, CleanupResult::Clean);
            assert!(runtime.supervisor.current().is_none());
            drop(peer);
            let occupied = tokio::net::TcpListener::bind(address).await.unwrap();
            let task = tokio::spawn(runtime.run());
            assert!(matches!(diagnostics.recv().await.unwrap().event(), super::super::DiagnosticEvent::ConnectionAttemptFailed { message } if !message.is_empty()));
            assert_eq!(handle.snapshot().phase(), EndpointPhase::Faulted);
            assert_eq!(handle.snapshot().generation(), GenerationSlotSnapshot::None);
            assert_eq!(handle.start().await, Err(EndpointError::Faulted));
            handle.stop().await.unwrap();
            assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
            drop(occupied);
            handle.start().await.unwrap();
            handle.stop().await.unwrap();
            drop(handle);
            task.await.unwrap().unwrap();
        }).await.unwrap();
    }

    /// A completed Reader exit wins over Stop without consuming exit evidence early.
    #[tokio::test]
    async fn reader_exit_precedes_queued_endpoint_stop() {
        use tokio::io::AsyncWriteExt;
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            for malformed in [false, true] {
                let config = EndpointConfig::passive(
                    "127.0.0.1:0".parse().unwrap(),
                    SessionId::new(7).unwrap(),
                );
                let (handle, mut runtime) = HsmsEndpoint::build(config).unwrap();
                runtime.supervisor.start().await.unwrap();
                let address = runtime.supervisor.local_address().unwrap().unwrap();
                let mut peer = tokio::net::TcpStream::connect(address).await.unwrap();
                let (_cancel, mut signal) = watch::channel(false);
                runtime
                    .supervisor
                    .connect(Observer(runtime.diagnostics.clone()), &mut signal)
                    .await
                    .unwrap();
                runtime.publish().unwrap();
                let mut stop = Box::pin(handle.stop());
                std::future::poll_fn(|cx| {
                    assert!(stop.as_mut().poll(cx).is_pending());
                    Poll::Ready(())
                })
                .await;
                if malformed {
                    peer.write_all(&[0, 0, 0, 0]).await.unwrap();
                } else {
                    peer.shutdown().await.unwrap();
                }
                // Observe the actual worker, keeping its queued/joined fact intact
                // until the outer endpoint command dispatch polls it normally.
                while !runtime.supervisor.current().unwrap().reader_finished() {
                    tokio::task::yield_now().await;
                }
                let command = runtime.commands.try_recv().unwrap();
                runtime.receive(Some(command)).await.unwrap();
                assert_eq!(
                    runtime
                        .supervisor
                        .current()
                        .unwrap()
                        .driver()
                        .close_reason(),
                    Some(if malformed {
                        GenerationCloseReason::ProtocolViolation
                    } else {
                        GenerationCloseReason::TransportLost
                    })
                );
                let task = tokio::spawn(runtime.run());
                stop.await.unwrap();
                assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
                drop(handle);
                task.await.unwrap().unwrap();
            }
        })
        .await
        .unwrap();
    }

    /// An outer queued Stop cannot replace an already due T7 as the first cause.
    #[tokio::test(start_paused = true)]
    async fn due_t7_precedes_queued_endpoint_stop() {
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
        let t7 = config.timeouts().t7();
        let (handle, mut runtime) = HsmsEndpoint::build(config).unwrap();
        runtime.supervisor.start().await.unwrap();
        let address = runtime.supervisor.local_address().unwrap().unwrap();
        let _peer = tokio::net::TcpStream::connect(address).await.unwrap();
        let (_cancel, mut signal) = watch::channel(false);
        runtime
            .supervisor
            .connect(Observer(runtime.diagnostics.clone()), &mut signal)
            .await
            .unwrap();
        runtime.publish().unwrap();
        let mut stop = Box::pin(handle.stop());
        std::future::poll_fn(|cx| {
            assert!(stop.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(t7).await;
        let command = runtime.commands.try_recv().unwrap();
        runtime.receive(Some(command)).await.unwrap();
        assert_eq!(
            runtime
                .supervisor
                .current()
                .unwrap()
                .driver()
                .close_reason(),
            Some(GenerationCloseReason::CommunicationsTimeout(
                CommunicationsTimeoutKind::T7
            ))
        );
        let task = tokio::spawn(runtime.run());
        stop.await.unwrap();
        assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
        drop(handle);
        task.await.unwrap().unwrap();
    }
}
