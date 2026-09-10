//! Creates connected TCP candidates for the endpoint Supervisor.
//! Passive mode suspends listening while a generation is occupied. Active preserves
//! T5 attempt spacing across cancelled waits and applies a separate connect bound.
//! This source never retries accepted protocol operations or launches generations.

use crate::hsms::{
    generation::transport::io::cancelled, ConfigError, ConnectionMode, EndpointConfig,
};
use std::{io, net::SocketAddr, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::watch,
    time::Instant,
};

/// Failure to prepare a source before endpoint startup can be acknowledged.
#[derive(Debug, thiserror::Error)]
pub(crate) enum SourceError {
    /// Invalid endpoint policy, rejected before binding a socket.
    #[error(transparent)]
    Config(#[from] ConfigError),
    /// Listener creation failed; Passive startup has not succeeded.
    #[error("cannot bind HSMS listener: {0}")]
    Bind(#[source] io::Error),
}

/// Failure of one connection attempt, classified independently of protocol work.
#[derive(Debug, thiserror::Error)]
pub(crate) enum AttemptError {
    /// A TCP connect or accept operation failed.
    #[error("HSMS connection attempt failed: {0}")]
    Transport(#[source] io::Error),
    /// Active connection establishment exceeded its configured absolute timeout.
    #[error("HSMS connection attempt timed out")]
    Timeout,
    /// A future monotonic deadline cannot be represented; do not retry this fault.
    #[error("HSMS connection deadline overflow")]
    DeadlineOverflow,
}

/// Stateful source of connected candidates, owned by one endpoint Supervisor.
pub(crate) enum ConnectionSource {
    /// Active connection intent and persistent retry timing.
    Active {
        /// Destination selected by endpoint configuration.
        address: SocketAddr,
        /// Minimum interval from one attempt's termination to the next start.
        t5: Duration,
        /// Independent upper bound for one connect attempt.
        connect: Duration,
        /// Earliest start of the next attempt, retained across future cancellation.
        next_attempt: Option<Instant>,
    },
    /// Stable Passive address, listened on only while awaiting a generation.
    Passive {
        /// Present while accepting; released before returning an accepted stream.
        listener: Option<TcpListener>,
        /// Actual initial bind address, including a resolved ephemeral port.
        address: SocketAddr,
    },
}

impl ConnectionSource {
    /// Validates policy and binds Passive before acknowledging startup.
    /// Active preparation performs no network operation; attempts occur in next.
    pub(crate) async fn prepare(config: &EndpointConfig) -> Result<Self, SourceError> {
        config.validate()?;
        match config.mode() {
            ConnectionMode::Active => Ok(Self::Active {
                address: config.address(),
                t5: config.timeouts().t5(),
                connect: config.timeouts().connect(),
                next_attempt: Some(Instant::now()),
            }),
            ConnectionMode::Passive => {
                let listener = TcpListener::bind(config.address())
                    .await
                    .map_err(SourceError::Bind)?;
                let address = listener.local_addr().map_err(SourceError::Bind)?;
                Ok(Self::Passive {
                    listener: Some(listener),
                    address,
                })
            }
        }
    }

    /// Returns the actual Passive bind address (including an assigned ephemeral
    /// port), or None for Active. Socket inspection failures remain explicit.
    pub(crate) fn local_address(&self) -> io::Result<Option<SocketAddr>> {
        match self {
            Self::Passive { address, .. } => Ok(Some(*address)),
            Self::Active { .. } => Ok(None),
        }
    }

    /// Guards the occupied source without accepting extra connections. A retained
    /// Passive listener is an invariant failure propagated through the existing
    /// Supervisor error/cleanup path; normal suspended sources wait indefinitely.
    pub(crate) async fn reject_extra(&self) -> io::Result<()> {
        if matches!(
            self,
            Self::Passive {
                listener: Some(_),
                ..
            }
        ) {
            return Err(io::Error::other(
                "Passive listener retained while generation is occupied",
            ));
        }
        std::future::pending().await
    }

    /// Waits for one candidate or one attempt failure; cancellation yields None.
    /// The Supervisor decides whether cleanup/recovery permits calling again.
    /// T5 begins when an attempted connect terminates, including cancellation.
    /// Cancelling a retry wait does not change its existing deadline.
    pub(crate) async fn next(
        &mut self,
        cancellation: &mut watch::Receiver<bool>,
    ) -> Result<Option<TcpStream>, AttemptError> {
        match self {
            Self::Passive { listener, address } => {
                if *cancellation.borrow() {
                    return Ok(None);
                }
                if listener.is_none() {
                    *listener = Some(
                        TcpListener::bind(*address)
                            .await
                            .map_err(AttemptError::Transport)?,
                    );
                }
                let accepted = tokio::select! {
                    biased;
                    () = cancelled(cancellation) => return Ok(None),
                    accepted = listener.as_ref().expect("listener installed").accept() => accepted.map_err(AttemptError::Transport)?,
                };
                // No listening socket or application-owned extra candidate remains
                // while the Supervisor owns this connection (E37 9.2.4.1(c)).
                listener.take();
                Ok(Some(accepted.0))
            }
            Self::Active {
                address,
                t5,
                connect,
                next_attempt,
            } => {
                let ready_at = next_attempt.ok_or(AttemptError::DeadlineOverflow)?;
                tokio::select! {
                    biased;
                    () = cancelled(cancellation) => return Ok(None),
                    () = tokio::time::sleep_until(ready_at) => {},
                }
                let now = Instant::now();
                let deadline = now
                    .checked_add(*connect)
                    .ok_or(AttemptError::DeadlineOverflow)?;
                connect_attempt(
                    next_attempt,
                    *t5,
                    deadline,
                    cancellation,
                    TcpStream::connect(*address),
                )
                .await
            }
        }
    }
}

/// Runs one cancellable attempt and commits its separation deadline on every exit.
async fn connect_attempt<T>(
    next_attempt: &mut Option<Instant>,
    t5: Duration,
    deadline: Instant,
    cancellation: &mut watch::Receiver<bool>,
    operation: impl std::future::Future<Output = io::Result<T>>,
) -> Result<Option<T>, AttemptError> {
    let _separation = ConnectSeparation { next_attempt, t5 };
    tokio::select! {
        biased;
        () = cancelled(cancellation) => Ok(None),
        () = tokio::time::sleep_until(deadline) => Err(AttemptError::Timeout),
        connected = operation => connected.map(Some).map_err(AttemptError::Transport),
    }
}

/// Records the next T5 boundary even when an in-progress connect future is dropped.
struct ConnectSeparation<'a> {
    /// Persistent next-attempt deadline; None latches monotonic-clock overflow.
    next_attempt: &'a mut Option<Instant>,
    /// Required quiet interval after this attempt terminates.
    t5: Duration,
}

impl Drop for ConnectSeparation<'_> {
    /// Commits end-to-start spacing for success, error, timeout or cancellation.
    fn drop(&mut self) {
        *self.next_attempt = Instant::now().checked_add(self.t5);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hsms::SessionId;

    /// Passive startup exposes the real bound port and cancellation releases waits.
    #[tokio::test]
    async fn passive_bind_accept_cancel_and_release() {
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap());
        let mut source = ConnectionSource::prepare(&config).await.unwrap();
        let address = source.local_address().unwrap().unwrap();
        assert_ne!(address.port(), 0);
        let (cancel, mut signal) = watch::channel(false);
        let peer = TcpStream::connect(address).await.unwrap();
        let candidate = source.next(&mut signal).await.unwrap().unwrap();
        assert_eq!(candidate.peer_addr().unwrap(), peer.local_addr().unwrap());
        cancel.send_replace(true);
        assert!(source.next(&mut signal).await.unwrap().is_none());
        drop(candidate);
        drop(peer);
        drop(source);
        let _rebound = TcpListener::bind(address).await.unwrap();
    }

    /// Active retry waits retain T5 across dropped futures and explicit cancellation.
    #[tokio::test(start_paused = true)]
    async fn active_attempt_spacing_survives_cancelled_wait() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config =
            EndpointConfig::active(listener.local_addr().unwrap(), SessionId::new(7).unwrap());
        let mut source = ConnectionSource::prepare(&config).await.unwrap();
        let (cancel, mut signal) = watch::channel(false);
        let first = source.next(&mut signal).await.unwrap().unwrap();
        let expected = match &source {
            ConnectionSource::Active { next_attempt, .. } => *next_attempt,
            _ => unreachable!(),
        };
        assert!(
            tokio::time::timeout(Duration::from_secs(1), source.next(&mut signal))
                .await
                .is_err()
        );
        cancel.send_replace(true);
        assert!(source.next(&mut signal).await.unwrap().is_none());
        assert!(
            matches!(&source, ConnectionSource::Active { next_attempt, .. } if *next_attempt == expected)
        );
        let (_cancel, mut resumed) = watch::channel(false);
        let second = source.next(&mut resumed).await.unwrap().unwrap();
        assert!(Instant::now() >= expected.unwrap());
        assert_ne!(first.local_addr().unwrap(), second.local_addr().unwrap());
    }

    /// A nonzero attempt duration is added to T5 after success, failure or timeout.
    #[tokio::test(start_paused = true)]
    async fn t5_begins_after_attempt_termination() {
        for outcome in 0..3 {
            let start = Instant::now();
            let mut next = Some(start);
            let (_cancel, mut signal) = watch::channel(false);
            let operation = async {
                tokio::time::sleep(Duration::from_secs(3)).await;
                if outcome == 1 {
                    Err(io::Error::other("connection refused"))
                } else {
                    Ok(())
                }
            };
            let timeout = if outcome == 2 { 2 } else { 10 };
            let result = connect_attempt(
                &mut next,
                Duration::from_secs(5),
                start + Duration::from_secs(timeout),
                &mut signal,
                operation,
            )
            .await;
            match outcome {
                0 => assert!(matches!(result, Ok(Some(())))),
                1 => assert!(matches!(result, Err(AttemptError::Transport(_)))),
                _ => assert!(matches!(result, Err(AttemptError::Timeout))),
            }
            let elapsed = if outcome == 2 { 2 } else { 3 };
            assert_eq!(next, Some(start + Duration::from_secs(elapsed + 5)));
        }
    }

    /// Dropping an in-progress attempt starts T5 at the cancellation instant.
    #[tokio::test(start_paused = true)]
    async fn cancelled_connect_attempt_starts_a_full_t5_interval() {
        use std::{future::Future, task::Poll};
        let start = Instant::now();
        let mut next = Some(start);
        let (_cancel, mut signal) = watch::channel(false);
        let mut attempt = Box::pin(connect_attempt(
            &mut next,
            Duration::from_secs(5),
            start + Duration::from_secs(30),
            &mut signal,
            std::future::pending::<io::Result<()>>(),
        ));
        std::future::poll_fn(|cx| {
            assert!(attempt.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        tokio::time::advance(Duration::from_secs(3)).await;
        drop(attempt);
        assert_eq!(next, Some(start + Duration::from_secs(8)));
    }

    /// Invalid configuration cannot bind a listener or acknowledge startup.
    #[tokio::test]
    async fn invalid_policy_is_rejected_before_passive_bind() {
        let config =
            EndpointConfig::passive("127.0.0.1:0".parse().unwrap(), SessionId::new(7).unwrap())
                .with_runtime(crate::hsms::RuntimePolicy::default().with_byte_budgets(0, 0, 0));
        assert!(matches!(
            ConnectionSource::prepare(&config).await,
            Err(SourceError::Config(_))
        ));
    }
}
