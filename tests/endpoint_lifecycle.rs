//! Exercises public endpoint lifecycle exclusively through exported library APIs.
//! Independent TCP peers verify startup acknowledgement, Select, replacement,
//! reusable Stop/Start and final-handle shutdown of the explicitly owned runtime.

#![cfg(feature = "runtime-tokio")]

use secs_rust::{
    EndpointConfig, EndpointError, EndpointPhase, EndpointStateSnapshot, GenerationSlotSnapshot,
    HsmsEndpoint, HsmsTimeouts, RuntimePolicy, SessionId, SessionState,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};

/// Waits for a latest-state predicate without losing a change between inspections.
async fn state_until(
    state: &mut watch::Receiver<EndpointStateSnapshot>,
    predicate: impl Fn(EndpointStateSnapshot) -> bool,
) -> EndpointStateSnapshot {
    loop {
        let snapshot = *state.borrow_and_update();
        if predicate(snapshot) {
            return snapshot;
        }
        state.changed().await.unwrap();
    }
}

/// Sends an independent Select request and checks its complete response bytes.
async fn select_passive(peer: &mut TcpStream) {
    peer.write_all(&[0, 0, 0, 10, 255, 255, 0, 0, 0, 1, 0, 0, 0, 7])
        .await
        .unwrap();
    let mut frame = [0; 14];
    peer.read_exact(&mut frame).await.unwrap();
    assert_eq!(frame, [0, 0, 0, 10, 255, 255, 0, 0, 0, 2, 0, 0, 0, 7]);
}

/// Answers the library's automatic Select without using its encoder.
async fn select_active(peer: &mut TcpStream) {
    let mut frame = [0; 14];
    peer.read_exact(&mut frame).await.unwrap();
    assert_eq!(&frame[..10], &[0, 0, 0, 10, 255, 255, 0, 0, 0, 1]);
    frame[9] = 2;
    peer.write_all(&frame).await.unwrap();
}

/// Extra Passive connects cannot succeed while the live session owns the endpoint.
#[tokio::test]
async fn passive_rejects_extra_connections_and_accepts_fresh_replacement() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let task = tokio::spawn(runtime.run());
        let address = handle.start().await.unwrap().local_address().unwrap();
        let mut peer = TcpStream::connect(address).await.unwrap();
        select_passive(&mut peer).await;
        let mut states = handle.subscribe();
        let first = state_until(&mut states, |s| s.session() == Some(SessionState::Selected)).await;
        let extras = tokio::spawn(async move {
            for _ in 0..16 {
                match tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(address))
                    .await
                {
                    Err(_) | Ok(Err(_)) => {}
                    Ok(Ok(_)) => panic!("occupied endpoint must not listen"),
                }
            }
        });
        for system in 19..35 {
            let linktest = [0, 0, 0, 10, 255, 255, 0, 0, 0, 5, 0, 0, 0, system];
            peer.write_all(&linktest).await.unwrap();
            let mut frame = [0; 14];
            peer.read_exact(&mut frame).await.unwrap();
            let mut expected = linktest;
            expected[9] = 6;
            assert_eq!(frame, expected);
            assert_eq!(handle.snapshot().generation(), first.generation());
        }
        extras.await.unwrap();
        handle.disconnect().await.unwrap();
        let mut replacement = TcpStream::connect(address).await.unwrap();
        select_passive(&mut replacement).await;
        state_until(&mut states, |s| {
            s.session() == Some(SessionState::Selected) && s.generation() != first.generation()
        })
        .await;
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// Queued business work cannot occupy the single reserved close slot.
#[tokio::test]
async fn queued_close_reservation_is_independent_and_bounded() {
    use std::{future::Future, task::Poll};
    let config =
        EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
            .with_limits(secs_rust::EndpointLimits::new(32, 1, 2, 2, 2, 2, 2, 2).unwrap());
    let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
    let mut start = Box::pin(handle.start());
    let mut stop = Box::pin(handle.stop());
    std::future::poll_fn(|cx| {
        assert!(start.as_mut().poll(cx).is_pending());
        assert!(stop.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
    assert_eq!(handle.start().await, Err(EndpointError::Backpressure));
    assert_eq!(handle.disconnect().await, Err(EndpointError::Backpressure));
    let task = tokio::spawn(runtime.run());
    tokio::time::timeout(Duration::from_secs(3), async {
        start.await.unwrap();
        stop.await.unwrap();
        assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
        // Cleanup released the close reservation for another independent call.
        handle.stop().await.unwrap();
    })
    .await
    .unwrap();
    drop(handle);
    task.await.unwrap().unwrap();
}

/// Stop remains admissible while the sole ordinary command awaits its Secondary.
#[tokio::test]
async fn stop_has_reserved_capacity_while_request_is_pending() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let second = Duration::from_secs(1);
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
                .with_limits(secs_rust::EndpointLimits::new(32, 1, 2, 2, 2, 2, 2, 2).unwrap())
                .with_runtime(RuntimePolicy::default().with_deadlines(
                    Duration::from_millis(20),
                    second,
                    second,
                    second,
                    second,
                ));
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let task = tokio::spawn(runtime.run());
        let address = handle.start().await.unwrap().local_address().unwrap();
        let mut peer = TcpStream::connect(address).await.unwrap();
        select_passive(&mut peer).await;
        state_until(&mut handle.subscribe(), |s| {
            s.session() == Some(SessionState::Selected)
        })
        .await;
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
        let mut frame = [0; 14];
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame[6], 129);
        assert!(!request.is_finished());
        assert_eq!(
            handle.control(secs_rust::ControlIntent::Linktest).await,
            Err(EndpointError::Backpressure)
        );
        handle
            .stop()
            .await
            .expect("business saturation must not prevent Stop");
        assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
        assert!(request.await.unwrap().is_err());
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(frame[9], 9);
        assert_eq!(peer.read(&mut [0]).await.unwrap(), 0);
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// Build neither requires a Tokio context nor attempts a Passive bind.
#[test]
fn endpoint_build_is_pure_even_when_its_port_is_occupied() {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let config =
        EndpointConfig::passive(occupied.local_addr().unwrap(), SessionId::new(7).unwrap());
    let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
    assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
    drop(runtime);
    drop(handle);
}

/// Passive start binds first, Disconnect replaces, and Stop/Start reuses the handle.
#[tokio::test]
async fn passive_lifecycle_is_reusable_and_final_handle_joins_runtime() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let mut states = handle.subscribe();
        let task = tokio::spawn(runtime.run());
        let address = handle.start().await.unwrap().local_address().unwrap();
        assert_ne!(address.port(), 0);
        let mut peer = TcpStream::connect(address).await.unwrap();
        select_passive(&mut peer).await;
        let first = state_until(&mut states, |s| s.session() == Some(SessionState::Selected)).await;
        handle.disconnect().await.unwrap();
        let mut separate = [0; 14];
        peer.read_exact(&mut separate).await.unwrap();
        assert_eq!(separate[9], 9);
        let mut byte = [0];
        assert_eq!(peer.read(&mut byte).await.unwrap(), 0);
        let mut peer2 = TcpStream::connect(address).await.unwrap();
        select_passive(&mut peer2).await;
        let second = state_until(&mut states, |s| {
            s.session() == Some(SessionState::Selected) && s.generation() != first.generation()
        })
        .await;
        assert!(matches!(
            second.generation(),
            GenerationSlotSnapshot::Open(_)
        ));
        handle.stop().await.unwrap();
        assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
        let restarted = handle.start().await.unwrap().local_address().unwrap();
        let _peer3 = TcpStream::connect(restarted).await.unwrap();
        let clone = handle.clone();
        drop(handle);
        clone.stop().await.unwrap();
        drop(clone);
        task.await.unwrap().unwrap();
        assert!(states.changed().await.is_ok() || states.has_changed().is_err());
    })
    .await
    .unwrap();
}

/// An Active endpoint automatically reconnects and selects a fresh generation.
#[tokio::test]
async fn active_loop_reconnects_without_resubmitting_start() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = Duration::from_secs(1);
        let config =
            EndpointConfig::active(listener.local_addr().unwrap(), SessionId::new(7).unwrap())
                .with_timeouts(
                    HsmsTimeouts::new(
                        second,
                        second,
                        Duration::from_millis(20),
                        second,
                        second,
                        second,
                        None,
                    )
                    .unwrap(),
                )
                .with_runtime(RuntimePolicy::default().with_deadlines(
                    Duration::from_millis(20),
                    second,
                    second,
                    second,
                    second,
                ));
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let mut states = handle.subscribe();
        let task = tokio::spawn(runtime.run());
        assert_eq!(handle.start().await.unwrap().local_address(), None);
        let (mut peer, _) = listener.accept().await.unwrap();
        select_active(&mut peer).await;
        let first = state_until(&mut states, |s| s.session() == Some(SessionState::Selected)).await;
        drop(peer);
        let (mut peer2, _) = listener.accept().await.unwrap();
        select_active(&mut peer2).await;
        state_until(&mut states, |s| {
            s.session() == Some(SessionState::Selected) && s.generation() != first.generation()
        })
        .await;
        drop(handle);
        task.await.unwrap().unwrap();
        let mut separate = [0; 14];
        peer2.read_exact(&mut separate).await.unwrap();
        assert_eq!(separate[9], 9);
    })
    .await
    .unwrap();
}

/// Public control calls use the real wire and publish state before acknowledgement.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_select_and_linktest_deliver_typed_completions() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config =
            EndpointConfig::active(listener.local_addr().unwrap(), SessionId::new(7).unwrap())
                .with_runtime(RuntimePolicy::default().with_auto_select(false));
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let mut states = handle.subscribe();
        let task = tokio::spawn(runtime.run());
        handle.start().await.unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        state_until(&mut states, |state| {
            matches!(state.generation(), GenerationSlotSnapshot::Open(_))
        })
        .await;
        let peer_task = tokio::spawn(async move {
            select_active(&mut peer).await;
            let mut probe = [0; 14];
            peer.read_exact(&mut probe).await.unwrap();
            assert_eq!(probe[9], 5);
            probe[9] = 6;
            peer.write_all(&probe).await.unwrap();
            peer
        });
        handle
            .control(secs_rust::ControlIntent::Select)
            .await
            .unwrap();
        assert_eq!(handle.snapshot().session(), Some(SessionState::Selected));
        handle
            .control(secs_rust::ControlIntent::Linktest)
            .await
            .unwrap();
        let _peer = peer_task.await.unwrap();
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// Dropping an unstarted runtime makes every later request fail deterministically.
#[tokio::test]
async fn dropped_runtime_rejects_handle_requests() {
    let config =
        EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
    let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
    drop(runtime);
    assert_eq!(handle.start().await, Err(EndpointError::RuntimeStopped));
    assert_eq!(handle.stop().await, Err(EndpointError::RuntimeStopped));
    assert_eq!(
        handle.disconnect().await,
        Err(EndpointError::RuntimeStopped)
    );
}
