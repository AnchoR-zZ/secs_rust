//! Validates public outbound Data operations against independent TCP frame bytes.
//! These tests exercise application ownership, W-bit policy, response matching,
//! byte pressure and release through real endpoint/supervisor/transport execution.

#![cfg(feature = "runtime-tokio")]
use secs_rust::{
    EndpointConfig, EndpointError, EndpointLimits, Function, HsmsEndpoint, HsmsHandle,
    PrimaryMessage, RuntimePolicy, SessionId, SessionState, Stream,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
};

/// Constructs one absent-body S1F1 Primary.
fn primary() -> PrimaryMessage {
    PrimaryMessage::new(Stream::new(1).unwrap(), Function::new(1), None)
}

/// Starts a selected Passive endpoint with a deliberately small command byte budget.
async fn connected_with_t3(
    t3: Duration,
) -> (HsmsHandle, JoinHandle<Result<(), EndpointError>>, TcpStream) {
    let config =
        EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
            .with_limits(EndpointLimits::new(32, 4, 2, 4, 2, 4, 4, 4).unwrap())
            .with_runtime(RuntimePolicy::default().with_byte_budgets(36, 36, 72))
            .with_timeouts(
                secs_rust::HsmsTimeouts::new(
                    Duration::from_secs(1),
                    t3,
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    None,
                )
                .unwrap(),
            );
    let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
    let task = tokio::spawn(runtime.run());
    let address = handle.start().await.unwrap().local_address().unwrap();
    let mut peer = TcpStream::connect(address).await.unwrap();
    peer.write_all(&[0, 0, 0, 10, 255, 255, 0, 0, 0, 1, 0, 0, 0, 7])
        .await
        .unwrap();
    let mut response = [0; 14];
    peer.read_exact(&mut response).await.unwrap();
    let mut state = handle.subscribe();
    while state.borrow_and_update().session() != Some(SessionState::Selected) {
        state.changed().await.unwrap();
    }
    (handle, task, peer)
}

/// Creates the normal fixture with a generous T3 for non-timeout scenarios.
async fn connected() -> (HsmsHandle, JoinHandle<Result<(), EndpointError>>, TcpStream) {
    connected_with_t3(Duration::from_secs(45)).await
}

/// T3 errors expose the exact committed Primary header, including F255 and W=1.
#[tokio::test]
async fn public_t3_timeout_retains_original_header_and_keeps_connection_selected() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (handle, task, mut peer) = connected_with_t3(Duration::from_millis(40)).await;
        for function in [1, 255] {
            let request_handle = handle.clone();
            let request = tokio::spawn(async move {
                request_handle
                    .request(PrimaryMessage::new(
                        Stream::new(3).unwrap(),
                        Function::new(function),
                        None,
                    ))
                    .await
            });
            let mut frame = [0; 14];
            peer.read_exact(&mut frame).await.unwrap();
            let failure = request.await.unwrap().unwrap_err();
            let EndpointError::Operation(secs_rust::OperationError::RequestTimeout { context }) =
                failure.error()
            else {
                panic!("T3 must expose request context")
            };
            assert_eq!(context.header(), &frame[4..]);
            assert_eq!(context.header()[2], 131);
            assert_eq!(context.header()[3], function);
            assert!(failure.message().is_none());
            assert_eq!(handle.snapshot().session(), Some(SessionState::Selected));
        }
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// W=0 yields a local write receipt; W=1 awaits its independently encoded Secondary.
#[tokio::test]
async fn public_send_and_request_use_distinct_wire_and_completion_semantics() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (handle, task, mut peer) = connected().await;
        let receipt = handle.send(primary()).await.unwrap();
        let mut frame = [0; 14];
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(&frame[..10], &[0, 0, 0, 10, 0, 7, 1, 1, 0, 0]);
        let request_handle = handle.clone();
        let request = tokio::spawn(async move { request_handle.request(primary()).await });
        peer.read_exact(&mut frame).await.unwrap();
        assert_eq!(&frame[..10], &[0, 0, 0, 10, 0, 7, 129, 1, 0, 0]);
        assert!(!request.is_finished());
        frame[6] = 1;
        frame[7] = 2;
        peer.write_all(&frame).await.unwrap();
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.function(), Function::new(2));
        assert_eq!(response.context().generation(), receipt.generation());
        assert_eq!(response.context().header(), &frame[4..]);
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// Aggregate byte pressure returns the original message and clears after settlement.
#[tokio::test]
async fn public_byte_budget_preserves_rejected_message_and_allows_retry() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (handle, task, mut peer) = connected().await;
        let first_handle = handle.clone();
        let first = tokio::spawn(async move { first_handle.request(primary()).await });
        let mut first_frame = [0; 14];
        peer.read_exact(&mut first_frame).await.unwrap();
        let second_handle = handle.clone();
        let second = tokio::spawn(async move { second_handle.request(primary()).await });
        let mut second_frame = [0; 14];
        peer.read_exact(&mut second_frame).await.unwrap();
        let rejected = handle.send(primary()).await.unwrap_err();
        assert_eq!(rejected.error(), &EndpointError::Backpressure);
        let (_, returned) = rejected.into_parts();
        assert_eq!(returned.as_ref(), Some(&primary()));
        first_frame[6] = 1;
        first_frame[7] = 2;
        peer.write_all(&first_frame).await.unwrap();
        first.await.unwrap().unwrap();
        handle.send(returned.unwrap()).await.unwrap();
        let mut send = [0; 14];
        peer.read_exact(&mut send).await.unwrap();
        assert_eq!(send[6], 1);
        second_frame[6] = 1;
        second_frame[7] = 2;
        peer.write_all(&second_frame).await.unwrap();
        second.await.unwrap().unwrap();
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// Aborting runtime before dequeuing an admitted Primary returns its original body.
#[tokio::test]
async fn runtime_abort_returns_primary_still_waiting_in_endpoint_queue() {
    use std::{future::Future, task::Poll};
    let (handle, task, _peer) = connected().await;
    let mut send = Box::pin(handle.send(primary()));
    assert!(std::future::poll_fn(|cx| Poll::Ready(send.as_mut().poll(cx).is_pending())).await);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let rejected = send.await.unwrap_err();
    assert_eq!(rejected.error(), &EndpointError::RuntimeStopped);
    assert_eq!(rejected.message(), Some(&primary()));
}

/// Admission without a current connection returns all Primary content unchanged.
#[tokio::test]
async fn disconnected_public_send_returns_original_primary() {
    let config =
        EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
    let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
    let failure = handle.send(primary()).await.unwrap_err();
    assert_eq!(failure.error(), &EndpointError::NotConnected);
    assert_eq!(failure.message(), Some(&primary()));
    drop(runtime);
}
