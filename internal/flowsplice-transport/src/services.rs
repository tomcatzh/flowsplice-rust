//! In-process business listeners for the authenticated Home transport.
use crate::{BoxStream, DatagramIo, IoFuture, ServicePeer, ServiceProvider, datagram_pair};
use anyhow::{Result, anyhow, bail};
use flowsplice_core::protocol::{Service, ServiceProtocol};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::mpsc;
use uuid::Uuid;

type TcpItem = (BoxStream, ServicePeer);
type UdpItem = (Arc<dyn DatagramIo>, ServicePeer);
#[derive(Clone)]
enum Sender {
    Tcp(mpsc::Sender<TcpItem>),
    Udp(mpsc::Sender<UdpItem>),
}
struct Entry {
    registration: Uuid,
    sender: Sender,
}
type Key = (String, ServiceProtocol);
/// Registered logical services backed exclusively by process-local channels.
#[derive(Default)]
pub struct SocketServices {
    entries: Mutex<HashMap<Key, Entry>>,
}
struct Registration {
    registry: Weak<SocketServices>,
    key: Key,
    id: Uuid,
}
impl Drop for Registration {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade()
            && let Ok(mut entries) = registry.entries.lock()
            && entries
                .get(&self.key)
                .is_some_and(|entry| entry.registration == self.id)
        {
            entries.remove(&self.key);
        }
    }
}
/// An in-process TCP-like service listener.
pub struct HomeTcpListener {
    receiver: mpsc::Receiver<TcpItem>,
    _registration: Registration,
}
/// An in-process UDP association listener.
pub struct HomeUdpListener {
    receiver: mpsc::Receiver<UdpItem>,
    _registration: Registration,
}
impl HomeTcpListener {
    /// Accept an authenticated stream and its verified peer metadata.
    ///
    /// # Errors
    /// Returns an error when the registration channel closes.
    pub async fn accept(&mut self) -> Result<TcpItem> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| anyhow!("TCP service listener closed"))
    }
}
impl HomeUdpListener {
    /// Accept an authenticated datagram association and its verified peer metadata.
    ///
    /// # Errors
    /// Returns an error when the registration channel closes.
    pub async fn accept(&mut self) -> Result<UdpItem> {
        self.receiver
            .recv()
            .await
            .ok_or_else(|| anyhow!("UDP service listener closed"))
    }
}
impl SocketServices {
    fn register(
        self: &Arc<Self>,
        key: Key,
        backlog: usize,
        sender: Sender,
    ) -> Result<Registration> {
        if key.0.is_empty() || backlog == 0 || backlog > tokio::sync::Semaphore::MAX_PERMITS {
            bail!("invalid service identifier or backlog");
        }
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| anyhow!("service registry lock poisoned"))?;
        if entries.contains_key(&key) {
            bail!("service already registered");
        }
        let id = Uuid::new_v4();
        entries.insert(
            key.clone(),
            Entry {
                registration: id,
                sender,
            },
        );
        Ok(Registration {
            registry: Arc::downgrade(self),
            key,
            id,
        })
    }
    /// Register an in-process byte-stream service with a bounded accept backlog.
    ///
    /// # Errors
    /// Rejects empty identifiers, invalid capacity, duplicate registrations or a poisoned registry.
    pub fn bind_tcp(
        self: &Arc<Self>,
        service_id: String,
        backlog: usize,
    ) -> Result<HomeTcpListener> {
        validate_backlog(backlog)?;
        let (sender, receiver) = mpsc::channel(backlog);
        let registration = self.register(
            (service_id, ServiceProtocol::Tcp),
            backlog,
            Sender::Tcp(sender),
        )?;
        Ok(HomeTcpListener {
            receiver,
            _registration: registration,
        })
    }
    /// Register an in-process datagram service with a bounded association backlog.
    ///
    /// # Errors
    /// Rejects empty identifiers, invalid capacity, duplicate registrations or a poisoned registry.
    pub fn bind_udp(
        self: &Arc<Self>,
        service_id: String,
        backlog: usize,
    ) -> Result<HomeUdpListener> {
        validate_backlog(backlog)?;
        let (sender, receiver) = mpsc::channel(backlog);
        let registration = self.register(
            (service_id, ServiceProtocol::Udp),
            backlog,
            Sender::Udp(sender),
        )?;
        Ok(HomeUdpListener {
            receiver,
            _registration: registration,
        })
    }
    fn sender(&self, service: &Service) -> Result<Sender> {
        self.entries
            .lock()
            .map_err(|_| anyhow!("service registry lock poisoned"))?
            .get(&(service.id.clone(), service.protocol))
            .map(|entry| entry.sender.clone())
            .ok_or_else(|| anyhow!("unknown or mismatched logical service"))
    }
}
fn validate_backlog(backlog: usize) -> Result<()> {
    if backlog == 0 || backlog > tokio::sync::Semaphore::MAX_PERMITS {
        bail!("invalid service backlog");
    }
    Ok(())
}
impl ServiceProvider for SocketServices {
    fn connect_tcp<'a>(
        &'a self,
        service: &'a Service,
        peer: ServicePeer,
    ) -> IoFuture<'a, BoxStream> {
        Box::pin(async move {
            let Sender::Tcp(sender) = self.sender(service)? else {
                bail!("mismatched TCP service");
            };
            let (engine, application) = tokio::io::duplex(65_536);
            sender
                .send((Box::new(application), peer))
                .await
                .map_err(|_| anyhow!("TCP service listener closed"))?;
            Ok(Box::new(engine) as BoxStream)
        })
    }
    fn connect_udp<'a>(
        &'a self,
        service: &'a Service,
        peer: ServicePeer,
    ) -> IoFuture<'a, Arc<dyn DatagramIo>> {
        Box::pin(async move {
            let Sender::Udp(sender) = self.sender(service)? else {
                bail!("mismatched UDP service");
            };
            let (engine, application) = datagram_pair(64)?;
            sender
                .send((application, peer))
                .await
                .map_err(|_| anyhow!("UDP service listener closed"))?;
            Ok(engine)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowsplice_core::authorization::{TravelCredential, TravelCredentialScope};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn peer() -> ServicePeer {
        ServicePeer {
            lifetime: crate::ServiceLifetime::new(u64::MAX).0,
            travel_id: "travel".into(),
            flow_id: Uuid::new_v4(),
            credential: TravelCredential {
                version: 1,
                object_type: "travel_credential".into(),
                deployment_id: "test".into(),
                deployment_trust_sha256: "trust".into(),
                credential_id: Uuid::new_v4(),
                authority_id: "issuer".into(),
                authority_epoch: 1,
                enrollment_request_id: Uuid::new_v4(),
                enrollment_nonce: "nonce".into(),
                enrollment_request_sha256: "request".into(),
                travel_id: "travel".into(),
                management_spki_sha256: "management".into(),
                business_spki_sha256: "business".into(),
                management_ca_sha256: "management-ca".into(),
                business_ca_sha256: "business-ca".into(),
                management_certificate_sha256: "management-cert".into(),
                business_certificate_sha256: "business-cert".into(),
                scope: TravelCredentialScope::Global,
                not_before_unix_secs: 1,
                not_after_unix_secs: u64::MAX,
            },
        }
    }
    fn service(protocol: ServiceProtocol) -> Service {
        Service {
            id: "terminal".into(),
            alias: String::new(),
            protocol,
            target: "never-resolve.invalid:1".into(),
        }
    }
    #[test]
    fn registration_validation_and_drop_rebind() -> Result<()> {
        let registry = Arc::new(SocketServices::default());
        assert!(registry.bind_tcp(String::new(), 1).is_err());
        assert!(registry.bind_tcp("terminal".into(), 0).is_err());
        assert!(registry.bind_udp("terminal".into(), usize::MAX).is_err());
        let listener = registry.bind_tcp("terminal".into(), 1)?;
        assert!(registry.bind_tcp("terminal".into(), 1).is_err());
        let _udp = registry.bind_udp("terminal".into(), 1)?;
        drop(listener);
        let replacement = registry.bind_tcp("terminal".into(), 1)?;
        let stale = Registration {
            registry: Arc::downgrade(&registry),
            key: ("terminal".into(), ServiceProtocol::Tcp),
            id: Uuid::new_v4(),
        };
        drop(stale);
        assert!(registry.bind_tcp("terminal".into(), 1).is_err());
        drop(replacement);
        Ok(())
    }
    #[tokio::test]
    async fn tcp_exchange_half_close_and_peer_context() -> Result<()> {
        let registry = Arc::new(SocketServices::default());
        let mut listener = registry.bind_tcp("terminal".into(), 1)?;
        let expected = peer();
        let mut engine = registry
            .connect_tcp(&service(ServiceProtocol::Tcp), expected.clone())
            .await?;
        let (mut application, received) = listener.accept().await?;
        assert_eq!(received.credential, expected.credential);
        assert_eq!(received.travel_id, expected.travel_id);
        assert_eq!(received.flow_id, expected.flow_id);
        engine.write_all(b"input").await?;
        engine.shutdown().await?;
        let mut input = Vec::new();
        application.read_to_end(&mut input).await?;
        assert_eq!(input, b"input");
        application.write_all(b"output after EOF").await?;
        application.shutdown().await?;
        let mut output = Vec::new();
        engine.read_to_end(&mut output).await?;
        assert_eq!(output, b"output after EOF");
        Ok(())
    }
    #[tokio::test]
    async fn backlog_waits_until_accept_and_drop_closes_queue() -> Result<()> {
        let registry = Arc::new(SocketServices::default());
        let mut listener = registry.bind_tcp("terminal".into(), 1)?;
        let tcp = service(ServiceProtocol::Tcp);
        let mut first = registry.connect_tcp(&tcp, peer()).await?;
        let mut second = registry.connect_tcp(&tcp, peer());
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );
        let accepted = listener.accept().await?;
        let mut second = tokio::time::timeout(Duration::from_secs(1), second).await??;
        drop(listener);
        assert_eq!(second.read(&mut [0]).await?, 0);
        drop(accepted);
        assert_eq!(first.read(&mut [0]).await?, 0);
        assert!(registry.connect_tcp(&tcp, peer()).await.is_err());
        assert!(registry.connect_udp(&tcp, peer()).await.is_err());
        Ok(())
    }
    #[tokio::test]
    async fn udp_associations_preserve_isolation_and_empty_datagrams() -> Result<()> {
        let registry = Arc::new(SocketServices::default());
        let mut listener = registry.bind_udp("terminal".into(), 2)?;
        let udp = service(ServiceProtocol::Udp);
        let expected = peer();
        let first = registry.connect_udp(&udp, expected.clone()).await?;
        let second = registry.connect_udp(&udp, peer()).await?;
        let (first_app, first_peer) = listener.accept().await?;
        let (second_app, _) = listener.accept().await?;
        assert_eq!(first_peer.credential, expected.credential);
        assert_eq!(first_peer.flow_id, expected.flow_id);
        first.send(b"").await?;
        second.send(b"second").await?;
        assert!(first_app.recv().await?.is_empty());
        assert_eq!(second_app.recv().await?, b"second");
        first_app.send(b"first reply").await?;
        second_app.send(b"").await?;
        assert_eq!(first.recv().await?, b"first reply");
        assert!(second.recv().await?.is_empty());
        assert!(registry.connect_tcp(&udp, peer()).await.is_err());
        Ok(())
    }
}
