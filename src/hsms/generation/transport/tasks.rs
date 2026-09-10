//! Owns the two TCP worker tasks for one connection generation.
//! Reports are consumed before each source's exit, preserving frame/FIN and
//! write/exit order. Cancellation keeps write settlement alive during bounded
//! cleanup; dropping the owner aborts tasks instead of silently detaching them.

use super::{
    bounded_reader::{ReaderReport, ReaderWorker},
    bounded_writer::{BoundedWriter, WriterReport},
    io::ReadFailure,
};
use crate::hsms::{
    generation::driver::TransportCloser,
    supervisor::session::{CleanupPoison, CleanupResult},
    ConfigError, EndpointConfig,
};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    net::TcpStream,
    sync::{mpsc, watch},
    task::{JoinError, JoinHandle},
    time::Instant,
};

/// One source-ordered event consumed by the generation's serialized run loop.
#[derive(Debug)]
pub(crate) enum IoEvent {
    /// A complete incoming frame retaining its resource reservations.
    Read(ReaderReport),
    /// Actual visibility of an admitted frame, retaining its reservations.
    Write(WriterReport),
    /// Reader exit, emitted only after its final published frame was consumed.
    ReaderStopped(Result<Result<(), ReadFailure>, JoinError>),
    /// Writer exit, emitted only after all published write outcomes were consumed.
    WriterStopped(Result<(), JoinError>),
}

/// Cancellation side retained by Driver; signalling never drops a write future.
pub(crate) struct IoCloser {
    /// Shared one-way cancellation latch for both workers.
    cancellation: watch::Sender<bool>,
}

impl TransportCloser for IoCloser {
    /// Latches cancellation; workers settle writes and release their socket halves.
    fn close(&mut self) {
        self.cancellation.send_replace(true);
    }
}

/// Exclusive task owner retained until cleanup yields a proof or sticky poison.
pub(crate) struct IoTasks {
    /// Shared cancellation latch, also handed to Driver through IoCloser.
    cancellation: watch::Sender<bool>,
    /// FIFO of complete incoming frames preceding Reader termination.
    reads: mpsc::Receiver<ReaderReport>,
    /// FIFO of write visibility facts preceding Writer termination.
    writes: mpsc::Receiver<WriterReport>,
    /// Reader task owned until its join result is observed.
    reader: Option<JoinHandle<Result<(), ReadFailure>>>,
    /// Writer task owned until its join result is observed.
    writer: Option<JoinHandle<()>>,
    /// First cleanup failure, never upgraded to Clean on subsequent calls.
    poison: Option<CleanupPoison>,
    /// At most one retained Reader head, still charged against worker capacity.
    pending_read: Option<ReaderReport>,
    /// At most one retained Writer head, preserving its original FIFO position.
    pending_write: Option<WriterReport>,
    /// Whether the next ordinary tie should favor Reader.
    prefer_read: bool,
}

impl IoTasks {
    /// Observes actual Reader task completion without consuming its ordered exit.
    #[cfg(test)]
    pub(crate) fn reader_finished(&self) -> bool {
        self.reader.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Observes Writer completion while preserving its queued reports and join fact.
    #[cfg(test)]
    pub(crate) fn writer_finished(&self) -> bool {
        self.writer.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// Returns whether Writer exit and every preceding report were consumed.
    pub(crate) fn writer_joined(&self) -> bool {
        self.writer.is_none()
    }

    /// Starts both workers for `stream` using one validated config and clock epoch.
    /// Must be called inside a running Tokio runtime after connection establishment.
    pub(crate) fn launch(
        stream: TcpStream,
        config: &EndpointConfig,
        epoch: Instant,
    ) -> Result<(Self, BoundedWriter, IoCloser), ConfigError> {
        config.validate()?;
        let (read, write) = stream.into_split();
        Self::launch_halves(read, write, config, epoch)
    }

    /// Owns transport halves in the same production workers, allowing bounded
    /// in-memory transports to exercise exact partial-write and shutdown races.
    pub(crate) fn launch_halves<R, W>(
        read: R,
        write: W,
        config: &EndpointConfig,
        epoch: Instant,
    ) -> Result<(Self, BoundedWriter, IoCloser), ConfigError>
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        config.validate()?;
        let (reader, reads) = ReaderWorker::from_config(read, config, epoch)?;
        let (ingress, writer, writes) = BoundedWriter::from_config(config, epoch)?;
        let (cancellation, signal) = watch::channel(false);
        let reader = tokio::spawn(reader.run(signal.clone()));
        let writer = tokio::spawn(writer.run(write, signal));
        let closer = IoCloser {
            cancellation: cancellation.clone(),
        };
        Ok((
            Self {
                cancellation,
                reads,
                writes,
                reader: Some(reader),
                writer: Some(writer),
                poison: None,
                pending_read: None,
                pending_write: None,
                prefer_read: true,
            },
            ingress,
            closer,
        ))
    }

    /// Waits fairly between source FIFOs, returning None after both joins.
    /// Safe to cancel while waiting: queue reception and retained joins are not
    /// removed until ready. Holding a returned report intentionally retains capacity.
    pub(crate) async fn next(&mut self) -> Option<IoEvent> {
        std::future::poll_fn(|cx| self.poll_next(cx)).await
    }

    /// Polls both FIFO heads, yielding visible terminal facts ahead of ordinary
    /// work. Prefetched reports stay owned and charged if the caller is cancelled.
    pub(crate) fn poll_terminal(&mut self, cx: &mut Context<'_>) -> Option<IoEvent> {
        let mut read_closed = false;
        let mut write_closed = false;
        if self.reader.is_some() && self.pending_read.is_none() {
            match self.reads.poll_recv(cx) {
                Poll::Ready(Some(report)) => self.pending_read = Some(report),
                Poll::Ready(None) => read_closed = true,
                Poll::Pending => {}
            }
        }
        if self.writer.is_some() && self.pending_write.is_none() {
            match self.writes.poll_recv(cx) {
                Poll::Ready(Some(report)) => self.pending_write = Some(report),
                Poll::Ready(None) => write_closed = true,
                Poll::Pending => {}
            }
        }
        if self.pending_write.as_ref().is_some_and(|report| {
            report.outcome != crate::hsms::model::runtime::WriteOutcome::Committed
        }) {
            return self.pending_write.take().map(IoEvent::Write);
        }
        if read_closed {
            if let Poll::Ready(result) =
                Pin::new(self.reader.as_mut().expect("Reader join retained")).poll(cx)
            {
                self.reader.take();
                if result.is_err() {
                    self.poison.get_or_insert(CleanupPoison::InvariantViolation);
                }
                return Some(IoEvent::ReaderStopped(result));
            }
        }
        if write_closed {
            if let Poll::Ready(result) =
                Pin::new(self.writer.as_mut().expect("Writer join retained")).poll(cx)
            {
                self.writer.take();
                if result.is_err() {
                    self.poison.get_or_insert(CleanupPoison::InvariantViolation);
                }
                return Some(IoEvent::WriterStopped(result));
            }
        }
        None
    }

    /// Polls terminal facts first, then alternates ready ordinary Reader/Writer
    /// heads. Per-source FIFO always precedes that source's joined exit.
    pub(crate) fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<IoEvent>> {
        if let Some(event) = self.poll_terminal(cx) {
            return Poll::Ready(Some(event));
        }
        if self.pending_read.is_some() && (self.prefer_read || self.pending_write.is_none()) {
            self.prefer_read = false;
            return Poll::Ready(self.pending_read.take().map(IoEvent::Read));
        }
        if self.pending_write.is_some() {
            self.prefer_read = true;
            return Poll::Ready(self.pending_write.take().map(IoEvent::Write));
        }
        if self.reader.is_none() && self.writer.is_none() {
            Poll::Ready(None)
        } else {
            Poll::Pending
        }
    }

    /// Cancels workers and drains actual write outcomes through `settle` until
    /// both tasks join or the absolute `deadline` expires. Caller must close Core
    /// admission first. Finalize missing Writer outcomes only after observing the
    /// Writer join and draining its reports; a timeout does not prove either fact.
    /// Reader frames may be discarded here because protocol shutdown has begun.
    pub(crate) async fn cleanup(
        &mut self,
        deadline: Instant,
        mut settle: impl FnMut(WriterReport),
    ) -> CleanupResult {
        self.cancellation.send_replace(true);
        loop {
            if self.reader.is_none() && self.writer.is_none() {
                break;
            }
            // Enforce the absolute boundary even within the timer's clock tick.
            if Instant::now() >= deadline {
                self.poison.get_or_insert(CleanupPoison::TaskDidNotStop);
                self.abort();
                break;
            }
            tokio::select! {
                biased;
                () = tokio::time::sleep_until(deadline) => {
                    self.poison.get_or_insert(CleanupPoison::TaskDidNotStop);
                    self.abort();
                    break;
                }
                event = self.next() => match event {
                    Some(IoEvent::Write(report)) => settle(report),
                    None => break,
                    _ => {},
                }
            }
        }
        self.poison
            .map_or(CleanupResult::Clean, CleanupResult::Poisoned)
    }

    /// Performs an explicit fresh cleanup proof after endpoint Stop. Unlike normal
    /// cleanup, this may retire historical poison, but only after both owned tasks
    /// join and every source report is consumed within the supplied attempt.
    pub(crate) async fn recover(
        &mut self,
        deadline: Instant,
        settle: impl FnMut(WriterReport),
    ) -> CleanupResult {
        let result = self.cleanup(deadline, settle).await;
        if self.reader.is_none()
            && self.writer.is_none()
            && self.pending_read.is_none()
            && self.pending_write.is_none()
            && self.reads.is_empty()
            && self.writes.is_empty()
        {
            self.poison = None;
            CleanupResult::Clean
        } else {
            result
        }
    }

    /// Requests task abortion without asserting that resources have been released.
    fn abort(&self) {
        if let Some(task) = &self.reader {
            task.abort();
        }
        if let Some(task) = &self.writer {
            task.abort();
        }
    }
}

impl Drop for IoTasks {
    /// Cancels and aborts retained tasks when the owning runtime is dropped.
    fn drop(&mut self) {
        self.cancellation.send_replace(true);
        self.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::{
        generation::transport::writer::{OutboundFrame, WriterIngress},
        model::{
            ids::{SystemBytes, WriteId},
            runtime::WriteOutcome,
        },
        protocol::{header::ControlMessage, message::ProtocolMessage},
        SessionId,
    };
    use std::time::Duration;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    /// Creates an independent loopback peer and starts the production task owner.
    async fn connection() -> (IoTasks, BoundedWriter, IoCloser, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = TcpStream::connect(address).await.unwrap();
        let (peer, _) = listener.accept().await.unwrap();
        let config = EndpointConfig::active(address, SessionId::new(7).unwrap());
        let (tasks, writer, closer) = IoTasks::launch(client, &config, Instant::now()).unwrap();
        (tasks, writer, closer, peer)
    }

    /// A complete frame immediately followed by FIN is delivered before Reader exit.
    #[tokio::test]
    async fn frame_precedes_reader_exit_and_cleanup_joins_both_halves() {
        let (mut tasks, _writer, _closer, mut peer) = connection().await;
        peer.write_all(&[0, 0, 0, 10, 255, 255, 0, 0, 0, 5, 0, 0, 0, 9])
            .await
            .unwrap();
        peer.shutdown().await.unwrap();
        let first = tokio::time::timeout(Duration::from_secs(2), tasks.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(&first, IoEvent::Read(_)));
        drop(first);
        let terminal = tokio::time::timeout(Duration::from_secs(2), tasks.next())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            terminal,
            IoEvent::ReaderStopped(Ok(Err(ReadFailure::EndOfStream { .. })))
        ));
        assert_eq!(
            tasks
                .cleanup(Instant::now() + Duration::from_secs(2), |_| panic!(
                    "no writes admitted"
                ))
                .await,
            CleanupResult::Clean
        );
        assert!(tasks.next().await.is_none());
    }

    /// A visible Reader exit precedes an ordinary committed Writer report.
    #[tokio::test]
    async fn visible_terminal_precedes_other_source_ordinary_work() {
        let (mut tasks, mut writer, _closer, mut peer) = connection().await;
        writer
            .try_admit(OutboundFrame::new(
                WriteId::new(1),
                ProtocolMessage::Control(ControlMessage::LinktestRequest {
                    system_bytes: SystemBytes::new(2),
                }),
            ))
            .unwrap();
        let mut bytes = [0; 14];
        peer.read_exact(&mut bytes).await.unwrap();
        peer.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !tasks.reader.as_ref().unwrap().is_finished() || tasks.writes.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            tasks.next().await,
            Some(IoEvent::ReaderStopped(Ok(Err(
                ReadFailure::EndOfStream { .. }
            ))))
        ));
        assert!(
            matches!(tasks.next().await, Some(IoEvent::Write(report)) if report.outcome == WriteOutcome::Committed)
        );
        assert_eq!(
            tasks
                .cleanup(Instant::now() + Duration::from_secs(1), |_| {})
                .await,
            CleanupResult::Clean
        );
    }

    /// Ordinary Writer work gets its turn even when a fresh Reader head is ready.
    #[tokio::test]
    async fn ordinary_sources_alternate_and_prefetch_retains_fifo() {
        let (mut tasks, mut writer, _closer, mut peer) = connection().await;
        writer
            .try_admit(OutboundFrame::new(
                WriteId::new(1),
                ProtocolMessage::Control(ControlMessage::LinktestRequest {
                    system_bytes: SystemBytes::new(2),
                }),
            ))
            .unwrap();
        let mut bytes = [0; 14];
        peer.read_exact(&mut bytes).await.unwrap();
        peer.write_all(&bytes).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while tasks.reads.is_empty() || tasks.writes.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let first = tasks.next().await.unwrap();
        assert!(matches!(first, IoEvent::Read(_)));
        drop(first);
        assert!(tasks.pending_write.is_some());
        peer.write_all(&bytes).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while tasks.reads.is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(tasks.next().await, Some(IoEvent::Write(_))));
        assert!(matches!(tasks.next().await, Some(IoEvent::Read(_))));
        assert_eq!(
            tasks
                .cleanup(Instant::now() + Duration::from_secs(1), |_| {})
                .await,
            CleanupResult::Clean
        );
    }

    /// Cancellation cleanup drains the actual committed outcome before proving Clean.
    #[tokio::test]
    async fn cleanup_preserves_committed_write_fact() {
        let (mut tasks, mut writer, _closer, mut peer) = connection().await;
        let write_id = WriteId::new(3);
        writer
            .try_admit(OutboundFrame::new(
                write_id,
                ProtocolMessage::Control(ControlMessage::LinktestRequest {
                    system_bytes: SystemBytes::new(4),
                }),
            ))
            .unwrap();
        let mut bytes = [0; 14];
        tokio::time::timeout(Duration::from_secs(2), peer.read_exact(&mut bytes))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(bytes, [0, 0, 0, 10, 255, 255, 0, 0, 0, 5, 0, 0, 0, 4]);
        let mut outcomes = Vec::new();
        assert_eq!(
            tasks
                .cleanup(Instant::now() + Duration::from_secs(2), |report| outcomes
                    .push((report.write_id, report.outcome)))
                .await,
            CleanupResult::Clean
        );
        assert_eq!(outcomes, vec![(write_id, WriteOutcome::Committed)]);
    }

    /// Expired cleanup is conservatively poisoned even if tasks join on a later call.
    #[tokio::test]
    async fn cleanup_deadline_poison_cannot_be_upgraded_to_clean() {
        let (mut tasks, _writer, _closer, _peer) = connection().await;
        let expected = CleanupResult::Poisoned(CleanupPoison::TaskDidNotStop);
        assert_eq!(tasks.cleanup(Instant::now(), |_| {}).await, expected);
        assert_eq!(
            tasks
                .cleanup(Instant::now() + Duration::from_secs(2), |_| {})
                .await,
            expected
        );
    }

    /// Explicit recovery consumes retained committed facts before clearing poison.
    #[tokio::test]
    async fn explicit_recovery_proves_joins_and_preserves_old_write_outcomes() {
        let (mut tasks, mut writer, _closer, mut peer) = connection().await;
        writer
            .try_admit(OutboundFrame::new(
                WriteId::new(12),
                ProtocolMessage::Control(ControlMessage::LinktestRequest {
                    system_bytes: SystemBytes::new(4),
                }),
            ))
            .unwrap();
        let mut bytes = [0; 14];
        peer.read_exact(&mut bytes).await.unwrap();
        assert!(matches!(
            tasks
                .cleanup(Instant::now(), |_| panic!(
                    "expired cleanup cannot consume a report"
                ))
                .await,
            CleanupResult::Poisoned(_)
        ));
        assert!(matches!(
            tasks.recover(Instant::now(), |_| {}).await,
            CleanupResult::Poisoned(_)
        ));
        let mut facts = Vec::new();
        assert_eq!(
            tasks
                .recover(Instant::now() + Duration::from_secs(1), |report| facts
                    .push((report.write_id, report.outcome)))
                .await,
            CleanupResult::Clean
        );
        assert_eq!(facts, vec![(WriteId::new(12), WriteOutcome::Committed)]);
        assert!(tasks.next().await.is_none());
    }

    /// Dropping the task owner releases transport even while ingress is retained.
    #[tokio::test]
    async fn dropping_owner_does_not_detach_socket_tasks() {
        let (tasks, _writer, _closer, mut peer) = connection().await;
        drop(tasks);
        let mut byte = [0];
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
