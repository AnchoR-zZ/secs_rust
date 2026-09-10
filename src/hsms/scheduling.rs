//! Persistent round-robin selection for endpoint commands and connection work.
//! Selection state changes only when an input is delivered; cancelled waits keep
//! their previous preference and pending sources retain their normal ownership.

use std::{future::Future, task::Poll};

/// The single completed input chosen for this scheduling turn.
pub(crate) enum Selected<L, R> {
    /// The left source completed.
    Left(L),
    /// The right source completed.
    Right(R),
}

/// Waits for one input, alternating ties via `prefer_left`. Each future must be
/// cancellation-safe. A ready source always progresses if its peer is pending.
pub(crate) async fn alternate<L: Future, R: Future>(
    prefer_left: &mut bool,
    left: L,
    right: R,
) -> Selected<L::Output, R::Output> {
    let mut left = std::pin::pin!(left);
    let mut right = std::pin::pin!(right);
    std::future::poll_fn(|cx| {
        if *prefer_left {
            if let Poll::Ready(value) = left.as_mut().poll(cx) {
                *prefer_left = false;
                return Poll::Ready(Selected::Left(value));
            }
        }
        if let Poll::Ready(value) = right.as_mut().poll(cx) {
            *prefer_left = true;
            return Poll::Ready(Selected::Right(value));
        }
        if !*prefer_left {
            if let Poll::Ready(value) = left.as_mut().poll(cx) {
                *prefer_left = false;
                return Poll::Ready(Selected::Left(value));
            }
        }
        Poll::Pending
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cancelling an empty wait preserves preference and does not consume input.
    #[tokio::test]
    async fn cancellation_and_busy_sources_preserve_fairness() {
        let (left_tx, mut left) = tokio::sync::mpsc::channel(2);
        let (right_tx, mut right) = tokio::sync::mpsc::channel(2);
        let mut prefer_left = false;
        {
            let mut wait = Box::pin(alternate(&mut prefer_left, left.recv(), right.recv()));
            std::future::poll_fn(|cx| {
                assert!(wait.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        assert!(!prefer_left);
        for value in 0..8 {
            left_tx.send(value).await.unwrap();
            right_tx.send(value).await.unwrap();
            assert!(
                matches!(alternate(&mut prefer_left, left.recv(), right.recv()).await, Selected::Right(Some(v)) if v == value)
            );
            assert!(
                matches!(alternate(&mut prefer_left, left.recv(), right.recv()).await, Selected::Left(Some(v)) if v == value)
            );
        }
        left_tx.send(9).await.unwrap();
        assert!(matches!(
            alternate(&mut prefer_left, left.recv(), right.recv()).await,
            Selected::Left(Some(9))
        ));
    }
}
