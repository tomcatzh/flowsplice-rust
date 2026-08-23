#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    io::{self, IsTerminal},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    rand::{SecureRandom, SystemRandom},
    signature::EcdsaKeyPair,
};
use axum::{
    Json, Router,
    extract::{Query, State as AxumState},
    response::{Html, IntoResponse},
    routing::get,
};
use clap::Parser;
use flowsplice_core::{
    CONTROL_FRAME_LIMIT,
    authorization::{
        AuthorizationCache, TravelAuthorizationSnapshot, VerifiedAuthorization,
        initialize_authorization_cache, load_initialized_authorization_cache, load_json,
        store_json_atomic, unix_time_secs,
    },
    config::load_toml,
    deployment::{DeploymentTrust, SignedControlSnapshot, SignedDeploymentTrust},
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{CONTROL_PROTOCOL_VERSION, ControlMessage, Role, TravelConnectionPurpose},
    route::{RouteSide, read_preface, verify_preface},
    statistics::{SignedStatisticsReport, statistics_dashboard_html, statistics_signing_key},
    tls::{
        PeerIdentity, identity_from_certificate_pem, load_private_key,
        optional_client_server_acceptor, peer_identity, require_peer, validate_spki_pins,
    },
};
use flowsplice_storage::{
    LocalStatistics, MetricPoint, MetricRollup, StateStore, summarize_metric_points,
};
use serde::{Deserialize, Serialize};
#[cfg(not(target_os = "linux"))]
use tokio::io::copy_bidirectional;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::{interval, timeout},
};
use tokio_rustls::server::TlsStream;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Parser)]
#[command(version)]
struct Args {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "relay.toml")]
    config: PathBuf,
    #[arg(long, conflicts_with = "initialize_authorization_state")]
    check_config: bool,
    /// Explicitly create the rollback and revocation cache for a first-time Relay install.
    #[arg(long, conflicts_with = "check_config")]
    initialize_authorization_state: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    id: String,
    management_listen: String,
    data_listen: String,
    data_public_addr: String,
    server_id: String,
    cert: PathBuf,
    key: PathBuf,
    management_ca: PathBuf,
    deployment_root_public_key: PathBuf,
    deployment_trust: PathBuf,
    #[serde(default)]
    server_spki_pins: Vec<String>,
    travel_authorization_cache: PathBuf,
    state_store: PathBuf,
    ui_listen: String,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout_secs: u64,
    #[serde(default = "default_route_ttl")]
    route_ttl_secs: u64,
    #[serde(default = "default_max_pending_routes")]
    max_pending_routes: usize,
    #[serde(default = "default_max_management_connections")]
    max_management_connections: usize,
    #[serde(default = "default_max_data_connections")]
    max_data_connections: usize,
}

const fn default_handshake_timeout() -> u64 {
    10
}

const fn default_route_ttl() -> u64 {
    15
}

const fn default_max_pending_routes() -> usize {
    256
}

const fn default_max_management_connections() -> usize {
    1_024
}

const fn default_max_data_connections() -> usize {
    2_048
}

struct ServerGrant {
    work_id: Uuid,
    work_secret: Vec<u8>,
    credential_id: Uuid,
    home_id: String,
    expires_at_unix_secs: u64,
}

struct RemoteEnrollmentResponse {
    accepted: bool,
    response_json: Option<Vec<u8>>,
    error: Option<String>,
}

struct RemoteEnrollmentInstalledResponse {
    accepted: bool,
    error: Option<String>,
}

struct BootstrapEnrollmentResponse {
    accepted: bool,
    response_json: Option<Vec<u8>>,
    seed_relays: Vec<String>,
    error: Option<String>,
}

struct TravelRouteContext {
    request: Uuid,
    credential: Uuid,
    travel: String,
    travel_session: Uuid,
    home: String,
}

struct PendingRoute {
    credential_id: Uuid,
    travel_id: String,
    home_id: String,
    work_id: Uuid,
    route_secret: Vec<u8>,
    work_secret: Vec<u8>,
    created: Instant,
    expires: Instant,
    travel: Option<BudgetedSocket>,
    home: Option<BudgetedSocket>,
    _permit: OwnedSemaphorePermit,
}

struct BudgetedSocket {
    stream: TcpStream,
    _permit: OwnedSemaphorePermit,
}

struct ServerSession {
    session_id: Uuid,
    tx: mpsc::Sender<ControlMessage>,
    shutdown: watch::Sender<bool>,
}

struct State {
    server_session: Mutex<Option<ServerSession>>,
    requests: Mutex<HashMap<Uuid, oneshot::Sender<Result<ServerGrant, String>>>>,
    session_requests: Mutex<HashMap<Uuid, oneshot::Sender<Result<SignedControlSnapshot, String>>>>,
    routes: Mutex<HashMap<Uuid, PendingRoute>>,
    authorization_tx: watch::Sender<Option<Arc<VerifiedAuthorization>>>,
    authorization_cache: Mutex<AuthorizationCache>,
    deployment_trust: Arc<DeploymentTrust>,
    route_permits: Arc<Semaphore>,
    statistics: Arc<LocalStatistics>,
    statistics_signer: Arc<EcdsaKeyPair>,
    statistics_certificate_pem: Arc<String>,
    report_requests: Mutex<HashMap<String, oneshot::Sender<std::result::Result<(), String>>>>,
    enrollment_requests: Mutex<HashMap<Uuid, oneshot::Sender<RemoteEnrollmentResponse>>>,
    enrollment_install_requests:
        Mutex<HashMap<Uuid, oneshot::Sender<RemoteEnrollmentInstalledResponse>>>,
    bootstrap_enrollment_requests:
        Mutex<HashMap<Uuid, oneshot::Sender<BootstrapEnrollmentResponse>>>,
}

impl State {
    fn new(
        authorization_cache: AuthorizationCache,
        deployment_trust: DeploymentTrust,
        max_pending_routes: usize,
        statistics: LocalStatistics,
        statistics_signer: EcdsaKeyPair,
        statistics_certificate_pem: String,
    ) -> Self {
        let (authorization_tx, _) = watch::channel(None);
        Self {
            server_session: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
            session_requests: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            authorization_tx,
            authorization_cache: Mutex::new(authorization_cache),
            deployment_trust: Arc::new(deployment_trust),
            route_permits: Arc::new(Semaphore::new(max_pending_routes)),
            statistics: Arc::new(statistics),
            statistics_signer: Arc::new(statistics_signer),
            statistics_certificate_pem: Arc::new(statistics_certificate_pem),
            report_requests: Mutex::new(HashMap::new()),
            enrollment_requests: Mutex::new(HashMap::new()),
            enrollment_install_requests: Mutex::new(HashMap::new()),
            bootstrap_enrollment_requests: Mutex::new(HashMap::new()),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_crypto();
    tracing_subscriber::fmt()
        .with_ansi(io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flowsplice_relay=info".into()),
        )
        .init();

    let args = Args::parse();
    let config: Config = load_toml(&args.config)?;
    let deployment_trust = validate_config(&config)?;
    if args.check_config {
        info!(event = "config_validated", path = %args.config.display(), "relay configuration is valid");
        return Ok(());
    }
    if args.initialize_authorization_state {
        if initialize_authorization_cache(&config.travel_authorization_cache)? {
            println!(
                "initialized Relay authorization state: {}",
                config.travel_authorization_cache.display()
            );
        } else {
            println!(
                "Relay authorization state is already initialized: {}",
                config.travel_authorization_cache.display()
            );
        }
        return Ok(());
    }
    let authorization_cache =
        load_initialized_authorization_cache(&config.travel_authorization_cache)?;
    let state_store = StateStore::open(&config.state_store)?;
    let statistics = LocalStatistics::new(state_store);
    let statistics_certificate_pem = fs::read_to_string(&config.cert)
        .context("failed to read Relay statistics signing certificate")?;
    let statistics_private_key = load_private_key(&config.key)?;
    let statistics_signer = statistics_signing_key(&statistics_private_key)?;
    let state = Arc::new(State::new(
        authorization_cache,
        deployment_trust.clone(),
        config.max_pending_routes,
        statistics,
        statistics_signer,
        statistics_certificate_pem,
    ));
    let trust_expiry = monitor_trust_expiry(deployment_trust.not_after_unix_secs);
    tokio::try_join!(
        run_management(config.clone(), Arc::clone(&state)),
        run_data(config.clone(), Arc::clone(&state)),
        cleanup_routes(Arc::clone(&state)),
        run_statistics_ui(config.ui_listen.clone(), Arc::clone(&state)),
        trust_expiry,
    )?;
    Ok(())
}

async fn monitor_trust_expiry(not_after_unix_secs: u64) -> Result<()> {
    loop {
        if unix_time_secs()? >= not_after_unix_secs {
            bail!("deployment trust expired; refusing to continue");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

fn validate_config(config: &Config) -> Result<DeploymentTrust> {
    if config.id.is_empty() || config.server_id.is_empty() || config.data_public_addr.is_empty() {
        bail!("Relay ids and advertised addresses must be non-empty");
    }
    if config.state_store.as_os_str().is_empty() {
        bail!("state_store must be non-empty");
    }
    let ui_address = config
        .ui_listen
        .parse::<std::net::SocketAddr>()
        .context("invalid Relay statistics UI listener")?;
    if !ui_address.ip().is_loopback() {
        bail!("Relay statistics UI must bind an exact loopback address");
    }
    for (label, address) in [
        ("management", config.management_listen.as_str()),
        ("data", config.data_listen.as_str()),
    ] {
        address
            .parse::<std::net::SocketAddr>()
            .with_context(|| format!("invalid Relay {label} listener {address}"))?;
    }
    if config.handshake_timeout_secs == 0
        || config.route_ttl_secs == 0
        || config.max_pending_routes == 0
        || config.max_management_connections == 0
        || config.max_data_connections == 0
    {
        bail!("Relay timeout and pending-route limits must be positive");
    }
    validate_spki_pins(&config.server_spki_pins, "server")?;
    let root_public_key = fs::read_to_string(&config.deployment_root_public_key)
        .context("failed to read deployment root public key")?;
    let trust: SignedDeploymentTrust = load_json(&config.deployment_trust)?;
    let trust = trust.verify(root_public_key.trim(), unix_time_secs()?)?;
    if fs::read_to_string(&config.management_ca)? != trust.management_ca_certificate_pem {
        bail!("Relay management CA does not match deployment trust");
    }
    if config.travel_authorization_cache.exists() {
        let _: AuthorizationCache = load_json(&config.travel_authorization_cache)?;
    }
    let _ = optional_client_server_acceptor(&config.cert, &config.key, &config.management_ca)?;
    Ok(trust)
}

async fn run_management(config: Config, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&config.management_listen)
        .await
        .with_context(|| format!("failed to bind management {}", config.management_listen))?;
    let acceptor =
        optional_client_server_acceptor(&config.cert, &config.key, &config.management_ca)?;
    let permits = Arc::new(Semaphore::new(config.max_management_connections));
    info!(address = %config.management_listen, "relay management listener ready");
    loop {
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("Relay management connection budget closed"))?;
        let (socket, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let config = config.clone();
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _permit = permit;
            let result = async {
                let stream = timeout(
                    Duration::from_secs(config.handshake_timeout_secs),
                    acceptor.accept(socket),
                )
                .await
                .context("management TLS handshake timed out")??;
                if stream.get_ref().1.peer_certificates().is_none() {
                    return handle_bootstrap_travel(stream, &config, state).await;
                }
                let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
                match identity.role {
                    Role::Server => {
                        require_peer(
                            &identity,
                            Role::Server,
                            Some(&config.server_id),
                            &config.server_spki_pins,
                        )?;
                        handle_server(stream, identity.id, &config, state).await
                    }
                    Role::Travel => {
                        require_peer(&identity, Role::Travel, None, &[])?;
                        let authorization_rx = state.authorization_tx.subscribe();
                        handle_travel(stream, identity, authorization_rx, &config, state).await
                    }
                    _ => bail!("unsupported management peer role"),
                }
            }
            .await;
            if let Err(error) = result {
                warn!(%peer, %error, "management connection closed");
            }
        });
    }
}

async fn handle_bootstrap_travel(
    mut stream: TlsStream<TcpStream>,
    config: &Config,
    state: Arc<State>,
) -> Result<()> {
    let message = JsonFrameReader::new(&mut stream, CONTROL_FRAME_LIMIT)
        .read_with_timeout::<ControlMessage>(Duration::from_secs(config.handshake_timeout_secs))
        .await?;
    let ControlMessage::BootstrapEnrollmentSubmit {
        protocol_version,
        request_id,
        travel_id,
        home_id,
        retrieval_token,
        request_json,
    } = message
    else {
        bail!("anonymous Relay peer may submit only first enrollment");
    };
    let response = if protocol_version == CONTROL_PROTOCOL_VERSION {
        match forward_bootstrap_enrollment(
            &state,
            request_id,
            travel_id,
            home_id,
            retrieval_token,
            request_json,
            Duration::from_secs(config.handshake_timeout_secs.saturating_mul(2)),
        )
        .await
        {
            Ok(response) => response,
            Err(error) => BootstrapEnrollmentResponse {
                accepted: false,
                response_json: None,
                seed_relays: Vec::new(),
                error: Some(error.to_string()),
            },
        }
    } else {
        BootstrapEnrollmentResponse {
            accepted: false,
            response_json: None,
            seed_relays: Vec::new(),
            error: Some("unsupported bootstrap protocol version".to_owned()),
        }
    };
    write_json(
        &mut stream,
        &ControlMessage::BootstrapEnrollmentResult {
            request_id,
            accepted: response.accepted,
            response_json: response.response_json,
            seed_relays: response.seed_relays,
            error: response.error,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    Ok(())
}

async fn forward_bootstrap_enrollment(
    state: &Arc<State>,
    request_id: Uuid,
    travel_id: String,
    home_id: String,
    retrieval_token: Vec<u8>,
    request_json: Vec<u8>,
    request_timeout: Duration,
) -> Result<BootstrapEnrollmentResponse> {
    if request_id.is_nil()
        || travel_id.is_empty()
        || home_id.is_empty()
        || retrieval_token.len() != 32
        || request_json.is_empty()
        || request_json.len() > 512 * 1024
    {
        bail!("first enrollment request is invalid or oversized");
    }
    let server = state
        .server_session
        .lock()
        .await
        .as_ref()
        .map(|session| session.tx.clone())
        .ok_or_else(|| anyhow!("Server is unavailable for first enrollment"))?;
    let (response, receive) = oneshot::channel();
    if state
        .bootstrap_enrollment_requests
        .lock()
        .await
        .insert(request_id, response)
        .is_some()
    {
        bail!("first enrollment request is already in flight");
    }
    if server
        .send(ControlMessage::BootstrapEnrollmentSubmit {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id,
            travel_id,
            home_id,
            retrieval_token,
            request_json,
        })
        .await
        .is_err()
    {
        state
            .bootstrap_enrollment_requests
            .lock()
            .await
            .remove(&request_id);
        bail!("Server connection closed during first enrollment");
    }
    match timeout(request_timeout, receive).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => bail!("first enrollment response channel closed"),
        Err(_) => {
            state
                .bootstrap_enrollment_requests
                .lock()
                .await
                .remove(&request_id);
            bail!("first enrollment request timed out")
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_server(
    stream: TlsStream<TcpStream>,
    server_id: String,
    config: &Config,
    state: Arc<State>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
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
            && id == server_id => {}
        _ => bail!("server HELLO does not match its certificate"),
    }
    write_json(
        &mut writer,
        &ControlMessage::Hello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            role: Role::Relay,
            id: config.id.clone(),
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
        bail!("server did not provide the initial Travel authorization snapshot");
    };
    let generation = apply_authorization_snapshot(&state, config, initial_snapshot).await?;
    write_json(
        &mut writer,
        &ControlMessage::TravelAuthorizationAck { generation },
        CONTROL_FRAME_LIMIT,
    )
    .await?;

    let (tx, mut rx) = mpsc::channel::<ControlMessage>(64);
    let session_id = Uuid::new_v4();
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let previous = state.server_session.lock().await.replace(ServerSession {
        session_id,
        tx,
        shutdown,
    });
    if let Some(previous) = previous {
        warn!(
            old_session_id = %previous.session_id,
            new_session_id = %session_id,
            %server_id,
            "superseding existing Server session"
        );
        let _ = previous.shutdown.send(true);
    }
    info!(%server_id, "server connected to relay");

    let mut liveness = interval(Duration::from_secs(10));
    let mut statistics_tick = interval(Duration::from_secs(5));
    let mut local_report_keys = HashMap::<String, Vec<u8>>::new();
    let mut last_received = Instant::now();
    let result: Result<()> = async {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        bail!("Server session was superseded");
                    }
                }
                outgoing = rx.recv() => {
                    let Some(outgoing) = outgoing else { bail!("server writer channel closed"); };
                    write_json(&mut writer, &outgoing, CONTROL_FRAME_LIMIT).await?;
                }
                incoming = reader.read::<ControlMessage>() => {
                    last_received = Instant::now();
                    match incoming? {
                        ControlMessage::TravelAuthorizationSnapshot { snapshot } => {
                            let generation = apply_authorization_snapshot(&state, config, snapshot).await?;
                            write_json(
                                &mut writer,
                                &ControlMessage::TravelAuthorizationAck { generation },
                                CONTROL_FRAME_LIMIT,
                            )
                            .await?;
                        }
                        ControlMessage::ServerRelayGrant { request_id, work_id, work_secret, credential_id, home_id, expires_at_unix_secs } => {
                            if let Some(waiter) = state.requests.lock().await.remove(&request_id) {
                                let _ = waiter.send(Ok(ServerGrant { work_id, work_secret, credential_id, home_id, expires_at_unix_secs }));
                            }
                        }
                        ControlMessage::RouteDenied { request_id, reason } => {
                            if let Some(waiter) = state.requests.lock().await.remove(&request_id) {
                                let _ = waiter.send(Err(reason));
                            }
                        }
                        ControlMessage::TravelSessionAccepted { request_id, snapshot } => {
                            if let Some(waiter) = state.session_requests.lock().await.remove(&request_id) {
                                let _ = waiter.send(Ok(snapshot));
                            }
                        }
                        ControlMessage::TravelSessionDenied { request_id, reason } => {
                            if let Some(waiter) = state.session_requests.lock().await.remove(&request_id) {
                                let _ = waiter.send(Err(reason));
                            }
                        }
                        ControlMessage::Heartbeat { nonce } => {
                            write_json(&mut writer, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                        }
                        ControlMessage::HeartbeatAck { .. } => {}
                        ControlMessage::StatisticsReportAck { digest_sha256, accepted, error } => {
                            if let Some(response) = state.report_requests.lock().await.remove(&digest_sha256) {
                                let _ = response.send(if accepted {
                                    Ok(())
                                } else {
                                    Err(error.unwrap_or_else(|| "Server rejected the statistics report".to_owned()))
                                });
                            } else if accepted
                                && let Some(key) = local_report_keys.remove(&digest_sha256)
                            {
                                let statistics = Arc::clone(&state.statistics);
                                let digest = digest_sha256.clone();
                                tokio::task::spawn_blocking(move || statistics.acknowledge_report(&key, &digest))
                                    .await
                                    .context("Relay statistics acknowledgement task failed")??;
                            }
                        }
                        ControlMessage::RemoteEnrollmentResult { request_id, accepted, response_json, error } => {
                            if let Some(response) = state.enrollment_requests.lock().await.remove(&request_id) {
                                let _ = response.send(RemoteEnrollmentResponse {
                                    accepted,
                                    response_json,
                                    error,
                                });
                            }
                        }
                        ControlMessage::RemoteEnrollmentInstalledAck { request_id, accepted, error } => {
                            if let Some(response) = state.enrollment_install_requests.lock().await.remove(&request_id) {
                                let _ = response.send(RemoteEnrollmentInstalledResponse {
                                    accepted,
                                    error,
                                });
                            }
                        }
                        ControlMessage::BootstrapEnrollmentResult { request_id, accepted, response_json, seed_relays, error } => {
                            if let Some(response) = state.bootstrap_enrollment_requests.lock().await.remove(&request_id) {
                                let _ = response.send(BootstrapEnrollmentResponse {
                                    accepted,
                                    response_json,
                                    seed_relays,
                                    error,
                                });
                            }
                        }
                        _ => bail!("unexpected message from server"),
                    }
                }
                _ = liveness.tick() => {
                    if last_received.elapsed() > Duration::from_secs(30) {
                        bail!("server control heartbeat timed out");
                    }
                }
                _ = statistics_tick.tick() => {
                    flush_and_send_relay_statistics(
                        &state,
                        &mut writer,
                        &mut local_report_keys,
                    )
                    .await?;
                }
            }
        }
    }
    .await;
    let mut current = state.server_session.lock().await;
    if current
        .as_ref()
        .is_some_and(|session| session.session_id == session_id)
    {
        *current = None;
    }
    result
}

async fn flush_and_send_relay_statistics<W: tokio::io::AsyncWrite + Unpin>(
    state: &Arc<State>,
    writer: &mut W,
    report_keys: &mut HashMap<String, Vec<u8>>,
) -> Result<()> {
    let statistics = Arc::clone(&state.statistics);
    let deployment_id = state.deployment_trust.deployment_id.clone();
    let certificate_pem = Arc::clone(&state.statistics_certificate_pem);
    let signer = Arc::clone(&state.statistics_signer);
    tokio::task::spawn_blocking(move || {
        statistics.flush_and_stage(
            &deployment_id,
            Role::Relay,
            &identity_from_certificate_pem(&certificate_pem)?.id,
            &certificate_pem,
            &signer,
        )
    })
    .await
    .context("Relay statistics flush task failed")??;
    let statistics = Arc::clone(&state.statistics);
    let reports = tokio::task::spawn_blocking(move || statistics.pending_reports(16))
        .await
        .context("Relay statistics outbox task failed")??;
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

async fn forward_travel_statistics(
    state: &Arc<State>,
    transport_identity: &PeerIdentity,
    report: SignedStatisticsReport,
    request_timeout: Duration,
) -> Result<()> {
    let report_identity = identity_from_certificate_pem(&report.certificate_pem)?;
    let verified = report.verify(&report_identity.signing_public_key)?;
    if report_identity.role != Role::Travel
        || report_identity.id != transport_identity.id
        || report_identity.certificate_sha256 != transport_identity.certificate_sha256
        || verified.payload.reporter_role != Role::Travel
        || verified.payload.reporter_id != transport_identity.id
        || verified.payload.deployment_id.as_str() != state.deployment_trust.deployment_id.as_str()
    {
        bail!("Travel statistics report is not bound to its authenticated session");
    }
    let digest = verified.digest_sha256;
    let server = state
        .server_session
        .lock()
        .await
        .as_ref()
        .map(|session| session.tx.clone())
        .ok_or_else(|| anyhow!("Server is unavailable for statistics reporting"))?;
    let (response, receive) = oneshot::channel();
    if state
        .report_requests
        .lock()
        .await
        .insert(digest.clone(), response)
        .is_some()
    {
        bail!("duplicate in-flight Travel statistics report");
    }
    if server
        .send(ControlMessage::StatisticsReport { report })
        .await
        .is_err()
    {
        state.report_requests.lock().await.remove(&digest);
        bail!("Server connection closed during statistics reporting");
    }
    match timeout(request_timeout, receive).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => bail!(error),
        Ok(Err(_)) => bail!("statistics response channel closed"),
        Err(_) => {
            state.report_requests.lock().await.remove(&digest);
            bail!("statistics report timed out")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_remote_enrollment(
    state: &Arc<State>,
    request_id: Uuid,
    travel_id: &str,
    travel_session_id: Uuid,
    credential_id: Uuid,
    home_id: String,
    request_json: Vec<u8>,
    request_timeout: Duration,
) -> Result<RemoteEnrollmentResponse> {
    if request_id.is_nil() || request_json.is_empty() || request_json.len() > 512 * 1024 {
        bail!("remote enrollment request is missing or oversized");
    }
    let server = state
        .server_session
        .lock()
        .await
        .as_ref()
        .map(|session| session.tx.clone())
        .ok_or_else(|| anyhow!("Server is unavailable for remote enrollment"))?;
    let (response, receive) = oneshot::channel();
    if state
        .enrollment_requests
        .lock()
        .await
        .insert(request_id, response)
        .is_some()
    {
        bail!("remote enrollment request is already in flight");
    }
    if server
        .send(ControlMessage::RemoteEnrollmentSubmit {
            request_id,
            travel_id: travel_id.to_owned(),
            travel_session_id,
            credential_id,
            home_id,
            request_json,
        })
        .await
        .is_err()
    {
        state.enrollment_requests.lock().await.remove(&request_id);
        bail!("Server connection closed during remote enrollment");
    }
    match timeout(request_timeout, receive).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => bail!("remote enrollment response channel closed"),
        Err(_) => {
            state.enrollment_requests.lock().await.remove(&request_id);
            bail!("remote enrollment request timed out")
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn forward_remote_enrollment_installed(
    state: &Arc<State>,
    request_id: Uuid,
    travel_id: &str,
    travel_session_id: Uuid,
    credential_id: Uuid,
    home_id: String,
    request_timeout: Duration,
) -> Result<RemoteEnrollmentInstalledResponse> {
    if request_id.is_nil() {
        bail!("remote enrollment install request is invalid");
    }
    let server = state
        .server_session
        .lock()
        .await
        .as_ref()
        .map(|session| session.tx.clone())
        .ok_or_else(|| anyhow!("Server is unavailable for remote enrollment acknowledgement"))?;
    let (response, receive) = oneshot::channel();
    if state
        .enrollment_install_requests
        .lock()
        .await
        .insert(request_id, response)
        .is_some()
    {
        bail!("remote enrollment install acknowledgement is already in flight");
    }
    if server
        .send(ControlMessage::RemoteEnrollmentInstalled {
            request_id,
            travel_id: travel_id.to_owned(),
            travel_session_id,
            credential_id,
            home_id,
        })
        .await
        .is_err()
    {
        state
            .enrollment_install_requests
            .lock()
            .await
            .remove(&request_id);
        bail!("Server connection closed during remote enrollment acknowledgement");
    }
    match timeout(request_timeout, receive).await {
        Ok(Ok(response)) => Ok(response),
        Ok(Err(_)) => bail!("remote enrollment acknowledgement channel closed"),
        Err(_) => {
            state
                .enrollment_install_requests
                .lock()
                .await
                .remove(&request_id);
            bail!("remote enrollment acknowledgement timed out")
        }
    }
}

fn record_relay_metric(
    state: &Arc<State>,
    family: &str,
    travel_id: &str,
    home_id: &str,
    value: u64,
) {
    record_relay_metric_sample(state, family, travel_id, home_id, value, None);
}

fn record_relay_metric_sample(
    state: &Arc<State>,
    family: &str,
    travel_id: &str,
    home_id: &str,
    value: u64,
    histogram_sample: Option<u64>,
) {
    let mut dimensions = BTreeMap::new();
    dimensions.insert("travel_id".to_owned(), travel_id.to_owned());
    dimensions.insert("home_id".to_owned(), home_id.to_owned());
    if let Ok(now) = unix_time_secs() {
        state
            .statistics
            .record(now, family, dimensions, value, histogram_sample);
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatisticsQuery {
    #[serde(default = "default_statistics_period")]
    period: String,
}

fn default_statistics_period() -> String {
    "day".to_owned()
}

#[derive(Serialize)]
struct RelayStatisticsResponse {
    period: String,
    from_unix_secs: u64,
    to_unix_secs: u64,
    dropped_events: u64,
    overview: Vec<MetricRollup>,
    breakdowns: Vec<MetricRollup>,
    points: Vec<MetricPoint>,
}

async fn run_statistics_ui(address: String, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind Relay statistics UI {address}"))?;
    let app = Router::new()
        .route("/", get(relay_statistics_page))
        .route("/api/statistics", get(relay_statistics_api))
        .with_state(state);
    info!(%address, "Relay statistics UI ready");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn relay_statistics_page() -> Html<String> {
    Html(statistics_dashboard_html(
        "Relay statistics",
        "Forwarded encrypted business transport, route outcomes, and per-Travel/Home load. Service IDs remain end-to-end encrypted.",
        false,
    ))
}

async fn relay_statistics_api(
    AxumState(state): AxumState<Arc<State>>,
    Query(query): Query<StatisticsQuery>,
) -> impl IntoResponse {
    let now = unix_time_secs().unwrap_or_default();
    let duration = match query.period.as_str() {
        "week" => 7 * 24 * 60 * 60,
        "month" => 31 * 24 * 60 * 60,
        "year" => 366 * 24 * 60 * 60,
        _ => 24 * 60 * 60,
    };
    let from = now.saturating_sub(duration);
    let statistics = Arc::clone(&state.statistics);
    let points = tokio::task::spawn_blocking(move || statistics.query(from, now))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    Json(RelayStatisticsResponse {
        period: query.period,
        from_unix_secs: from,
        to_unix_secs: now,
        dropped_events: state.statistics.dropped_events(),
        overview: summarize_metric_points(&points, false),
        breakdowns: summarize_metric_points(&points, true),
        points,
    })
}

#[allow(clippy::too_many_lines)]
async fn handle_travel(
    stream: TlsStream<TcpStream>,
    identity: PeerIdentity,
    mut authorization_rx: watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    config: &Config,
    state: Arc<State>,
) -> Result<()> {
    let travel_id = identity.id.clone();
    let mut session_credential_id = authorize_management_identity(&identity, &authorization_rx)?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    let (travel_session_id, purpose) = match reader
        .read_with_timeout::<ControlMessage>(Duration::from_secs(config.handshake_timeout_secs))
        .await?
    {
        ControlMessage::TravelHello {
            protocol_version,
            id,
            session_id,
            purpose,
        } if protocol_version == CONTROL_PROTOCOL_VERSION && id == travel_id => {
            (session_id, purpose)
        }
        _ => bail!("travel HELLO does not match its certificate"),
    };
    let lease_id = (purpose == TravelConnectionPurpose::Catalog).then(Uuid::new_v4);
    let initial_snapshot = match authorize_travel_session(
        session_credential_id,
        &travel_id,
        travel_session_id,
        lease_id,
        config,
        &state,
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let reason = error.clone();
            write_json(
                &mut writer,
                &ControlMessage::TravelHelloDenied {
                    reason: reason.clone(),
                },
                CONTROL_FRAME_LIMIT,
            )
            .await?;
            bail!(reason);
        }
    };
    write_json(
        &mut writer,
        &ControlMessage::TravelHelloAccepted {
            relay_id: config.id.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    if purpose == TravelConnectionPurpose::Catalog {
        info!(
            event = "travel_session_accepted",
            %travel_id,
            "accepted primary Travel login session"
        );
    }
    write_json(
        &mut writer,
        &ControlMessage::ControlSnapshot {
            snapshot: initial_snapshot,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;

    let mut liveness = interval(Duration::from_secs(10));
    let mut last_received = Instant::now();
    let result: Result<()> = async {
        loop {
            tokio::select! {
            message = reader.read::<ControlMessage>() => {
                last_received = Instant::now();
                match message? {
                    ControlMessage::TravelRouteRequest {
                        request_id,
                        travel_id: declared_id,
                        travel_session_id: declared_session,
                        home_id,
                    } if purpose == TravelConnectionPurpose::Route
                        && declared_id == travel_id
                        && declared_session == travel_session_id => {
                        let credential_id = authorize_management_for_home(
                            &identity,
                            &authorization_rx,
                            &home_id,
                        )?;
                        handle_travel_route(
                            TravelRouteContext {
                                request: request_id,
                                credential: credential_id,
                                travel: travel_id.clone(),
                                travel_session: travel_session_id,
                                home: home_id,
                            },
                            config,
                            &state,
                            &mut writer,
                        )
                        .await?;
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(
                            &mut writer,
                            &ControlMessage::HeartbeatAck { nonce },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
                    ControlMessage::StatisticsReport { report }
                        if purpose == TravelConnectionPurpose::Catalog => {
                        let digest_sha256 = report.digest_sha256().unwrap_or_default();
                        let result = forward_travel_statistics(
                            &state,
                            &identity,
                            report,
                            Duration::from_secs(config.handshake_timeout_secs),
                        )
                        .await;
                        write_json(
                            &mut writer,
                            &ControlMessage::StatisticsReportAck {
                                digest_sha256,
                                accepted: result.is_ok(),
                                error: result.err().map(|error| error.to_string()),
                            },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
                    }
                    ControlMessage::TravelEnrollmentSubmit {
                        request_id,
                        travel_id: declared_id,
                        travel_session_id: declared_session,
                        home_id,
                        request_json,
                    } if purpose == TravelConnectionPurpose::Catalog
                        && declared_id == travel_id
                        && declared_session == travel_session_id => {
                        let response = forward_remote_enrollment(
                            &state,
                            request_id,
                            &travel_id,
                            travel_session_id,
                            session_credential_id,
                            home_id,
                            request_json,
                            Duration::from_secs(config.handshake_timeout_secs.saturating_mul(2)),
                        )
                        .await?;
                        write_json(
                            &mut writer,
                            &ControlMessage::RemoteEnrollmentResult {
                                request_id,
                                accepted: response.accepted,
                                response_json: response.response_json,
                                error: response.error,
                            },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
                    }
                    ControlMessage::RemoteEnrollmentInstalled {
                        request_id,
                        travel_id: declared_id,
                        travel_session_id: declared_session,
                        credential_id,
                        home_id,
                    } if purpose == TravelConnectionPurpose::Catalog
                        && declared_id == travel_id
                        && declared_session == travel_session_id
                        && credential_id == session_credential_id => {
                        let response = forward_remote_enrollment_installed(
                            &state,
                            request_id,
                            &travel_id,
                            travel_session_id,
                            credential_id,
                            home_id,
                            Duration::from_secs(config.handshake_timeout_secs.saturating_mul(2)),
                        )
                        .await?;
                        write_json(
                            &mut writer,
                            &ControlMessage::RemoteEnrollmentInstalledAck {
                                request_id,
                                accepted: response.accepted,
                                error: response.error,
                            },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
                    }
                    _ => bail!("unexpected message from travel agent"),
                }
            }
            changed = authorization_rx.changed() => {
                changed.map_err(|_| anyhow!("Travel authorization publisher closed"))?;
                let selected_credential =
                    match authorize_management_identity(&identity, &authorization_rx) {
                        Ok(credential_id) => credential_id,
                        Err(error) => {
                            // The last grant for this authenticated Travel identity was
                            // removed. Ask Server to sign the now-empty catalog before
                            // closing the primary session, so Travel cannot retain stale
                            // permissions and does not have to trust a Relay-originated
                            // denial as control-plane state.
                            if let Some(lease_id) = lease_id
                                && let Ok(snapshot) = authorize_travel_session(
                                    session_credential_id,
                                    &travel_id,
                                    travel_session_id,
                                    Some(lease_id),
                                    config,
                                    &state,
                                )
                                .await
                                {
                                    write_json(
                                        &mut writer,
                                        &ControlMessage::ControlSnapshot { snapshot },
                                        CONTROL_FRAME_LIMIT,
                                    )
                                    .await?;
                            }
                            return Err(error);
                        }
                    };
                session_credential_id = selected_credential;
                if let Some(lease_id) = lease_id {
                    let snapshot = authorize_travel_session(
                        session_credential_id,
                        &travel_id,
                        travel_session_id,
                        Some(lease_id),
                        config,
                        &state,
                    )
                    .await
                    .map_err(anyhow::Error::msg)?;
                    write_json(
                        &mut writer,
                        &ControlMessage::ControlSnapshot { snapshot },
                        CONTROL_FRAME_LIMIT,
                    )
                    .await?;
                }
            }
            _ = liveness.tick() => {
                if last_received.elapsed() > Duration::from_secs(30) {
                    bail!("travel management heartbeat timed out");
                }
                if let Some(lease_id) = lease_id {
                    // Credentials can expire without producing a new authorization snapshot.
                    // Re-select on every lease renewal so another active grant for the same
                    // Travel identity can take over without forcing a login interruption.
                    session_credential_id = authorize_management_identity(
                        &identity,
                        &authorization_rx,
                    )?;
                    let snapshot = authorize_travel_session(
                        session_credential_id,
                        &travel_id,
                        travel_session_id,
                        Some(lease_id),
                        config,
                        &state,
                    )
                    .await
                    .map_err(anyhow::Error::msg)?;
                    write_json(
                        &mut writer,
                        &ControlMessage::ControlSnapshot { snapshot },
                        CONTROL_FRAME_LIMIT,
                    )
                    .await?;
                }
            }
            }
        }
    }
    .await;
    if let Some(lease_id) = lease_id {
        release_travel_session(&state, &travel_id, travel_session_id, lease_id).await;
    }
    result
}

async fn release_travel_session(
    state: &Arc<State>,
    travel_id: &str,
    travel_session_id: Uuid,
    lease_id: Uuid,
) {
    let server = state
        .server_session
        .lock()
        .await
        .as_ref()
        .map(|session| session.tx.clone());
    if let Some(server) = server {
        let _ = server
            .send(ControlMessage::TravelSessionRelease {
                travel_id: travel_id.to_owned(),
                travel_session_id,
                lease_id,
            })
            .await;
    }
}

async fn authorize_travel_session(
    credential_id: Uuid,
    travel_id: &str,
    travel_session_id: Uuid,
    lease_id: Option<Uuid>,
    config: &Config,
    state: &Arc<State>,
) -> Result<SignedControlSnapshot, String> {
    let server = state
        .server_session
        .lock()
        .await
        .as_ref()
        .map(|session| session.tx.clone())
        .ok_or_else(|| "server is unavailable for Travel session authorization".to_owned())?;
    let request_id = Uuid::new_v4();
    let (tx, rx) = oneshot::channel();
    state.session_requests.lock().await.insert(request_id, tx);
    if server
        .send(ControlMessage::TravelSessionAuthorize {
            request_id,
            travel_id: travel_id.to_owned(),
            travel_session_id,
            credential_id,
            lease_id,
        })
        .await
        .is_err()
    {
        state.session_requests.lock().await.remove(&request_id);
        return Err("server connection closed during Travel session authorization".to_owned());
    }
    match timeout(Duration::from_secs(config.handshake_timeout_secs), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("Travel session response channel closed".to_owned()),
        Err(_) => {
            state.session_requests.lock().await.remove(&request_id);
            Err("Travel session authorization timed out".to_owned())
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_travel_route<W: tokio::io::AsyncWrite + Unpin>(
    request: TravelRouteContext,
    config: &Config,
    state: &Arc<State>,
    writer: &mut W,
) -> Result<()> {
    let Ok(route_permit) = Arc::clone(&state.route_permits).try_acquire_owned() else {
        record_relay_metric(
            state,
            "relay_route_denied",
            &request.travel,
            &request.home,
            1,
        );
        write_json(
            writer,
            &ControlMessage::RouteDenied {
                request_id: request.request,
                reason: "relay pending-route limit reached".to_owned(),
            },
            CONTROL_FRAME_LIMIT,
        )
        .await?;
        return Ok(());
    };
    match request_server_route(&request, state).await {
        Ok(grant) if grant.credential_id == request.credential && grant.home_id == request.home => {
            let now = unix_time_secs()?;
            let remaining = grant.expires_at_unix_secs.saturating_sub(now);
            if remaining == 0 {
                record_relay_metric(
                    state,
                    "relay_route_expired",
                    &request.travel,
                    &request.home,
                    1,
                );
                write_json(
                    writer,
                    &ControlMessage::RouteDenied {
                        request_id: request.request,
                        reason: "Server direct-route grant expired".to_owned(),
                    },
                    CONTROL_FRAME_LIMIT,
                )
                .await?;
                return Ok(());
            }
            let route_id = Uuid::new_v4();
            let mut route_secret = vec![0_u8; 32];
            SystemRandom::new()
                .fill(&mut route_secret)
                .map_err(|_| anyhow!("AWS-LC random generation failed"))?;
            state.routes.lock().await.insert(
                route_id,
                PendingRoute {
                    credential_id: request.credential,
                    travel_id: request.travel.clone(),
                    home_id: request.home.clone(),
                    work_id: grant.work_id,
                    route_secret: route_secret.clone(),
                    work_secret: grant.work_secret,
                    created: Instant::now(),
                    expires: Instant::now()
                        + Duration::from_secs(remaining.min(config.route_ttl_secs)),
                    travel: None,
                    home: None,
                    _permit: route_permit,
                },
            );
            let server = state
                .server_session
                .lock()
                .await
                .as_ref()
                .map(|session| session.tx.clone())
                .ok_or_else(|| anyhow!("server disconnected before Relay work readiness"))?;
            if server
                .send(ControlMessage::RelayWorkReady {
                    request_id: request.request,
                    work_id: grant.work_id,
                })
                .await
                .is_err()
            {
                state.routes.lock().await.remove(&route_id);
                bail!("server disconnected before Relay work readiness");
            }
            write_json(
                writer,
                &ControlMessage::RouteGrant {
                    request_id: request.request,
                    route_id,
                    route_secret,
                    data_addr: config.data_public_addr.clone(),
                },
                CONTROL_FRAME_LIMIT,
            )
            .await?;
        }
        Ok(_) => {
            record_relay_metric(
                state,
                "relay_route_denied",
                &request.travel,
                &request.home,
                1,
            );
            write_json(
                writer,
                &ControlMessage::RouteDenied {
                    request_id: request.request,
                    reason: "Server route credential or Home mismatch".to_owned(),
                },
                CONTROL_FRAME_LIMIT,
            )
            .await?;
        }
        Err(reason) => {
            record_relay_metric(
                state,
                "relay_route_denied",
                &request.travel,
                &request.home,
                1,
            );
            write_json(
                writer,
                &ControlMessage::RouteDenied {
                    request_id: request.request,
                    reason,
                },
                CONTROL_FRAME_LIMIT,
            )
            .await?;
        }
    }
    Ok(())
}

async fn request_server_route(
    request: &TravelRouteContext,
    state: &Arc<State>,
) -> Result<ServerGrant, String> {
    let server = state
        .server_session
        .lock()
        .await
        .as_ref()
        .map(|session| session.tx.clone())
        .ok_or_else(|| "server is unavailable".to_owned())?;
    let (tx, rx) = oneshot::channel();
    state.requests.lock().await.insert(request.request, tx);
    if server
        .send(ControlMessage::RouteRequest {
            request_id: request.request,
            travel_id: request.travel.clone(),
            travel_session_id: request.travel_session,
            credential_id: request.credential,
            home_id: request.home.clone(),
        })
        .await
        .is_err()
    {
        state.requests.lock().await.remove(&request.request);
        return Err("server connection closed".to_owned());
    }
    match timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("route response channel closed".to_owned()),
        Err(_) => {
            state.requests.lock().await.remove(&request.request);
            Err("route request timed out".to_owned())
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn run_data(config: Config, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&config.data_listen)
        .await
        .with_context(|| format!("failed to bind relay data {}", config.data_listen))?;
    info!(address = %config.data_listen, "relay data listener ready");
    let permits = Arc::new(Semaphore::new(config.max_data_connections));
    loop {
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("Relay data connection budget closed"))?;
        let (stream, peer) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let result = async {
                let mut socket = Some(BudgetedSocket {
                    stream,
                    _permit: permit,
                });
                let (preface, mac) = timeout(
                    Duration::from_secs(10),
                    read_preface(
                        &mut socket
                            .as_mut()
                            .ok_or_else(|| anyhow!("missing admitted data socket"))?
                            .stream,
                    ),
                )
                    .await
                    .context("direct route preface timed out")??;
                let pair = {
                    let mut routes = state.routes.lock().await;
                    let route_id = resolve_direct_route_id(&routes, preface).ok_or_else(|| {
                        anyhow!(match preface.side {
                            RouteSide::Travel => "unknown or expired Travel route id",
                            RouteSide::Home => "unknown or expired Home work id",
                        })
                    })?;
                    let route = routes
                        .get_mut(&route_id)
                        .ok_or_else(|| anyhow!("unknown or expired Travel route id"))?;
                    if route.expires <= Instant::now() {
                        bail!("direct route ticket expired");
                    }
                    ensure_credential_active(
                        &state.authorization_tx.subscribe(),
                        route.credential_id,
                    )?;
                    validate_direct_route_admission(
                        DirectRouteAdmission {
                            route_id,
                            work_id: route.work_id,
                            route_secret: &route.route_secret,
                            work_secret: &route.work_secret,
                            travel_present: route.travel.is_some(),
                            home_present: route.home.is_some(),
                            expires: route.expires,
                        },
                        preface,
                        &mac,
                        Instant::now(),
                    )?;
                    match preface.side {
                        RouteSide::Travel if route.travel.is_none() => {
                            route.travel = socket.take();
                        }
                        RouteSide::Home if route.home.is_none() => {
                            route.home = socket.take();
                        }
                        RouteSide::Travel | RouteSide::Home => {
                            bail!("duplicate direct route side");
                        }
                    }
                    if route.travel.is_some() && route.home.is_some() {
                        let mut completed = routes
                            .remove(&route_id)
                            .ok_or_else(|| anyhow!("direct route vanished during pairing"))?;
                        let setup_latency_ms =
                            u64::try_from(completed.created.elapsed().as_millis())
                                .unwrap_or(u64::MAX);
                        Some((
                            route_id,
                            completed.work_id,
                            completed.credential_id,
                            completed.travel_id,
                            completed.home_id,
                            completed
                                .travel
                                .take()
                                .ok_or_else(|| anyhow!("paired route is missing Travel"))?,
                            completed
                                .home
                                .take()
                                .ok_or_else(|| anyhow!("paired route is missing Home"))?,
                            setup_latency_ms,
                        ))
                    } else {
                        None
                    }
                };
                let Some((route_id, work_id, credential_id, travel_id, home_id, mut travel, mut home, setup_latency_ms)) = pair
                else {
                    return Ok(());
                };
                let authorization_rx = state.authorization_tx.subscribe();
                info!(%route_id, %work_id, %home_id, "relay paired direct Travel/Home sockets");
                record_relay_metric(&state, "relay_route_paired", &travel_id, &home_id, 1);
                record_relay_metric_sample(
                    &state,
                    "relay_route_setup_latency_ms",
                    &travel_id,
                    &home_id,
                    setup_latency_ms,
                    Some(setup_latency_ms),
                );
                let transferred = tokio::select! {
                    result = zero_copy_or_portable(&mut travel.stream, &mut home.stream) => Some(result?),
                    () = wait_until_authorization_inactive(authorization_rx, credential_id) => {
                        info!(event = "revoked_carrier_closed", %route_id, %credential_id, "Relay closed direct data carrier for inactive Travel credential");
                        record_relay_metric(&state, "relay_route_revoked", &travel_id, &home_id, 1);
                        None
                    }
                };
                if let Some((travel_to_home, home_to_travel)) = transferred {
                    record_relay_metric(&state, "relay_transport_upload_bytes", &travel_id, &home_id, travel_to_home);
                    record_relay_metric(&state, "relay_transport_download_bytes", &travel_id, &home_id, home_to_travel);
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                warn!(%peer, %error, "relay data connection closed");
            }
        });
    }
}

fn resolve_direct_route_id(
    routes: &HashMap<Uuid, PendingRoute>,
    preface: flowsplice_core::route::RoutePreface,
) -> Option<Uuid> {
    match preface.side {
        RouteSide::Travel => routes.contains_key(&preface.id).then_some(preface.id),
        RouteSide::Home => routes
            .iter()
            .find_map(|(route_id, route)| (route.work_id == preface.id).then_some(*route_id)),
    }
}

#[derive(Clone, Copy)]
struct DirectRouteAdmission<'a> {
    route_id: Uuid,
    work_id: Uuid,
    route_secret: &'a [u8],
    work_secret: &'a [u8],
    travel_present: bool,
    home_present: bool,
    expires: Instant,
}

fn validate_direct_route_admission(
    admission: DirectRouteAdmission<'_>,
    preface: flowsplice_core::route::RoutePreface,
    mac: &[u8],
    now: Instant,
) -> Result<()> {
    let (expected_id, expected_secret, occupied) = match preface.side {
        RouteSide::Travel => (
            admission.route_id,
            admission.route_secret,
            admission.travel_present,
        ),
        RouteSide::Home => (
            admission.work_id,
            admission.work_secret,
            admission.home_present,
        ),
    };
    if preface.id != expected_id {
        bail!("direct route preface identifier is bound to the wrong side");
    }
    if admission.expires <= now {
        bail!("direct route ticket expired");
    }
    if !verify_preface(preface, mac, expected_secret) {
        bail!("invalid direct route preface MAC");
    }
    if occupied {
        bail!("duplicate direct route side");
    }
    Ok(())
}

#[cfg(target_os = "linux")]
async fn zero_copy_or_portable(
    left: &mut TcpStream,
    right: &mut TcpStream,
) -> std::io::Result<(u64, u64)> {
    tokio_splice::zero_copy_bidirectional(left, right).await
}

#[cfg(not(target_os = "linux"))]
async fn zero_copy_or_portable(
    left: &mut TcpStream,
    right: &mut TcpStream,
) -> std::io::Result<(u64, u64)> {
    copy_bidirectional(left, right).await
}

async fn cleanup_routes(state: Arc<State>) -> Result<()> {
    let mut timer = interval(Duration::from_secs(1));
    loop {
        timer.tick().await;
        let now = Instant::now();
        state.routes.lock().await.retain(|route_id, route| {
            let keep = route.expires > now;
            if !keep {
                record_relay_metric(
                    &state,
                    "relay_route_expired",
                    &route.travel_id,
                    &route.home_id,
                    1,
                );
                warn!(%route_id, "expired unused relay route");
            }
            keep
        });
    }
}

fn authorize_management_identity(
    identity: &PeerIdentity,
    authorization_rx: &watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
) -> Result<Uuid> {
    let now = unix_time_secs()?;
    let authorization = authorization_rx
        .borrow()
        .clone()
        .ok_or_else(|| anyhow!("Travel authorization has not synchronized from Server"))?;
    authorization
        .authorize_management_all(identity, now)?
        .into_iter()
        .max_by_key(|credential| credential.not_after_unix_secs)
        .map(|credential| credential.credential_id)
        .ok_or_else(|| anyhow!("Travel identity has no active credential"))
}

fn authorize_management_for_home(
    identity: &PeerIdentity,
    authorization_rx: &watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    home_id: &str,
) -> Result<Uuid> {
    let now = unix_time_secs()?;
    let authorization = authorization_rx
        .borrow()
        .clone()
        .ok_or_else(|| anyhow!("Travel authorization has not synchronized from Server"))?;
    Ok(authorization
        .authorize_management_for_home(identity, home_id, now)?
        .credential_id)
}

fn ensure_credential_active(
    authorization_rx: &watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    credential_id: Uuid,
) -> Result<u64> {
    let now = unix_time_secs()?;
    let authorization = authorization_rx
        .borrow()
        .clone()
        .ok_or_else(|| anyhow!("Travel authorization has not synchronized from Server"))?;
    if !authorization.is_active(credential_id, now) {
        bail!("Travel credential is revoked, expired, or not yet valid");
    }
    authorization
        .credential(credential_id)
        .map(|credential| credential.not_after_unix_secs)
        .ok_or_else(|| anyhow!("unknown Travel credential"))
}

async fn sleep_until_unix(not_after_unix_secs: u64) {
    let now = unix_time_secs().unwrap_or(not_after_unix_secs);
    tokio::time::sleep(Duration::from_secs(not_after_unix_secs.saturating_sub(now))).await;
}

async fn wait_until_authorization_inactive(
    mut authorization_rx: watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    credential_id: Uuid,
) {
    loop {
        let Ok(not_after) = ensure_credential_active(&authorization_rx, credential_id) else {
            return;
        };
        tokio::select! {
            changed = authorization_rx.changed() => {
                if changed.is_err() {
                    return;
                }
            }
            () = sleep_until_unix(not_after) => return,
        }
    }
}

async fn apply_authorization_snapshot(
    state: &Arc<State>,
    config: &Config,
    snapshot: TravelAuthorizationSnapshot,
) -> Result<u64> {
    let authorities = state
        .deployment_trust
        .travel_authorities_with_home_delegations(
            &snapshot.home_endpoint_credentials,
            unix_time_secs()?,
        )?;
    let authorization = VerifiedAuthorization::verify(
        &snapshot,
        &authorities,
        &state.deployment_trust.deployment_id,
    )?;
    let mut cache = state.authorization_cache.lock().await;
    let proposed_cache = cache.accept(&authorization)?;
    if proposed_cache != *cache {
        store_json_atomic(&config.travel_authorization_cache, &proposed_cache)?;
        *cache = proposed_cache;
    }
    drop(cache);
    let now = unix_time_secs()?;
    state
        .routes
        .lock()
        .await
        .retain(|_, route| authorization.is_active(route.credential_id, now));
    let generation = authorization.generation();
    state
        .authorization_tx
        .send_replace(Some(Arc::new(authorization)));
    info!(
        event = "travel_authorization_applied",
        generation,
        revoked = snapshot.revocations.len(),
        credentials = snapshot.credentials.len(),
        "Relay applied Travel authorization state without restart"
    );
    Ok(generation)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use flowsplice_core::route::{RoutePreface, read_preface, write_preface};
    use tokio::io::duplex;

    async fn authenticated_preface(
        side: RouteSide,
        id: Uuid,
        secret: &[u8],
    ) -> (RoutePreface, [u8; 32]) {
        let (mut left, mut right) = duplex(128);
        let (write, read) = tokio::join!(
            write_preface(&mut left, side, id, secret),
            read_preface(&mut right)
        );
        write.unwrap();
        read.unwrap()
    }

    fn admission<'a>(
        route_id: Uuid,
        work_id: Uuid,
        route_secret: &'a [u8],
        work_secret: &'a [u8],
        travel_present: bool,
        home_present: bool,
        expires: Instant,
    ) -> DirectRouteAdmission<'a> {
        DirectRouteAdmission {
            route_id,
            work_id,
            route_secret,
            work_secret,
            travel_present,
            home_present,
            expires,
        }
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn direct_route_admission_rejects_swaps_duplicates_wrong_ids_and_expiry() {
        let route_id = Uuid::new_v4();
        let work_id = Uuid::new_v4();
        let route_secret = [7_u8; 32];
        let work_secret = [9_u8; 32];
        let now = Instant::now();
        let expires = now + Duration::from_secs(30);
        let (travel, travel_mac) =
            authenticated_preface(RouteSide::Travel, route_id, &route_secret).await;
        let (home, home_mac) = authenticated_preface(RouteSide::Home, work_id, &work_secret).await;

        assert!(
            validate_direct_route_admission(
                admission(
                    route_id,
                    work_id,
                    &route_secret,
                    &work_secret,
                    false,
                    false,
                    expires,
                ),
                travel,
                &travel_mac,
                now,
            )
            .is_ok()
        );
        assert!(
            validate_direct_route_admission(
                admission(
                    route_id,
                    work_id,
                    &route_secret,
                    &work_secret,
                    false,
                    false,
                    expires,
                ),
                home,
                &home_mac,
                now,
            )
            .is_ok()
        );

        let (_, swapped_secret_mac) =
            authenticated_preface(RouteSide::Travel, route_id, &work_secret).await;
        assert!(
            validate_direct_route_admission(
                admission(
                    route_id,
                    work_id,
                    &route_secret,
                    &work_secret,
                    false,
                    false,
                    expires,
                ),
                travel,
                &swapped_secret_mac,
                now,
            )
            .unwrap_err()
            .to_string()
            .contains("MAC")
        );

        let (wrong_side, wrong_side_mac) =
            authenticated_preface(RouteSide::Home, route_id, &route_secret).await;
        assert!(
            validate_direct_route_admission(
                admission(
                    route_id,
                    work_id,
                    &route_secret,
                    &work_secret,
                    false,
                    false,
                    expires,
                ),
                wrong_side,
                &wrong_side_mac,
                now,
            )
            .unwrap_err()
            .to_string()
            .contains("wrong side")
        );

        assert!(
            validate_direct_route_admission(
                admission(
                    route_id,
                    work_id,
                    &route_secret,
                    &work_secret,
                    true,
                    false,
                    expires,
                ),
                travel,
                &travel_mac,
                now,
            )
            .unwrap_err()
            .to_string()
            .contains("duplicate")
        );
        assert!(
            validate_direct_route_admission(
                admission(
                    route_id,
                    work_id,
                    &route_secret,
                    &work_secret,
                    false,
                    false,
                    now,
                ),
                home,
                &home_mac,
                now,
            )
            .unwrap_err()
            .to_string()
            .contains("expired")
        );

        // Both valid arrival orders are admitted: each side remains independently one-use.
        assert!(
            validate_direct_route_admission(
                admission(
                    route_id,
                    work_id,
                    &route_secret,
                    &work_secret,
                    false,
                    true,
                    expires,
                ),
                travel,
                &travel_mac,
                now,
            )
            .is_ok()
        );
        assert!(
            validate_direct_route_admission(
                admission(
                    route_id,
                    work_id,
                    &route_secret,
                    &work_secret,
                    true,
                    false,
                    expires,
                ),
                home,
                &home_mac,
                now,
            )
            .is_ok()
        );
    }

    #[test]
    fn completed_route_replay_is_unknown_and_pending_limit_is_released() {
        let route_id = Uuid::new_v4();
        let work_id = Uuid::new_v4();
        let permits = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&permits).try_acquire_owned().unwrap();
        let mut routes = HashMap::new();
        routes.insert(
            route_id,
            PendingRoute {
                credential_id: Uuid::new_v4(),
                travel_id: "travel-1".to_owned(),
                home_id: "home-1".to_owned(),
                work_id,
                route_secret: vec![7; 32],
                work_secret: vec![9; 32],
                created: Instant::now(),
                expires: Instant::now() + Duration::from_secs(30),
                travel: None,
                home: None,
                _permit: permit,
            },
        );
        let travel = RoutePreface {
            side: RouteSide::Travel,
            id: route_id,
        };
        let home = RoutePreface {
            side: RouteSide::Home,
            id: work_id,
        };
        assert_eq!(resolve_direct_route_id(&routes, travel), Some(route_id));
        assert_eq!(resolve_direct_route_id(&routes, home), Some(route_id));
        assert!(Arc::clone(&permits).try_acquire_owned().is_err());

        routes.remove(&route_id);
        assert_eq!(resolve_direct_route_id(&routes, travel), None);
        assert_eq!(resolve_direct_route_id(&routes, home), None);
        assert!(Arc::clone(&permits).try_acquire_owned().is_ok());
    }
}
