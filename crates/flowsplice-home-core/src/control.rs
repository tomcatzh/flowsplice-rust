//! Shared Home control transport; enrollment payload handling stays in product adapters.
// Stable tracing targets preserve pre-extraction deployment log filters.
use crate::{HomeFlowConfig, HomeTls, TcpFlowRegistry, ensure_credential_active, run_work};
use anyhow::{Context, Result, bail};
use aws_lc_rs::signature::EcdsaKeyPair;
use flowsplice_core::{
    CONTROL_FRAME_LIMIT,
    authorization::{
        AuthorizationCache, SignedTravelCredential, TravelAuthorizationSnapshot,
        VerifiedAuthorization, load_json, store_json_atomic, unix_time_secs,
    },
    deployment::DeploymentTrust,
    frame::{JsonFrameReader, write_json},
    protocol::{CONTROL_PROTOCOL_VERSION, ControlMessage, HomeCatalog, Role, Service},
};
use flowsplice_storage::{LocalStatistics, StateStore};
use flowsplice_transport::IoFuture;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    net::TcpStream,
    sync::{Mutex, Semaphore, mpsc, oneshot, watch},
    time::{interval, sleep},
};
use tokio_rustls::client::TlsStream as ClientTlsStream;
use tracing::{info, warn};
use uuid::Uuid;

/// Existing Home control configuration without issuer or user-interface settings.
#[derive(Clone)]
pub struct HomeControlConfig {
    pub id: String,
    pub alias: String,
    pub server_control_addr: String,
    pub server_name: String,
    pub server_id: String,
    pub server_spki_pins: Vec<String>,
    pub endpoint_credential: Option<PathBuf>,
    pub services: Vec<Service>,
    pub handshake_timeout_secs: u64,
    pub udp_idle_secs: u64,
    pub travel_authorization_cache: PathBuf,
}
/// Product-owned handling of enrollment payloads and periodic maintenance.
pub trait HomeControlHooks: Send + Sync {
    /// Handle one enrollment submission or installation acknowledgement.
    fn handle(&self, message: ControlMessage) -> IoFuture<'_, ControlMessage>;
    /// Perform periodic adapter maintenance at the existing sixty-second interval.
    fn tick(&self) -> IoFuture<'_, ()>;
}

/// A serving-only runtime rejects enrollment requests without loading issuer code or keys.
pub struct ServingOnly;

impl HomeControlHooks for ServingOnly {
    fn handle(&self, message: ControlMessage) -> IoFuture<'_, ControlMessage> {
        Box::pin(async move {
            let error = Some("this Home has no issuer".to_owned());
            match message {
                ControlMessage::RemoteEnrollmentSubmit { request_id, .. }
                | ControlMessage::BootstrapEnrollmentSubmit { request_id, .. } => {
                    Ok(ControlMessage::RemoteEnrollmentResult {
                        request_id,
                        accepted: false,
                        response_json: None,
                        error,
                    })
                }
                ControlMessage::HomeEnrollmentSubmit { request_id, .. } => {
                    Ok(ControlMessage::HomeEnrollmentResult {
                        request_id,
                        accepted: false,
                        response_json: None,
                        error,
                    })
                }
                ControlMessage::RemoteEnrollmentInstalled { request_id, .. } => {
                    Ok(ControlMessage::RemoteEnrollmentInstalledAck {
                        request_id,
                        accepted: false,
                        error,
                    })
                }
                _ => bail!("unexpected serving-only control request"),
            }
        })
    }

    fn tick(&self) -> IoFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// Synchronized, rollback-protected Travel authorization state.
pub struct TravelAuthorizationState {
    tx: watch::Sender<Option<Arc<VerifiedAuthorization>>>,
    cache: Mutex<AuthorizationCache>,
    trust: Arc<DeploymentTrust>,
    deployment_id: Arc<String>,
}

impl TravelAuthorizationState {
    /// Initialize the runtime from an explicitly loaded authorization cache and trust.
    #[must_use]
    pub fn new(cache: AuthorizationCache, trust: DeploymentTrust) -> Arc<Self> {
        let (tx, _) = watch::channel(None);
        Arc::new(Self {
            tx,
            cache: Mutex::new(cache),
            deployment_id: Arc::new(trust.deployment_id.clone()),
            trust: Arc::new(trust),
        })
    }
    /// Subscribe to verified authorization updates.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<VerifiedAuthorization>>> {
        self.tx.subscribe()
    }
    /// Return the current verified authorization snapshot, if synchronized.
    #[must_use]
    pub fn current(&self) -> Option<Arc<VerifiedAuthorization>> {
        self.tx.borrow().clone()
    }
}

/// Transport commands carrying an already-signed credential or a revocation request.
pub enum IssuerControlRequest {
    Publish {
        credential: SignedTravelCredential,
        response: oneshot::Sender<std::result::Result<u64, String>>,
    },
    Revoke {
        credential_id: Uuid,
        reason: String,
        response: oneshot::Sender<std::result::Result<u64, String>>,
    },
}

/// Statistics state shared by the Home transport and product adapter.
#[derive(Clone)]
pub struct HomeStatistics {
    pub store: Arc<StateStore>,
    pub local: Arc<LocalStatistics>,
    pub signer: Arc<EcdsaKeyPair>,
    pub certificate_pem: Arc<String>,
    pub deployment_id: Arc<String>,
    pub reporter_id: Arc<String>,
}

/// Maintain authenticated Server control, service registration and authorization updates.
///
/// # Errors
/// Session failures reconnect internally; an adapter hook failure ends its session.
#[allow(clippy::too_many_arguments)]
pub async fn run_control_loop(
    config: Arc<HomeControlConfig>,
    permits: Arc<Semaphore>,
    tls: Arc<HomeTls>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization: Arc<TravelAuthorizationState>,
    mut issuer_control_rx: Option<mpsc::Receiver<IssuerControlRequest>>,
    statistics: HomeStatistics,
    hooks: Arc<dyn HomeControlHooks>,
) -> Result<()> {
    loop {
        if let Err(error) = run_control(
            Arc::clone(&config),
            Arc::clone(&permits),
            Arc::clone(&tls),
            Arc::clone(&tcp_flows),
            Arc::clone(&authorization),
            &mut issuer_control_rx,
            statistics.clone(),
            Arc::clone(&hooks),
        )
        .await
        {
            warn!(target: "flowsplice_homeagent", %error, "server control disconnected; reconnecting");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_control(
    config: Arc<HomeControlConfig>,
    permits: Arc<Semaphore>,
    tls: Arc<HomeTls>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization: Arc<TravelAuthorizationState>,
    issuer_control_rx: &mut Option<mpsc::Receiver<IssuerControlRequest>>,
    statistics: HomeStatistics,
    hooks: Arc<dyn HomeControlHooks>,
) -> Result<()> {
    let stream = tls
        .connect_management(
            &config.server_control_addr,
            &config.server_name,
            &config.server_id,
            &config.server_spki_pins,
            Duration::from_secs(config.handshake_timeout_secs),
        )
        .await?;
    run_control_session(
        stream,
        config,
        permits,
        tls,
        tcp_flows,
        authorization,
        issuer_control_rx,
        statistics,
        hooks,
    )
    .await
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
async fn run_control_session(
    stream: ClientTlsStream<TcpStream>,
    config: Arc<HomeControlConfig>,
    permits: Arc<Semaphore>,
    tls: Arc<HomeTls>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization: Arc<TravelAuthorizationState>,
    issuer_control_rx: &mut Option<mpsc::Receiver<IssuerControlRequest>>,
    statistics: HomeStatistics,
    hooks: Arc<dyn HomeControlHooks>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    write_json(
        &mut writer,
        &ControlMessage::Hello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            role: Role::Home,
            id: config.id.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    match reader
        .read_with_timeout::<ControlMessage>(Duration::from_secs(config.handshake_timeout_secs))
        .await?
    {
        ControlMessage::Hello {
            protocol_version,
            role,
            id,
        } if protocol_version == CONTROL_PROTOCOL_VERSION
            && role == Role::Server
            && id == config.server_id => {}
        _ => bail!("server sent an invalid HELLO"),
    }
    write_json(
        &mut writer,
        &ControlMessage::HomeRegister {
            home: HomeCatalog {
                home_id: config.id.clone(),
                home_alias: config.alias.clone(),
                endpoint_credential: config
                    .endpoint_credential
                    .as_deref()
                    .map(load_json)
                    .transpose()?,
                services: config.services.clone(),
            },
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    let ControlMessage::TravelAuthorizationSnapshot {
        snapshot: initial_snapshot,
    } = reader
        .read_with_timeout::<ControlMessage>(Duration::from_secs(config.handshake_timeout_secs))
        .await?
    else {
        bail!("Server did not send initial Travel authorization state");
    };
    let generation =
        apply_authorization_snapshot(&authorization, &tcp_flows, &config, initial_snapshot).await?;
    write_json(
        &mut writer,
        &ControlMessage::TravelAuthorizationAck { generation },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    info!(target: "flowsplice_homeagent", server = %config.server_control_addr, "home agent registered");

    let mut heartbeat = interval(Duration::from_secs(10));
    let mut statistics_tick = interval(Duration::from_secs(5));
    let mut report_keys = HashMap::<String, Vec<u8>>::new();
    let mut last_enrollment_prune = Instant::now();
    let mut nonce = 0_u64;
    let mut last_received = Instant::now();
    let mut pending_issuer =
        HashMap::<Uuid, oneshot::Sender<std::result::Result<u64, String>>>::new();
    loop {
        tokio::select! {
            command = receive_issuer_command(issuer_control_rx) => {
                let Some(command) = command else { bail!("Home issuer control channel closed"); };
                let request_id = Uuid::new_v4();
                let (message, response) = match command {
                    IssuerControlRequest::Publish { credential, response } => (
                        ControlMessage::PublishTravelCredential { request_id, credential },
                        response,
                    ),
                    IssuerControlRequest::Revoke { credential_id, reason, response } => (
                        ControlMessage::RevokeTravelCredential { request_id, credential_id, reason },
                        response,
                    ),
                };
                write_json(&mut writer, &message, CONTROL_FRAME_LIMIT).await?;
                pending_issuer.insert(request_id, response);
            }
            message = reader.read::<ControlMessage>() => {
                last_received = Instant::now();
                match message? {
                    ControlMessage::TravelAuthorizationSnapshot { snapshot } => {
                        let generation = apply_authorization_snapshot(
                            &authorization,
                            &tcp_flows,
                            &config,
                            snapshot,
                        )
                        .await?;
                        write_json(
                            &mut writer,
                            &ControlMessage::TravelAuthorizationAck { generation },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
                    }
                    ControlMessage::OpenRelayWork {
                        work_id,
                        work_secret,
                        credential_id,
                        relay_id,
                        relay_data_addr,
                        expires_at_unix_secs,
                    } => {
                        ensure_credential_active(&authorization.subscribe(), credential_id)?;
                        let config = Arc::new(HomeFlowConfig { id: config.id.clone(), services: config.services.clone(), handshake_timeout_secs: config.handshake_timeout_secs, udp_idle_secs: config.udp_idle_secs });
                        let tls = tls.business_acceptor();
                        let permits = Arc::clone(&permits);
                        let work_registry = Arc::clone(&tcp_flows);
                        let authorization_rx = authorization.subscribe();
                        tcp_flows.tasks.spawn(async move {
                            if let Err(error) = run_work(config, tls, permits, work_registry, authorization_rx, credential_id, work_id, work_secret, relay_id.clone(), relay_data_addr, expires_at_unix_secs).await {
                                warn!(target: "flowsplice_homeagent", %work_id, %relay_id, %error, "home direct Relay work failed");
                            }
                        })?;
                    }
                    message @ (ControlMessage::RemoteEnrollmentSubmit { .. }
                        | ControlMessage::BootstrapEnrollmentSubmit { .. }
                        | ControlMessage::HomeEnrollmentSubmit { .. }
                        | ControlMessage::RemoteEnrollmentInstalled { .. }) => {
                        let response = hooks.handle(message).await?;
                        write_json(&mut writer, &response, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(&mut writer, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
                    ControlMessage::PublishTravelCredentialResult { request_id, accepted, generation, error }
                    | ControlMessage::RevokeTravelCredentialResult { request_id, accepted, generation, error } => {
                        if let Some(response) = pending_issuer.remove(&request_id) {
                            let result = if accepted {
                                Ok(generation)
                            } else {
                                Err(error.unwrap_or_else(|| "Server rejected the request".to_owned()))
                            };
                            let _ = response.send(result);
                        }
                    }
                    ControlMessage::StatisticsReportAck { digest_sha256, accepted, error } => {
                        if accepted {
                            if let Some(key) = report_keys.remove(&digest_sha256) {
                                let local = Arc::clone(&statistics.local);
                                let digest = digest_sha256.clone();
                                tcp_flows.tasks.blocking(move || local.acknowledge_report(&key, &digest))
                                    .await
                                    .context("Home statistics acknowledgement task failed")?;
                            }
                        } else {
                            warn!(target: "flowsplice_homeagent", ?error, %digest_sha256, "Server rejected Home statistics report");
                        }
                    }
                    _ => bail!("unexpected message from server"),
                }
            }
            _ = heartbeat.tick() => {
                if last_received.elapsed() > Duration::from_secs(30) {
                    bail!("server control heartbeat timed out");
                }
                nonce = nonce.wrapping_add(1);
                write_json(&mut writer, &ControlMessage::Heartbeat { nonce }, CONTROL_FRAME_LIMIT).await?;
            }
            _ = statistics_tick.tick() => {
                flush_and_send_home_statistics(&statistics, &tcp_flows.tasks, &mut writer, &mut report_keys).await?;
                if last_enrollment_prune.elapsed() >= Duration::from_secs(60) {
                    hooks.tick().await?;
                    last_enrollment_prune = Instant::now();
                }
            }
        }
    }
}

async fn flush_and_send_home_statistics<W: tokio::io::AsyncWrite + Unpin>(
    statistics: &HomeStatistics,
    tasks: &crate::tasks::Tasks,
    writer: &mut W,
    report_keys: &mut HashMap<String, Vec<u8>>,
) -> Result<()> {
    let local = Arc::clone(&statistics.local);
    let deployment_id = Arc::clone(&statistics.deployment_id);
    let reporter_id = Arc::clone(&statistics.reporter_id);
    let certificate_pem = Arc::clone(&statistics.certificate_pem);
    let signer = Arc::clone(&statistics.signer);
    let flush_result = tasks
        .blocking(move || {
            local.flush_and_stage(
                &deployment_id,
                Role::Home,
                &reporter_id,
                &certificate_pem,
                &signer,
            )
        })
        .await;
    if let Err(error) = flush_result {
        warn!(target: "flowsplice_homeagent", %error, "Home statistics write failed; business processing remains active and pending deltas were retained");
    }
    let local = Arc::clone(&statistics.local);
    let reports = tasks
        .blocking(move || local.pending_reports(16))
        .await
        .context("Home statistics outbox task failed")?;
    for (key, report) in reports {
        let digest = report.digest_sha256()?;
        report_keys.insert(digest, key);
        write_json(
            writer,
            &ControlMessage::StatisticsReport { report },
            CONTROL_FRAME_LIMIT,
        )
        .await?;
    }
    Ok(())
}

async fn receive_issuer_command(
    receiver: &mut Option<mpsc::Receiver<IssuerControlRequest>>,
) -> Option<IssuerControlRequest> {
    match receiver {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

async fn apply_authorization_snapshot(
    state: &TravelAuthorizationState,
    tcp_flows: &TcpFlowRegistry,
    config: &HomeControlConfig,
    snapshot: TravelAuthorizationSnapshot,
) -> Result<u64> {
    let authorities = state.trust.travel_authorities_with_home_delegations(
        &snapshot.home_endpoint_credentials,
        unix_time_secs()?,
    )?;
    let authorization =
        VerifiedAuthorization::verify(&snapshot, &authorities, &state.deployment_id)?;
    let mut cache = state.cache.lock().await;
    let proposed_cache = cache.accept(&authorization)?;
    if proposed_cache != *cache {
        store_json_atomic(&config.travel_authorization_cache, &proposed_cache)?;
        *cache = proposed_cache;
    }
    drop(cache);
    let now = unix_time_secs()?;
    tcp_flows.revoke_inactive(&authorization, now).await;
    let generation = authorization.generation();
    state.tx.send_replace(Some(Arc::new(authorization)));
    info!(target: "flowsplice_homeagent",
        event = "travel_authorization_applied",
        generation,
        revoked = snapshot.revocations.len(),
        credentials = snapshot.credentials.len(),
        "Home applied Travel authorization state without restart"
    );
    Ok(generation)
}
