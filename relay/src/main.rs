#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    fs,
    io::{self, IsTerminal},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::rand::{SecureRandom, SystemRandom};
use clap::Parser;
use flowsplice_core::{
    CONTROL_FRAME_LIMIT,
    authorization::{
        AuthorizationCache, TravelAuthorizationSnapshot, TrustedTravelAuthority,
        VerifiedAuthorization, load_json, store_json_atomic, unix_time_secs,
    },
    config::load_toml,
    deployment::{DeploymentTrust, SignedControlSnapshot, SignedDeploymentTrust},
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{ControlMessage, Role, TravelConnectionPurpose},
    route::{RouteSide, read_preface, verify_preface, write_preface},
    tls::{PeerIdentity, peer_identity, require_peer, server_acceptor, validate_spki_pins},
};
use serde::Deserialize;
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
    #[arg(long)]
    check_config: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    id: String,
    management_listen: String,
    data_listen: String,
    data_public_addr: String,
    server_data_addr: String,
    server_id: String,
    cert: PathBuf,
    key: PathBuf,
    management_ca: PathBuf,
    deployment_root_public_key: PathBuf,
    deployment_trust: PathBuf,
    #[serde(default)]
    server_spki_pins: Vec<String>,
    travel_authorization_cache: PathBuf,
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
    home_id: String,
    work_id: Uuid,
    route_secret: Vec<u8>,
    work_secret: Vec<u8>,
    expires: Instant,
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
    travel_authorities: Arc<Vec<TrustedTravelAuthority>>,
    deployment_id: Arc<String>,
    route_permits: Arc<Semaphore>,
}

impl State {
    fn new(
        authorization_cache: AuthorizationCache,
        deployment_id: String,
        travel_authorities: Vec<TrustedTravelAuthority>,
        max_pending_routes: usize,
    ) -> Self {
        let (authorization_tx, _) = watch::channel(None);
        Self {
            server_session: Mutex::new(None),
            requests: Mutex::new(HashMap::new()),
            session_requests: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            authorization_tx,
            authorization_cache: Mutex::new(authorization_cache),
            travel_authorities: Arc::new(travel_authorities),
            deployment_id: Arc::new(deployment_id),
            route_permits: Arc::new(Semaphore::new(max_pending_routes)),
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
    let authorization_cache = if config.travel_authorization_cache.exists() {
        load_json(&config.travel_authorization_cache)?
    } else {
        AuthorizationCache::default()
    };
    let state = Arc::new(State::new(
        authorization_cache,
        deployment_trust.deployment_id,
        deployment_trust.travel_authorities,
        config.max_pending_routes,
    ));
    let trust_expiry = monitor_trust_expiry(deployment_trust.not_after_unix_secs);
    tokio::try_join!(
        run_management(config.clone(), Arc::clone(&state)),
        run_data(config, Arc::clone(&state)),
        cleanup_routes(state),
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
    if config.id.is_empty()
        || config.server_id.is_empty()
        || config.data_public_addr.is_empty()
        || config.server_data_addr.is_empty()
    {
        bail!("Relay ids and advertised/Server addresses must be non-empty");
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
    let _ = server_acceptor(&config.cert, &config.key, &config.management_ca)?;
    Ok(trust)
}

async fn run_management(config: Config, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&config.management_listen)
        .await
        .with_context(|| format!("failed to bind management {}", config.management_listen))?;
    let acceptor = server_acceptor(&config.cert, &config.key, &config.management_ca)?;
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
        ControlMessage::Hello { role, id } if role == Role::Server && id == server_id => {}
        _ => bail!("server HELLO does not match its certificate"),
    }
    write_json(
        &mut writer,
        &ControlMessage::Hello {
            role: Role::Relay,
            id: config.id.clone(),
        },
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
                        ControlMessage::ServerRouteGrant { request_id, work_id, work_secret, credential_id, home_id } => {
                            if let Some(waiter) = state.requests.lock().await.remove(&request_id) {
                                let _ = waiter.send(Ok(ServerGrant { work_id, work_secret, credential_id, home_id }));
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
                        _ => bail!("unexpected message from server"),
                    }
                }
                _ = liveness.tick() => {
                    if last_received.elapsed() > Duration::from_secs(30) {
                        bail!("server control heartbeat timed out");
                    }
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
            id,
            session_id,
            purpose,
        } if id == travel_id => (session_id, purpose),
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

async fn handle_travel_route<W: tokio::io::AsyncWrite + Unpin>(
    request: TravelRouteContext,
    config: &Config,
    state: &Arc<State>,
    writer: &mut W,
) -> Result<()> {
    let Ok(route_permit) = Arc::clone(&state.route_permits).try_acquire_owned() else {
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
            let route_id = Uuid::new_v4();
            let mut route_secret = vec![0_u8; 32];
            SystemRandom::new()
                .fill(&mut route_secret)
                .map_err(|_| anyhow!("AWS-LC random generation failed"))?;
            state.routes.lock().await.insert(
                route_id,
                PendingRoute {
                    credential_id: request.credential,
                    home_id: request.home.clone(),
                    work_id: grant.work_id,
                    route_secret: route_secret.clone(),
                    work_secret: grant.work_secret,
                    expires: Instant::now() + Duration::from_secs(config.route_ttl_secs),
                    _permit: route_permit,
                },
            );
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
        let (mut travel, peer) = listener.accept().await?;
        let config = config.clone();
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let _permit = permit;
            let result = async {
                let (preface, mac) = timeout(Duration::from_secs(10), read_preface(&mut travel))
                    .await
                    .context("travel route preface timed out")??;
                if preface.side != RouteSide::Travel {
                    bail!("relay data ingress requires travel side");
                }
                let route = state
                    .routes
                    .lock()
                    .await
                    .remove(&preface.id)
                    .ok_or_else(|| anyhow!("unknown, expired, or already consumed route"))?;
                if route.expires <= Instant::now() {
                    bail!("route ticket expired");
                }
                let authorization_rx = state.authorization_tx.subscribe();
                ensure_credential_active(&authorization_rx, route.credential_id)?;
                if !verify_preface(preface, &mac, &route.route_secret) {
                    bail!("invalid route ticket MAC");
                }
                let mut server = timeout(
                    Duration::from_secs(config.handshake_timeout_secs),
                    async {
                        let mut server = TcpStream::connect(&config.server_data_addr).await?;
                        write_preface(
                            &mut server,
                            RouteSide::Relay,
                            route.work_id,
                            &route.work_secret,
                        )
                        .await?;
                        Ok::<_, anyhow::Error>(server)
                    },
                )
                .await
                .context("Server data route setup timed out")??;
                info!(route_id = %preface.id, work_id = %route.work_id, home_id = %route.home_id, "relay entered opaque forwarding");
                tokio::select! {
                    result = zero_copy_or_portable(&mut travel, &mut server) => result?,
                    () = wait_until_authorization_inactive(authorization_rx, route.credential_id) => {
                        info!(event = "revoked_carrier_closed", route_id = %preface.id, credential_id = %route.credential_id, "Relay closed data carrier for inactive Travel credential");
                    }
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

#[cfg(target_os = "linux")]
async fn zero_copy_or_portable(left: &mut TcpStream, right: &mut TcpStream) -> std::io::Result<()> {
    tokio_splice::zero_copy_bidirectional(left, right)
        .await
        .map(|_| ())
}

#[cfg(not(target_os = "linux"))]
async fn zero_copy_or_portable(left: &mut TcpStream, right: &mut TcpStream) -> std::io::Result<()> {
    copy_bidirectional(left, right).await.map(|_| ())
}

async fn cleanup_routes(state: Arc<State>) -> Result<()> {
    let mut timer = interval(Duration::from_secs(1));
    loop {
        timer.tick().await;
        let now = Instant::now();
        state.routes.lock().await.retain(|route_id, route| {
            let keep = route.expires > now;
            if !keep {
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
    let authorization =
        VerifiedAuthorization::verify(&snapshot, &state.travel_authorities, &state.deployment_id)?;
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
