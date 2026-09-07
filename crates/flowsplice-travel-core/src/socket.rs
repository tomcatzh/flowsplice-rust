//! Demand-driven application I/O over the existing encrypted Flow implementation.

use std::{
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{Result, anyhow, bail};
use flowsplice_core::protocol::{Catalog, ServiceProtocol};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf},
    sync::{Mutex, oneshot, watch},
    task::JoinSet,
};

use super::{FlowGuard, Mapping, TravelCore, tcp_flow};

/// An approved business destination with no local bind address.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceBinding {
    pub home_id: String,
    pub service_id: String,
    pub protocol: ServiceProtocol,
}

impl ServiceBinding {
    fn mapping(&self) -> Mapping {
        Mapping {
            home_id: self.home_id.clone(),
            service_id: self.service_id.clone(),
            protocol: self.protocol,
            bind: String::new(),
        }
    }

    fn check_catalog(&self, catalog: &Catalog) -> Result<()> {
        // Startup may still be discovering the Home. Once its signed catalog is
        // available, a missing service is an admission error, not a Carrier outage.
        if let Some(home) = catalog
            .homes
            .iter()
            .find(|home| home.home_id == self.home_id)
            && !home
                .services
                .iter()
                .any(|service| service.id == self.service_id && service.protocol == self.protocol)
        {
            bail!("service is unavailable in the authenticated Home catalog");
        }
        Ok(())
    }
}

pub(super) struct SocketTasks {
    pub(super) tasks: Mutex<JoinSet<()>>,
    pub(super) shutdown: watch::Sender<bool>,
}

impl Default for SocketTasks {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(JoinSet::new()),
            shutdown: watch::channel(false).0,
        }
    }
}

impl SocketTasks {
    pub(super) async fn shutdown(&self) {
        self.shutdown.send_replace(true);
        let mut tasks = self.tasks.lock().await;
        while tasks.join_next().await.is_some() {}
    }
}

impl Drop for SocketTasks {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
    }
}

/// A bounded in-process byte stream. Dropping it cancels its Flow, not a remote session.
pub struct SocketStream {
    io: DuplexStream,
    stop: watch::Sender<bool>,
    failure: watch::Receiver<Option<String>>,
}

impl SocketStream {
    fn check_failure(&self) -> io::Result<()> {
        if let Some(reason) = self.failure.borrow().as_ref() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                reason.clone(),
            ));
        }
        Ok(())
    }
}

impl Drop for SocketStream {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

impl AsyncRead for SocketStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let polled = Pin::new(&mut self.io).poll_read(cx, buf);
        if matches!(polled, Poll::Ready(Ok(()))) && buf.filled().len() == before {
            self.check_failure()?;
        }
        polled
    }
}

impl AsyncWrite for SocketStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.check_failure()?;
        Pin::new(&mut self.io).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_failure()?;
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.check_failure()?;
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

impl TravelCore {
    /// Connect an in-process byte stream using the same authenticated, recoverable Flow as old mappings.
    ///
    /// Returns only after Home accepts the service and a Carrier wins selection.
    /// No local listening socket is created. Cancellation and dropping the stream end this Flow.
    ///
    /// # Errors
    /// Returns invalid binding, stopped runtime, capacity, trust, route or connection errors.
    pub async fn connect_tcp(&self, binding: &ServiceBinding) -> Result<SocketStream> {
        let lifecycle = self.shutdown_lock.lock().await;
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            bail!("Travel runtime is stopped");
        }
        if binding.protocol != ServiceProtocol::Tcp
            || binding.service_id.is_empty()
            || !self
                .state
                .config
                .homes
                .iter()
                .any(|home| home.id == binding.home_id)
        {
            bail!("invalid TCP business binding");
        }
        binding.check_catalog(&*self.state.catalog.read().await)?;
        let permit = Arc::clone(&self.state.permits)
            .try_acquire_owned()
            .map_err(|_| anyhow!("travel active-flow limit reached"))?;
        let (io, engine) = tokio::io::duplex(65_536);
        let (stop, shutdown) = watch::channel(false);
        let (failure_tx, failure) = watch::channel(None);
        let (ready_tx, ready) = oneshot::channel();
        let stream = SocketStream {
            io,
            stop: stop.clone(),
            failure,
        };
        let state = self.state.clone();
        let mapping = binding.mapping();
        let mut runtime_shutdown = self.socket_tasks.shutdown.subscribe();
        let mut tasks = self.socket_tasks.tasks.lock().await;
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let _permit = permit;
            let _guard = FlowGuard::new(
                Arc::clone(&state.active_flows),
                state.status_generation.clone(),
            );
            let flow = tcp_flow::run_io(
                state,
                mapping,
                Box::new(engine),
                shutdown,
                Some(ready_tx),
                Some(failure_tx),
            );
            tokio::pin!(flow);
            tokio::select! {
                result = &mut flow => { let _ = result; }
                _ = runtime_shutdown.changed() => {
                    stop.send_replace(true);
                    let _ = flow.await;
                }
            }
        });
        drop(tasks);
        drop(lifecycle);
        ready
            .await
            .map_err(|_| anyhow!("connection task stopped"))?
            .map_err(anyhow::Error::msg)?;
        Ok(stream)
    }
}

/// Bounded in-process datagrams. Dropping this handle cancels only its Flow.
pub struct SocketDatagrams {
    endpoint: Arc<dyn flowsplice_transport::DatagramIo>,
    stop: watch::Sender<bool>,
    failure: watch::Receiver<Option<String>>,
}

impl SocketDatagrams {
    fn check_failure(&self) -> Result<()> {
        if let Some(reason) = self.failure.borrow().as_ref() {
            bail!("{reason}");
        }
        Ok(())
    }

    /// Sends one message with backpressure, preserving empty datagrams.
    ///
    /// # Errors
    /// Returns a terminal Flow error, closed peer, or oversized datagram error.
    pub async fn send(&self, bytes: &[u8]) -> Result<()> {
        self.check_failure()?;
        let result = self.endpoint.send(bytes).await;
        if result.is_err() {
            self.check_failure()?;
        }
        result
    }

    /// Receives one message. Cancelling this future does not consume a datagram.
    ///
    /// # Errors
    /// Returns the terminal Flow error or a closed-peer error after queued data drains.
    pub async fn recv(&self) -> Result<Vec<u8>> {
        let result = self.endpoint.recv().await;
        if result.is_err() {
            self.check_failure()?;
        }
        result
    }
}

impl Drop for SocketDatagrams {
    fn drop(&mut self) {
        self.stop.send_replace(true);
    }
}

impl TravelCore {
    /// Connects application datagrams over the existing authenticated UDP Flow.
    /// No physical socket or listener is opened for the application endpoint.
    ///
    /// # Errors
    /// Returns invalid binding, stopped runtime, capacity, trust, route or connection errors.
    pub async fn connect_udp(&self, binding: &ServiceBinding) -> Result<SocketDatagrams> {
        let lifecycle = self.shutdown_lock.lock().await;
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            bail!("Travel runtime is stopped");
        }
        if binding.protocol != ServiceProtocol::Udp
            || binding.service_id.is_empty()
            || !self
                .state
                .config
                .homes
                .iter()
                .any(|home| home.id == binding.home_id)
        {
            bail!("invalid UDP business binding");
        }
        binding.check_catalog(&*self.state.catalog.read().await)?;
        let permit = Arc::clone(&self.state.permits)
            .try_acquire_owned()
            .map_err(|_| anyhow!("travel active-flow limit reached"))?;
        let (endpoint, engine) = flowsplice_transport::datagram_pair(64)?;
        let (stop, shutdown) = watch::channel(false);
        let (failure_tx, failure) = watch::channel(None);
        let (ready_tx, ready) = oneshot::channel();
        let datagrams = SocketDatagrams {
            endpoint,
            stop: stop.clone(),
            failure,
        };
        let state = self.state.clone();
        let mapping = binding.mapping();
        let mut runtime_shutdown = self.socket_tasks.shutdown.subscribe();
        let mut tasks = self.socket_tasks.tasks.lock().await;
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let _permit = permit;
            let _guard = FlowGuard::new(
                Arc::clone(&state.active_flows),
                state.status_generation.clone(),
            );
            let flow = super::run_udp_association(
                &state,
                &mapping,
                engine,
                shutdown,
                Some(ready_tx),
                Some(failure_tx),
            );
            tokio::pin!(flow);
            tokio::select! {
                result = &mut flow => { let _ = result; }
                _ = runtime_shutdown.changed() => {
                    stop.send_replace(true);
                    let _ = flow.await;
                }
            }
        });
        drop(tasks);
        drop(lifecycle);
        ready
            .await
            .map_err(|_| anyhow!("UDP connection task stopped"))?
            .map_err(anyhow::Error::msg)?;
        Ok(datagrams)
    }
}

#[cfg(test)]
mod datagram_tests {
    use super::*;

    #[test]
    fn embedded_and_legacy_flow_statistics_keep_a_valid_wire_shape() -> Result<()> {
        use flowsplice_core::{
            protocol::Role,
            statistics::{
                MetricValue, STATISTICS_METRIC_VERSION, STATISTICS_REPORT_VERSION,
                StatisticsReportPayload,
            },
        };
        for protocol in [ServiceProtocol::Tcp, ServiceProtocol::Udp] {
            for bind in ["", "127.0.0.1:10080"] {
                let mapping = Mapping {
                    home_id: "home".to_owned(),
                    service_id: "echo".to_owned(),
                    protocol,
                    bind: bind.to_owned(),
                };
                let dimensions = super::super::travel_flow_metric_dimensions(&mapping);
                assert_eq!(dimensions.contains_key("mapping"), !bind.is_empty());
                StatisticsReportPayload {
                    version: STATISTICS_REPORT_VERSION,
                    deployment_id: "test".to_owned(),
                    reporter_role: Role::Travel,
                    reporter_id: "travel".to_owned(),
                    bucket_start_unix_secs: 0,
                    bucket_end_unix_secs: 300,
                    metric_family: "flow_started".to_owned(),
                    dimensions,
                    report_sequence: 1,
                    value: MetricValue {
                        version: STATISTICS_METRIC_VERSION,
                        revision: 1,
                        ..MetricValue::default()
                    },
                }
                .validate()?;
            }
        }
        Ok(())
    }

    #[tokio::test]
    async fn datagrams_preserve_empty_payload_and_drop_signals_stop() -> Result<()> {
        let (endpoint, engine) = flowsplice_transport::datagram_pair(2)?;
        let (stop, mut stopped) = watch::channel(false);
        let (_failure_tx, failure) = watch::channel(None);
        let io = SocketDatagrams {
            endpoint,
            stop,
            failure,
        };
        io.send(b"").await?;
        assert!(engine.recv().await?.is_empty());
        engine.send(b"reply").await?;
        assert_eq!(io.recv().await?, b"reply");
        drop(io);
        stopped.changed().await?;
        assert!(*stopped.borrow());
        Ok(())
    }

    #[tokio::test]
    async fn failure_is_reported_after_queued_datagrams_drain() -> Result<()> {
        let (endpoint, engine) = flowsplice_transport::datagram_pair(2)?;
        let (stop, _) = watch::channel(false);
        let (failure_tx, failure) = watch::channel(None);
        let io = SocketDatagrams {
            endpoint,
            stop,
            failure,
        };
        engine.send(b"queued").await?;
        failure_tx.send_replace(Some("carrier failed".to_owned()));
        drop(engine);
        assert_eq!(io.recv().await?, b"queued");
        assert_eq!(
            io.recv().await.err().map(|e| e.to_string()).as_deref(),
            Some("carrier failed")
        );
        assert_eq!(
            io.send(b"x").await.err().map(|e| e.to_string()).as_deref(),
            Some("carrier failed")
        );
        Ok(())
    }
    #[test]
    fn authenticated_catalog_admission_matches_home_service_and_protocol() -> Result<()> {
        use flowsplice_core::protocol::{Catalog, HomeCatalog, Service};
        let catalog = Catalog {
            generation: 3,
            homes: vec![HomeCatalog {
                home_id: "home-a".to_owned(),
                home_alias: "A".to_owned(),
                endpoint_credential: None,
                services: vec![
                    Service {
                        id: "tcp-only".to_owned(),
                        alias: "TCP".to_owned(),
                        protocol: ServiceProtocol::Tcp,
                        target: "in-process".to_owned(),
                    },
                    Service {
                        id: "udp-only".to_owned(),
                        alias: "UDP".to_owned(),
                        protocol: ServiceProtocol::Udp,
                        target: "in-process".to_owned(),
                    },
                ],
            }],
        };
        let binding = ServiceBinding {
            home_id: "home-a".to_owned(),
            service_id: "tcp-only".to_owned(),
            protocol: ServiceProtocol::Tcp,
        };
        binding.check_catalog(&catalog)?;
        ServiceBinding {
            service_id: "udp-only".to_owned(),
            protocol: ServiceProtocol::Udp,
            ..binding.clone()
        }
        .check_catalog(&catalog)?;
        assert!(
            ServiceBinding {
                service_id: "missing".to_owned(),
                ..binding.clone()
            }
            .check_catalog(&catalog)
            .is_err()
        );
        assert!(
            ServiceBinding {
                protocol: ServiceProtocol::Udp,
                ..binding.clone()
            }
            .check_catalog(&catalog)
            .is_err()
        );
        assert!(
            ServiceBinding {
                service_id: "udp-only".to_owned(),
                ..binding.clone()
            }
            .check_catalog(&catalog)
            .is_err()
        );
        ServiceBinding {
            home_id: "undiscovered-home".to_owned(),
            ..binding.clone()
        }
        .check_catalog(&catalog)?;
        binding.check_catalog(&Catalog::default())?;
        let mut empty_home = catalog;
        empty_home.homes[0].services.clear();
        assert!(binding.check_catalog(&empty_home).is_err());
        Ok(())
    }
}
