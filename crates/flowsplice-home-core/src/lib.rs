#![forbid(unsafe_code)]
//! Shared Home transport runtime, independent of issuer and user-interface adapters.
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use flowsplice_core::{
    DATA_FRAME_LIMIT,
    authorization::{TravelCredential, VerifiedAuthorization, unix_time_secs},
    frame::{DataFrameCodec, DataFrameReader, JsonFrameReader, write_data_frame, write_json},
    protocol::{DataFrame, Role, Service, ServiceProtocol},
    route::{RouteSide, write_preface},
    tls::{client_connector, peer_identity, require_peer, server_acceptor, server_name},
};
use flowsplice_storage::{LocalStatistics, MetricBatch};
use flowsplice_transport::{ServicePeer, ServiceProvider};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::TcpStream,
    sync::{Semaphore, watch},
    time::{MissedTickBehavior, interval, timeout},
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector, client::TlsStream as ClientTlsStream,
    server::TlsStream as ServerTlsStream,
};
use uuid::Uuid;
mod lifetime;
mod tasks;
mod tcp_flow;
pub use tcp_flow::{IncomingCarrier, TcpFlowRegistry};

/// Transport-only configuration for one serving Home.
#[derive(Clone)]
pub struct HomeFlowConfig {
    pub id: String,
    pub services: Vec<Service>,
    /// Metadata loaded from the verified signed Home service grant, never raw advertisements.
    pub business_services: Vec<flowsplice_core::business::BusinessService>,
    pub handshake_timeout_secs: u64,
    pub udp_idle_secs: u64,
}

/// Management client and business server TLS material for a serving Home.
#[derive(Clone)]
pub struct HomeTls {
    management_connector: TlsConnector,
    business_acceptor: TlsAcceptor,
}
impl HomeTls {
    /// Load existing Home identities and trust without creating or changing certificates.
    ///
    /// # Errors
    /// Returns filesystem, certificate, private-key or TLS configuration errors.
    pub fn load(
        management_cert: &Path,
        management_key: &Path,
        management_ca: &Path,
        business_cert: &Path,
        business_key: &Path,
        business_ca: &Path,
    ) -> Result<Self> {
        Ok(Self {
            management_connector: client_connector(management_cert, management_key, management_ca)?,
            business_acceptor: server_acceptor(business_cert, business_key, business_ca)?,
        })
    }
    /// Clone the authenticated business TLS acceptor.
    #[must_use]
    pub fn business_acceptor(&self) -> TlsAcceptor {
        self.business_acceptor.clone()
    }
    /// Connect to the management Server and verify its role, identity and SPKI pins.
    ///
    /// # Errors
    /// Returns socket, handshake timeout, TLS or peer authorization errors.
    pub async fn connect_management(
        &self,
        addr: &str,
        name: &str,
        server_id: &str,
        pins: &[String],
        handshake_timeout: Duration,
    ) -> Result<ClientTlsStream<TcpStream>> {
        let socket = TcpStream::connect(addr).await?;
        let stream = timeout(
            handshake_timeout,
            self.management_connector
                .connect(server_name(name)?, socket),
        )
        .await
        .context("server TLS handshake timed out")??;
        let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
        require_peer(&identity, Role::Server, Some(server_id), pins)?;
        Ok(stream)
    }
}

/// Accept a Relay work assignment and dispatch an authorized encrypted service flow.
///
/// # Errors
/// Returns connection, TLS, framing, authorization, capacity or service errors.
#[allow(clippy::too_many_arguments)]
pub async fn run_work(
    config: Arc<HomeFlowConfig>,
    business_acceptor: TlsAcceptor,
    permits: Arc<Semaphore>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization_rx: watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    expected_credential_id: Uuid,
    work_id: Uuid,
    work_secret: Vec<u8>,
    relay_id: String,
    relay_data_addr: String,
    expires_at_unix_secs: u64,
) -> Result<()> {
    if unix_time_secs()? >= expires_at_unix_secs {
        bail!("direct Relay work expired before Home connection");
    }
    let socket = timeout(Duration::from_secs(config.handshake_timeout_secs), async {
        let mut socket = TcpStream::connect(&relay_data_addr).await?;
        write_preface(&mut socket, RouteSide::Home, work_id, &work_secret).await?;
        Ok::<_, anyhow::Error>(socket)
    })
    .await
    .with_context(|| format!("Relay {relay_id} direct work setup timed out"))??;
    let mut stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        business_acceptor.accept(socket),
    )
    .await
    .context("business TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Travel, None, &[])?;
    let credential =
        authorize_business_identity(&identity, &authorization_rx, expected_credential_id)?;
    let credential_id = credential.credential_id;
    let not_after_unix_secs = credential
        .not_after_unix_secs
        .min(identity.not_after_unix_secs);

    let open = JsonFrameReader::new(&mut stream, DATA_FRAME_LIMIT)
        .read_with_timeout::<DataFrame>(Duration::from_secs(config.handshake_timeout_secs))
        .await?;
    let DataFrame::Open {
        flow_id,
        carrier_id,
        service_id,
        protocol,
        data_protocol_version,
    } = open
    else {
        bail!("first business frame must be OPEN");
    };
    let data_codec = DataFrameCodec::negotiate(data_protocol_version);
    let service = config
        .services
        .iter()
        .find(|candidate| candidate.id == service_id && candidate.protocol == protocol)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown or mismatched service"))?;
    if !credential.allows_business_service(
        &config.id,
        &service.id,
        service.protocol,
        config
            .business_services
            .iter()
            .find(|approved| approved.service_id == service.id),
    ) {
        bail!("Travel credential is not authorized for this logical service");
    }

    match protocol {
        ServiceProtocol::Tcp => {
            tcp_flows
                .attach(
                    credential.clone(),
                    identity.id,
                    flow_id,
                    service,
                    IncomingCarrier {
                        carrier_id,
                        relay_id,
                        stream,
                        global_permit: None,
                        flow_permit: None,
                        data_codec,
                    },
                    not_after_unix_secs,
                )
                .await
        }
        ServiceProtocol::Udp => {
            let _permit = permits
                .try_acquire_owned()
                .map_err(|_| anyhow::anyhow!("home active-flow limit reached"))?;
            serve_udp(
                stream,
                flow_id,
                carrier_id,
                &service,
                config.udp_idle_secs,
                authorization_rx,
                credential_id,
                not_after_unix_secs,
                tcp_flows.statistics(),
                identity.id,
                relay_id,
                data_codec,
                tcp_flows.provider(),
                credential,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn serve_udp(
    stream: ServerTlsStream<TcpStream>,
    flow_id: Uuid,
    carrier_id: Uuid,
    service: &Service,
    idle_secs: u64,
    mut authorization_rx: watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    credential_id: Uuid,
    not_after_unix_secs: u64,
    statistics: Arc<LocalStatistics>,
    travel_id: String,
    relay_id: String,
    data_codec: DataFrameCodec,
    provider: Arc<dyn ServiceProvider>,
    credential: TravelCredential,
) -> Result<()> {
    ensure_credential_active(&authorization_rx, credential_id)?;
    let (lifetime, _lifetime_guard) =
        flowsplice_transport::ServiceLifetime::new(not_after_unix_secs);
    let authorization_ended = async move {
        loop {
            authorization_rx
                .changed()
                .await
                .map_err(|_| anyhow::anyhow!("Travel authorization publisher closed"))?;
            ensure_credential_active(&authorization_rx, credential_id)?;
        }
    };
    let operation = async move {
        let target_started = Instant::now();
        let socket = timeout(
            Duration::from_secs(idle_secs),
            provider.connect_udp(
                service,
                ServicePeer {
                    credential,
                    travel_id: travel_id.clone(),
                    flow_id,
                    lifetime,
                },
            ),
        )
        .await
        .context("UDP application accept timed out")??;
        record_udp_target_connected(&statistics, &travel_id, service, &relay_id, target_started);
        let (mut tls_reader, mut tls_writer) = tokio::io::split(stream);
        let mut tls_reader = DataFrameReader::new(&mut tls_reader, DATA_FRAME_LIMIT, data_codec);
        timeout(
            Duration::from_secs(idle_secs),
            write_json(
                &mut tls_writer,
                &DataFrame::OpenOk {
                    flow_id,
                    carrier_id,
                    receive_offset: 0,
                    send_offset: 0,
                    data_protocol_version: data_codec.version(),
                },
                DATA_FRAME_LIMIT,
            ),
        )
        .await
        .context("UDP OPEN acknowledgement timed out")??;
        let mut send_sequence = 0_u64;
        let mut receive_sequence = 0_u64;
        let dimensions = home_udp_metric_dimensions(&travel_id, service, &relay_id);
        let mut observed_datagram_bytes = MetricBatch::new(
            statistics.as_ref().clone(),
            "home_flow_download_observed_datagram_bytes".to_owned(),
            dimensions.clone(),
        );
        let mut delivered_datagram_bytes = MetricBatch::new(
            statistics.as_ref().clone(),
            "delivered_upload_datagram_bytes".to_owned(),
            dimensions,
        );
        let mut statistics_tick = interval(Duration::from_secs(5));
        statistics_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        statistics_tick.tick().await;
        let mut idle_deadline = tokio::time::Instant::now() + Duration::from_secs(idle_secs);
        loop {
            tokio::select! {
                datagram = socket.recv() => {
                    let datagram = datagram?;
                    let count = datagram.len();
                    idle_deadline = tokio::time::Instant::now() + Duration::from_secs(idle_secs);
                    if count > 0 && let Ok(now) = unix_time_secs() {
                        observed_datagram_bytes.record(now, count as u64);
                    }
                    timeout(Duration::from_secs(idle_secs), write_data_frame(&mut tls_writer, &DataFrame::Datagram { flow_id, sequence: send_sequence, bytes: Bytes::from(datagram) }, DATA_FRAME_LIMIT, data_codec)).await.context("UDP network write timed out")??;
                    send_sequence = send_sequence.wrapping_add(1);
                }
                frame = tls_reader.read() => {
                    let frame = frame?;
                    idle_deadline = tokio::time::Instant::now() + Duration::from_secs(idle_secs);
                    match frame {
                        DataFrame::Datagram { flow_id: id, sequence, bytes } if id == flow_id && sequence >= receive_sequence && bytes.len() <= 65_507 => {
                            if sequence == receive_sequence {
                                timeout(Duration::from_secs(idle_secs), socket.send(&bytes)).await.context("UDP application write timed out")??;
                                let delivered = bytes.len();
                                if delivered > 0 && let Ok(now) = unix_time_secs() {
                                    delivered_datagram_bytes.record(now, delivered as u64);
                                }
                                receive_sequence = receive_sequence.wrapping_add(1);
                            }
                        }
                        DataFrame::Close { flow_id: id, .. } if id == flow_id => return Ok(()),
                        _ => bail!("invalid UDP flow frame"),
                    }
                }
                _ = statistics_tick.tick() => {
                    observed_datagram_bytes.flush();
                    delivered_datagram_bytes.flush();
                }
                () = tokio::time::sleep_until(idle_deadline) => {
                    bail!("UDP association idle timeout");
                }
            }
        }
    };
    lifetime::guard(operation, authorization_ended, not_after_unix_secs).await
}

fn record_udp_target_connected(
    statistics: &LocalStatistics,
    travel_id: &str,
    service: &Service,
    relay_id: &str,
    target_started: Instant,
) {
    let target_latency_ms = u64::try_from(target_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    record_home_udp_metric_sample(
        statistics,
        travel_id,
        service,
        relay_id,
        "target_connection_latency_ms",
        target_latency_ms,
        Some(target_latency_ms),
    );
    record_home_udp_metric(
        statistics,
        travel_id,
        service,
        relay_id,
        "home_flow_accepted",
        1,
    );
}

fn home_udp_metric_dimensions(
    travel_id: &str,
    service: &Service,
    relay_id: &str,
) -> BTreeMap<String, String> {
    let mut dimensions = BTreeMap::new();
    dimensions.insert("travel_id".to_owned(), travel_id.to_owned());
    dimensions.insert("service_id".to_owned(), service.id.clone());
    dimensions.insert("protocol".to_owned(), "udp".to_owned());
    dimensions.insert("relay_id".to_owned(), relay_id.to_owned());
    dimensions
}

fn record_home_udp_metric(
    statistics: &LocalStatistics,
    travel_id: &str,
    service: &Service,
    relay_id: &str,
    family: &str,
    value: u64,
) {
    if value == 0 {
        return;
    }
    record_home_udp_metric_sample(
        statistics, travel_id, service, relay_id, family, value, None,
    );
}

fn record_home_udp_metric_sample(
    statistics: &LocalStatistics,
    travel_id: &str,
    service: &Service,
    relay_id: &str,
    family: &str,
    value: u64,
    histogram_sample: Option<u64>,
) {
    let dimensions = home_udp_metric_dimensions(travel_id, service, relay_id);
    if let Ok(now) = unix_time_secs() {
        statistics.record(now, family, dimensions, value, histogram_sample);
    }
}

fn authorize_business_identity(
    identity: &flowsplice_core::tls::PeerIdentity,
    authorization_rx: &watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    credential_id: Uuid,
) -> Result<flowsplice_core::authorization::TravelCredential> {
    let now = unix_time_secs()?;
    let authorization = authorization_rx
        .borrow()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Travel authorization has not synchronized from Server"))?;
    Ok(authorization
        .authorize_business_credential(identity, credential_id, now)?
        .clone())
}

/// Check that a credential remains active in the synchronized authorization snapshot.
///
/// # Errors
/// Returns an error for unavailable snapshots, unknown or inactive credentials.
pub fn ensure_credential_active(
    authorization_rx: &watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    credential_id: Uuid,
) -> Result<u64> {
    let now = unix_time_secs()?;
    let authorization = authorization_rx
        .borrow()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Travel authorization has not synchronized from Server"))?;
    if !authorization.is_active(credential_id, now) {
        bail!("Travel credential is revoked, expired, or not yet valid");
    }
    authorization
        .credential(credential_id)
        .map(|credential| credential.not_after_unix_secs)
        .ok_or_else(|| anyhow::anyhow!("unknown Travel credential"))
}

async fn sleep_until_unix(not_after_unix_secs: u64) {
    let now = unix_time_secs().unwrap_or(not_after_unix_secs);
    tokio::time::sleep(Duration::from_secs(not_after_unix_secs.saturating_sub(now))).await;
}

mod control;
pub use control::{
    HomeControlConfig, HomeControlHooks, HomeStatistics, IssuerControlRequest,
    TravelAuthorizationState, run_control_loop,
};

mod runtime;
pub use runtime::{HomeRuntime, HomeRuntimeConfig};

#[cfg(test)]
mod admission_tests;
