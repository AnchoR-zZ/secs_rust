//! Public best-effort diagnostic delivery under saturation and connection failure.
//! Tests ensure diagnostic pressure cannot block lifecycle progress or hide loss,
//! and records preserve the actual reason and incarnation of completed cleanup.

#![cfg(feature = "runtime-tokio")]
use secs_rust::{
    ConnectionCloseReason, DiagnosticEvent, EndpointConfig, EndpointPhase, GenerationSlotSnapshot,
    HsmsEndpoint, HsmsHandle, RuntimePolicy, SessionId,
};
use std::time::Duration;
use tokio::net::TcpStream;

/// Connects one passive generation then waits for a clean public Stop acknowledgement.
async fn connect_and_stop(handle: &HsmsHandle) {
    let previous_exit = handle.snapshot().last_exit();
    let address = handle.start().await.unwrap().local_address().unwrap();
    assert_eq!(handle.snapshot().last_exit(), previous_exit);
    let _peer = TcpStream::connect(address).await.unwrap();
    let mut state = handle.subscribe();
    while !matches!(
        state.borrow_and_update().generation(),
        GenerationSlotSnapshot::Open(_)
    ) {
        state.changed().await.unwrap();
    }
    handle.stop().await.unwrap();
    assert_eq!(handle.snapshot().phase(), EndpointPhase::StoppedClean);
}

/// A full diagnostic queue neither blocks Stop/Start nor overwrites its oldest record.
#[tokio::test]
async fn diagnostic_loss_is_counted_and_lifecycle_keeps_progressing() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let config = EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
            .with_runtime(RuntimePolicy::default().with_diagnostic_capacity(1));
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let mut diagnostics = handle.take_diagnostics().unwrap();
        assert!(handle.clone().take_diagnostics().is_none());
        let task = tokio::spawn(runtime.run());
        assert!(handle.snapshot().last_exit().is_none());
        connect_and_stop(&handle).await;
        let first_exit = handle.snapshot().last_exit().unwrap();
        connect_and_stop(&handle).await;
        let second_exit = handle.snapshot().last_exit().unwrap();
        assert!(second_exit.generation() > first_exit.generation());
        assert_eq!(second_exit.reason(), ConnectionCloseReason::LocalStop);
        assert!(second_exit.clean());
        assert_eq!(diagnostics.dropped_count(), 1);
        let first = diagnostics.recv().await.unwrap();
        assert_eq!(first.sequence(), 0);
        let DiagnosticEvent::ConnectionClosed { generation: first_generation, reason, clean } = first.into_event() else { panic!("closed diagnostic") };
        assert_eq!(reason, ConnectionCloseReason::LocalStop); assert!(clean);
        connect_and_stop(&handle).await;
        let third = diagnostics.recv().await.unwrap();
        assert_eq!(third.sequence(), 2);
        assert!(matches!(third.event(), DiagnosticEvent::ConnectionClosed { generation, clean: true, .. } if *generation > first_generation));
        let retained = handle.subscribe();
        let final_exit = handle.snapshot().last_exit().unwrap();
        drop(handle); task.await.unwrap().unwrap();
        assert_eq!(retained.borrow().last_exit(), Some(final_exit));
        assert!(diagnostics.recv().await.is_none());
    }).await.unwrap();
}

/// Protocol mismatches and late responses are observed without stealing a request.
#[tokio::test]
async fn response_mismatch_late_response_and_unknown_reject_are_classified() {
    use secs_rust::{
        Function, PeerRejectDisposition, PrimaryMessage, ProtocolNotice, SessionState, Stream,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(Duration::from_secs(3), async {
        let config = EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let mut diagnostics = handle.take_diagnostics().unwrap();
        let task = tokio::spawn(runtime.run());
        let mut peer = TcpStream::connect(handle.start().await.unwrap().local_address().unwrap()).await.unwrap();
        peer.write_all(&[0,0,0,10,255,255,0,0,0,1,0,0,0,7]).await.unwrap();
        let mut frame = [0;14]; peer.read_exact(&mut frame).await.unwrap();
        let mut state = handle.subscribe();
        while state.borrow_and_update().session() != Some(SessionState::Selected) { state.changed().await.unwrap(); }
        let request_handle = handle.clone();
        let request = tokio::spawn(async move { request_handle.request(PrimaryMessage::new(Stream::new(1).unwrap(), Function::new(1), None)).await });
        peer.read_exact(&mut frame).await.unwrap();
        frame[6] = 2; frame[7] = 2;
        peer.write_all(&frame).await.unwrap();
        assert!(matches!(diagnostics.recv().await.unwrap().event(), DiagnosticEvent::Protocol { notice: ProtocolNotice::SecondaryMismatch, .. }));
        assert!(!request.is_finished());
        frame[6] = 1; peer.write_all(&frame).await.unwrap(); request.await.unwrap().unwrap();
        peer.write_all(&frame).await.unwrap();
        assert!(matches!(diagnostics.recv().await.unwrap().event(), DiagnosticEvent::Protocol { notice: ProtocolNotice::StaleEventIgnored, .. }));
        peer.write_all(&[0,0,0,10,255,255,5,3,0,7,0,0,0,99]).await.unwrap();
        assert!(matches!(diagnostics.recv().await.unwrap().event(), DiagnosticEvent::Protocol { notice: ProtocolNotice::PeerReject(notice), .. } if notice.disposition() == PeerRejectDisposition::Unknown));
        handle.stop().await.unwrap(); drop(handle); task.await.unwrap().unwrap();
    }).await.unwrap();
}

/// Automatic and explicit Select share command-backed Reject attribution.
#[tokio::test]
async fn select_rejection_reports_command_attribution_for_both_origins() {
    use secs_rust::{ControlIntent, PeerRejectDisposition, ProtocolNotice, SessionState};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config =
            EndpointConfig::active(listener.local_addr().unwrap(), SessionId::new(7).unwrap());
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let mut diagnostics = handle.take_diagnostics().unwrap();
        let task = tokio::spawn(runtime.run());
        handle.start().await.unwrap();
        let (mut peer, _) = listener.accept().await.unwrap();
        for explicit in [false, true] {
            let operation = if explicit {
                let selected = handle.clone();
                Some(tokio::spawn(async move {
                    selected.control(ControlIntent::Select).await
                }))
            } else {
                None
            };
            let mut frame = [0; 14];
            peer.read_exact(&mut frame).await.unwrap();
            assert_eq!(frame[9], 1);
            frame[6] = 1;
            frame[7] = 3;
            frame[9] = 7;
            peer.write_all(&frame).await.unwrap();
            assert!(matches!(diagnostics.recv().await.unwrap().event(),
                DiagnosticEvent::Protocol { notice: ProtocolNotice::PeerReject(notice), .. }
                if notice.disposition() == PeerRejectDisposition::OperationRejected));
            if let Some(operation) = operation {
                assert!(operation.await.unwrap().is_err());
            }
            assert_eq!(handle.snapshot().session(), Some(SessionState::NotSelected));
        }
        handle.stop().await.unwrap();
        drop(handle);
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
}

/// Connect failure is observable while Active running intent continues under T5.
#[tokio::test]
async fn failed_active_attempt_emits_diagnostic_without_failing_start() {
    tokio::time::timeout(Duration::from_secs(3), async {
        let config = EndpointConfig::active("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
        let (handle, runtime) = HsmsEndpoint::build(config).unwrap();
        let mut diagnostics = handle.take_diagnostics().unwrap();
        let task = tokio::spawn(runtime.run());
        handle.start().await.unwrap();
        assert!(matches!(diagnostics.recv().await.unwrap().event(), DiagnosticEvent::ConnectionAttemptFailed { message } if !message.is_empty()));
        assert_eq!(handle.snapshot().phase(), EndpointPhase::Running);
        handle.stop().await.unwrap(); drop(handle); task.await.unwrap().unwrap();
    }).await.unwrap();
}
