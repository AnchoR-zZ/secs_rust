//! Scripted I/O and real TCP tests of framing, T8 and write visibility.

use super::*;
use crate::{
    hsms::protocol::{header::ControlMessage, message::ProtocolMessage},
    secs2::DecodeLimits,
};
use std::{
    collections::VecDeque,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::{
    io::duplex,
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};

/// Independent E37 Linktest.req vector with System Bytes 0x12345678.
const LINKTEST: &[u8] = &[0, 0, 0, 10, 255, 255, 0, 0, 0, 5, 0x12, 0x34, 0x56, 0x78];

/// Deterministic behavior selected for one write poll.
enum Step {
    /// Accept at most the given byte count.
    Write(usize),
    /// Return a terminal error without accepting bytes in this poll.
    Error(io::ErrorKind),
    /// Stay pending until cancellation or the independent timer fires.
    Pending,
}

/// Byte sink exposing exact progress without OS buffer heuristics.
struct ScriptedWriter {
    /// Remaining poll behaviors in FIFO order.
    steps: VecDeque<Step>,
    /// Bytes accepted by successful write polls.
    bytes: Vec<u8>,
    /// Optional cancellation immediately after positive progress.
    cancel_after_progress: Option<watch::Sender<bool>>,
}

impl ScriptedWriter {
    /// Builds an empty sink using the supplied ordered steps.
    fn new(steps: impl IntoIterator<Item = Step>) -> Self {
        Self {
            steps: steps.into_iter().collect(),
            bytes: Vec::new(),
            cancel_after_progress: None,
        }
    }
}

impl AsyncWrite for ScriptedWriter {
    /// Applies one scripted write and reports its actual progress into `buffer`.
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.steps.pop_front().unwrap_or(Step::Write(buffer.len())) {
            Step::Write(limit) => {
                let amount = limit.min(buffer.len());
                self.bytes.extend_from_slice(&buffer[..amount]);
                if amount > 0 {
                    if let Some(cancel) = self.cancel_after_progress.take() {
                        let _ = cancel.send(true);
                    }
                }
                Poll::Ready(Ok(amount))
            }
            Step::Error(kind) => Poll::Ready(Err(io::Error::from(kind))),
            Step::Pending => {
                self.steps.push_front(Step::Pending);
                Poll::Pending
            }
        }
    }
    /// Reports no pending flush work for the in-memory fixture.
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    /// Reports immediate shutdown of the in-memory fixture.
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Creates the production reader around an arbitrary test stream.
fn reader<Read: AsyncRead + Unpin>(read: Read) -> FrameReader<Read> {
    FrameReader::new(
        read,
        EndpointLimits::default(),
        Secs2Decoder::new(DecodeLimits::default()),
        Instant::now(),
    )
}

/// Verifies preserved header bytes and decoded Linktest correlation.
fn assert_linktest(frame: ReadFrame) {
    assert_eq!(frame.header.as_bytes().as_slice(), &LINKTEST[4..]);
    assert!(
        matches!(frame.decoded, HsmsSsDecodeStep::Message(ProtocolMessage::Control(ControlMessage::LinktestRequest { system_bytes })) if system_bytes.get() == 0x12345678)
    );
}

/// Short writes concatenate without duplicating or skipping bytes.
#[tokio::test]
async fn short_writes_commit_exact_frame() {
    let (_cancel, mut signal) = watch::channel(false);
    let mut writer = ScriptedWriter::new([Step::Write(1), Step::Write(2), Step::Write(3)]);
    assert_eq!(
        write_frame(
            &mut writer,
            LINKTEST,
            Instant::now() + Duration::from_secs(1),
            &mut signal
        )
        .await,
        WriteOutcome::Committed
    );
    assert_eq!(writer.bytes, LINKTEST);
}

/// Errors and WriteZero distinguish zero bytes from partial visibility.
#[tokio::test]
async fn write_fault_matrix_preserves_visibility() {
    for progress in [0, 1, 7, 13] {
        for fault in [Step::Error(io::ErrorKind::BrokenPipe), Step::Write(0)] {
            let (_cancel, mut signal) = watch::channel(false);
            let mut steps = Vec::new();
            if progress > 0 {
                steps.push(Step::Write(progress));
            }
            steps.push(fault);
            let mut writer = ScriptedWriter::new(steps);
            let outcome = write_frame(
                &mut writer,
                LINKTEST,
                Instant::now() + Duration::from_secs(1),
                &mut signal,
            )
            .await;
            assert_eq!(writer.bytes, &LINKTEST[..progress]);
            if progress == 0 {
                assert!(matches!(outcome, WriteOutcome::NotWritten(_)));
            } else {
                assert!(matches!(outcome, WriteOutcome::Indeterminate(_)));
            }
        }
    }
}

/// Cancellation after progress must retain uncertain visibility.
#[tokio::test]
async fn cancellation_after_write_is_indeterminate() {
    let (cancel, mut signal) = watch::channel(false);
    let mut writer = ScriptedWriter::new([Step::Write(2)]);
    writer.cancel_after_progress = Some(cancel);
    assert_eq!(
        write_frame(
            &mut writer,
            LINKTEST,
            Instant::now() + Duration::from_secs(1),
            &mut signal
        )
        .await,
        WriteOutcome::Indeterminate(TransportFault::new(TransportFaultKind::Cancelled))
    );
    assert_eq!(writer.bytes, &LINKTEST[..2]);
}

/// Cancellation before a fresh frame proves zero bytes were written.
#[tokio::test]
async fn cancellation_before_write_proves_zero_bytes() {
    let (_cancel, mut signal) = watch::channel(true);
    let mut writer = ScriptedWriter::new([]);
    assert_eq!(
        write_frame(
            &mut writer,
            LINKTEST,
            Instant::now() + Duration::from_secs(1),
            &mut signal
        )
        .await,
        WriteOutcome::NotWritten(TransportFault::new(TransportFaultKind::Cancelled))
    );
    assert!(writer.bytes.is_empty());
}

/// A stalled frame has an absolute timeout preserving its partial offset.
#[tokio::test(start_paused = true)]
async fn write_timeout_preserves_offset() {
    for progress in [0, 3] {
        let (_cancel, mut signal) = watch::channel(false);
        let mut steps = Vec::new();
        if progress > 0 {
            steps.push(Step::Write(progress));
        }
        steps.push(Step::Pending);
        let mut writer = ScriptedWriter::new(steps);
        let outcome = write_frame(
            &mut writer,
            LINKTEST,
            Instant::now() + Duration::from_secs(1),
            &mut signal,
        )
        .await;
        let fault = TransportFault::new(TransportFaultKind::TimedOut);
        assert_eq!(
            outcome,
            if progress == 0 {
                WriteOutcome::NotWritten(fault)
            } else {
                WriteOutcome::Indeterminate(fault)
            }
        );
    }
}

/// Coalesced messages are framed individually before a subsequent EOF.
#[tokio::test]
async fn reader_respects_boundaries_and_reports_eof_after_messages() {
    let (_cancel, mut signal) = watch::channel(false);
    let bytes = [LINKTEST, LINKTEST].concat();
    let mut reader = reader(bytes.as_slice());
    assert_linktest(
        reader
            .next(Duration::from_secs(1), &mut signal)
            .await
            .unwrap(),
    );
    assert_eq!(reader.reader.len(), LINKTEST.len());
    assert_linktest(
        reader
            .next(Duration::from_secs(1), &mut signal)
            .await
            .unwrap(),
    );
    assert!(matches!(
        reader.next(Duration::from_secs(1), &mut signal).await,
        Err(ReadFailure::EndOfStream {
            progress: FrameReadProgress::AwaitingLength { bytes_seen: 0 }
        })
    ));
}

/// EOF preserves every possible truncation point in prefix and message body.
#[tokio::test]
async fn eof_reports_exact_partial_prefix_or_body_position() {
    for length in 0..LINKTEST.len() {
        let (_cancel, mut signal) = watch::channel(false);
        let mut reader = reader(&LINKTEST[..length]);
        let expected = if length < 4 {
            FrameReadProgress::AwaitingLength {
                bytes_seen: length as u8,
            }
        } else {
            FrameReadProgress::PartialBody {
                target_length: 10,
                bytes_seen: length - 4,
            }
        };
        assert_eq!(
            reader
                .next(Duration::from_secs(1), &mut signal)
                .await
                .unwrap_err(),
            ReadFailure::EndOfStream { progress: expected }
        );
    }
}

/// Invalid lengths fail before reading or allocating their claimed bodies.
#[tokio::test]
async fn bad_length_is_terminal_before_body_read() {
    for bytes in [&[0, 0, 0, 9][..], &[255, 255, 255, 255][..]] {
        let (_cancel, mut signal) = watch::channel(false);
        let mut reader = reader(bytes);
        assert!(matches!(
            reader.next(Duration::from_secs(1), &mut signal).await,
            Err(ReadFailure::Framing(_))
        ));
        assert_eq!(reader.buffer.len(), 4);
        assert!(matches!(
            reader.next(Duration::from_secs(1), &mut signal).await,
            Err(ReadFailure::Framing(_))
        ));
    }
}

/// T8 covers partial prefixes and bodies, including a single received byte.
#[tokio::test(start_paused = true)]
async fn partial_prefix_and_body_have_t8() {
    for count in [1, 2, 3, 4, 9, 13] {
        let (_cancel, mut signal) = watch::channel(false);
        let (mut peer, input) = duplex(32);
        peer.write_all(&LINKTEST[..count]).await.unwrap();
        let mut reader = reader(input);
        assert!(matches!(
            reader.next(Duration::from_secs(1), &mut signal).await,
            Err(ReadFailure::IntercharacterTimeout)
        ));
    }
}

/// Idle connections have no T8 deadline until their first byte arrives.
#[tokio::test(start_paused = true)]
async fn idle_reader_does_not_start_t8() {
    let (_cancel, mut signal) = watch::channel(false);
    let (_peer, input) = duplex(32);
    let mut reader = reader(input);
    assert!(timeout(
        Duration::from_secs(2),
        reader.next(Duration::from_secs(1), &mut signal)
    )
    .await
    .is_err());
    assert!(reader.last_progress.is_none());
}

/// Real positive reads refresh T8 throughout a slowly progressing frame.
#[tokio::test(start_paused = true)]
async fn byte_progress_refreshes_t8() {
    let (_cancel, mut signal) = watch::channel(false);
    let (mut peer, input) = duplex(32);
    let sender = tokio::spawn(async move {
        for byte in LINKTEST {
            sleep(Duration::from_millis(500)).await;
            peer.write_all(&[*byte]).await.unwrap();
        }
    });
    let mut reader = reader(input);
    let frame = reader
        .next(Duration::from_secs(1), &mut signal)
        .await
        .unwrap();
    assert!(frame.occurred_at.elapsed() >= Duration::from_secs(7));
    assert_linktest(frame);
    sender.await.unwrap();
}

/// Dropping a read future does not discard already-read framing progress.
#[tokio::test(start_paused = true)]
async fn dropped_read_future_retains_partial_frame() {
    let (_cancel, mut signal) = watch::channel(false);
    let (mut peer, input) = duplex(32);
    peer.write_all(&LINKTEST[..5]).await.unwrap();
    let mut reader = reader(input);
    assert!(timeout(
        Duration::from_millis(200),
        reader.next(Duration::from_secs(1), &mut signal)
    )
    .await
    .is_err());
    assert_eq!(reader.buffer.as_ref(), &LINKTEST[..5]);
    peer.write_all(&LINKTEST[5..]).await.unwrap();
    assert_linktest(
        reader
            .next(Duration::from_secs(1), &mut signal)
            .await
            .unwrap(),
    );
}

/// Real loopback TCP uses the same adapters as the scripted tests.
#[tokio::test]
async fn real_tcp_carries_independent_wire_vector() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (_cancel, signal) = watch::channel(false);
    let mut write_signal = signal.clone();
    let sender = tokio::spawn(async move {
        let mut stream = TcpStream::connect(address).await.unwrap();
        write_frame(
            &mut stream,
            LINKTEST,
            Instant::now() + Duration::from_secs(2),
            &mut write_signal,
        )
        .await
    });
    let (stream, _) = listener.accept().await.unwrap();
    let mut read_signal = signal;
    let mut reader = reader(stream);
    assert_linktest(
        timeout(
            Duration::from_secs(2),
            reader.next(Duration::from_secs(1), &mut read_signal),
        )
        .await
        .unwrap()
        .unwrap(),
    );
    assert_eq!(sender.await.unwrap(), WriteOutcome::Committed);
}
