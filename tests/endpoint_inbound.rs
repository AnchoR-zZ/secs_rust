//! Exercises public incoming-message delivery and single-use reply capabilities.
//! Independent peer frames verify response headers, abort-only tokens, queue
//! isolation, ownership rejection and fresh capacity after application consumption.

#![cfg(feature = "runtime-tokio")]
use secs_rust::{
    EndpointConfig, EndpointError, EndpointLimits, HsmsEndpoint, HsmsHandle, HsmsReceiver,
    InboundToken, ReplyAdmissionReason, RuntimePolicy, SessionId, SessionState,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
};

/// Opens a Selected Passive endpoint with exactly one Primary queue slot.
async fn connected_with_capacity(
    capacity: usize,
) -> (
    HsmsHandle,
    HsmsReceiver,
    JoinHandle<Result<(), EndpointError>>,
    TcpStream,
) {
    let config =
        EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
            .with_limits(EndpointLimits::new(32, 8, 2, 4, capacity, 4, 4, 4).unwrap())
            .with_runtime(RuntimePolicy::default().with_byte_budgets(72, 36, 72));
    connected_with_config(config).await
}

/// Connects an independent TCP peer using the supplied endpoint resource policy.
async fn connected_with_config(
    config: EndpointConfig,
) -> (
    HsmsHandle,
    HsmsReceiver,
    JoinHandle<Result<(), EndpointError>>,
    TcpStream,
) {
    let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
    let receiver = handle.take_receiver().unwrap();
    assert!(handle.clone().take_receiver().is_none());
    let task = tokio::spawn(runtime.run());
    let mut peer = TcpStream::connect(handle.start().await.unwrap().local_address().unwrap())
        .await
        .unwrap();
    peer.write_all(&[0, 0, 0, 10, 255, 255, 0, 0, 0, 1, 0, 0, 0, 7])
        .await
        .unwrap();
    let mut frame = [0; 14];
    peer.read_exact(&mut frame).await.unwrap();
    let mut state = handle.subscribe();
    while state.borrow_and_update().session() != Some(SessionState::Selected) {
        state.changed().await.unwrap();
    }
    (handle, receiver, task, peer)
}

/// Opens the single-slot receiver used by normal reply and isolation tests.
async fn connected() -> (
    HsmsHandle,
    HsmsReceiver,
    JoinHandle<Result<(), EndpointError>>,
    TcpStream,
) {
    connected_with_capacity(1).await
}

/// Constructs a header-only incoming Primary with a chosen stream/function/token.
fn primary(stream: u8, function: u8, wait: bool, system: u8) -> [u8; 14] {
    [
        0,
        0,
        0,
        10,
        0,
        7,
        stream | if wait { 128 } else { 0 },
        function,
        0,
        0,
        0,
        0,
        0,
        system,
    ]
}

/// Reply count/byte pressure preserves tokens and cancellation retains accepted work.
#[tokio::test]
async fn reply_budget_rejection_and_cancelled_wait_preserve_ownership() {
    use std::{future::Future, task::Poll};
    tokio::time::timeout(Duration::from_secs(3), async {
        for (count, bytes) in [(1, 28), (2, 14)] {
            let config =
                EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
                    .with_limits(EndpointLimits::new(10, 4, 2, 2, 2, 2, 2, 2).unwrap())
                    .with_runtime(RuntimePolicy::default().with_reply_budget(count, bytes));
            let (handle, mut incoming, task, mut peer) = connected_with_config(config).await;
            peer.write_all(&primary(3, 1, true, 70)).await.unwrap();
            let (_, InboundToken::Reply(first)) =
                incoming.recv_primary().await.unwrap().into_parts()
            else {
                panic!("first token")
            };
            peer.write_all(&primary(3, 1, true, 71)).await.unwrap();
            let (_, InboundToken::Reply(second)) =
                incoming.recv_primary().await.unwrap().into_parts()
            else {
                panic!("second token")
            };
            let mut first_reply = Box::pin(handle.reply(first, None));
            std::future::poll_fn(|cx| {
                assert!(first_reply.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let rejected = handle.reply(second, None).await.unwrap_err();
            assert_eq!(rejected.error(), &EndpointError::Backpressure);
            let (_, admission) = rejected.into_parts();
            let (_, second, body, _) = admission.unwrap().into_parts();
            assert!(body.is_none());
            drop(first_reply);
            // No scheduler yield has occurred: cancelling only the completion
            // receiver cannot release a command still owned by the runtime queue.
            let rejected = handle.reply(second, None).await.unwrap_err();
            assert_eq!(rejected.error(), &EndpointError::Backpressure);
            let (_, admission) = rejected.into_parts();
            let (_, mut second, _, _) = admission.unwrap().into_parts();
            let mut frame = [0; 14];
            peer.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame, primary(3, 2, false, 70));
            loop {
                match handle.reply(second, None).await {
                    Ok(_) => break,
                    Err(error) => {
                        assert_eq!(error.error(), &EndpointError::Backpressure);
                        let (_, admission) = error.into_parts();
                        (_, second, _, _) = admission.unwrap().into_parts();
                        tokio::task::yield_now().await;
                    }
                }
            }
            peer.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame, primary(3, 2, false, 71));
            handle.stop().await.unwrap();
            drop(handle);
            task.await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
}

/// Replies remain possible when a pending Primary owns all ordinary count/bytes.
#[tokio::test]
async fn pending_primary_cannot_exhaust_reply_admission() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
                .with_limits(EndpointLimits::new(10, 1, 2, 2, 2, 2, 2, 2).unwrap())
                .with_runtime(
                    RuntimePolicy::default()
                        .with_byte_budgets(14, 28, 28)
                        .with_reply_budget(1, 14),
                );
        let (handle, mut incoming, task, mut peer) = connected_with_config(config).await;
        let requesting = handle.clone();
        let request = tokio::spawn(async move {
            requesting
                .request(secs_rust::PrimaryMessage::new(
                    secs_rust::Stream::new(1).unwrap(),
                    secs_rust::Function::new(1),
                    None,
                ))
                .await
        });
        let mut outgoing = [0; 14];
        peer.read_exact(&mut outgoing).await.unwrap();
        assert_eq!(outgoing[6], 129);
        for (function, system) in [(1, 50), (255, 51), (3, 52)] {
            peer.write_all(&primary(3, function, true, system))
                .await
                .unwrap();
            let (_, InboundToken::Reply(token)) =
                incoming.recv_primary().await.unwrap().into_parts()
            else {
                panic!("reply token")
            };
            match function {
                1 => {
                    handle.reply(token, None).await.unwrap();
                }
                255 => {
                    handle.abort_reply(token).await.unwrap();
                }
                _ => {
                    handle.abandon_reply(token).await.unwrap();
                }
            }
            if function != 3 {
                let mut response = [0; 14];
                peer.read_exact(&mut response).await.unwrap();
                assert_eq!(
                    response,
                    primary(3, if function == 255 { 0 } else { 2 }, false, system)
                );
            }
            assert!(!request.is_finished());
        }
        outgoing[6] &= 127;
        outgoing[7] = 2;
        peer.write_all(&outgoing).await.unwrap();
        request.await.unwrap().unwrap();
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// Normal reply, F255 abort and abandonment all preserve single-use peer authority.
#[tokio::test]
async fn normal_reply_abort_and_abandon_use_original_peer_headers() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (handle, mut incoming, task, mut peer) = connected().await;
        peer.write_all(&primary(3, 5, true, 10)).await.unwrap();
        let received = incoming.recv_primary().await.unwrap();
        assert_eq!(received.context().header(), &primary(3, 5, true, 10)[4..]);
        let (_, InboundToken::Reply(token)) = received.into_parts() else {
            panic!("W=1 reply token")
        };
        handle.reply(token, None).await.unwrap();
        let mut frame = [0; 14];
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame, primary(3, 6, false, 10));
        peer.write_all(&primary(3, 255, true, 11)).await.unwrap();
        let (_, InboundToken::Reply(token)) = incoming.recv_primary().await.unwrap().into_parts()
        else {
            panic!("abort token")
        };
        let rejected = handle.reply(token, None).await.unwrap_err();
        let (_, admission) = rejected.into_parts();
        let (_, token, _, reason) = admission.unwrap().into_parts();
        assert_eq!(reason, ReplyAdmissionReason::ReplyRequiresAbort);
        handle.abort_reply(token).await.unwrap();
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame, primary(3, 0, false, 11));
        peer.write_all(&primary(1, 1, true, 12)).await.unwrap();
        let (_, InboundToken::Reply(token)) = incoming.recv_primary().await.unwrap().into_parts()
        else {
            panic!("abandon token")
        };
        handle.abandon_reply(token).await.unwrap();
        handle.stop().await.unwrap();
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame[9], 9);
        drop(handle);
        task.await.unwrap().unwrap();
        assert!(incoming.recv_primary().await.is_none());
    })
    .await
    .unwrap();
}

/// A full application byte budget cannot block a later matched Secondary.
#[tokio::test]
async fn primary_byte_budget_is_independent_of_reader_and_released_on_receive() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
                .with_limits(EndpointLimits::new(10, 2, 2, 2, 2, 2, 2, 2).unwrap())
                .with_runtime(
                    RuntimePolicy::default()
                        .with_byte_budgets(28, 28, 28)
                        .with_primary_queue_bytes(14),
                );
        let (handle, mut incoming, task, mut peer) = connected_with_config(config).await;
        for system in [60, 61] {
            let request_handle = handle.clone();
            let request = tokio::spawn(async move {
                request_handle
                    .request(secs_rust::PrimaryMessage::new(
                        secs_rust::Stream::new(1).unwrap(),
                        secs_rust::Function::new(1),
                        None,
                    ))
                    .await
            });
            let mut response = [0; 14];
            peer.read_exact(&mut response).await.unwrap();
            peer.write_all(&primary(3, 1, false, system)).await.unwrap();
            response[6] &= 127;
            response[7] = 2;
            peer.write_all(&response).await.unwrap();
            // FIFO processing has filled the Primary byte budget before this
            // Secondary is processed. No Primary has been consumed yet.
            request.await.unwrap().unwrap();
            assert_eq!(
                incoming.recv_primary().await.unwrap().context().header()[9],
                system
            );
        }
        // With two count slots available, a third pair exhausts bytes alone.
        peer.write_all(&primary(3, 1, false, 62)).await.unwrap();
        peer.write_all(&primary(3, 1, false, 63)).await.unwrap();
        let mut states = handle.subscribe();
        while states.borrow_and_update().phase() != secs_rust::EndpointPhase::Faulted {
            states.changed().await.unwrap();
        }
        assert_eq!(
            incoming.recv_primary().await.unwrap().context().header()[9],
            62
        );
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
        assert!(incoming.recv_primary().await.is_none());
    })
    .await
    .unwrap();
}

/// Error delivery remains usable with a full Primary queue; later consumption frees it.
#[tokio::test]
async fn independent_error_queue_and_consumed_primary_capacity_remain_usable() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (handle, mut incoming, task, mut peer) = connected().await;
        peer.write_all(&primary(1, 1, false, 20)).await.unwrap();
        let invalid = [0, 0, 0, 10, 255, 255, 0, 0, 0, 99, 0, 0, 0, 21];
        peer.write_all(&invalid).await.unwrap();
        let error = incoming.recv_protocol_error().await.unwrap();
        assert_eq!(error.context().header(), &invalid[4..]);
        let mut reject = [0; 14];
        peer.read_exact(&mut reject).await.unwrap();
        assert_eq!(reject[9], 7);
        assert!(matches!(
            incoming.recv_primary().await.unwrap().into_parts().1,
            InboundToken::Data(_)
        ));
        peer.write_all(&primary(1, 1, false, 22)).await.unwrap();
        assert_eq!(
            incoming.recv_primary().await.unwrap().context().header()[9],
            22
        );
        assert_eq!(handle.snapshot().session(), Some(SessionState::Selected));
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// Count and byte saturation close the generation without overwriting its first event.
#[tokio::test]
async fn receiver_count_and_byte_limits_fail_closed_without_overwrite() {
    tokio::time::timeout(Duration::from_secs(3), async {
        for capacity in [1, 3] {
            let (handle, mut incoming, task, mut peer) = connected_with_capacity(capacity).await;
            let mut states = handle.subscribe();
            for system in [40, 41] {
                let mut frame = primary(1, 1, false, system).to_vec();
                if capacity == 3 {
                    frame[3] = 15;
                    frame.extend_from_slice(&[0x21, 3, 1, 2, 3]);
                }
                peer.write_all(&frame).await.unwrap();
            }
            while states.borrow_and_update().phase() != secs_rust::EndpointPhase::Faulted {
                states.changed().await.unwrap();
            }
            assert_eq!(
                incoming.recv_primary().await.unwrap().context().header()[9],
                40
            );
            handle.stop().await.unwrap();
            drop(handle);
            task.await.unwrap().unwrap();
            assert!(incoming.recv_primary().await.is_none());
        }
    })
    .await
    .unwrap();
}

/// Dropping either reliable receiver preserves the other until delivery needs it.
#[tokio::test]
async fn dropped_reliable_receiver_faults_only_when_its_delivery_is_required() {
    use secs_rust::{ConnectionCloseReason, DiagnosticEvent, EndpointPhase};
    tokio::time::timeout(Duration::from_secs(3), async {
        for drop_primary in [true, false] {
            let (handle, incoming, task, mut peer) = connected().await;
            let mut diagnostics = handle.take_diagnostics().unwrap();
            let (primaries, errors) = incoming.split();
            let invalid = [0, 0, 0, 10, 255, 255, 0, 0, 0, 99, 0, 0, 0, 80];
            if drop_primary {
                drop(primaries);
                let mut errors = errors;
                peer.write_all(&invalid).await.unwrap();
                assert_eq!(
                    errors.recv().await.unwrap().context().header(),
                    &invalid[4..]
                );
                let mut reject = [0; 14];
                peer.read_exact(&mut reject).await.unwrap();
                assert_eq!(reject[9], 7);
                assert_eq!(handle.snapshot().session(), Some(SessionState::Selected));
                peer.write_all(&primary(3, 1, true, 81)).await.unwrap();
            } else {
                drop(errors);
                let mut primaries = primaries;
                peer.write_all(&primary(3, 1, false, 81)).await.unwrap();
                assert_eq!(primaries.recv().await.unwrap().context().header()[9], 81);
                assert_eq!(handle.snapshot().session(), Some(SessionState::Selected));
                peer.write_all(&invalid).await.unwrap();
            }
            loop {
                if let DiagnosticEvent::ConnectionClosed { reason, clean, .. } =
                    diagnostics.recv().await.unwrap().event()
                {
                    assert_eq!(*reason, ConnectionCloseReason::ApplicationBackpressure);
                    assert!(*clean);
                    break;
                }
            }
            assert_eq!(handle.snapshot().phase(), EndpointPhase::Faulted);
            handle.stop().await.unwrap();
            assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
            drop(handle);
            task.await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
}

/// A token rejected by another endpoint remains usable by its original owner.
#[tokio::test]
async fn cross_endpoint_reply_rejection_returns_original_capability() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (first, mut incoming, first_task, mut peer) = connected().await;
        let (second, _incoming2, second_task, _peer2) = connected().await;
        peer.write_all(&primary(1, 1, true, 30)).await.unwrap();
        let (_, InboundToken::Reply(token)) = incoming.recv_primary().await.unwrap().into_parts()
        else {
            panic!("reply token")
        };
        let failure = second.reply(token, None).await.unwrap_err();
        let (_, admission) = failure.into_parts();
        let (_, token, body, reason) = admission.unwrap().into_parts();
        assert_eq!(reason, ReplyAdmissionReason::CapabilityUnavailable);
        first.reply(token, body).await.unwrap();
        let mut frame = [0; 14];
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame, primary(1, 2, false, 30));
        first.stop().await.unwrap();
        second.stop().await.unwrap();
        drop(first);
        drop(second);
        first_task.await.unwrap().unwrap();
        second_task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}
