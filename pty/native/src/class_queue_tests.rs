use super::{Forwarded, forward_home_action};
use anyhow::Result;
use flowsplice_pty_protocol::Operation;
use std::{future::Future, pin::Pin, task::Poll, time::Duration};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

fn input(data: u8) -> Operation {
    Operation::Input {
        attachment_id: Uuid::from_u128(1),
        writer_epoch: 7,
        data: vec![data],
    }
}

// Poll admission before changing capacity or signalling cancellation. This proves
// the operation really reached the blocked queue, without scheduler sleeps.
async fn assert_pending<T>(mut future: Pin<&mut impl Future<Output = T>>) {
    std::future::poll_fn(|cx| {
        assert!(future.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn full_queue_waits_for_capacity_and_admits_each_input_once() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let (sender, mut receiver) = mpsc::channel(1);
        let (_cancel_sender, cancel) = watch::channel(false);
        let (_overflow_sender, overflow) = watch::channel(false);
        sender.send(input(b'A')).await?;
        let forwarding = forward_home_action(&sender, input(b'B'), cancel, overflow);
        tokio::pin!(forwarding);
        assert_pending(forwarding.as_mut()).await;
        let consume = async {
            assert_eq!(receiver.recv().await, Some(input(b'A')));
            assert_eq!(receiver.recv().await, Some(input(b'B')));
        };
        let (result, ()) = tokio::join!(forwarding, consume);
        assert_eq!(result, Forwarded::Sent);
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn full_queue_timeout_does_not_admit_pending_input() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let (sender, mut receiver) = mpsc::channel(1);
        let (_cancel_sender, cancel) = watch::channel(false);
        let (_overflow_sender, overflow) = watch::channel(false);
        sender.send(input(b'A')).await?;
        let forwarding = forward_home_action(&sender, input(b'B'), cancel, overflow);
        tokio::pin!(forwarding);
        assert_pending(forwarding.as_mut()).await;
        assert_eq!(forwarding.await, Forwarded::Unavailable);
        assert_eq!(receiver.recv().await, Some(input(b'A')));
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn closed_queue_returns_unavailable() -> Result<()> {
    let (sender, receiver) = mpsc::channel(1);
    let (_cancel_sender, cancel) = watch::channel(false);
    let (_overflow_sender, overflow) = watch::channel(false);
    drop(receiver);
    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            forward_home_action(&sender, input(b'B'), cancel, overflow),
        )
        .await?,
        Forwarded::Unavailable
    );
    Ok(())
}

async fn interrupted_admission(overflowed: bool) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(1), async {
        let (sender, mut receiver) = mpsc::channel(1);
        let (cancel_sender, cancel) = watch::channel(false);
        let (overflow_sender, overflow) = watch::channel(false);
        sender.send(input(b'A')).await?;
        let forwarding = forward_home_action(&sender, input(b'B'), cancel, overflow);
        tokio::pin!(forwarding);
        assert_pending(forwarding.as_mut()).await;
        if overflowed {
            overflow_sender.send_replace(true);
        } else {
            cancel_sender.send_replace(true);
        }
        // The next poll must resolve on the signal, not on the 250ms deadline.
        std::future::poll_fn(|cx| {
            assert_eq!(
                forwarding.as_mut().poll(cx),
                Poll::Ready(Forwarded::Interrupted)
            );
            Poll::Ready(())
        })
        .await;
        assert_eq!(receiver.recv().await, Some(input(b'A')));
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));
        Ok::<_, anyhow::Error>(())
    })
    .await?
}

#[tokio::test]
async fn cancellation_interrupts_blocked_admission_without_losing_admitted_input() -> Result<()> {
    interrupted_admission(false).await
}

#[tokio::test]
async fn output_overflow_interrupts_blocked_admission_without_losing_admitted_input() -> Result<()>
{
    interrupted_admission(true).await
}
