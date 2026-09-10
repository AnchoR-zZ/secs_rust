//! Executes one connected generation with the real Core, Driver and TCP tasks.
//! Each step applies a complete synchronous Driver turn. Endpoint routing and
//! graceful lifecycle requests belong above this layer; this owner supplies
//! network/timer progress and explicit forced cleanup with delivery settlement.

use super::{
    driver::{CommandCompletion, DriverCommandResult, SessionDriver, SessionStateObserver},
    transport::{
        bounded_writer::BoundedWriter,
        io::ReadFailure,
        tasks::{IoCloser, IoEvent, IoTasks},
    },
};
use crate::hsms::{
    model::runtime::{CommunicationsTimeoutKind, GenerationCloseReason, MonoTime},
    supervisor::session::SessionExit,
    ConfigError, ConnectionGeneration, ConnectionMode, ControlIntent, EndpointConfig,
};
use std::{future::Future, task::Poll};
use tokio::{net::TcpStream, time::Instant};

/// One selected input for a complete synchronous Driver turn.
enum RoundInput {
    /// Graceful drain can advance to its final Separate barrier.
    Shutdown,
    /// A source-ordered network event, or exhausted task ownership.
    Io(Option<IoEvent>),
    /// Core's earliest deadline is due at processing time.
    Timer,
    /// The oldest admitted command can enter Core.
    Command,
}

/// Fixed deadlines for one non-extendable graceful lifecycle request.
#[derive(Clone, Copy)]
struct ShutdownDrain {
    /// Original Stop/Disconnect cause, retained through the Separate barrier.
    reason: GenerationCloseReason,
    /// Absolute end of waiting for existing transactions and reply capabilities.
    drain_deadline: Instant,
    /// Absolute end of shutdown, including final write and resource cleanup.
    shutdown_deadline: Instant,
    /// Whether the one final Separate/transport-close action has been issued.
    finalizing: bool,
}

/// The concrete Driver port combination used by connected TCP generations.
pub(crate) type TcpDriver<Observer, Completion> =
    SessionDriver<BoundedWriter, Observer, RuntimeCompletion<Completion>, IoCloser>;

/// Completion ownership for application work and runtime-initiated procedures.
pub(crate) enum RuntimeCompletion<Completion> {
    /// Application endpoint that receives its unique result.
    Application(Completion),
    /// Autonomous procedure whose protocol effects and timers remain Core-owned.
    Internal,
}

impl<Completion: CommandCompletion> CommandCompletion for RuntimeCompletion<Completion> {
    /// Delivers application results; internal work needs no application receiver.
    fn complete(self, result: DriverCommandResult) {
        if let Self::Application(completion) = self {
            completion.complete(result);
        }
    }
}

/// Exclusive generation execution owner; dropping it aborts both transport tasks.
pub(crate) struct GenerationRuntime<Observer, Completion> {
    /// Connection incarnation retained for the final Supervisor report.
    generation: ConnectionGeneration,
    /// Monotonic origin shared by Core deadlines and both network workers.
    epoch: Instant,
    /// Single protocol owner and application command/completion correlation.
    driver: TcpDriver<Observer, Completion>,
    /// Sole transport task and join owner.
    io: IoTasks,
    /// Alternates command admission with ordinary network work under contention.
    prefer_command: bool,
    /// Validated finite lifecycle policy used by this generation.
    policy: crate::hsms::RuntimePolicy,
    /// First graceful lifecycle request; repeated requests never extend deadlines.
    shutdown: Option<ShutdownDrain>,
}

impl<Observer: SessionStateObserver, Completion: CommandCompletion>
    GenerationRuntime<Observer, Completion>
{
    /// Starts a connected generation and supplies Core's initial connection input.
    /// Active auto-Select enters the common command FIFO exactly once per launch.
    pub(crate) fn from_stream(
        stream: TcpStream,
        generation: ConnectionGeneration,
        config: &EndpointConfig,
        observer: Observer,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        let epoch = Instant::now();
        let (io, writer, closer) = IoTasks::launch(stream, config, epoch)?;
        Self::from_transport(generation, config, observer, epoch, io, writer, closer)
    }

    /// Attaches owned workers to the protocol owner using their shared epoch.
    fn from_transport(
        generation: ConnectionGeneration,
        config: &EndpointConfig,
        observer: Observer,
        epoch: Instant,
        io: IoTasks,
        writer: BoundedWriter,
        closer: IoCloser,
    ) -> Result<Self, ConfigError> {
        let mut driver = SessionDriver::from_config(generation, config, writer, observer, closer)?;
        driver.on_connected(MonoTime::ZERO);
        if config.mode() == ConnectionMode::Active && config.runtime().auto_select() {
            // A fresh validated Driver always has room for its first command.
            if driver
                .try_accept_control(ControlIntent::Select, RuntimeCompletion::Internal)
                .is_err()
            {
                driver.on_shutdown(
                    GenerationCloseReason::RuntimeInvariant,
                    None,
                    MonoTime::ZERO,
                );
            }
        }
        Ok(Self {
            generation,
            epoch,
            driver,
            io,
            prefer_command: false,
            policy: config.runtime(),
            shutdown: None,
        })
    }

    /// Borrows the Driver for snapshots and completion/resource inspection.
    pub(crate) fn driver(&self) -> &TcpDriver<Observer, Completion> {
        &self.driver
    }

    /// Returns the immutable connection incarnation for endpoint command routing.
    pub(crate) const fn generation(&self) -> ConnectionGeneration {
        self.generation
    }

    /// Reports whether endpoint lifecycle admission has entered closure.
    pub(crate) fn is_draining(&self) -> bool {
        self.shutdown.is_some() || self.driver.close_reason().is_some()
    }

    /// Borrows the Driver for synchronous command admission and event extraction.
    /// No reference may be retained across a runtime step.
    pub(crate) fn driver_mut(&mut self) -> &mut TcpDriver<Observer, Completion> {
        &mut self.driver
    }

    /// Begins finite Stop/Disconnect drain without blocking ongoing reply work.
    /// The first request owns the reason and deadlines; repeated calls are no-ops.
    pub(crate) fn begin_graceful_close(&mut self, reason: GenerationCloseReason) {
        if self.shutdown.is_some() || self.driver.close_reason().is_some() {
            return;
        }
        self.driver.begin_shutdown_drain();
        let now = Instant::now();
        let deadlines = now
            .checked_add(self.policy.drain())
            .zip(now.checked_add(self.policy.shutdown()));
        if let Some((drain_deadline, shutdown_deadline)) = deadlines {
            self.shutdown = Some(ShutdownDrain {
                reason,
                drain_deadline: drain_deadline.min(shutdown_deadline),
                shutdown_deadline,
                finalizing: false,
            });
        } else {
            self.driver.on_shutdown(
                GenerationCloseReason::RuntimeInvariant,
                None,
                MonoTime::from_elapsed(self.epoch.elapsed()),
            );
        }
    }

    /// Advances graceful close once work drains or an absolute deadline expires.
    fn advance_shutdown(&mut self) {
        let Some(drain) = self.shutdown else {
            return;
        };
        let now = Instant::now();
        let logical = MonoTime::from_elapsed(self.epoch.elapsed());
        if now >= drain.shutdown_deadline {
            self.driver.on_shutdown(drain.reason, None, logical);
        } else if !drain.finalizing
            && (now >= drain.drain_deadline || self.driver.shutdown_drain_ready())
        {
            self.shutdown
                .as_mut()
                .expect("shutdown retained")
                .finalizing = true;
            self.driver.finish_shutdown_drain(drain.reason, logical);
        }
    }

    /// Joins a terminated generation using both cleanup and total shutdown bounds.
    pub(crate) async fn finish(&mut self) -> SessionExit {
        let deadline = Instant::now()
            .checked_add(self.policy.cleanup())
            .unwrap_or_else(Instant::now);
        let deadline = self
            .shutdown
            .map_or(deadline, |drain| deadline.min(drain.shutdown_deadline));
        let reason = self
            .driver
            .close_reason()
            .or(self.shutdown.map(|drain| drain.reason))
            .unwrap_or(GenerationCloseReason::RuntimeInvariant);
        self.close(reason, deadline).await
    }

    /// Services one queued command, network fact or due timer without a busy loop.
    /// Cancelling this wait preserves task ownership and unconsumed channel input.
    /// Returns false once transport closure requires the owner to run cleanup.
    #[cfg(test)]
    pub(crate) async fn step(&mut self) -> bool {
        self.step_with_delivery_capacity(|| None).await
    }

    /// Samples downstream slots after waiting, so consumer progress during idle
    /// time is visible before Core admits a new application event.
    pub(crate) async fn step_with_delivery_capacity(
        &mut self,
        capacity: impl Fn() -> Option<(usize, usize)>,
    ) -> bool {
        if self.driver.transport_closed() {
            return false;
        }
        let core_deadline = self
            .driver
            .next_deadline()
            .and_then(|time| self.epoch.checked_add(time.elapsed()));
        let shutdown_deadline = self.shutdown.map(|drain| {
            if drain.finalizing {
                drain.shutdown_deadline
            } else {
                drain.drain_deadline
            }
        });
        let deadline = core_deadline.into_iter().chain(shutdown_deadline).min();
        let mut timer = Box::pin(wait_deadline(deadline));
        let input = std::future::poll_fn(|cx| {
            if let Some(event) = self.io.poll_terminal(cx) {
                return Poll::Ready(RoundInput::Io(Some(event)));
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline)
                || timer.as_mut().poll(cx).is_ready()
            {
                return Poll::Ready(RoundInput::Timer);
            }
            let command_ready = self.driver.queued_command_count() != 0;
            if self.shutdown.is_some_and(|drain| !drain.finalizing)
                && self.driver.shutdown_drain_ready()
            {
                return Poll::Ready(RoundInput::Shutdown);
            }
            if self.prefer_command && command_ready {
                return Poll::Ready(RoundInput::Command);
            }
            if let Poll::Ready(event) = self.io.poll_next(cx) {
                return Poll::Ready(RoundInput::Io(event));
            }
            if command_ready {
                Poll::Ready(RoundInput::Command)
            } else {
                Poll::Pending
            }
        })
        .await;
        self.apply_round(input, capacity)
    }

    /// Observes worker completion so contention tests need no timing assumptions.
    #[cfg(test)]
    pub(crate) fn reader_finished(&self) -> bool {
        self.io.reader_finished()
    }

    /// Applies ready terminal facts and deadlines before an outer endpoint command.
    /// This polls once without waiting for network progress, preserving ordinary
    /// source heads and their charges for the normal fair scheduler.
    pub(crate) async fn process_ready_priority(&mut self) {
        if self.driver.transport_closed() {
            return;
        }
        let terminal = std::future::poll_fn(|cx| Poll::Ready(self.io.poll_terminal(cx))).await;
        if let Some(event) = terminal {
            self.apply_round(RoundInput::Io(Some(event)), || None);
        }
        if !self.driver.transport_closed() {
            self.apply_round(RoundInput::Timer, || None);
        }
    }

    /// Applies a selected input and samples current application capacity once.
    fn apply_round(
        &mut self,
        input: RoundInput,
        capacity: impl Fn() -> Option<(usize, usize)>,
    ) -> bool {
        if let Some((primaries, errors)) = capacity() {
            self.driver.set_delivery_capacity(primaries, errors);
        }
        match input {
            RoundInput::Shutdown => self.advance_shutdown(),
            RoundInput::Io(event) => {
                self.prefer_command = true;
                let now = MonoTime::from_elapsed(self.epoch.elapsed());
                match event {
                    Some(IoEvent::Read(report)) => {
                        if report.frame.occurred_at > now {
                            self.driver.on_shutdown(
                                GenerationCloseReason::RuntimeInvariant,
                                None,
                                now,
                            );
                        } else {
                            self.driver.on_decode_step_with_header(
                                report.frame.decoded,
                                Some(*report.frame.header.as_bytes()),
                                now,
                            );
                        }
                    }
                    Some(IoEvent::Write(report)) => {
                        Self::apply_write_report(&mut self.driver, report, now);
                    }
                    Some(IoEvent::ReaderStopped(result)) => {
                        let reason = match result {
                            Err(_) | Ok(Err(ReadFailure::DeadlineOverflow)) => {
                                GenerationCloseReason::RuntimeInvariant
                            }
                            Ok(Err(ReadFailure::IntercharacterTimeout)) => {
                                GenerationCloseReason::CommunicationsTimeout(
                                    CommunicationsTimeoutKind::T8,
                                )
                            }
                            Ok(Err(ReadFailure::Framing(_))) => {
                                GenerationCloseReason::ProtocolViolation
                            }
                            _ => GenerationCloseReason::TransportLost,
                        };
                        self.driver.on_shutdown(reason, None, now);
                    }
                    Some(IoEvent::WriterStopped(result)) => {
                        let reason = if result.is_err() {
                            GenerationCloseReason::RuntimeInvariant
                        } else {
                            GenerationCloseReason::TransportLost
                        };
                        self.driver.on_shutdown(reason, None, now);
                        self.driver.on_writer_stopped(now);
                    }
                    None => {
                        self.driver
                            .on_shutdown(GenerationCloseReason::RuntimeInvariant, None, now)
                    }
                }
            }
            RoundInput::Timer => {
                self.driver
                    .advance_time(MonoTime::from_elapsed(self.epoch.elapsed()));
                self.advance_shutdown();
            }
            RoundInput::Command => {
                self.prefer_command = false;
                self.driver
                    .drive_next_command(MonoTime::from_elapsed(self.epoch.elapsed()));
            }
        }
        !self.driver.transport_closed()
    }

    /// Validates Writer's admission identity before applying a visibility fact.
    /// Invalid reports leave unresolved writes for conservative finalization.
    fn apply_write_report(
        driver: &mut TcpDriver<Observer, Completion>,
        report: super::transport::bounded_writer::WriterReport,
        now: MonoTime,
    ) {
        if driver.wire_sequence(report.write_id) != Some(report.sequence) {
            driver.on_shutdown(GenerationCloseReason::RuntimeInvariant, None, now);
            return;
        }
        driver.on_write_outcome_at(report.write_id, report.outcome, report.occurred_at, now);
    }

    /// Forces a generation close using its own monotonic clock and normal settlement.
    pub(crate) fn force_close(&mut self, reason: GenerationCloseReason) {
        self.driver
            .on_shutdown(reason, None, MonoTime::from_elapsed(self.epoch.elapsed()));
    }

    /// Retries resource proof after an explicit endpoint Stop. This never reopens
    /// protocol admission or resends work; it drains facts before final settlement.
    pub(crate) async fn recover_cleanup(&mut self) -> SessionExit {
        let reason = self
            .driver
            .close_reason()
            .unwrap_or(GenerationCloseReason::RuntimeInvariant);
        self.driver
            .on_shutdown(reason, None, MonoTime::from_elapsed(self.epoch.elapsed()));
        let deadline = Instant::now()
            .checked_add(self.policy.cleanup())
            .unwrap_or_else(Instant::now);
        let driver = &mut self.driver;
        let epoch = self.epoch;
        let cleanup = self
            .io
            .recover(deadline, |report| {
                Self::apply_write_report(driver, report, MonoTime::from_elapsed(epoch.elapsed()));
            })
            .await;
        if self.io.writer_joined() {
            self.driver
                .on_writer_stopped(MonoTime::from_elapsed(self.epoch.elapsed()));
        }
        SessionExit {
            generation: self.generation,
            reason,
            cleanup,
        }
    }

    /// Forces admission closed with `reason`, then settles writes while joining
    /// within an absolute deadline. Poisoned cleanup forbids generation replacement.
    /// Missing writes are finalized only when Writer join/report drain is proven.
    pub(crate) async fn close(
        &mut self,
        reason: GenerationCloseReason,
        deadline: Instant,
    ) -> SessionExit {
        self.driver
            .on_shutdown(reason, None, MonoTime::from_elapsed(self.epoch.elapsed()));
        let driver = &mut self.driver;
        let epoch = self.epoch;
        let cleanup = self
            .io
            .cleanup(deadline, |report| {
                Self::apply_write_report(driver, report, MonoTime::from_elapsed(epoch.elapsed()));
            })
            .await;
        if self.io.writer_joined() {
            self.driver
                .on_writer_stopped(MonoTime::from_elapsed(self.epoch.elapsed()));
        }
        SessionExit {
            generation: self.generation,
            reason: self.driver.close_reason().unwrap_or(reason),
            cleanup,
        }
    }
}

/// Waits for the earliest Core deadline, remaining pending when Core has none.
async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::{
        generation::driver::DriverCommandResult, lifecycle::SessionState,
        supervisor::session::CleanupResult, Function, PrimaryMessage, SessionId, Stream,
    };
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
        sync::oneshot,
    };

    /// State sink for runtime tests that inspect Driver's committed snapshot.
    struct Observer;
    impl SessionStateObserver for Observer {
        /// Accepts each committed state; the Driver retains the inspected value.
        fn observe(&mut self, _: SessionState) {}
    }

    /// Exactly-once completion endpoint used by independently driven TCP tests.
    struct Completion(
        /// Single receiver for the command's terminal protocol result.
        oneshot::Sender<DriverCommandResult>,
    );
    impl CommandCompletion for Completion {
        /// Delivers a terminal result without making receiver drop cancel the work.
        fn complete(self, result: DriverCommandResult) {
            let _ = self.0.send(result);
        }
    }

    /// Builds a connected production generation and independent loopback peer.
    async fn connection(
        mode: ConnectionMode,
        auto_select: bool,
    ) -> (GenerationRuntime<Observer, Completion>, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let stream = TcpStream::connect(address).await.unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        let config = match mode {
            ConnectionMode::Active => EndpointConfig::active(address, SessionId::new(7).unwrap()),
            ConnectionMode::Passive => EndpointConfig::passive(address, SessionId::new(7).unwrap()),
        }
        .with_runtime(crate::hsms::RuntimePolicy::default().with_auto_select(auto_select));
        (
            GenerationRuntime::from_stream(stream, ConnectionGeneration::new(1), &config, Observer)
                .unwrap(),
            peer,
        )
    }

    /// Selects a real Active generation using an independently encoded response.
    async fn selected_connection() -> (GenerationRuntime<Observer, Completion>, TcpStream) {
        let (mut runtime, mut peer) = connection(ConnectionMode::Active, true).await;
        let peer_task = tokio::spawn(async move {
            let mut frame = [0; 14];
            peer.read_exact(&mut frame).await.unwrap();
            frame[9] = 2;
            peer.write_all(&frame).await.unwrap();
            peer
        });
        while runtime.driver().state() != Some(SessionState::Selected) {
            assert!(runtime.step().await);
        }
        (runtime, peer_task.await.unwrap())
    }

    /// The total close deadline bounds a stalled Separate or Data preceding it.
    #[tokio::test(start_paused = true)]
    async fn absolute_shutdown_bounds_partial_data_and_separate_writes() {
        for data_first in [false, true] {
            let config = EndpointConfig::active(
                "127.0.0.1:5000".parse().unwrap(),
                SessionId::new(7).unwrap(),
            )
            .with_runtime(crate::hsms::RuntimePolicy::default().with_deadlines(
                Duration::from_secs(1),
                Duration::from_secs(30),
                Duration::from_secs(30),
                Duration::from_secs(1),
                Duration::from_secs(2),
            ));
            let (stream, mut peer) = tokio::io::duplex(4);
            let (read, write) = tokio::io::split(stream);
            let epoch = Instant::now();
            let (io, writer, closer) = IoTasks::launch_halves(read, write, &config, epoch).unwrap();
            let mut runtime: GenerationRuntime<Observer, Completion> =
                GenerationRuntime::from_transport(
                    ConnectionGeneration::new(1),
                    &config,
                    Observer,
                    epoch,
                    io,
                    writer,
                    closer,
                )
                .unwrap();
            let selecting = tokio::spawn(async move {
                let mut frame = [0; 14];
                peer.read_exact(&mut frame).await.unwrap();
                frame[9] = 2;
                peer.write_all(&frame).await.unwrap();
                peer
            });
            while runtime.driver().state() != Some(SessionState::Selected)
                || runtime.driver().pending_write_count() != 0
            {
                assert!(runtime.step().await);
            }
            let mut peer = selecting.await.unwrap();
            let (sender, mut result) = oneshot::channel();
            if data_first {
                assert!(runtime
                    .driver_mut()
                    .try_accept_send(
                        PrimaryMessage::new(Stream::new(1).unwrap(), Function::new(1), None),
                        RuntimeCompletion::Application(Completion(sender)),
                    )
                    .is_ok());
                assert!(runtime.step().await);
            } else {
                drop(sender);
            }
            let started = Instant::now();
            runtime.begin_graceful_close(GenerationCloseReason::LocalStop);
            while runtime.step().await {}
            assert_eq!(started.elapsed(), Duration::from_secs(2));
            assert_eq!(
                runtime.driver().close_reason(),
                Some(GenerationCloseReason::LocalStop)
            );
            let exit = runtime.finish().await;
            assert!(matches!(exit.cleanup, CleanupResult::Poisoned(_)));
            assert_eq!(started.elapsed(), Duration::from_secs(2));
            if data_first {
                assert!(result.try_recv().is_err());
            }
            // An expired total deadline cannot claim resource release. An explicit
            // fresh cleanup attempt joins aborted tasks before finalizing outcomes.
            let recovered = runtime.recover_cleanup().await;
            assert_eq!(recovered.cleanup, CleanupResult::Clean);
            assert_eq!(recovered.reason, GenerationCloseReason::LocalStop);
            if data_first {
                assert!(matches!(
                    result.await.unwrap(),
                    DriverCommandResult::Send(Err(
                        crate::hsms::OperationError::DeliveryIndeterminate
                    ))
                ));
            }
            let mut bytes = Vec::new();
            peer.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(bytes, [0, 0, 0, 10]);
        }
    }

    /// An unconsumed Control write report cannot leave a rejected tail barrier stuck.
    #[tokio::test(start_paused = true)]
    async fn full_control_lane_rejects_tail_separate_and_still_cleans_up() {
        let config = EndpointConfig::active(
            "127.0.0.1:5000".parse().unwrap(),
            SessionId::new(7).unwrap(),
        )
        .with_limits(crate::hsms::EndpointLimits::new(32, 4, 1, 2, 2, 2, 2, 2).unwrap())
        .with_runtime(crate::hsms::RuntimePolicy::default().with_deadlines(
            Duration::from_secs(1),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Duration::from_secs(1),
            Duration::from_secs(3),
        ));
        let (stream, mut peer) = tokio::io::duplex(64);
        let (read, write) = tokio::io::split(stream);
        let epoch = Instant::now();
        let (io, writer, closer) = IoTasks::launch_halves(read, write, &config, epoch).unwrap();
        let mut runtime: GenerationRuntime<Observer, Completion> =
            GenerationRuntime::from_transport(
                ConnectionGeneration::new(1),
                &config,
                Observer,
                epoch,
                io,
                writer,
                closer,
            )
            .unwrap();
        let selecting = tokio::spawn(async move {
            let mut frame = [0; 14];
            peer.read_exact(&mut frame).await.unwrap();
            frame[9] = 2;
            peer.write_all(&frame).await.unwrap();
            peer
        });
        while runtime.driver().state() != Some(SessionState::Selected)
            || runtime.driver().pending_write_count() != 0
        {
            assert!(runtime.step().await);
        }
        let mut peer = selecting.await.unwrap();
        let (sender, result) = oneshot::channel();
        assert!(runtime
            .driver_mut()
            .try_accept_control(
                ControlIntent::Linktest,
                RuntimeCompletion::Application(Completion(sender))
            )
            .is_ok());
        assert!(runtime.step().await);
        let mut frame = [0; 14];
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame[9], 5);
        assert_eq!(runtime.driver().pending_write_count(), 1);
        runtime.begin_graceful_close(GenerationCloseReason::LocalStop);
        tokio::time::advance(Duration::from_secs(1)).await;
        // Due drain wins over the ordinary commit report: its owned Control
        // reservation is still held when the tail Separate attempts admission.
        assert!(!runtime.step().await);
        let exit = runtime.finish().await;
        assert_eq!(exit.cleanup, CleanupResult::Clean);
        assert_eq!(exit.reason, GenerationCloseReason::LocalStop);
        assert!(matches!(
            result.await.unwrap(),
            DriverCommandResult::Control(Err(_))
        ));
        let mut remaining = Vec::new();
        peer.read_to_end(&mut remaining).await.unwrap();
        assert!(
            remaining.is_empty(),
            "rejected Separate must never reach wire"
        );
    }

    /// Actual Writer failure precedes a later Stop and retains visibility distinctions.
    #[tokio::test]
    async fn writer_terminal_priority_preserves_zero_and_partial_delivery() {
        tokio::time::timeout(Duration::from_secs(3), async {
            for partial in [false, true] {
                let config = EndpointConfig::active(
                    "127.0.0.1:5000".parse().unwrap(),
                    SessionId::new(7).unwrap(),
                );
                let (read, mut peer_write) = tokio::io::duplex(64);
                let (write, mut peer_read) = tokio::io::duplex(4);
                let epoch = Instant::now();
                let (io, writer, closer) =
                    IoTasks::launch_halves(read, write, &config, epoch).unwrap();
                let mut runtime: GenerationRuntime<Observer, Completion> =
                    GenerationRuntime::from_transport(
                        ConnectionGeneration::new(1),
                        &config,
                        Observer,
                        epoch,
                        io,
                        writer,
                        closer,
                    )
                    .unwrap();
                let selecting = tokio::spawn(async move {
                    let mut frame = [0; 14];
                    peer_read.read_exact(&mut frame).await.unwrap();
                    frame[9] = 2;
                    peer_write.write_all(&frame).await.unwrap();
                    (peer_read, peer_write)
                });
                while runtime.driver().state() != Some(SessionState::Selected)
                    || runtime.driver().pending_write_count() != 0
                {
                    assert!(runtime.step().await);
                }
                let (mut peer_read, _peer_write) = selecting.await.unwrap();
                let (sender, result) = oneshot::channel();
                assert!(runtime
                    .driver_mut()
                    .try_accept_send(
                        PrimaryMessage::new(Stream::new(1).unwrap(), Function::new(1), None),
                        RuntimeCompletion::Application(Completion(sender)),
                    )
                    .is_ok());
                assert!(runtime.step().await);
                if partial {
                    let mut prefix = [0; 4];
                    peer_read.read_exact(&mut prefix).await.unwrap();
                    assert_eq!(prefix, [0, 0, 0, 10]);
                }
                drop(peer_read);
                while !runtime.io.writer_finished() {
                    tokio::task::yield_now().await;
                }
                // This is the same priority pass used before endpoint dispatch;
                // neither the write report nor join result has been consumed yet.
                runtime.process_ready_priority().await;
                runtime.begin_graceful_close(GenerationCloseReason::LocalStop);
                assert_eq!(
                    runtime.driver().close_reason(),
                    Some(GenerationCloseReason::TransportLost)
                );
                let exit = runtime.finish().await;
                assert_eq!(exit.cleanup, CleanupResult::Clean);
                let DriverCommandResult::Send(Err(error)) = result.await.unwrap() else {
                    panic!("failed send")
                };
                assert_eq!(
                    error,
                    if partial {
                        crate::hsms::OperationError::DeliveryIndeterminate
                    } else {
                        crate::hsms::OperationError::ConnectionLost
                    }
                );
            }
        })
        .await
        .unwrap();
    }

    /// A corrupted Writer sequence cannot fabricate a successful local send receipt.
    #[tokio::test]
    async fn mismatched_writer_sequence_closes_and_settles_conservatively() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (mut runtime, mut peer) = selected_connection().await;
            while runtime.driver().pending_write_count() != 0 {
                assert!(runtime.step().await);
            }
            let (sender, receiver) = oneshot::channel();
            assert!(runtime
                .driver_mut()
                .try_accept_send(
                    PrimaryMessage::new(Stream::new(1).unwrap(), Function::new(1), None),
                    RuntimeCompletion::Application(Completion(sender))
                )
                .is_ok());
            let now = MonoTime::from_elapsed(runtime.epoch.elapsed());
            assert!(runtime.driver_mut().drive_next_command(now));
            let mut frame = [0; 14];
            peer.read_exact(&mut frame).await.unwrap();
            let Some(IoEvent::Write(mut report)) = runtime.io.next().await else {
                panic!("Writer result expected")
            };
            report.sequence = crate::hsms::model::ids::WireSequence::new(u64::MAX);
            let now = MonoTime::from_elapsed(runtime.epoch.elapsed());
            GenerationRuntime::<Observer, Completion>::apply_write_report(
                &mut runtime.driver,
                report,
                now,
            );
            assert_eq!(
                runtime.driver().close_reason(),
                Some(GenerationCloseReason::RuntimeInvariant)
            );
            let exit = runtime.finish().await;
            assert_eq!(exit.cleanup, CleanupResult::Clean);
            assert!(matches!(
                receiver.await.unwrap(),
                DriverCommandResult::Send(Err(crate::hsms::OperationError::DeliveryIndeterminate))
            ));
        })
        .await
        .unwrap();
    }

    /// Stop drains an accepted request, refuses new Primaries, and writes Separate last.
    #[tokio::test]
    async fn graceful_stop_settles_request_before_tail_separate() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (mut runtime, mut peer) = selected_connection().await;
            let peer_task = tokio::spawn(async move {
                let mut frame = [0;14];
                peer.read_exact(&mut frame).await.unwrap();
                assert_eq!(frame[6], 129);
                frame[6] = 1; frame[7] = 2;
                peer.write_all(&frame).await.unwrap();
                peer.read_exact(&mut frame).await.unwrap();
                assert_eq!(&frame[..10], &[0,0,0,10,255,255,0,0,0,9]);
                let mut byte = [0];
                assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
            });
            let (sender, receiver) = oneshot::channel();
            assert!(runtime.driver_mut().try_accept_request(PrimaryMessage::new(Stream::new(1).unwrap(), Function::new(1), None), RuntimeCompletion::Application(Completion(sender))).is_ok());
            runtime.begin_graceful_close(GenerationCloseReason::LocalStop);
            let original_deadline = runtime.shutdown.unwrap().shutdown_deadline;
            runtime.begin_graceful_close(GenerationCloseReason::LocalDisconnect);
            assert_eq!(runtime.shutdown.unwrap().shutdown_deadline, original_deadline);
            let (sender, _receiver) = oneshot::channel();
            let rejected = runtime.driver_mut().try_accept_send(PrimaryMessage::new(Stream::new(1).unwrap(), Function::new(1), None), RuntimeCompletion::Application(Completion(sender)));
            assert!(matches!(rejected, Err(error) if error.kind() == super::super::driver::DataAdmissionErrorKind::Draining));
            while runtime.step().await {}
            assert!(matches!(receiver.await.unwrap(), DriverCommandResult::Request(Ok(_))));
            let exit = runtime.finish().await;
            assert_eq!(exit.reason, GenerationCloseReason::LocalStop);
            assert_eq!(exit.cleanup, CleanupResult::Clean);
            assert_eq!(runtime.driver().pending_write_count(), 0);
            peer_task.await.unwrap();
        }).await.unwrap();
    }

    /// A retained peer reply token cannot extend the configured drain interval.
    #[tokio::test]
    async fn graceful_disconnect_expires_drain_with_unanswered_peer_primary() {
        tokio::time::timeout(Duration::from_secs(2), async {
            let (mut runtime, mut peer) = selected_connection().await;
            runtime.policy = runtime.policy.with_deadlines(
                Duration::from_millis(10),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(1),
                Duration::from_secs(2),
            );
            peer.write_all(&[0, 0, 0, 10, 0, 7, 129, 1, 0, 0, 0, 0, 0, 42])
                .await
                .unwrap();
            let _capability = loop {
                assert!(runtime.step().await);
                if let Some(primary) = runtime.driver_mut().take_inbound() {
                    break primary;
                }
            };
            runtime.begin_graceful_close(GenerationCloseReason::LocalDisconnect);
            assert!(!runtime.driver().shutdown_drain_ready());
            while runtime.step().await {}
            let mut separate = [0; 14];
            peer.read_exact(&mut separate).await.unwrap();
            assert_eq!(separate[9], 9);
            let exit = runtime.finish().await;
            assert_eq!(exit.reason, GenerationCloseReason::LocalDisconnect);
            assert_eq!(exit.cleanup, CleanupResult::Clean);
        })
        .await
        .unwrap();
    }

    /// Autonomous I/O rounds match a Secondary before processing its following FIN.
    #[tokio::test]
    async fn runtime_select_request_and_fin_preserve_completion() {
        let (mut runtime, mut peer) = connection(ConnectionMode::Passive, true).await;
        let peer_task = tokio::spawn(async move {
            peer.write_all(&[0, 0, 0, 10, 255, 255, 0, 0, 0, 1, 0, 0, 0, 9])
                .await
                .unwrap();
            let mut select = [0; 14];
            peer.read_exact(&mut select).await.unwrap();
            assert_eq!(select, [0, 0, 0, 10, 255, 255, 0, 0, 0, 2, 0, 0, 0, 9]);
            let mut request = [0; 14];
            peer.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..10], &[0, 0, 0, 10, 0, 7, 129, 1, 0, 0]);
            request[6] = 1;
            request[7] = 2;
            peer.write_all(&request).await.unwrap();
            peer.shutdown().await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while runtime.driver().state() != Some(SessionState::Selected) {
                assert!(runtime.step().await);
            }
            let (sender, receiver) = oneshot::channel();
            assert!(runtime
                .driver_mut()
                .try_accept_request(
                    PrimaryMessage::new(Stream::new(1).unwrap(), Function::new(1), None),
                    RuntimeCompletion::Application(Completion(sender))
                )
                .is_ok());
            while runtime.step().await {}
            let DriverCommandResult::Request(Ok(secondary)) = receiver.await.unwrap() else {
                panic!("Secondary must precede FIN closure")
            };
            assert_eq!(secondary.function(), Function::new(2));
            assert_eq!(
                secondary.context().generation(),
                ConnectionGeneration::new(1)
            );
            let exit = runtime
                .close(
                    GenerationCloseReason::LocalStop,
                    Instant::now() + Duration::from_secs(1),
                )
                .await;
            assert_eq!(exit.reason, GenerationCloseReason::TransportLost);
            assert_eq!(exit.cleanup, CleanupResult::Clean);
            assert_eq!(runtime.driver().pending_completion_count(), 0);
            assert_eq!(runtime.driver().pending_write_count(), 0);
            peer_task.await.unwrap();
        })
        .await
        .unwrap();
    }

    /// Active auto-Select uses normal framing, matching and completion settlement.
    #[tokio::test]
    async fn active_auto_select_runs_once_and_matches_independent_response() {
        let (mut runtime, mut peer) = connection(ConnectionMode::Active, true).await;
        assert_eq!(runtime.driver().queued_command_count(), 1);
        let peer_task = tokio::spawn(async move {
            let mut select = [0; 14];
            peer.read_exact(&mut select).await.unwrap();
            assert_eq!(&select[..10], &[0, 0, 0, 10, 255, 255, 0, 0, 0, 1]);
            select[9] = 2;
            peer.write_all(&select).await.unwrap();
            peer
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while runtime.driver().state() != Some(SessionState::Selected) {
                assert!(runtime.step().await);
            }
        })
        .await
        .unwrap();
        let _peer = peer_task.await.unwrap();
        assert_eq!(runtime.driver().queued_command_count(), 0);
        assert_eq!(runtime.driver().open_core_command_count(), 0);
        let exit = runtime
            .close(
                GenerationCloseReason::LocalStop,
                Instant::now() + Duration::from_secs(1),
            )
            .await;
        assert_eq!(exit.cleanup, CleanupResult::Clean);
        assert_eq!(runtime.driver().pending_completion_count(), 0);
    }

    /// An unanswered automatic Select uses the normal post-commit T6 deadline.
    #[tokio::test(start_paused = true)]
    async fn automatic_select_timeout_closes_without_application_completion() {
        let (mut runtime, _peer) = connection(ConnectionMode::Active, true).await;
        while runtime.step().await {}
        assert_eq!(
            runtime.driver().close_reason(),
            Some(GenerationCloseReason::CommunicationsTimeout(
                CommunicationsTimeoutKind::T6
            ))
        );
        let exit = runtime
            .close(
                GenerationCloseReason::LocalStop,
                Instant::now() + Duration::from_secs(1),
            )
            .await;
        assert_eq!(exit.cleanup, CleanupResult::Clean);
        assert_eq!(runtime.driver().pending_completion_count(), 0);
        assert_eq!(runtime.driver().pending_write_count(), 0);
    }

    /// Disabling auto-Select leaves Active connected and governed by T7.
    #[tokio::test(start_paused = true)]
    async fn active_without_auto_select_does_not_queue_control_work() {
        let (mut runtime, _peer) = connection(ConnectionMode::Active, false).await;
        assert_eq!(runtime.driver().queued_command_count(), 0);
        assert!(!runtime.step().await);
        assert_eq!(
            runtime.driver().close_reason(),
            Some(GenerationCloseReason::CommunicationsTimeout(
                CommunicationsTimeoutKind::T7
            ))
        );
        assert_eq!(
            runtime
                .close(
                    GenerationCloseReason::LocalStop,
                    Instant::now() + Duration::from_secs(1)
                )
                .await
                .cleanup,
            CleanupResult::Clean
        );
    }

    /// A silent connected peer expires T7 without external timer injection.
    #[tokio::test(start_paused = true)]
    async fn runtime_drives_t7_and_joins_cancelled_workers() {
        let (mut runtime, _peer) = connection(ConnectionMode::Passive, true).await;
        assert!(!runtime.step().await);
        let exit = runtime
            .close(
                GenerationCloseReason::LocalStop,
                Instant::now() + Duration::from_secs(1),
            )
            .await;
        assert_eq!(
            exit.reason,
            GenerationCloseReason::CommunicationsTimeout(CommunicationsTimeoutKind::T7)
        );
        assert_eq!(exit.cleanup, CleanupResult::Clean);
    }
}
