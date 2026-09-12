//! In-process I/O contracts shared by encrypted transport and business services.
//! This crate neither opens physical sockets nor defines transport wire formats.

use std::{future::Future, pin::Pin, sync::Arc};

use anyhow::{Result, bail};
use flowsplice_core::{authorization::TravelCredential, protocol::Service};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{Mutex, mpsc},
};
use uuid::Uuid;

/// An owned, asynchronous, bidirectional byte stream.
pub trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + ?Sized> AsyncStream for T {}

/// A business stream independent of its transport implementation.
pub type BoxStream = Box<dyn AsyncStream>;

/// A fallible asynchronous I/O operation borrowing its provider.
pub type IoFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Authenticated peer context supplied by the transport after authorization.
#[derive(Clone, Debug)]
pub struct ServicePeer {
    /// The authenticated Travel credential.
    pub credential: TravelCredential,
    /// The authenticated Travel identifier.
    pub travel_id: String,
    /// Identifier of this transport flow.
    pub flow_id: Uuid,
    /// Ends with authorization, expiry or the owning transport flow. Buffered business
    /// operations must check this lease before performing their side effects.
    pub lifetime: ServiceLifetime,
}

/// Transport-owned admission lifetime for in-process application work.
#[derive(Clone, Debug)]
pub struct ServiceLifetime {
    active: tokio::sync::watch::Receiver<bool>,
    expires_at: u64,
}

/// Retained by the transport flow; dropping it invalidates all application leases.
pub struct ServiceLifetimeGuard(tokio::sync::watch::Sender<bool>);

impl Drop for ServiceLifetimeGuard {
    fn drop(&mut self) {
        self.0.send_replace(false);
    }
}

impl ServiceLifetime {
    /// Creates a lease and its transport owner. The application receives only the lease.
    #[must_use]
    pub fn new(expires_at: u64) -> (Self, ServiceLifetimeGuard) {
        let (sender, active) = tokio::sync::watch::channel(true);
        (Self { active, expires_at }, ServiceLifetimeGuard(sender))
    }

    /// Returns false after transport closure, authorization cancellation or expiry.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.has_changed().is_ok()
            && *self.active.borrow()
            && flowsplice_core::authorization::unix_time_secs()
                .is_ok_and(|now| now < self.expires_at)
    }

    /// Waits for the transport owner to end the lease or its absolute expiry.
    pub async fn ended(&self) {
        let mut active = self.active.clone();
        while self.is_active() {
            let remaining = self.expires_at.saturating_sub(
                flowsplice_core::authorization::unix_time_secs().unwrap_or(u64::MAX),
            );
            tokio::select! {
                _ = active.changed() => {},
                () = tokio::time::sleep(std::time::Duration::from_secs(remaining.min(60))) => {},
            }
        }
    }
}

/// Message-preserving bidirectional datagram I/O.
pub trait DatagramIo: Send + Sync {
    /// Sends exactly one datagram, including an empty datagram.
    fn send<'a>(&'a self, bytes: &'a [u8]) -> IoFuture<'a, ()>;

    /// Receives one datagram. Cancelling the future must not consume a message.
    fn recv(&self) -> IoFuture<'_, Vec<u8>>;
}

/// Opens business I/O for an already authenticated and authorized service flow.
pub trait ServiceProvider: Send + Sync {
    /// Opens a byte stream for the selected service.
    fn connect_tcp<'a>(
        &'a self,
        service: &'a Service,
        peer: ServicePeer,
    ) -> IoFuture<'a, BoxStream>;

    /// Opens message-preserving I/O for the selected service.
    fn connect_udp<'a>(
        &'a self,
        service: &'a Service,
        peer: ServicePeer,
    ) -> IoFuture<'a, Arc<dyn DatagramIo>>;
}

struct ChannelDatagrams {
    sender: mpsc::Sender<Vec<u8>>,
    receiver: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl DatagramIo for ChannelDatagrams {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> IoFuture<'a, ()> {
        Box::pin(async move {
            if bytes.len() > 65_507 {
                bail!("datagram exceeds 65507 bytes");
            }
            self.sender
                .send(bytes.to_vec())
                .await
                .map_err(|_| anyhow::anyhow!("datagram peer closed"))
        })
    }

    fn recv(&self) -> IoFuture<'_, Vec<u8>> {
        Box::pin(async move {
            self.receiver
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("datagram peer closed"))
        })
    }
}

/// Creates connected in-process endpoints with `capacity` messages per direction.
///
/// Sending waits for capacity and rejects payloads larger than 65507 bytes.
/// Receivers drain queued messages before reporting a closed peer.
///
/// # Errors
/// Returns an error if capacity is zero or exceeds Tokio's semaphore limit.
pub fn datagram_pair(capacity: usize) -> Result<(Arc<dyn DatagramIo>, Arc<dyn DatagramIo>)> {
    if capacity == 0 || capacity > tokio::sync::Semaphore::MAX_PERMITS {
        bail!("datagram capacity is outside the supported range");
    }
    let (left_sender, right_receiver) = mpsc::channel(capacity);
    let (right_sender, left_receiver) = mpsc::channel(capacity);
    Ok((
        Arc::new(ChannelDatagrams {
            sender: left_sender,
            receiver: Mutex::new(left_receiver),
        }),
        Arc::new(ChannelDatagrams {
            sender: right_sender,
            receiver: Mutex::new(right_receiver),
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::datagram_pair;

    #[tokio::test]
    async fn preserves_boundaries_and_empty_messages() -> anyhow::Result<()> {
        let (left, right) = datagram_pair(3)?;
        left.send(b"first").await?;
        left.send(b"").await?;
        left.send(b"last").await?;
        assert_eq!(right.recv().await?, b"first");
        assert!(right.recv().await?.is_empty());
        assert_eq!(right.recv().await?, b"last");
        right.send(b"reply").await?;
        assert_eq!(left.recv().await?, b"reply");
        Ok(())
    }

    #[tokio::test]
    async fn rejects_oversized_messages_without_consuming_capacity() -> anyhow::Result<()> {
        let (left, right) = datagram_pair(1)?;
        assert!(left.send(&vec![0; 65_508]).await.is_err());
        left.send(&vec![1; 65_507]).await?;
        assert_eq!(right.recv().await?, vec![1; 65_507]);
        Ok(())
    }

    #[tokio::test]
    async fn closed_peer_drains_then_errors() -> anyhow::Result<()> {
        let (left, right) = datagram_pair(1)?;
        left.send(b"queued").await?;
        drop(left);
        assert_eq!(right.recv().await?, b"queued");
        assert!(right.recv().await.is_err());
        assert!(right.send(b"closed").await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn capacity_applies_backpressure() -> anyhow::Result<()> {
        let (left, right) = datagram_pair(1)?;
        left.send(b"first").await?;
        let mut next = left.send(b"second");
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(next.as_mut().poll(cx).is_pending())
            })
            .await
        );
        assert_eq!(right.recv().await?, b"first");
        next.await?;
        assert_eq!(right.recv().await?, b"second");
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_receive_leaves_next_message_available() -> anyhow::Result<()> {
        let (left, right) = datagram_pair(1)?;
        let mut receive = right.recv();
        assert!(
            std::future::poll_fn(|cx| {
                std::task::Poll::Ready(receive.as_mut().poll(cx).is_pending())
            })
            .await
        );
        left.send(b"retained").await?;
        drop(receive);
        assert_eq!(right.recv().await?, b"retained");
        Ok(())
    }

    #[test]
    fn invalid_capacity_is_an_error() {
        assert!(datagram_pair(0).is_err());
        assert!(datagram_pair(usize::MAX).is_err());
    }
}

mod services;
pub use services::{HomeTcpListener, HomeUdpListener, SocketServices};

#[cfg(test)]
mod lifetime_tests {
    use super::ServiceLifetime;
    use std::time::Duration;
    #[tokio::test]
    async fn transport_drop_invalidates_all_leases_and_wakes_blocked_work() -> anyhow::Result<()> {
        let (lease, guard) = ServiceLifetime::new(u64::MAX);
        let copy = lease.clone();
        assert!(lease.is_active());
        let mut waiting = Box::pin(copy.ended());
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut waiting)
                .await
                .is_err()
        );
        drop(guard);
        assert!(!lease.is_active());
        tokio::time::timeout(Duration::from_secs(1), waiting).await?;
        Ok(())
    }
    #[tokio::test]
    async fn expired_lease_rejects_work_even_while_transport_owner_is_alive() -> anyhow::Result<()>
    {
        let now = flowsplice_core::authorization::unix_time_secs()?;
        let (lease, _guard) = ServiceLifetime::new(now);
        assert!(!lease.is_active());
        tokio::time::timeout(Duration::from_millis(50), lease.ended()).await?;
        Ok(())
    }
}
