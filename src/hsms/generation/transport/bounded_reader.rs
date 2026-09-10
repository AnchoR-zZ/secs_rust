//! Bounded frame production for one generation's read half.
//! Capacity is reserved before starting a frame, so downstream pressure pauses
//! only at frame boundaries and cannot become a false T8 failure. Reports retain
//! their wire-byte reservation until consumed; terminal faults use the task result.

use super::io::{cancelled, FrameReader, ReadFailure, ReadFrame};
use crate::{
    hsms::{
        model::runtime::{TransportFault, TransportFaultKind},
        EndpointLimits,
    },
    secs2::codec::Secs2Decoder,
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::AsyncRead,
    sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

/// A reliable complete-frame envelope with retained byte and count ownership.
#[derive(Debug)]
pub(crate) struct ReaderReport {
    /// Decoded frame, immutable header and actual completion timestamp.
    pub(crate) frame: ReadFrame,
    /// Worst-case wire-byte charge retained through Driver processing.
    _bytes: OwnedSemaphorePermit,
    /// Count charge prevents dequeuing without processing from growing ownership.
    _slot: OwnedSemaphorePermit,
}

/// Unique task producing frames under a conservative wire-byte budget.
pub(crate) struct ReaderWorker<Read> {
    /// Persistent frame decoder and sole read-half owner.
    reader: FrameReader<Read>,
    /// Reliable bounded FIFO consumed by the generation Driver.
    reports: mpsc::Sender<ReaderReport>,
    /// Aggregate wire representation budget for incomplete and reported frames.
    bytes: Arc<Semaphore>,
    /// Independent bound on in-flight and reported frames.
    slots: Arc<Semaphore>,
    /// Worst-case frame bytes reserved before the first byte is read.
    frame_bytes: u32,
    /// T8 policy enforced only by actual partial-frame byte progress.
    t8: Duration,
}

impl<Read: AsyncRead + Unpin> ReaderWorker<Read> {
    /// Builds the generation's one-result Reader using endpoint wire/decoder limits.
    pub(crate) fn from_config(
        reader: Read,
        config: &crate::hsms::EndpointConfig,
        epoch: Instant,
    ) -> Result<(Self, mpsc::Receiver<ReaderReport>), crate::hsms::ConfigError> {
        config.validate()?;
        Self::new(
            reader,
            config.limits(),
            Secs2Decoder::new(config.secs2_limits()),
            epoch,
            config.timeouts().t8(),
            1,
            config.runtime().inbound_bytes(),
        )
        .map_err(|description| crate::hsms::ConfigError::RuntimePolicy { description })
    }
    /// Builds a Reader with `capacity` reports and `wire_bytes` aggregate charge.
    /// Budget must hold at least one maximum frame. This bounds wire ownership;
    /// decoded tree overhead is separately bounded by the supplied SECS-II limits.
    pub(crate) fn new(
        reader: Read,
        limits: EndpointLimits,
        decoder: Secs2Decoder,
        epoch: Instant,
        t8: Duration,
        capacity: usize,
        wire_bytes: u32,
    ) -> Result<(Self, mpsc::Receiver<ReaderReport>), &'static str> {
        let frame_bytes = limits
            .max_message_length()
            .checked_add(4)
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or("Reader frame reservation is not representable")?;
        if capacity == 0
            || capacity > Semaphore::MAX_PERMITS
            || wire_bytes < frame_bytes
            || wire_bytes as usize > Semaphore::MAX_PERMITS
        {
            return Err("Reader capacity cannot hold a maximum frame");
        }
        if t8.is_zero() || epoch.checked_add(t8).is_none() {
            return Err("Reader T8 is not representable");
        }
        let (reports, consumer) = mpsc::channel(capacity);
        Ok((
            Self {
                reader: FrameReader::new(reader, limits, decoder, epoch),
                reports,
                bytes: Arc::new(Semaphore::new(wire_bytes as usize)),
                slots: Arc::new(Semaphore::new(capacity)),
                frame_bytes,
                t8,
            },
            consumer,
        ))
    }

    /// Publishes frames until cancellation, consumer loss, framing or I/O failure.
    /// No complete frame is dropped due to Full: all capacity precedes reception.
    /// The caller observes terminal facts by joining this task, even with a full
    /// report queue. The owner must process already published reports before
    /// presenting this Reader's terminal result to Core, preserving source FIFO
    /// (in particular a complete Secondary immediately followed by peer FIN).
    pub(crate) async fn run(
        mut self,
        mut cancellation: watch::Receiver<bool>,
    ) -> Result<(), ReadFailure> {
        loop {
            let reservation = async {
                let slot = self.slots.clone().acquire_owned().await.map_err(|_| ())?;
                let bytes = self
                    .bytes
                    .clone()
                    .acquire_many_owned(self.frame_bytes)
                    .await
                    .map_err(|_| ())?;
                let queue = self.reports.reserve().await.map_err(|_| ())?;
                Ok::<_, ()>((slot, bytes, queue))
            };
            let (slot, bytes, queue) = tokio::select! {
                biased;
                () = cancelled(&mut cancellation) => return Err(cancelled_failure()),
                () = self.reports.closed() => return Ok(()),
                reserved = reservation => match reserved { Ok(reserved) => reserved, Err(()) => return Ok(()) },
            };
            let frame = tokio::select! {
                biased;
                () = self.reports.closed() => return Ok(()),
                frame = self.reader.next(self.t8, &mut cancellation) => frame?,
            };
            queue.send(ReaderReport {
                frame,
                _bytes: bytes,
                _slot: slot,
            });
        }
    }
}

/// Stable local cancellation fact shared across pre-frame capacity waits.
fn cancelled_failure() -> ReadFailure {
    ReadFailure::Transport(TransportFault::new(TransportFaultKind::Cancelled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::{
        codec::HsmsSsDecodeStep,
        protocol::{header::ControlMessage, message::ProtocolMessage},
    };
    use tokio::io::AsyncWriteExt;

    /// Independent header-only Linktest wire fixture used without local encoding.
    const LINKTEST: [u8; 14] = [0, 0, 0, 10, 255, 255, 0, 0, 0, 5, 0, 0, 0, 7];

    /// Constructs a tiny Reader with selectable count and wire-byte budgets.
    fn build(
        reader: tokio::io::DuplexStream,
        capacity: usize,
        bytes: u32,
    ) -> (
        ReaderWorker<tokio::io::DuplexStream>,
        mpsc::Receiver<ReaderReport>,
    ) {
        ReaderWorker::new(
            reader,
            EndpointLimits::new(10, 2, 1, 2, 2, 2, 2, 2).unwrap(),
            Secs2Decoder::default(),
            Instant::now(),
            Duration::from_secs(1),
            capacity,
            bytes,
        )
        .unwrap()
    }

    /// Checks decoded content and raw context using the independent fixture.
    fn assert_linktest(report: &ReaderReport) {
        assert!(
            matches!(&report.frame.decoded, HsmsSsDecodeStep::Message(ProtocolMessage::Control(
            ControlMessage::LinktestRequest { system_bytes }
        )) if system_bytes.get() == 7)
        );
    }

    /// Retained byte ownership pauses between frames without falsely running T8.
    #[tokio::test(start_paused = true)]
    async fn byte_pressure_pauses_at_frame_boundary_without_t8() {
        let (reader, mut peer) = tokio::io::duplex(64);
        let (worker, mut reports) = build(reader, 2, 14);
        let (cancel, cancellation) = watch::channel(false);
        let task = tokio::spawn(worker.run(cancellation));
        peer.write_all(&LINKTEST).await.unwrap();
        peer.write_all(&LINKTEST).await.unwrap();
        let first = reports.recv().await.unwrap();
        assert_linktest(&first);
        tokio::time::advance(Duration::from_secs(100)).await;
        assert!(!task.is_finished());
        assert!(reports.try_recv().is_err());
        drop(first);
        let second = reports.recv().await.unwrap();
        assert_linktest(&second);
        assert!(second.frame.occurred_at.elapsed() >= Duration::from_secs(100));
        cancel.send(true).unwrap();
        assert_eq!(task.await.unwrap(), Err(cancelled_failure()));
    }

    /// Count ownership remains charged after dequeue, independently of byte space.
    #[tokio::test]
    async fn count_pressure_and_cancellation_do_not_depend_on_consumer_progress() {
        let (reader, mut peer) = tokio::io::duplex(64);
        let (worker, mut reports) = build(reader, 1, 28);
        let (cancel, cancellation) = watch::channel(false);
        let task = tokio::spawn(worker.run(cancellation));
        peer.write_all(&LINKTEST).await.unwrap();
        let retained = reports.recv().await.unwrap();
        peer.write_all(&LINKTEST).await.unwrap();
        tokio::task::yield_now().await;
        assert!(reports.try_recv().is_err());
        cancel.send(true).unwrap();
        assert_eq!(task.await.unwrap(), Err(cancelled_failure()));
        assert_linktest(&retained);
        assert!(reports.recv().await.is_none());
    }

    /// T8 terminal delivery bypasses buffered frame capacity via the task result.
    #[tokio::test(start_paused = true)]
    async fn partial_frame_timeout_keeps_prior_complete_frame_available() {
        let (reader, mut peer) = tokio::io::duplex(64);
        let (worker, mut reports) = build(reader, 2, 28);
        let (_cancel, cancellation) = watch::channel(false);
        let task = tokio::spawn(worker.run(cancellation));
        peer.write_all(&LINKTEST).await.unwrap();
        peer.write_all(&[0]).await.unwrap();
        assert_eq!(task.await.unwrap(), Err(ReadFailure::IntercharacterTimeout));
        assert_linktest(&reports.recv().await.unwrap());
        assert!(reports.recv().await.is_none());
    }

    /// Losing the consumer cancels an idle/partial read without waiting for T8.
    #[tokio::test(start_paused = true)]
    async fn consumer_drop_releases_read_half_promptly() {
        let (reader, mut peer) = tokio::io::duplex(64);
        let (worker, reports) = build(reader, 1, 14);
        let (_cancel, cancellation) = watch::channel(false);
        let task = tokio::spawn(worker.run(cancellation));
        peer.write_all(&[0]).await.unwrap();
        tokio::task::yield_now().await;
        drop(reports);
        assert_eq!(task.await.unwrap(), Ok(()));
    }

    /// Invalid lengths terminate before a report or Message Text allocation.
    #[tokio::test]
    async fn invalid_prefix_is_returned_as_terminal_framing_fault() {
        let (reader, mut peer) = tokio::io::duplex(64);
        let (worker, mut reports) = build(reader, 1, 14);
        let (_cancel, cancellation) = watch::channel(false);
        peer.write_all(&[0, 0, 0, 9]).await.unwrap();
        assert!(matches!(
            worker.run(cancellation).await,
            Err(ReadFailure::Framing(_))
        ));
        assert!(reports.recv().await.is_none());
    }

    /// Rejects budgets that could otherwise wait forever before a maximum frame.
    #[tokio::test]
    async fn insufficient_frame_budget_is_rejected_at_construction() {
        let (reader, _peer) = tokio::io::duplex(64);
        assert!(ReaderWorker::new(
            reader,
            EndpointLimits::default(),
            Secs2Decoder::default(),
            Instant::now(),
            Duration::from_secs(1),
            1,
            14
        )
        .is_err());
    }
}
