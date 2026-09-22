//! Runs two public HSMS endpoints through a complete request/reply and shutdown.
//! The message body is illustrative application data, not an implementation of a
//! GEM procedure. Both endpoints use a local ephemeral port and release all tasks.

use secs_rust::{
    EndpointConfig, Function, HsmsEndpoint, HsmsHandle, InboundToken, PrimaryMessage, SecsItem,
    SessionId, SessionState, Stream,
};
use std::{error::Error, io, time::Duration};

/// Common example error type compatible with Tokio task results.
type ExampleError = Box<dyn Error + Send + Sync>;

/// Waits for Select using a latest-value subscription that cannot miss a change.
async fn wait_selected(handle: &HsmsHandle) -> Result<(), ExampleError> {
    let mut state = handle.subscribe();
    while state.borrow_and_update().session() != Some(SessionState::Selected) {
        state.changed().await?;
    }
    Ok(())
}

/// Demonstrates explicit runtime ownership, single-consumer reception and cleanup.
#[tokio::main]
async fn main() -> Result<(), ExampleError> {
    let session = SessionId::new(7)?;
    let (passive, passive_runtime) =
        HsmsEndpoint::build(EndpointConfig::passive("127.0.0.1:0".parse()?, session))?;
    let mut incoming = passive.take_receiver().expect("unique receiver");
    let passive_task = tokio::spawn(passive_runtime.run());
    let address = passive
        .start()
        .await?
        .local_address()
        .expect("Passive listener");
    let (active, active_runtime) = HsmsEndpoint::build(EndpointConfig::active(address, session))?;
    let active_task = tokio::spawn(active_runtime.run());
    active.start().await?;

    let exchange = tokio::time::timeout(Duration::from_secs(10), async {
        wait_selected(&active).await?;
        let request = PrimaryMessage::new(
            Stream::new(3)?,
            Function::new(1),
            Some(SecsItem::Binary(vec![1, 2, 3])),
        );
        let respond = async {
            let incoming = incoming.recv_primary().await.ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "Primary stream ended")
            })?;
            let (message, token) = incoming.into_parts();
            let InboundToken::Reply(token) = token else {
                return Err::<(), ExampleError>(
                    io::Error::other("request has no reply capability").into(),
                );
            };
            println!(
                "received S{}F{}: {:?}",
                message.stream().get(),
                message.function().get(),
                message.body()
            );
            let receipt = passive
                .reply(token, Some(SecsItem::Binary(vec![4, 5, 6])))
                .await?;
            println!("reply committed on connection {}", receipt.generation());
            Ok::<(), ExampleError>(())
        };
        let (response, ()) = tokio::try_join!(
            async { active.request(request).await.map_err(ExampleError::from) },
            respond
        )?;
        if response.function() != Function::new(2)
            || response.body() != Some(&SecsItem::Binary(vec![4, 5, 6]))
        {
            return Err::<(), ExampleError>(io::Error::other("unexpected Secondary").into());
        }
        println!(
            "matched S{}F{} on generation {}",
            response.stream().get(),
            response.function().get(),
            response.context().generation().get()
        );
        Ok::<(), ExampleError>(())
    })
    .await;

    // Keep both runtimes alive until both Stop operations and task joins finish,
    // including when the exchange itself failed or its application timeout expired.
    let (active_stop, passive_stop) = tokio::join!(active.stop(), passive.stop());
    drop(active);
    drop(passive);
    let (active_exit, passive_exit) = tokio::join!(active_task, passive_task);
    active_stop?;
    passive_stop?;
    active_exit??;
    passive_exit??;
    exchange??;
    println!("both endpoints stopped and runtime tasks joined");
    Ok(())
}
