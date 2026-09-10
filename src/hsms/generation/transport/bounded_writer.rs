//! One FIFO Writer with independent lane reservations and aggregate byte limits.
//! Data is encoded before Core admission; the Core-assigned header is stamped
//! afterward. Capacity remains charged through outcome delivery, bounding both
//! queued frames and unprocessed results. Only the worker owns the write half.

use super::{
    io::{cancelled, write_frame},
    writer::{
        DataPermitError, DataReserveError, OutboundFrame, ReservedDataAdmissionError,
        WriteAdmissionError, WriterIngress,
    },
};
use crate::{
    hsms::{
        model::{
            ids::{WireSequence, WriteId},
            runtime::{MonoTime, TransportFault, TransportFaultKind, WriteOutcome},
        },
        profile::secs2::{Secs2Profile, StrictSecs2Profile},
        protocol::message::ProtocolMessage,
        wire::framer::HsmsWireEncoder,
        EndpointLimits, OperationError,
    },
    secs2::{codec::Secs2Decoder, SecsItem},
};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::AsyncWrite,
    sync::{mpsc, watch, OwnedSemaphorePermit, Semaphore},
    time::Instant,
};

/// Settings for finite Writer memory and residence time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct WriterPolicy {
    /// Aggregate Data bytes including each frame's prefix and header.
    pub(crate) data_bytes: u32,
    /// Maximum time from admission through full write, including queue residence.
    pub(crate) residence: Duration,
    /// Maximum duration of the active write, also capped by residence.
    pub(crate) active_write: Duration,
}

/// Capacity charged until an admitted frame's result leaves the consumer.
#[derive(Debug)]
struct Charge {
    /// One independent Data or Control slot.
    _slot: OwnedSemaphorePermit,
    /// Exact Data byte reservation; controls have a fixed fourteen-byte size.
    _bytes: Option<OwnedSemaphorePermit>,
}

/// Fully encoded frame waiting in the common FIFO.
struct QueuedFrame {
    /// Core identity used for exactly-once settlement.
    write_id: WriteId,
    /// Writer-assigned total order, shared by both lanes.
    sequence: WireSequence,
    /// Complete immutable wire representation owned by this queue entry.
    bytes: Vec<u8>,
    /// Absolute admission-to-completion deadline.
    deadline: Instant,
    /// Capacity retained until the result is consumed or discarded.
    charge: Charge,
}

/// Prepared Data permit owned only by one synchronous Driver/Core turn.
pub(crate) struct PreparedData {
    /// Instance identity preventing cross-generation permit transfer.
    owner: Arc<()>,
    /// Reserved common FIFO slot, guaranteeing subsequent synchronous admission.
    queue: mpsc::OwnedPermit<QueuedFrame>,
    /// Placeholder prefix/header followed by already encoded Message Text.
    bytes: Vec<u8>,
    /// Independent lane and aggregate byte permits.
    charge: Charge,
}

/// Unique terminal report; drop returns capacity after Driver processes it.
#[derive(Debug)]
pub(crate) struct WriterReport {
    /// Exact Core-assigned identity of this admitted frame.
    pub(crate) write_id: WriteId,
    /// Original FIFO admission sequence.
    pub(crate) sequence: WireSequence,
    /// Actual local write visibility fact.
    pub(crate) outcome: WriteOutcome,
    /// Occurrence time measured from the generation's shared epoch.
    pub(crate) occurred_at: MonoTime,
    /// Reservation retained to bound outstanding outcomes as well as frames.
    _charge: Charge,
}

/// Single-owner synchronous WriterIngress used by SessionDriver.
pub(crate) struct BoundedWriter {
    /// Instance identity shared only with its own prepared permits.
    owner: Arc<()>,
    /// Common FIFO for Data and Control after synchronous admission.
    queue: mpsc::Sender<QueuedFrame>,
    /// Independently reserved Control slots.
    control_slots: Arc<Semaphore>,
    /// Independently reserved Data slots.
    data_slots: Arc<Semaphore>,
    /// Aggregate Data-frame bytes charged before encoding allocation.
    data_bytes: Arc<Semaphore>,
    /// Pure length/header encoder using validated endpoint limits.
    encoder: HsmsWireEncoder,
    /// Immutable body measurement and encoding profile.
    profile: StrictSecs2Profile,
    /// Endpoint bound retained for precise local size errors.
    limits: EndpointLimits,
    /// Writer byte/time policy shared with the worker.
    policy: WriterPolicy,
    /// Next FIFO sequence, absent after checked exhaustion.
    next_sequence: Option<u64>,
}

/// Sole write-half task and its bounded report producer.
pub(crate) struct WriterWorker {
    /// Common admission FIFO, never reordered by lane.
    queue: mpsc::Receiver<QueuedFrame>,
    /// Capacity equals all charged lane slots, preventing result-send deadlock.
    reports: mpsc::Sender<WriterReport>,
    /// Absolute residence and active-write policies.
    policy: WriterPolicy,
    /// Generation-local monotonic origin shared with Core's Driver.
    epoch: Instant,
}

impl BoundedWriter {
    /// Builds the Writer directly from endpoint byte and deadline configuration.
    pub(crate) fn from_config(
        config: &crate::hsms::EndpointConfig,
        epoch: Instant,
    ) -> Result<(Self, WriterWorker, mpsc::Receiver<WriterReport>), crate::hsms::ConfigError> {
        config.validate()?;
        let policy = config.runtime();
        Self::new(
            config.limits(),
            WriterPolicy {
                data_bytes: policy.write_bytes(),
                residence: policy.residence(),
                active_write: policy.write(),
            },
            epoch,
        )
        .map_err(|description| crate::hsms::ConfigError::RuntimePolicy { description })
    }
    /// Builds bounded ingress, its unique worker, and the result consumer.
    /// Rejects impossible capacities or clock durations before opening transport.
    pub(crate) fn new(
        limits: EndpointLimits,
        policy: WriterPolicy,
        epoch: Instant,
    ) -> Result<(Self, WriterWorker, mpsc::Receiver<WriterReport>), &'static str> {
        let count = limits
            .data_lane_capacity()
            .checked_add(limits.critical_lane_capacity())
            .ok_or("Writer capacity overflow")?;
        if count > Semaphore::MAX_PERMITS
            || policy.data_bytes < 14
            || policy.data_bytes as usize > Semaphore::MAX_PERMITS
        {
            return Err("Writer capacity is not representable");
        }
        if policy.residence.is_zero()
            || policy.active_write.is_zero()
            || epoch.checked_add(policy.residence).is_none()
            || epoch.checked_add(policy.active_write).is_none()
        {
            return Err("Writer deadline is not representable");
        }
        let (queue, receiver) = mpsc::channel(count);
        let (reports, consumer) = mpsc::channel(count);
        Ok((
            Self {
                owner: Arc::new(()),
                queue,
                control_slots: Arc::new(Semaphore::new(limits.critical_lane_capacity())),
                data_slots: Arc::new(Semaphore::new(limits.data_lane_capacity())),
                data_bytes: Arc::new(Semaphore::new(policy.data_bytes as usize)),
                encoder: HsmsWireEncoder::new(limits),
                profile: StrictSecs2Profile::new(Secs2Decoder::default()),
                limits,
                policy,
                next_sequence: Some(0),
            },
            WriterWorker {
                queue: receiver,
                reports,
                policy,
                epoch,
            },
            consumer,
        ))
    }

    /// Assigns FIFO identity and deadline only at actual admission.
    fn admission(&mut self) -> Option<(WireSequence, Instant)> {
        let deadline = Instant::now().checked_add(self.policy.residence)?;
        let raw = self.next_sequence?;
        self.next_sequence = raw.checked_add(1);
        Some((WireSequence::new(raw), deadline))
    }
}

impl WriterIngress for BoundedWriter {
    type DataPermit = PreparedData;

    /// Reserves an absent-body frame for the slot-only internal seam.
    fn try_reserve_data(&mut self) -> Result<PreparedData, DataReserveError> {
        self.try_reserve_message(None).map_err(|error| {
            if error == OperationError::ConnectionLost {
                DataReserveError::Closed
            } else {
                DataReserveError::Full
            }
        })
    }

    /// Measures, reserves and encodes once, before Core allocates identifiers.
    fn try_reserve_message(
        &mut self,
        body: Option<&SecsItem>,
    ) -> Result<PreparedData, OperationError> {
        if self.queue.is_closed() {
            return Err(OperationError::ConnectionLost);
        }
        let plan = self.profile.prepare_body(body)?;
        let frame = self.encoder.plan_data(plan.encoded_length()).map_err(|_| {
            OperationError::OutboundFrameTooLarge {
                text_length: plan.encoded_length(),
                maximum_message_length: self.limits.max_message_length(),
            }
        })?;
        let length =
            u32::try_from(frame.encoded_length()).map_err(|_| OperationError::Backpressure)?;
        let slot = self
            .data_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| OperationError::Backpressure)?;
        let bytes = self
            .data_bytes
            .clone()
            .try_acquire_many_owned(length)
            .map_err(|_| OperationError::Backpressure)?;
        let queue = self.queue.clone().try_reserve_owned().map_err(|_| {
            if self.queue.is_closed() {
                OperationError::ConnectionLost
            } else {
                OperationError::Backpressure
            }
        })?;
        let mut encoded = Vec::with_capacity(frame.encoded_length());
        encoded.resize(14, 0);
        plan.write_into(&mut encoded)?;
        Ok(PreparedData {
            owner: self.owner.clone(),
            queue,
            bytes: encoded,
            charge: Charge {
                _slot: slot,
                _bytes: Some(bytes),
            },
        })
    }

    /// Drops one instance-local permit, returning both byte and lane capacity.
    fn release_data(&mut self, permit: PreparedData) -> Result<(), DataPermitError> {
        if !Arc::ptr_eq(&self.owner, &permit.owner) {
            return Err(DataPermitError::Invariant);
        }
        drop(permit);
        Ok(())
    }

    /// Stamps Core's header without re-encoding or cloning the prepared body.
    fn admit_reserved_data(
        &mut self,
        mut permit: PreparedData,
        frame: OutboundFrame,
    ) -> Result<WireSequence, ReservedDataAdmissionError> {
        if !Arc::ptr_eq(&self.owner, &permit.owner) {
            return Err(ReservedDataAdmissionError::Invariant);
        }
        if self.queue.is_closed() {
            return Err(ReservedDataAdmissionError::Closed);
        }
        let (write_id, message) = frame.into_parts();
        let ProtocolMessage::Data(message) = message else {
            return Err(ReservedDataAdmissionError::Invariant);
        };
        let plan = self
            .encoder
            .plan_data(permit.bytes.len() - 14)
            .map_err(|_| ReservedDataAdmissionError::Invariant)?;
        let mut header = Vec::with_capacity(14);
        plan.write_prefix_and_header(&mut header, message.header());
        permit.bytes[..14].copy_from_slice(&header);
        let (sequence, deadline) = self
            .admission()
            .ok_or(ReservedDataAdmissionError::Invariant)?;
        permit.queue.send(QueuedFrame {
            write_id,
            sequence,
            bytes: permit.bytes,
            deadline,
            charge: permit.charge,
        });
        Ok(sequence)
    }

    /// Admits fixed-size Control using its independent lane and the same FIFO.
    fn try_admit(&mut self, frame: OutboundFrame) -> Result<WireSequence, WriteAdmissionError> {
        let (write_id, message) = frame.into_parts();
        let ProtocolMessage::Control(message) = message else {
            return Err(WriteAdmissionError::Invariant);
        };
        if self.queue.is_closed() {
            return Err(WriteAdmissionError::Closed);
        }
        let slot = self
            .control_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| WriteAdmissionError::Full)?;
        let queue = self.queue.clone().try_reserve_owned().map_err(|_| {
            if self.queue.is_closed() {
                WriteAdmissionError::Closed
            } else {
                WriteAdmissionError::Full
            }
        })?;
        let bytes = self.encoder.encode_control(message).to_vec();
        let (sequence, deadline) = self.admission().ok_or(WriteAdmissionError::Invariant)?;
        queue.send(QueuedFrame {
            write_id,
            sequence,
            bytes,
            deadline,
            charge: Charge {
                _slot: slot,
                _bytes: None,
            },
        });
        Ok(sequence)
    }
}

impl WriterWorker {
    /// Runs until admission ends or transport fails, reporting every queued write.
    /// Cancellation is passed into write_frame, never used to drop its future;
    /// its retained offset therefore determines the actual terminal visibility.
    pub(crate) async fn run<Write: AsyncWrite + Unpin>(
        mut self,
        mut writer: Write,
        mut cancellation: watch::Receiver<bool>,
    ) {
        let mut terminal = None;
        loop {
            let entry = if terminal.is_some() {
                self.queue.recv().await
            } else {
                tokio::select! {
                    biased;
                    () = cancelled(&mut cancellation) => {
                        self.queue.close();
                        terminal = Some(TransportFault::new(TransportFaultKind::Cancelled));
                        continue;
                    }
                    entry = self.queue.recv() => entry,
                }
            };
            let Some(entry) = entry else {
                break;
            };
            let outcome = if let Some(fault) = terminal {
                WriteOutcome::NotWritten(fault)
            } else if let Some(active_deadline) =
                Instant::now().checked_add(self.policy.active_write)
            {
                write_frame(
                    &mut writer,
                    &entry.bytes,
                    entry.deadline.min(active_deadline),
                    &mut cancellation,
                )
                .await
            } else {
                WriteOutcome::NotWritten(TransportFault::new(TransportFaultKind::TimedOut))
            };
            if let WriteOutcome::NotWritten(fault) | WriteOutcome::Indeterminate(fault) = outcome {
                terminal = Some(fault);
                self.queue.close();
            }
            let report = WriterReport {
                write_id: entry.write_id,
                sequence: entry.sequence,
                outcome,
                occurred_at: MonoTime::from_elapsed(self.epoch.elapsed()),
                _charge: entry.charge,
            };
            // Every report retains one of exactly channel-capacity lane permits.
            // Thus Full is impossible; a dropped Driver closes the consumer.
            if self.reports.try_send(report).is_err() {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::{
        model::ids::SystemBytes,
        protocol::{
            header::{ControlMessage, DataHeader},
            message::DataMessage,
        },
        Function, SessionId, Stream,
    };
    use tokio::io::AsyncReadExt;

    /// Creates a small queue whose Data bytes can be exhausted independently.
    fn build(bytes: u32) -> (BoundedWriter, WriterWorker, mpsc::Receiver<WriterReport>) {
        BoundedWriter::new(
            EndpointLimits::new(100, 4, 1, 2, 4, 4, 4, 4).unwrap(),
            WriterPolicy {
                data_bytes: bytes,
                residence: Duration::from_secs(3),
                active_write: Duration::from_secs(1),
            },
            Instant::now(),
        )
        .unwrap()
    }

    /// Builds a header-only S1F1 frame with visible identity fields.
    fn data(id: u64) -> OutboundFrame {
        OutboundFrame::new(
            WriteId::new(id),
            ProtocolMessage::Data(DataMessage::new(
                DataHeader::new(
                    SessionId::new(7).unwrap(),
                    Stream::new(1).unwrap(),
                    Function::new(1),
                    false,
                    SystemBytes::new(id as u32),
                ),
                None,
            )),
        )
    }

    /// Builds a Linktest request using the same numerical correlation as its ID.
    fn control(id: u64) -> OutboundFrame {
        OutboundFrame::new(
            WriteId::new(id),
            ProtocolMessage::Control(ControlMessage::LinktestRequest {
                system_bytes: SystemBytes::new(id as u32),
            }),
        )
    }

    /// One Data permit exhausts bytes but leaves independent Control admission.
    #[tokio::test]
    async fn byte_budget_and_permit_drop_leave_control_capacity_reserved() {
        let (mut ingress, _worker, _reports) = build(14);
        let permit = ingress.try_reserve_message(None).unwrap();
        assert!(matches!(
            ingress.try_reserve_message(None),
            Err(OperationError::Backpressure)
        ));
        assert_eq!(ingress.try_admit(control(1)), Ok(WireSequence::new(0)));
        ingress.release_data(permit).unwrap();
        assert!(ingress.try_reserve_message(None).is_ok());
        assert_eq!(
            ingress.try_admit(control(2)),
            Err(WriteAdmissionError::Full)
        );
    }

    /// Oversize content fails before any lane, bytes, queue or sequence is spent.
    #[tokio::test]
    async fn oversize_body_does_not_consume_reservations() {
        let (mut ingress, _worker, _reports) = build(14);
        let body = SecsItem::Binary(vec![0; 101]);
        assert!(matches!(
            ingress.try_reserve_message(Some(&body)),
            Err(OperationError::OutboundFrameTooLarge { .. })
        ));
        let permit = ingress.try_reserve_message(None).unwrap();
        assert_eq!(
            ingress.admit_reserved_data(permit, data(1)),
            Ok(WireSequence::new(0))
        );
    }

    /// Cross-instance permits fail without stealing either owner's capacity.
    #[tokio::test]
    async fn foreign_permit_is_rejected_and_its_capacity_is_returned() {
        let (mut first, _worker1, _reports1) = build(14);
        let (mut second, _worker2, _reports2) = build(14);
        let permit = first.try_reserve_message(None).unwrap();
        assert_eq!(
            second.admit_reserved_data(permit, data(1)),
            Err(ReservedDataAdmissionError::Invariant)
        );
        assert!(first.try_reserve_message(None).is_ok());
        assert!(second.try_reserve_message(None).is_ok());
    }

    /// Complete bytes and reports preserve a single total order across both lanes.
    #[tokio::test]
    async fn mixed_lane_fifo_matches_independent_wire_bytes() {
        let (mut ingress, worker, mut reports) = build(28);
        let first = ingress.try_reserve_message(None).unwrap();
        let second = ingress.try_reserve_message(None).unwrap();
        assert_eq!(
            ingress.admit_reserved_data(first, data(1)),
            Ok(WireSequence::new(0))
        );
        assert_eq!(ingress.try_admit(control(2)), Ok(WireSequence::new(1)));
        assert_eq!(
            ingress.admit_reserved_data(second, data(3)),
            Ok(WireSequence::new(2))
        );
        let (writer, mut peer) = tokio::io::duplex(128);
        let (_cancel, cancellation) = watch::channel(false);
        drop(ingress);
        worker.run(writer, cancellation).await;
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(
            bytes,
            [
                0, 0, 0, 10, 0, 7, 1, 1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 10, 255, 255, 0, 0, 0, 5, 0, 0,
                0, 2, 0, 0, 0, 10, 0, 7, 1, 1, 0, 0, 0, 0, 0, 3,
            ]
        );
        for index in 0..3 {
            let report = reports.recv().await.unwrap();
            assert_eq!(report.write_id, WriteId::new(index + 1));
            assert_eq!(report.sequence, WireSequence::new(index));
            assert_eq!(report.outcome, WriteOutcome::Committed);
            assert!(report.occurred_at.elapsed() < Duration::from_secs(3));
        }
        assert!(reports.recv().await.is_none());
    }

    /// Consuming a report without dropping its charge cannot reopen Data capacity.
    #[tokio::test]
    async fn unprocessed_outcome_retains_aggregate_budget() {
        let (mut ingress, worker, mut reports) = build(14);
        let permit = ingress.try_reserve_message(None).unwrap();
        ingress.admit_reserved_data(permit, data(1)).unwrap();
        let (writer, _peer) = tokio::io::duplex(32);
        let (cancel, cancellation) = watch::channel(false);
        let task = tokio::spawn(worker.run(writer, cancellation));
        let report = reports.recv().await.unwrap();
        assert!(matches!(
            ingress.try_reserve_message(None),
            Err(OperationError::Backpressure)
        ));
        drop(report);
        let permit = ingress.try_reserve_message(None).unwrap();
        ingress.release_data(permit).unwrap();
        cancel.send(true).unwrap();
        task.await.unwrap();
    }

    /// A partial first write poisons the stream; queued frames provably write zero.
    #[tokio::test(start_paused = true)]
    async fn stalled_write_reports_partial_then_not_written_without_reordering() {
        let (mut ingress, worker, mut reports) = build(28);
        let permit = ingress.try_reserve_message(None).unwrap();
        ingress.admit_reserved_data(permit, data(1)).unwrap();
        ingress.try_admit(control(2)).unwrap();
        let (writer, mut peer) = tokio::io::duplex(3);
        let (_cancel, cancellation) = watch::channel(false);
        worker.run(writer, cancellation).await;
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, [0, 0, 0]);
        assert!(matches!(
            reports.recv().await.unwrap().outcome,
            WriteOutcome::Indeterminate(_)
        ));
        assert!(matches!(
            reports.recv().await.unwrap().outcome,
            WriteOutcome::NotWritten(_)
        ));
        assert!(matches!(
            ingress.try_reserve_message(None),
            Err(OperationError::ConnectionLost)
        ));
    }

    /// Residence time includes the entire queue wait, not just active writing.
    #[tokio::test(start_paused = true)]
    async fn already_expired_queue_entry_never_writes_any_bytes() {
        let (mut ingress, worker, mut reports) = build(14);
        ingress.try_admit(control(1)).unwrap();
        tokio::time::advance(Duration::from_secs(3)).await;
        let (writer, mut peer) = tokio::io::duplex(32);
        let (_cancel, cancellation) = watch::channel(false);
        worker.run(writer, cancellation).await;
        let report = reports.recv().await.unwrap();
        assert_eq!(
            report.outcome,
            WriteOutcome::NotWritten(TransportFault::new(TransportFaultKind::TimedOut))
        );
        let mut bytes = Vec::new();
        peer.read_to_end(&mut bytes).await.unwrap();
        assert!(bytes.is_empty());
    }

    /// Cancellation drains every accepted frame and closes all subsequent admission.
    #[tokio::test]
    async fn cancellation_reports_each_queued_write_once() {
        let (mut ingress, worker, mut reports) = build(28);
        let permit = ingress.try_reserve_message(None).unwrap();
        ingress.admit_reserved_data(permit, data(1)).unwrap();
        ingress.try_admit(control(2)).unwrap();
        let (writer, _peer) = tokio::io::duplex(32);
        let (_cancel, cancellation) = watch::channel(true);
        worker.run(writer, cancellation).await;
        for id in 1..=2 {
            let report = reports.recv().await.unwrap();
            assert_eq!(report.write_id, WriteId::new(id));
            assert_eq!(
                report.outcome,
                WriteOutcome::NotWritten(TransportFault::new(TransportFaultKind::Cancelled))
            );
        }
        assert!(reports.recv().await.is_none());
        assert_eq!(
            ingress.try_admit(control(3)),
            Err(WriteAdmissionError::Closed)
        );
    }
}
