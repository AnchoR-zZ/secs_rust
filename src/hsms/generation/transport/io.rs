//! Tokio byte-stream adapters with persistent framing and explicit write progress.
//!
//! These routines own I/O facts: frame boundaries, T8, cancellation and terminal
//! write visibility. They never classify transactions or complete API commands.
//! They accept TCP halves and scripted test streams through the same code path.

use std::{future::pending, io, time::Duration};

use bytes::BytesMut;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::watch,
    time::{sleep_until, Instant},
};

use crate::{
    hsms::{
        codec::{HsmsSsCodec, HsmsSsDecodeStep},
        model::runtime::{MonoTime, TransportFault, TransportFaultKind, WriteOutcome},
        protocol::violation::HeaderSnapshot,
        wire::{framer::FrameReadProgress, validation::FramingFault},
        EndpointLimits,
    },
    secs2::codec::Secs2Decoder,
};

#[cfg(test)]
mod tests;

/// A complete frame and its occurrence time, ready for the generation Driver.
#[derive(Debug)]
pub(crate) struct ReadFrame {
    /// Original ten-byte header for immutable application/diagnostic context.
    pub(crate) header: HeaderSnapshot,
    /// Valid message or structured header/payload violation; never NeedMore.
    pub(crate) decoded: HsmsSsDecodeStep,
    /// Time framing and decoding completed in the generation epoch.
    pub(crate) occurred_at: MonoTime,
}

/// Terminal Reader facts that cannot be resumed on the same TCP generation.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadFailure {
    /// Peer EOF at a frame boundary or at the exact retained truncation position.
    EndOfStream {
        /// Framing progress when the peer closed its readable byte stream.
        progress: FrameReadProgress,
    },
    /// Length validation permanently terminated the framer.
    Framing(FramingFault),
    /// Socket failure, EOF or cancellation ended byte reception.
    Transport(TransportFault),
    /// No byte progress occurred within T8 while a frame remained partial.
    IntercharacterTimeout,
    /// Configured duration cannot be represented by the monotonic clock.
    DeadlineOverflow,
}

/// One persistent, generation-local read half and its bounded frame buffer.
pub(crate) struct FrameReader<Read> {
    /// Sole owner of this generation's readable byte stream.
    reader: Read,
    /// Existing pure HSMS framing, validation and SECS-II composition.
    codec: HsmsSsCodec,
    /// Prefix/body bytes of only the current incomplete frame.
    buffer: BytesMut,
    /// Last actual positive read, absent between complete frames.
    last_progress: Option<Instant>,
    /// Shared generation origin for runtime-neutral occurrence timestamps.
    epoch: Instant,
}

impl<Read: AsyncRead + Unpin> FrameReader<Read> {
    /// Owns `reader` and a fresh codec with `limits`/`decoder` under `epoch`.
    pub(crate) fn new(
        reader: Read,
        limits: EndpointLimits,
        decoder: Secs2Decoder,
        epoch: Instant,
    ) -> Self {
        Self {
            reader,
            codec: HsmsSsCodec::new(limits, decoder),
            buffer: BytesMut::new(),
            last_progress: None,
            epoch,
        }
    }

    /// Reads exactly one frame, preserving partial progress across future drops.
    ///
    /// Reserve downstream result capacity before calling. No next-frame bytes
    /// are read. T8 starts with the first byte, including partial prefixes,
    /// and stops at complete-frame boundaries.
    pub(crate) async fn next(
        &mut self,
        t8: Duration,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<ReadFrame, ReadFailure> {
        let mut scratch = [0u8; 8192];
        loop {
            let header = self.buffer.get(4..14).map(|bytes| {
                let mut header = [0; 10];
                header.copy_from_slice(bytes);
                HeaderSnapshot::new(header)
            });
            let (needed, progress) = match self
                .codec
                .decode(&mut self.buffer)
                .map_err(ReadFailure::Framing)?
            {
                HsmsSsDecodeStep::NeedMore(progress) => {
                    let needed = match progress {
                        FrameReadProgress::AwaitingLength { bytes_seen } => {
                            4 - usize::from(bytes_seen)
                        }
                        FrameReadProgress::PartialBody {
                            target_length,
                            bytes_seen,
                        } => target_length - bytes_seen,
                    };
                    (needed, progress)
                }
                decoded => {
                    self.last_progress = None;
                    return Ok(ReadFrame {
                        header: header.expect("complete frame includes its ten-byte header"),
                        decoded,
                        occurred_at: MonoTime::from_elapsed(self.epoch.elapsed()),
                    });
                }
            };
            let deadline = self
                .last_progress
                .map(|last| last.checked_add(t8).ok_or(ReadFailure::DeadlineOverflow))
                .transpose()?;
            let amount = needed.min(scratch.len());
            let read = tokio::select! {
                biased;
                () = cancelled(cancellation) => return Err(ReadFailure::Transport(TransportFault::new(TransportFaultKind::Cancelled))),
                () = wait_deadline(deadline) => return Err(ReadFailure::IntercharacterTimeout),
                result = self.reader.read(&mut scratch[..amount]) => result,
            }.map_err(|error| ReadFailure::Transport(transport_fault(&error)))?;
            if read == 0 {
                return Err(ReadFailure::EndOfStream { progress });
            }
            self.buffer.extend_from_slice(&scratch[..read]);
            self.last_progress = Some(Instant::now());
        }
    }
}

/// Writes one entire `frame` before `deadline` and preserves its visible offset.
///
/// Call only from the single Writer loop after FIFO admission. Cancellation or
/// failure before any successful write is NotWritten; after progress it is
/// Indeterminate. Complete local writing is Committed, not peer receipt proof.
pub(crate) async fn write_frame<Write: AsyncWrite + Unpin>(
    writer: &mut Write,
    frame: &[u8],
    deadline: Instant,
    cancellation: &mut watch::Receiver<bool>,
) -> WriteOutcome {
    let mut offset = 0;
    while offset < frame.len() {
        let result = tokio::select! {
            biased;
            () = cancelled(cancellation) => Err(TransportFault::new(TransportFaultKind::Cancelled)),
            () = sleep_until(deadline) => Err(TransportFault::new(TransportFaultKind::TimedOut)),
            result = writer.write(&frame[offset..]) => result.map_err(|error| transport_fault(&error)),
        };
        match result {
            Ok(0) => {
                return failed_write(offset, TransportFault::new(TransportFaultKind::WriteZero))
            }
            Ok(written) => offset += written,
            Err(fault) => return failed_write(offset, fault),
        }
    }
    WriteOutcome::Committed
}

/// Converts an I/O error into the stable neutral transport category.
pub(crate) fn transport_fault(error: &io::Error) -> TransportFault {
    TransportFault::new(match error.kind() {
        io::ErrorKind::ConnectionReset => TransportFaultKind::ConnectionReset,
        io::ErrorKind::BrokenPipe => TransportFaultKind::BrokenPipe,
        io::ErrorKind::UnexpectedEof => TransportFaultKind::UnexpectedEof,
        io::ErrorKind::TimedOut => TransportFaultKind::TimedOut,
        io::ErrorKind::WriteZero => TransportFaultKind::WriteZero,
        _ => TransportFaultKind::Other,
    })
}

/// Classifies write visibility using the offset retained by the single owner.
fn failed_write(offset: usize, fault: TransportFault) -> WriteOutcome {
    if offset == 0 {
        WriteOutcome::NotWritten(fault)
    } else {
        WriteOutcome::Indeterminate(fault)
    }
}

/// Waits for cancellation or loss of its controlling sender.
pub(crate) async fn cancelled(cancellation: &mut watch::Receiver<bool>) {
    loop {
        if *cancellation.borrow_and_update() {
            return;
        }
        if cancellation.changed().await.is_err() {
            return;
        }
    }
}

/// Waits for an optional absolute deadline; absence stays pending indefinitely.
async fn wait_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => sleep_until(deadline).await,
        None => pending().await,
    }
}
