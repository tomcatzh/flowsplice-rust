#![forbid(unsafe_code)]

use std::{
    collections::{BTreeSet, HashMap},
    fs,
    io::{self, IsTerminal},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    rand::{SecureRandom, SystemRandom},
    signature::{EcdsaKeyPair, KeyPair as _},
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
    authorization::{TravelAuthorizationSnapshot, load_json, store_json_atomic, unix_time_secs},
    config::load_toml,
    deployment::{
        CONTROL_SNAPSHOT_OBJECT_TYPE, CONTROL_SNAPSHOT_VERSION, ControlSnapshotPayload,
        DeploymentTrust, MAX_CONTROL_SNAPSHOT_TTL_SECS, SignedControlSnapshot,
        SignedDeploymentTrust, control_signing_key_from_pkcs8, validate_catalog,
    },
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{
        CONTROL_PROTOCOL_VERSION, Catalog, ControlMessage, HomeCatalog, RelayDirectory,
        RelayEndpoint, Role,
    },
    statistics::{SignedStatisticsReport, statistics_dashboard_html},
    tls::{
        PeerIdentity, identity_client_connector, identity_from_certificate_pem,
        identity_server_name, load_private_key, peer_identity, require_peer, server_acceptor,
    },
};
use flowsplice_storage::{
    AcceptedReport, MetricRollup, ReportAcceptance, StateStore, accepted_reports_as_metric_points,
    summarize_metric_points,
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, Semaphore, mpsc, watch},
    task::JoinSet,
    time::{interval, sleep, timeout},
};
use tokio_rustls::server::TlsStream;
use tracing::{info, warn};
use uuid::Uuid;

mod authorization;

use authorization::ServerAuthorization;

#[derive(Parser)]
#[command(version)]
struct Args {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "server.toml")]
    config: PathBuf,
    #[arg(long)]
    check_config: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    id: String,
    control_listen: String,
    relays: Vec<ConfiguredRelay>,
    cert: PathBuf,
    key: PathBuf,
    management_ca: PathBuf,
    deployment_root_public_key: PathBuf,
    deployment_trust: PathBuf,
    control_signing_key: PathBuf,
    homes: Vec<ConfiguredHome>,
    travel_authorization_state: PathBuf,
    control_generation_state: PathBuf,
    state_store: PathBuf,
    ui_listen: String,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout_secs: u64,
    #[serde(default = "default_work_ttl")]
    work_ttl_secs: u64,
    #[serde(default = "default_max_pending_work")]
    max_pending_work: usize,
    #[serde(default = "default_max_control_connections")]
    max_control_connections: usize,
    #[serde(default = "default_control_snapshot_ttl")]
    control_snapshot_ttl_secs: u64,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredRelay {
    id: String,
    management_addr: String,
    data_public_addr: String,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredHome {
    id: String,
}

const fn default_handshake_timeout() -> u64 {
    10
}

const fn default_work_ttl() -> u64 {
    15
}

const fn default_max_pending_work() -> usize {
    256
}

const fn default_max_control_connections() -> usize {
    256
}

const fn default_control_snapshot_ttl() -> u64 {
    120
}

const TRAVEL_SESSION_LEASE: Duration = Duration::from_secs(45);

struct ControlSigner {
    server_id: String,
    signer_epoch: u64,
    signed_trust: SignedDeploymentTrust,
    root_public_key: String,
    trust: DeploymentTrust,
    key: EcdsaKeyPair,
    next_generation: StdMutex<u64>,
    generation_path: PathBuf,
    ttl_secs: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ControlGenerationState {
    next_generation: u64,
}

impl ControlSigner {
    fn load(config: &Config) -> Result<Self> {
        let root_public_key = fs::read_to_string(&config.deployment_root_public_key)
            .context("failed to read deployment root public key")?;
        let signed_trust: SignedDeploymentTrust =
            flowsplice_core::authorization::load_json(&config.deployment_trust)?;
        let now = unix_time_secs()?;
        let trust = signed_trust.verify(root_public_key.trim(), now)?;
        if fs::read_to_string(&config.management_ca)
            .context("failed to read Server management CA")?
            != trust.management_ca_certificate_pem
        {
            bail!("Server management CA does not match deployment trust");
        }
        let private_key = load_private_key(&config.control_signing_key)
            .context("failed to read Server control signing key")?;
        let key = control_signing_key_from_pkcs8(private_key.secret_der())?;
        let actual_public_key = hex::encode(key.public_key().as_ref());
        let matching_keys = trust
            .server_control_keys
            .iter()
            .filter(|candidate| {
                candidate.server_id == config.id
                    && candidate
                        .public_key
                        .eq_ignore_ascii_case(&actual_public_key)
            })
            .collect::<Vec<_>>();
        let [matching_key] = matching_keys.as_slice() else {
            bail!("Server control signing key must match exactly one deployment-trusted epoch");
        };
        let generation: ControlGenerationState = load_json(&config.control_generation_state)
            .context("failed to load persistent Server control generation")?;
        if generation.next_generation == 0 {
            bail!("Server control generation must be positive");
        }
        Ok(Self {
            server_id: config.id.clone(),
            signer_epoch: matching_key.epoch,
            signed_trust,
            root_public_key: root_public_key.trim().to_owned(),
            trust,
            key,
            next_generation: StdMutex::new(generation.next_generation),
            generation_path: config.control_generation_state.clone(),
            ttl_secs: config.control_snapshot_ttl_secs,
        })
    }

    fn sign(
        &self,
        directory: RelayDirectory,
        catalog: Catalog,
        travel_id: &str,
        travel_management_spki_sha256: &str,
    ) -> Result<SignedControlSnapshot> {
        let now = unix_time_secs()?;
        let current_trust = self.signed_trust.verify(&self.root_public_key, now)?;
        if current_trust != self.trust {
            bail!("Server deployment trust changed unexpectedly in memory");
        }
        let generation = {
            let mut next = self
                .next_generation
                .lock()
                .map_err(|_| anyhow!("Server control generation lock is poisoned"))?;
            let generation = *next;
            let following = generation
                .checked_add(1)
                .ok_or_else(|| anyhow!("Server control generation exhausted"))?;
            store_json_atomic(
                &self.generation_path,
                &ControlGenerationState {
                    next_generation: following,
                },
            )?;
            *next = following;
            generation
        };
        SignedControlSnapshot::sign(
            self.signed_trust.clone(),
            &self.trust,
            &ControlSnapshotPayload {
                version: CONTROL_SNAPSHOT_VERSION,
                object_type: CONTROL_SNAPSHOT_OBJECT_TYPE.to_owned(),
                deployment_id: self.trust.deployment_id.clone(),
                server_id: self.server_id.clone(),
                signer_epoch: self.signer_epoch,
                travel_id: travel_id.to_owned(),
                travel_management_spki_sha256: travel_management_spki_sha256.to_owned(),
                generation,
                issued_at_unix_secs: now,
                expires_at_unix_secs: now.saturating_add(self.ttl_secs),
                relay_directory: directory,
                catalog,
            },
            &self.key,
        )
    }
}

struct PendingRelayGrant {
    relay_id: String,
    work_id: Uuid,
    credential_id: Uuid,
    home: mpsc::Sender<ControlMessage>,
    open_message: ControlMessage,
    expires: Instant,
}

struct PendingEnrollment {
    relay_id: String,
    travel_id: String,
    home_id: String,
    relay: mpsc::Sender<ControlMessage>,
    bootstrap: bool,
    installed: bool,
    expires: Instant,
}

struct RouteRequestContext {
    request: Uuid,
    travel: String,
    travel_session: Uuid,
    credential: Uuid,
    home: String,
}

#[derive(Clone)]
struct HomeSession {
    session_id: Uuid,
    tx: mpsc::Sender<ControlMessage>,
    shutdown: watch::Sender<bool>,
}

#[derive(Default)]
struct HomeRegistry {
    generation: u64,
    sessions: HashMap<String, HomeSession>,
    catalogs: HashMap<String, HomeCatalog>,
}

impl HomeRegistry {
    fn catalog(&self) -> Catalog {
        let mut homes = self.catalogs.values().cloned().collect::<Vec<_>>();
        homes.sort_by(|left, right| left.home_id.cmp(&right.home_id));
        Catalog {
            generation: self.generation,
            homes,
        }
    }

    fn register(
        &mut self,
        home: HomeCatalog,
        session: HomeSession,
    ) -> (Option<HomeSession>, Option<Catalog>) {
        let home_id = home.home_id.clone();
        let previous = self.sessions.insert(home_id.clone(), session);
        let changed = self.catalogs.get(&home_id) != Some(&home);
        self.catalogs.insert(home_id, home);
        if changed {
            self.generation = self.generation.saturating_add(1);
            (previous, Some(self.catalog()))
        } else {
            (previous, None)
        }
    }

    fn update_catalog(&mut self, home: HomeCatalog, session_id: Uuid) -> Option<Catalog> {
        let home_id = home.home_id.clone();
        if self
            .sessions
            .get(&home_id)
            .is_none_or(|session| session.session_id != session_id)
            || self.catalogs.get(&home_id) == Some(&home)
        {
            return None;
        }
        self.catalogs.insert(home_id, home);
        self.generation = self.generation.saturating_add(1);
        Some(self.catalog())
    }

    fn remove(&mut self, home_id: &str, session_id: Uuid) -> Option<Catalog> {
        if self
            .sessions
            .get(home_id)
            .is_none_or(|session| session.session_id != session_id)
        {
            return None;
        }
        self.sessions.remove(home_id);
        if self.catalogs.remove(home_id).is_some() {
            self.generation = self.generation.saturating_add(1);
            return Some(self.catalog());
        }
        None
    }

    fn sender(&self, home_id: &str) -> Option<mpsc::Sender<ControlMessage>> {
        self.sessions.get(home_id).map(|session| session.tx.clone())
    }

    fn senders(&self) -> Vec<(String, mpsc::Sender<ControlMessage>)> {
        self.sessions
            .iter()
            .map(|(home_id, session)| (home_id.clone(), session.tx.clone()))
            .collect()
    }
}

struct TravelSession {
    session_id: Uuid,
    leases: HashMap<Uuid, Instant>,
}

#[derive(Default)]
struct TravelSessionRegistry {
    sessions: HashMap<String, TravelSession>,
}

#[derive(Default)]
struct RelayDirectoryRegistry {
    generation: u64,
    endpoints: HashMap<String, RelayEndpoint>,
}

impl RelayDirectoryRegistry {
    fn register(&mut self, endpoint: RelayEndpoint) -> RelayDirectory {
        let changed = self.endpoints.get(&endpoint.id) != Some(&endpoint);
        if changed {
            self.endpoints.insert(endpoint.id.clone(), endpoint);
            self.generation = self.generation.saturating_add(1);
        }
        self.directory()
    }

    fn directory(&self) -> RelayDirectory {
        let mut relays = self.endpoints.values().cloned().collect::<Vec<_>>();
        relays.sort_by(|left, right| left.id.cmp(&right.id));
        RelayDirectory {
            generation: self.generation,
            relays,
        }
    }
}

impl TravelSessionRegistry {
    fn authorize(
        &mut self,
        travel_id: &str,
        session_id: Uuid,
        lease_id: Option<Uuid>,
        now: Instant,
    ) -> std::result::Result<(), String> {
        self.prune(now);
        if session_id.is_nil() {
            return Err("Travel session id must not be nil".to_owned());
        }
        match self.sessions.get_mut(travel_id) {
            Some(active) if active.session_id != session_id => {
                Err("another session for this Travel credential is already online".to_owned())
            }
            Some(active) => {
                if let Some(lease_id) = lease_id {
                    active.leases.insert(lease_id, now + TRAVEL_SESSION_LEASE);
                }
                Ok(())
            }
            None => {
                let Some(lease_id) = lease_id else {
                    return Err("Travel has no active login session".to_owned());
                };
                self.sessions.insert(
                    travel_id.to_owned(),
                    TravelSession {
                        session_id,
                        leases: HashMap::from([(lease_id, now + TRAVEL_SESSION_LEASE)]),
                    },
                );
                Ok(())
            }
        }
    }

    fn prune(&mut self, now: Instant) {
        self.sessions.retain(|_, session| {
            session.leases.retain(|_, expires| *expires > now);
            !session.leases.is_empty()
        });
    }

    fn release(&mut self, travel_id: &str, session_id: Uuid, lease_id: Uuid) -> bool {
        let Some(active) = self.sessions.get_mut(travel_id) else {
            return false;
        };
        if active.session_id != session_id || active.leases.remove(&lease_id).is_none() {
            return false;
        }
        if active.leases.is_empty() {
            self.sessions.remove(travel_id);
        }
        true
    }
}

struct State {
    homes: Mutex<HomeRegistry>,
    relay_txs: Mutex<HashMap<String, mpsc::Sender<ControlMessage>>>,
    relay_directory: Mutex<RelayDirectoryRegistry>,
    travel_sessions: Mutex<TravelSessionRegistry>,
    pending_grants: Mutex<HashMap<Uuid, PendingRelayGrant>>,
    pending_enrollments: Mutex<HashMap<Uuid, PendingEnrollment>>,
    authorization: RwLock<ServerAuthorization>,
    authorization_acks: Mutex<HashMap<String, u64>>,
    authorization_tx: watch::Sender<u64>,
    control_signer: ControlSigner,
    statistics_store: Arc<StateStore>,
}

impl State {
    fn new(
        authorization: ServerAuthorization,
        relay_directory_generation: u64,
        control_signer: ControlSigner,
        statistics_store: StateStore,
    ) -> Self {
        let (authorization_tx, _) = watch::channel(authorization.snapshot().generation);
        Self {
            homes: Mutex::new(HomeRegistry::default()),
            relay_txs: Mutex::new(HashMap::new()),
            relay_directory: Mutex::new(RelayDirectoryRegistry {
                generation: relay_directory_generation,
                endpoints: HashMap::new(),
            }),
            travel_sessions: Mutex::new(TravelSessionRegistry::default()),
            pending_grants: Mutex::new(HashMap::new()),
            pending_enrollments: Mutex::new(HashMap::new()),
            authorization: RwLock::new(authorization),
            authorization_acks: Mutex::new(HashMap::new()),
            authorization_tx,
            control_signer,
            statistics_store: Arc::new(statistics_store),
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
                .unwrap_or_else(|_| "flowsplice_server=info".into()),
        )
        .init();

    let args = Args::parse();
    let config: Config = load_toml(&args.config)?;
    validate_config(&config)?;
    if args.check_config {
        info!(event = "config_validated", path = %args.config.display(), "server configuration is valid");
        return Ok(());
    }
    let control_signer = ControlSigner::load(&config)?;
    let statistics_store = StateStore::open(&config.state_store)?;
    let authorization = ServerAuthorization::load(
        control_signer.trust.deployment_id.clone(),
        control_signer.trust.travel_authorities.clone(),
        config.travel_authorization_state.clone(),
    )?;
    let relay_directory_generation = 1;
    let state = Arc::new(State::new(
        authorization,
        relay_directory_generation,
        control_signer,
        statistics_store,
    ));

    let control = run_home_listeners(config.clone(), Arc::clone(&state));
    let relay = run_relay_connectors(config.clone(), Arc::clone(&state));
    let cleanup = cleanup_pending(Arc::clone(&state));
    let trust_expiry = monitor_trust_expiry(state.control_signer.trust.not_after_unix_secs);
    let statistics_ui = run_statistics_ui(config.ui_listen.clone(), Arc::clone(&state));
    tokio::try_join!(control, relay, cleanup, trust_expiry, statistics_ui)?;
    Ok(())
}

async fn monitor_trust_expiry(not_after_unix_secs: u64) -> Result<()> {
    loop {
        if unix_time_secs()? >= not_after_unix_secs {
            bail!("deployment trust expired; refusing to continue");
        }
        sleep(Duration::from_secs(30)).await;
    }
}

fn validate_config(config: &Config) -> Result<()> {
    if config.id.is_empty() {
        bail!("server id must be non-empty");
    }
    validate_listen(&config.control_listen, "control")?;
    validate_loopback_listen(&config.ui_listen, "statistics UI")?;
    if config.state_store.as_os_str().is_empty() {
        bail!("state_store must be non-empty");
    }
    let control_signer = ControlSigner::load(config)?;
    validate_homes(&config.homes, &control_signer.trust)?;
    ServerAuthorization::validate(
        control_signer.trust.deployment_id,
        control_signer.trust.travel_authorities,
        config.travel_authorization_state.clone(),
    )?;
    validate_relays(&config.relays)?;
    if config.handshake_timeout_secs == 0
        || config.work_ttl_secs == 0
        || config.max_pending_work == 0
        || config.max_control_connections == 0
        || config.control_snapshot_ttl_secs == 0
        || config.control_snapshot_ttl_secs > MAX_CONTROL_SNAPSHOT_TTL_SECS
    {
        bail!("server timeout and pending-work limits must be positive");
    }
    let _ = server_acceptor(&config.cert, &config.key, &config.management_ca)?;
    let _ = identity_client_connector(&config.cert, &config.key, &config.management_ca)?;
    Ok(())
}

fn validate_homes(homes: &[ConfiguredHome], trust: &DeploymentTrust) -> Result<()> {
    if homes.is_empty() {
        bail!("at least one Home Agent is required");
    }
    let mut ids = std::collections::HashSet::new();
    for home in homes {
        if home.id.is_empty() || !ids.insert(&home.id) {
            bail!("Home ids must be non-empty and unique");
        }
        trust.home_endpoint(&home.id)?;
    }
    Ok(())
}

fn trusted_home_management_pins<'a>(
    trust: &'a DeploymentTrust,
    home_id: &str,
) -> Result<&'a [String]> {
    Ok(&trust.home_endpoint(home_id)?.management_spki_pins)
}

fn validate_listen(address: &str, label: &str) -> Result<()> {
    if address.is_empty() {
        bail!("{label} listener address must be non-empty");
    }
    address
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid {label} listener {address}"))?;
    Ok(())
}

fn validate_loopback_listen(address: &str, label: &str) -> Result<()> {
    let address = address
        .parse::<std::net::SocketAddr>()
        .with_context(|| format!("invalid {label} listener {address}"))?;
    if !address.ip().is_loopback() {
        bail!("{label} must bind an exact loopback address");
    }
    Ok(())
}

fn validate_relays(relays: &[ConfiguredRelay]) -> Result<()> {
    if relays.is_empty() {
        bail!("at least one relay is required");
    }
    let mut ids = std::collections::HashSet::new();
    for relay in relays {
        if relay.id.is_empty()
            || relay.management_addr.is_empty()
            || relay.data_public_addr.is_empty()
            || !ids.insert(&relay.id)
        {
            bail!("relay ids must be non-empty and unique, with non-empty addresses");
        }
    }
    Ok(())
}

async fn run_home_listeners(config: Config, state: Arc<State>) -> Result<()> {
    let acceptor = server_acceptor(&config.cert, &config.key, &config.management_ca)?;
    let address = config.control_listen.clone();
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind home control {address}"))?;
    let permits = Arc::new(Semaphore::new(config.max_control_connections));
    run_home_accept_loop(listener, address, acceptor, config, state, permits).await
}

async fn run_home_accept_loop(
    listener: TcpListener,
    address: String,
    acceptor: tokio_rustls::TlsAcceptor,
    config: Config,
    state: Arc<State>,
    permits: Arc<Semaphore>,
) -> Result<()> {
    info!(%address, "home control listener ready");
    loop {
        let permit = Arc::clone(&permits)
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("home control connection budget closed"))?;
        let (socket, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let state = Arc::clone(&state);
        let config = config.clone();
        let timeout_duration = Duration::from_secs(config.handshake_timeout_secs);
        tokio::spawn(async move {
            let _permit = permit;
            let result = async {
                let stream = timeout(timeout_duration, acceptor.accept(socket))
                    .await
                    .context("home TLS handshake timed out")??;
                handle_home(stream, state, &config).await
            }
            .await;
            if let Err(error) = result {
                warn!(%peer, %error, "home control connection closed");
            }
        });
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_home(
    stream: TlsStream<TcpStream>,
    state: Arc<State>,
    config: &Config,
) -> Result<()> {
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    let configured_home = config
        .homes
        .iter()
        .find(|home| home.id == identity.id)
        .ok_or_else(|| anyhow!("Home {} is not configured", identity.id))?;
    let trusted_home_pins =
        trusted_home_management_pins(&state.control_signer.trust, &configured_home.id)?;
    require_peer(
        &identity,
        Role::Home,
        Some(&configured_home.id),
        trusted_home_pins,
    )?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    let setup_timeout = Duration::from_secs(config.handshake_timeout_secs);
    match reader
        .read_with_timeout::<ControlMessage>(setup_timeout)
        .await?
    {
        ControlMessage::Hello {
            protocol_version,
            role,
            id,
        } if protocol_version == CONTROL_PROTOCOL_VERSION
            && role == Role::Home
            && id == identity.id => {}
        _ => bail!("home HELLO does not match its certificate"),
    }
    write_json(
        &mut writer,
        &ControlMessage::Hello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            role: Role::Server,
            id: config.id.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;

    let ControlMessage::HomeRegister { home } = reader
        .read_with_timeout::<ControlMessage>(setup_timeout)
        .await?
    else {
        bail!("home did not register a catalog after HELLO");
    };
    if home.home_id != identity.id {
        bail!("catalog home id does not match the authenticated home");
    }
    validate_catalog(&Catalog {
        generation: 1,
        homes: vec![home.clone()],
    })?;
    let snapshot = state.authorization.read().await.snapshot();
    write_json(
        &mut writer,
        &ControlMessage::TravelAuthorizationSnapshot {
            snapshot: snapshot.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    match reader
        .read_with_timeout::<ControlMessage>(setup_timeout)
        .await?
    {
        ControlMessage::TravelAuthorizationAck { generation }
            if generation == snapshot.generation =>
        {
            record_authorization_ack(&state, format!("home:{}", identity.id), generation).await;
        }
        _ => bail!("home did not acknowledge the current Travel authorization snapshot"),
    }
    let (tx, mut rx) = mpsc::channel::<ControlMessage>(32);
    let session_id = Uuid::new_v4();
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let (previous, catalog) = state.homes.lock().await.register(
        home,
        HomeSession {
            session_id,
            tx: tx.clone(),
            shutdown,
        },
    );
    if let Some(previous) = previous {
        warn!(
            old_session_id = %previous.session_id,
            new_session_id = %session_id,
            home_id = %identity.id,
            "superseding existing Home session"
        );
        let _ = previous.shutdown.send(true);
    }
    let _ = catalog;
    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            write_json(&mut writer, &message, CONTROL_FRAME_LIMIT).await?;
        }
        Ok::<_, std::io::Error>(())
    });
    let current_snapshot = state.authorization.read().await.snapshot();
    if current_snapshot.generation != snapshot.generation {
        tx.send(ControlMessage::TravelAuthorizationSnapshot {
            snapshot: current_snapshot,
        })
        .await?;
    }

    info!(home_id = %identity.id, "home agent connected");
    let idle_timeout = Duration::from_secs(config.handshake_timeout_secs.saturating_mul(3).max(1));
    let result: Result<()> = async {
        loop {
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        bail!("Home session was superseded");
                    }
                }
                message = reader.read_with_timeout::<ControlMessage>(idle_timeout) => {
                    match message? {
                        ControlMessage::HomeRegister { home } => {
                            if home.home_id != identity.id {
                                bail!("catalog home id does not match the authenticated home");
                            }
                            validate_catalog(&Catalog {
                                generation: 1,
                                homes: vec![home.clone()],
                            })?;
                            let _ = state.homes.lock().await.update_catalog(home, session_id);
                        }
                        ControlMessage::Heartbeat { nonce } => {
                            tx.send(ControlMessage::HeartbeatAck { nonce }).await?;
                        }
                        ControlMessage::HeartbeatAck { .. } => {}
                        ControlMessage::TravelAuthorizationAck { generation } => {
                            record_authorization_ack(&state, format!("home:{}", identity.id), generation).await;
                        }
                        ControlMessage::PublishTravelCredential { request_id, credential } => {
                            let result = import_home_credential(&state, &identity.id, credential).await;
                            let (accepted, generation, error) = match result {
                                Ok((_changed, generation)) => (true, generation, None),
                                Err(error) => (
                                    false,
                                    state.authorization.read().await.snapshot().generation,
                                    Some(error.to_string()),
                                ),
                            };
                            tx.send(ControlMessage::PublishTravelCredentialResult {
                                request_id,
                                accepted,
                                generation,
                                error,
                            })
                            .await?;
                        }
                        ControlMessage::RevokeTravelCredential { request_id, credential_id, reason } => {
                            let result = revoke_home_credential(
                                &state,
                                &identity.id,
                                credential_id,
                                &reason,
                            )
                            .await;
                            let (accepted, generation, error) = match result {
                                Ok((_changed, generation)) => (true, generation, None),
                                Err(error) => (
                                    false,
                                    state.authorization.read().await.snapshot().generation,
                                    Some(error.to_string()),
                                ),
                            };
                            tx.send(ControlMessage::RevokeTravelCredentialResult {
                                request_id,
                                accepted,
                                generation,
                                error,
                            })
                            .await?;
                        }
                        ControlMessage::StatisticsReport { report } => {
                            let digest_sha256 = report.digest_sha256().unwrap_or_default();
                            let result = accept_statistics_report(
                                &state,
                                &identity,
                                report,
                                false,
                            )
                            .await;
                            tx.send(ControlMessage::StatisticsReportAck {
                                digest_sha256,
                                accepted: result.is_ok(),
                                error: result.err().map(|error| error.to_string()),
                            })
                            .await?;
                        }
                        ControlMessage::RemoteEnrollmentResult {
                            request_id,
                            accepted,
                            response_json,
                            error,
                        } => {
                            complete_remote_enrollment(
                                &state,
                                &identity.id,
                                request_id,
                                accepted,
                                response_json,
                                error,
                            )
                            .await?;
                        }
                        ControlMessage::RemoteEnrollmentInstalledAck {
                            request_id,
                            accepted,
                            error,
                        } => {
                            complete_remote_enrollment_installed(
                                &state,
                                &identity.id,
                                request_id,
                                accepted,
                                error,
                            )
                            .await?;
                        }
                        _ => bail!("unexpected message from home agent"),
                    }
                    if writer_task.is_finished() {
                        bail!("home control writer stopped");
                    }
                }
            }
        }
    }
    .await;
    writer_task.abort();
    clear_home_session(&state, session_id, &identity.id).await;
    result
}

async fn import_home_credential(
    state: &Arc<State>,
    home_id: &str,
    credential: flowsplice_core::authorization::SignedTravelCredential,
) -> Result<(bool, u64)> {
    let (changed, snapshot) = {
        let mut authorization = state.authorization.write().await;
        let changed = authorization.import_credential(credential, home_id)?;
        (changed, authorization.snapshot())
    };
    if changed {
        broadcast_authorization(state, snapshot.clone()).await;
    }
    info!(
        event = "travel_credential_published_by_home",
        %home_id,
        generation = snapshot.generation,
        changed,
        "Home published a signed Travel credential"
    );
    Ok((changed, snapshot.generation))
}

async fn revoke_home_credential(
    state: &Arc<State>,
    home_id: &str,
    credential_id: Uuid,
    reason: &str,
) -> Result<(bool, u64)> {
    let reason = reason.trim();
    if reason.is_empty() || reason.len() > 256 {
        bail!("revocation reason must contain 1 to 256 bytes");
    }
    let (changed, snapshot) = {
        let mut authorization = state.authorization.write().await;
        let changed = authorization.revoke_from_home(credential_id, reason.to_owned(), home_id)?;
        (changed, authorization.snapshot())
    };
    if changed {
        state
            .pending_grants
            .lock()
            .await
            .retain(|_, work| work.credential_id != credential_id);
        broadcast_authorization(state, snapshot.clone()).await;
    }
    info!(
        event = "travel_credential_revoked_by_home",
        %home_id,
        %credential_id,
        generation = snapshot.generation,
        changed,
        "Home revoked a Travel credential"
    );
    Ok((changed, snapshot.generation))
}

async fn clear_home_session(state: &Arc<State>, session_id: Uuid, home_id: &str) {
    if state
        .homes
        .lock()
        .await
        .remove(home_id, session_id)
        .is_some()
    {
        state
            .authorization_acks
            .lock()
            .await
            .remove(&format!("home:{home_id}"));
    }
}

async fn run_relay_connectors(config: Config, state: Arc<State>) -> Result<()> {
    let mut tasks = JoinSet::new();
    for relay in config.relays.clone() {
        tasks.spawn(run_relay_connector(
            config.clone(),
            relay,
            Arc::clone(&state),
        ));
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    bail!("all relay connector tasks stopped")
}

async fn run_relay_connector(
    config: Config,
    relay: ConfiguredRelay,
    state: Arc<State>,
) -> Result<()> {
    let connector = identity_client_connector(&config.cert, &config.key, &config.management_ca)?;
    loop {
        let result = async {
            let socket = TcpStream::connect(&relay.management_addr).await?;
            socket.set_nodelay(true)?;
            let stream = timeout(
                Duration::from_secs(config.handshake_timeout_secs),
                connector.connect(identity_server_name()?, socket),
            )
            .await
            .context("relay TLS handshake timed out")??;
            let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
            require_peer(&identity, Role::Relay, Some(&relay.id), &[])?;
            let endpoint = RelayEndpoint {
                id: relay.id.clone(),
                management_addr: relay.management_addr.clone(),
                data_public_addr: relay.data_public_addr.clone(),
                management_spki_sha256: identity.spki_sha256.clone(),
            };
            run_relay_session(stream, &config, &relay, endpoint, identity, &state).await
        }
        .await;
        if let Err(error) = result {
            warn!(relay_id = %relay.id, %error, "relay control disconnected; reconnecting");
        }
        state.relay_txs.lock().await.remove(&relay.id);
        state
            .authorization_acks
            .lock()
            .await
            .remove(&format!("relay:{}", relay.id));
        sleep(Duration::from_secs(1)).await;
    }
}

#[allow(clippy::too_many_lines)]
async fn run_relay_session(
    stream: tokio_rustls::client::TlsStream<TcpStream>,
    config: &Config,
    relay: &ConfiguredRelay,
    endpoint: RelayEndpoint,
    relay_identity: PeerIdentity,
    state: &Arc<State>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    write_json(
        &mut writer,
        &ControlMessage::Hello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            role: Role::Server,
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
            && role == Role::Relay
            && id == relay.id => {}
        _ => bail!("relay HELLO does not match expected identity"),
    }

    let snapshot = state.authorization.read().await.snapshot();
    write_json(
        &mut writer,
        &ControlMessage::TravelAuthorizationSnapshot {
            snapshot: snapshot.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    match reader
        .read_with_timeout::<ControlMessage>(Duration::from_secs(config.handshake_timeout_secs))
        .await?
    {
        ControlMessage::TravelAuthorizationAck { generation }
            if generation == snapshot.generation =>
        {
            record_authorization_ack(state, format!("relay:{}", relay.id), generation).await;
        }
        _ => bail!("relay did not acknowledge the current Travel authorization snapshot"),
    }

    let (tx, mut rx) = mpsc::channel::<ControlMessage>(64);
    state
        .relay_txs
        .lock()
        .await
        .insert(relay.id.clone(), tx.clone());
    let current_snapshot = state.authorization.read().await.snapshot();
    if current_snapshot.generation != snapshot.generation {
        tx.send(ControlMessage::TravelAuthorizationSnapshot {
            snapshot: current_snapshot,
        })
        .await?;
    }
    let directory = state.relay_directory.lock().await.register(endpoint);
    info!(
        event = "relay_directory_updated",
        generation = directory.generation,
        relay_count = directory.relays.len(),
        "updated authenticated Relay directory"
    );
    info!(relay_id = %relay.id, "relay control connected");

    let mut heartbeat = interval(Duration::from_secs(10));
    let mut nonce = 0_u64;
    let mut last_received = Instant::now();
    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(outgoing) = outgoing else { bail!("relay writer channel closed"); };
                write_json(&mut writer, &outgoing, CONTROL_FRAME_LIMIT).await?;
            }
            incoming = reader.read::<ControlMessage>() => {
                last_received = Instant::now();
                match incoming? {
                    ControlMessage::TravelSessionAuthorize {
                        request_id,
                        travel_id,
                        travel_session_id,
                        credential_id,
                        lease_id,
                    } => {
                        let result = authorize_travel(
                            state,
                            credential_id,
                            &travel_id,
                            travel_session_id,
                            lease_id,
                            None,
                        ).await;
                        match result {
                            Ok(snapshot) => {
                                write_json(
                                    &mut writer,
                                    &ControlMessage::TravelSessionAccepted {
                                        request_id,
                                        snapshot,
                                    },
                                    CONTROL_FRAME_LIMIT,
                                )
                                .await?;
                            }
                            Err(reason) => {
                                warn!(
                                    event = "travel_session_rejected",
                                    relay_id = %relay.id,
                                    %travel_id,
                                    %reason,
                                    "rejected concurrent Travel session"
                                );
                                write_json(
                                    &mut writer,
                                    &ControlMessage::TravelSessionDenied { request_id, reason },
                                    CONTROL_FRAME_LIMIT,
                                )
                                .await?;
                            }
                        }
                    }
                    ControlMessage::TravelSessionRelease {
                        travel_id,
                        travel_session_id,
                        lease_id,
                    } => {
                        if state.travel_sessions.lock().await.release(
                            &travel_id,
                            travel_session_id,
                            lease_id,
                        ) {
                            info!(
                                event = "travel_session_released",
                                relay_id = %relay.id,
                                %travel_id,
                                %travel_session_id,
                                %lease_id,
                                "released closed Travel login session"
                            );
                        }
                    }
                    ControlMessage::RouteRequest {
                        request_id,
                        travel_id,
                        travel_session_id,
                        credential_id,
                        home_id,
                    } => {
                        handle_route_request(
                            RouteRequestContext {
                                request: request_id,
                                travel: travel_id,
                                travel_session: travel_session_id,
                                credential: credential_id,
                                home: home_id,
                            },
                            config,
                            relay,
                            state,
                            &mut writer,
                        )
                        .await?;
                    }
                    ControlMessage::RelayWorkReady { request_id, work_id } => {
                        complete_relay_work(state, &relay.id, request_id, work_id).await?;
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(&mut writer, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
                    ControlMessage::TravelAuthorizationAck { generation } => {
                        record_authorization_ack(state, format!("relay:{}", relay.id), generation).await;
                    }
                    ControlMessage::StatisticsReport { report } => {
                        let digest_sha256 = report.digest_sha256().unwrap_or_default();
                        let result = accept_statistics_report(
                            state,
                            &relay_identity,
                            report,
                            true,
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
                    ControlMessage::RemoteEnrollmentSubmit {
                        request_id,
                        travel_id,
                        travel_session_id,
                        credential_id,
                        home_id,
                        request_json,
                    } => {
                        handle_remote_enrollment_submit(
                            state,
                            relay,
                            &tx,
                            request_id,
                            travel_id,
                            travel_session_id,
                            credential_id,
                            home_id,
                            request_json,
                            config,
                        )
                        .await?;
                    }
                    ControlMessage::RemoteEnrollmentInstalled {
                        request_id,
                        travel_id,
                        travel_session_id,
                        credential_id,
                        home_id,
                    } => {
                        handle_remote_enrollment_installed(
                            state,
                            relay,
                            &tx,
                            request_id,
                            travel_id,
                            travel_session_id,
                            credential_id,
                            home_id,
                            config,
                        )
                        .await?;
                    }
                    ControlMessage::BootstrapEnrollmentSubmit {
                        protocol_version,
                        request_id,
                        travel_id,
                        home_id,
                        retrieval_token,
                        request_json,
                    } => {
                        handle_bootstrap_enrollment_submit(
                            state,
                            relay,
                            &tx,
                            protocol_version,
                            request_id,
                            travel_id,
                            home_id,
                            retrieval_token,
                            request_json,
                            config,
                        )
                        .await?;
                    }
                    _ => bail!("unexpected message from relay"),
                }
            }
            _ = heartbeat.tick() => {
                if last_received.elapsed() > Duration::from_secs(30) {
                    bail!("relay control heartbeat timed out");
                }
                nonce = nonce.wrapping_add(1);
                write_json(&mut writer, &ControlMessage::Heartbeat { nonce }, CONTROL_FRAME_LIMIT).await?;
            }
        }
    }
}

async fn handle_route_request<W: tokio::io::AsyncWrite + Unpin>(
    request: RouteRequestContext,
    config: &Config,
    relay: &ConfiguredRelay,
    state: &Arc<State>,
    writer: &mut W,
) -> Result<()> {
    if let Err(reason) = authorize_travel(
        state,
        request.credential,
        &request.travel,
        request.travel_session,
        None,
        Some(&request.home),
    )
    .await
    {
        write_json(
            writer,
            &ControlMessage::RouteDenied {
                request_id: request.request,
                reason,
            },
            CONTROL_FRAME_LIMIT,
        )
        .await?;
        return Ok(());
    }
    let home = state.homes.lock().await.sender(&request.home);
    let Some(home) = home else {
        write_json(
            writer,
            &ControlMessage::RouteDenied {
                request_id: request.request,
                reason: format!("Home Agent {} is unavailable", request.home),
            },
            CONTROL_FRAME_LIMIT,
        )
        .await?;
        return Ok(());
    };

    let work_id = Uuid::new_v4();
    let mut secret = vec![0_u8; 32];
    SystemRandom::new()
        .fill(&mut secret)
        .map_err(|_| anyhow!("AWS-LC random generation failed"))?;
    let expires_at_unix_secs = unix_time_secs()?.saturating_add(config.work_ttl_secs);
    let mut pending = state.pending_grants.lock().await;
    if pending.len() >= config.max_pending_work {
        write_json(
            writer,
            &ControlMessage::RouteDenied {
                request_id: request.request,
                reason: "server pending-work limit reached".to_owned(),
            },
            CONTROL_FRAME_LIMIT,
        )
        .await?;
        return Ok(());
    }
    pending.insert(
        request.request,
        PendingRelayGrant {
            relay_id: relay.id.clone(),
            work_id,
            credential_id: request.credential,
            home,
            open_message: ControlMessage::OpenRelayWork {
                work_id,
                work_secret: secret.clone(),
                credential_id: request.credential,
                relay_id: relay.id.clone(),
                relay_data_addr: relay.data_public_addr.clone(),
                expires_at_unix_secs,
            },
            expires: Instant::now() + Duration::from_secs(config.work_ttl_secs),
        },
    );
    drop(pending);
    write_json(
        writer,
        &ControlMessage::ServerRelayGrant {
            request_id: request.request,
            work_id,
            work_secret: secret,
            credential_id: request.credential,
            home_id: request.home,
            expires_at_unix_secs,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    Ok(())
}

async fn complete_relay_work(
    state: &Arc<State>,
    relay_id: &str,
    request_id: Uuid,
    work_id: Uuid,
) -> Result<()> {
    let mut grants = state.pending_grants.lock().await;
    let pending =
        take_matching_pending_grant(&mut grants, relay_id, request_id, work_id, Instant::now())?;
    drop(grants);
    pending
        .home
        .send(pending.open_message)
        .await
        .map_err(|_| anyhow!("Home control session closed before direct work delivery"))?;
    Ok(())
}

fn take_matching_pending_grant(
    grants: &mut HashMap<Uuid, PendingRelayGrant>,
    relay_id: &str,
    request_id: Uuid,
    work_id: Uuid,
    now: Instant,
) -> Result<PendingRelayGrant> {
    let pending = grants
        .get(&request_id)
        .ok_or_else(|| anyhow!("unknown or expired Relay work readiness"))?;
    if pending.expires <= now {
        grants.remove(&request_id);
        bail!("Relay work readiness expired");
    }
    if pending.relay_id != relay_id || pending.work_id != work_id {
        bail!("Relay work readiness does not match the authorized Relay grant");
    }
    grants
        .remove(&request_id)
        .ok_or_else(|| anyhow!("Relay work readiness vanished"))
}

#[allow(clippy::too_many_arguments)]
async fn handle_remote_enrollment_submit(
    state: &Arc<State>,
    relay: &ConfiguredRelay,
    relay_tx: &mpsc::Sender<ControlMessage>,
    request_id: Uuid,
    travel_id: String,
    travel_session_id: Uuid,
    credential_id: Uuid,
    home_id: String,
    request_json: Vec<u8>,
    config: &Config,
) -> Result<()> {
    let result = async {
        if request_id.is_nil() || request_json.is_empty() || request_json.len() > 512 * 1024 {
            bail!("remote enrollment request is missing or oversized");
        }
        authorize_travel(
            state,
            credential_id,
            &travel_id,
            travel_session_id,
            None,
            Some(&home_id),
        )
        .await
        .map_err(anyhow::Error::msg)?;
        let home = state
            .homes
            .lock()
            .await
            .sender(&home_id)
            .ok_or_else(|| anyhow!("target Home is unavailable"))?;
        let mut pending = state.pending_enrollments.lock().await;
        if pending.len() >= config.max_pending_work {
            bail!("Server pending enrollment limit reached");
        }
        if pending.contains_key(&request_id) {
            bail!("remote enrollment request is already in flight");
        }
        pending.insert(
            request_id,
            PendingEnrollment {
                relay_id: relay.id.clone(),
                travel_id: travel_id.clone(),
                home_id: home_id.clone(),
                relay: relay_tx.clone(),
                bootstrap: false,
                installed: false,
                expires: Instant::now() + Duration::from_secs(config.work_ttl_secs.max(15)),
            },
        );
        drop(pending);
        if home
            .send(ControlMessage::RemoteEnrollmentSubmit {
                request_id,
                travel_id,
                travel_session_id,
                credential_id,
                home_id,
                request_json,
            })
            .await
            .is_err()
        {
            state.pending_enrollments.lock().await.remove(&request_id);
            bail!("Home control session closed during enrollment delivery");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        relay_tx
            .send(ControlMessage::RemoteEnrollmentResult {
                request_id,
                accepted: false,
                response_json: None,
                error: Some(error.to_string()),
            })
            .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_bootstrap_enrollment_submit(
    state: &Arc<State>,
    relay: &ConfiguredRelay,
    relay_tx: &mpsc::Sender<ControlMessage>,
    protocol_version: u32,
    request_id: Uuid,
    travel_id: String,
    home_id: String,
    retrieval_token: Vec<u8>,
    request_json: Vec<u8>,
    config: &Config,
) -> Result<()> {
    let result = async {
        if protocol_version != CONTROL_PROTOCOL_VERSION
            || request_id.is_nil()
            || travel_id.is_empty()
            || home_id.is_empty()
            || retrieval_token.len() != 32
            || request_json.is_empty()
            || request_json.len() > 512 * 1024
        {
            bail!("first enrollment request is invalid or oversized");
        }
        let home = state
            .homes
            .lock()
            .await
            .sender(&home_id)
            .ok_or_else(|| anyhow!("target Home is unavailable"))?;
        let mut pending = state.pending_enrollments.lock().await;
        if pending.len() >= config.max_pending_work {
            bail!("Server pending enrollment limit reached");
        }
        if let Some(existing) = pending.get(&request_id) {
            if existing.relay_id != relay.id
                || existing.travel_id != travel_id
                || existing.home_id != home_id
                || !existing.bootstrap
            {
                bail!("conflicting first enrollment request is already in flight");
            }
            bail!("first enrollment request is already in flight");
        }
        pending.insert(
            request_id,
            PendingEnrollment {
                relay_id: relay.id.clone(),
                travel_id: travel_id.clone(),
                home_id: home_id.clone(),
                relay: relay_tx.clone(),
                bootstrap: true,
                installed: false,
                expires: Instant::now() + Duration::from_secs(config.work_ttl_secs.max(15)),
            },
        );
        drop(pending);
        if home
            .send(ControlMessage::BootstrapEnrollmentSubmit {
                protocol_version,
                request_id,
                travel_id,
                home_id,
                retrieval_token,
                request_json,
            })
            .await
            .is_err()
        {
            state.pending_enrollments.lock().await.remove(&request_id);
            bail!("Home control session closed during first enrollment delivery");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        relay_tx
            .send(ControlMessage::BootstrapEnrollmentResult {
                request_id,
                accepted: false,
                response_json: None,
                seed_relays: Vec::new(),
                error: Some(error.to_string()),
            })
            .await?;
    }
    Ok(())
}

async fn complete_remote_enrollment(
    state: &Arc<State>,
    home_id: &str,
    request_id: Uuid,
    accepted: bool,
    response_json: Option<Vec<u8>>,
    error: Option<String>,
) -> Result<()> {
    let pending = state
        .pending_enrollments
        .lock()
        .await
        .remove(&request_id)
        .ok_or_else(|| anyhow!("unknown or expired remote enrollment request"))?;
    if pending.home_id != home_id {
        bail!("remote enrollment response came from the wrong Home");
    }
    if pending.installed {
        bail!("Home returned an enrollment response for an install acknowledgement");
    }
    let message = if pending.bootstrap {
        let seed_relays = state
            .relay_directory
            .lock()
            .await
            .directory()
            .relays
            .into_iter()
            .map(|relay| relay.management_addr)
            .collect();
        ControlMessage::BootstrapEnrollmentResult {
            request_id,
            accepted,
            response_json,
            seed_relays,
            error,
        }
    } else {
        ControlMessage::RemoteEnrollmentResult {
            request_id,
            accepted,
            response_json,
            error,
        }
    };
    pending
        .relay
        .send(message)
        .await
        .map_err(|_| anyhow!("Relay disconnected before enrollment response delivery"))?;
    info!(
        %request_id,
        travel_id = %pending.travel_id,
        relay_id = %pending.relay_id,
        %home_id,
        accepted,
        "forwarded remote enrollment response"
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_remote_enrollment_installed(
    state: &Arc<State>,
    relay: &ConfiguredRelay,
    relay_tx: &mpsc::Sender<ControlMessage>,
    request_id: Uuid,
    travel_id: String,
    travel_session_id: Uuid,
    credential_id: Uuid,
    home_id: String,
    config: &Config,
) -> Result<()> {
    let result = async {
        if request_id.is_nil() {
            bail!("remote enrollment install acknowledgement is invalid");
        }
        authorize_travel(
            state,
            credential_id,
            &travel_id,
            travel_session_id,
            None,
            Some(&home_id),
        )
        .await
        .map_err(anyhow::Error::msg)?;
        {
            let authorization = state.authorization.read().await;
            let credential = authorization
                .verified()
                .credential(credential_id)
                .ok_or_else(|| anyhow!("installed Travel credential is unavailable"))?;
            if credential.enrollment_request_id != request_id {
                bail!("installed credential does not belong to this enrollment request");
            }
        }
        let home = state
            .homes
            .lock()
            .await
            .sender(&home_id)
            .ok_or_else(|| anyhow!("target Home is unavailable"))?;
        let mut pending = state.pending_enrollments.lock().await;
        if pending.len() >= config.max_pending_work {
            bail!("Server pending enrollment limit reached");
        }
        if pending.contains_key(&request_id) {
            bail!("remote enrollment acknowledgement is already in flight");
        }
        pending.insert(
            request_id,
            PendingEnrollment {
                relay_id: relay.id.clone(),
                travel_id: travel_id.clone(),
                home_id: home_id.clone(),
                relay: relay_tx.clone(),
                bootstrap: false,
                installed: true,
                expires: Instant::now() + Duration::from_secs(config.work_ttl_secs.max(15)),
            },
        );
        drop(pending);
        if home
            .send(ControlMessage::RemoteEnrollmentInstalled {
                request_id,
                travel_id,
                travel_session_id,
                credential_id,
                home_id,
            })
            .await
            .is_err()
        {
            state.pending_enrollments.lock().await.remove(&request_id);
            bail!("Home control session closed during enrollment acknowledgement");
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = result {
        relay_tx
            .send(ControlMessage::RemoteEnrollmentInstalledAck {
                request_id,
                accepted: false,
                error: Some(error.to_string()),
            })
            .await?;
    }
    Ok(())
}

async fn complete_remote_enrollment_installed(
    state: &Arc<State>,
    home_id: &str,
    request_id: Uuid,
    accepted: bool,
    error: Option<String>,
) -> Result<()> {
    let pending = state
        .pending_enrollments
        .lock()
        .await
        .remove(&request_id)
        .ok_or_else(|| anyhow!("unknown or expired enrollment acknowledgement"))?;
    if pending.home_id != home_id || !pending.installed {
        bail!("enrollment acknowledgement came from the wrong Home or lifecycle stage");
    }
    pending
        .relay
        .send(ControlMessage::RemoteEnrollmentInstalledAck {
            request_id,
            accepted,
            error,
        })
        .await
        .map_err(|_| anyhow!("Relay disconnected before enrollment acknowledgement delivery"))?;
    Ok(())
}

async fn accept_statistics_report(
    state: &Arc<State>,
    transport_identity: &PeerIdentity,
    report: SignedStatisticsReport,
    travel_forwarding_allowed: bool,
) -> Result<ReportAcceptance> {
    let report_identity = identity_from_certificate_pem(&report.certificate_pem)?;
    let verified = report.verify(&report_identity.signing_public_key)?;
    if verified.payload.deployment_id != state.control_signer.trust.deployment_id
        || verified.payload.reporter_role != report_identity.role
        || verified.payload.reporter_id != report_identity.id
    {
        bail!("statistics reporter identity is inconsistent");
    }
    match report_identity.role {
        Role::Travel if travel_forwarding_allowed && transport_identity.role == Role::Relay => {
            let now = unix_time_secs()?;
            let authorization = state.authorization.read().await;
            if authorization
                .verified()
                .authorize_management_all(&report_identity, now)?
                .is_empty()
            {
                bail!("statistics reporter has no active Travel credential");
            }
        }
        role if role == transport_identity.role
            && report_identity.id == transport_identity.id
            && report_identity.certificate_sha256 == transport_identity.certificate_sha256 => {}
        _ => bail!("statistics report is not bound to the authenticated control peer"),
    }
    validate_metric_ownership(report_identity.role, &verified.payload.metric_family)?;
    let now = unix_time_secs()?;
    validate_statistics_time_window(verified.payload.bucket_start_unix_secs, now)?;
    let store = Arc::clone(&state.statistics_store);
    tokio::task::spawn_blocking(move || store.accept_statistics_report(&verified))
        .await
        .context("statistics ingestion task failed")?
}

fn validate_statistics_time_window(bucket_start_unix_secs: u64, now: u64) -> Result<()> {
    if bucket_start_unix_secs > now.saturating_add(300)
        || now.saturating_sub(bucket_start_unix_secs) > 90 * 24 * 60 * 60
    {
        bail!("statistics report is outside the accepted time window");
    }
    Ok(())
}

fn validate_metric_ownership(role: Role, family: &str) -> Result<()> {
    let allowed = match role {
        Role::Travel => {
            family.starts_with("delivered_download")
                || family.starts_with("carrier_")
                || family.starts_with("travel_flow_")
        }
        Role::Relay => family.starts_with("relay_transport_") || family.starts_with("relay_route_"),
        Role::Home => {
            family.starts_with("delivered_upload")
                || family.starts_with("home_flow_")
                || family.starts_with("target_")
                || family.starts_with("issuer_")
        }
        Role::Server => false,
    };
    if !allowed {
        bail!("statistics metric family is not owned by this reporter role");
    }
    Ok(())
}

fn authoritative_global_metric(report: &AcceptedReport) -> bool {
    let family = report.payload.metric_family.as_str();
    match report.payload.reporter_role {
        Role::Travel => family.starts_with("delivered_download") || family.starts_with("carrier_"),
        Role::Home => {
            family.starts_with("delivered_upload")
                || family.starts_with("home_flow_accepted")
                || family.starts_with("home_flow_completed")
                || family.starts_with("home_flow_failed")
                || family.starts_with("target_")
                || family.starts_with("issuer_")
        }
        Role::Relay => family.starts_with("relay_"),
        Role::Server => false,
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
struct ServerStatisticsResponse {
    period: String,
    from_unix_secs: u64,
    to_unix_secs: u64,
    overview: Vec<MetricRollup>,
    breakdowns: Vec<MetricRollup>,
    nodes: Vec<NodeReportStatus>,
    reports: Vec<AcceptedReport>,
}

#[derive(Serialize)]
struct NodeReportStatus {
    reporter_role: Role,
    reporter_id: String,
    report_count: u64,
    metric_family_count: usize,
    bucket_count: usize,
    missing_five_minute_intervals: u64,
    last_bucket_start_unix_secs: u64,
    last_report_age_secs: u64,
    highest_revision: u64,
}

fn node_report_status(reports: &[AcceptedReport], from: u64, now: u64) -> Vec<NodeReportStatus> {
    let mut nodes =
        HashMap::<(Role, String), (u64, BTreeSet<String>, BTreeSet<u64>, u64, u64)>::new();
    for report in reports {
        let entry = nodes
            .entry((
                report.payload.reporter_role,
                report.payload.reporter_id.clone(),
            ))
            .or_insert_with(|| (0, BTreeSet::new(), BTreeSet::new(), 0, 0));
        entry.0 = entry.0.saturating_add(1);
        entry.1.insert(report.payload.metric_family.clone());
        entry.2.insert(report.payload.bucket_start_unix_secs);
        entry.3 = entry.3.max(report.payload.bucket_start_unix_secs);
        entry.4 = entry.4.max(report.payload.value.revision);
    }
    let expected = now.saturating_sub(from).div_ceil(300);
    let mut result = nodes
        .into_iter()
        .map(
            |((reporter_role, reporter_id), (report_count, families, buckets, last, revision))| {
                NodeReportStatus {
                    reporter_role,
                    reporter_id,
                    report_count,
                    metric_family_count: families.len(),
                    bucket_count: buckets.len(),
                    missing_five_minute_intervals: expected.saturating_sub(buckets.len() as u64),
                    last_bucket_start_unix_secs: last,
                    last_report_age_secs: now.saturating_sub(last.saturating_add(300)),
                    highest_revision: revision,
                }
            },
        )
        .collect::<Vec<_>>();
    result.sort_by(|left, right| {
        format!("{:?}", left.reporter_role)
            .cmp(&format!("{:?}", right.reporter_role))
            .then_with(|| left.reporter_id.cmp(&right.reporter_id))
    });
    result
}

async fn run_statistics_ui(address: String, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&address)
        .await
        .with_context(|| format!("failed to bind Server statistics UI {address}"))?;
    let app = Router::new()
        .route("/", get(server_statistics_page))
        .route("/api/statistics", get(server_statistics_api))
        .with_state(state);
    info!(%address, "Server statistics UI ready");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn server_statistics_page() -> Html<String> {
    Html(statistics_dashboard_html(
        "Global statistics",
        "Authoritative five-minute business reports collected from Travel, Relay, and Home. Server control traffic is excluded.",
        true,
    ))
}

async fn server_statistics_api(
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
    let store = Arc::clone(&state.statistics_store);
    let reports = tokio::task::spawn_blocking(move || store.query_accepted_reports(from, now))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let authoritative_reports = reports
        .iter()
        .filter(|report| authoritative_global_metric(report))
        .cloned()
        .collect::<Vec<_>>();
    let points = accepted_reports_as_metric_points(&authoritative_reports);
    Json(ServerStatisticsResponse {
        period: query.period,
        from_unix_secs: from,
        to_unix_secs: now,
        overview: summarize_metric_points(&points, false),
        breakdowns: summarize_metric_points(&points, true),
        nodes: node_report_status(&reports, from, now),
        reports,
    })
}

async fn cleanup_pending(state: Arc<State>) -> Result<()> {
    let mut timer = interval(Duration::from_secs(1));
    loop {
        timer.tick().await;
        let now = Instant::now();
        state.pending_grants.lock().await.retain(|request_id, pending| {
            let keep = pending.expires > now;
            if !keep {
                warn!(%request_id, work_id = %pending.work_id, "expired unacknowledged Relay work grant");
            }
            keep
        });
        state
            .pending_enrollments
            .lock()
            .await
            .retain(|request_id, pending| {
                let keep = pending.expires > now;
                if !keep {
                    warn!(%request_id, "expired remote enrollment forwarding state");
                }
                keep
            });
        state.travel_sessions.lock().await.prune(now);
    }
}

async fn authorize_travel(
    state: &Arc<State>,
    credential_id: Uuid,
    travel_id: &str,
    travel_session_id: Uuid,
    lease_id: Option<Uuid>,
    home_id: Option<&str>,
) -> std::result::Result<SignedControlSnapshot, String> {
    let now = unix_time_secs().map_err(|error| error.to_string())?;
    let (travel_management_spki_sha256, credentials) = {
        let authorization = state.authorization.read().await;
        let credential = authorization
            .verified()
            .credential(credential_id)
            .cloned()
            .ok_or_else(|| "unknown Travel credential".to_owned())?;
        if credential.travel_id != travel_id {
            return Err("Travel credential identity does not match".to_owned());
        }
        let credentials = authorization
            .verified()
            .credentials()
            .filter(|candidate| {
                candidate.travel_id == travel_id
                    && candidate
                        .management_spki_sha256
                        .eq_ignore_ascii_case(&credential.management_spki_sha256)
                    && authorization
                        .verified()
                        .is_active(candidate.credential_id, now)
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(home_id) = home_id
            && !credentials
                .iter()
                .any(|candidate| candidate.allows_home(home_id))
        {
            return Err("Travel credential is not authorized for the requested Home".to_owned());
        }
        (credential.management_spki_sha256, credentials)
    };
    if !credentials.is_empty() {
        state.travel_sessions.lock().await.authorize(
            travel_id,
            travel_session_id,
            lease_id,
            Instant::now(),
        )?;
    } else if home_id.is_some() {
        return Err("Travel credential is revoked, expired, or not yet valid".to_owned());
    }
    let directory = state.relay_directory.lock().await.directory();
    let catalog = state.homes.lock().await.catalog();
    let catalog = catalog_for_credentials(&catalog, &credentials);
    state
        .control_signer
        .sign(
            directory,
            catalog,
            travel_id,
            &travel_management_spki_sha256,
        )
        .map_err(|error| error.to_string())
}

fn catalog_for_credentials(
    catalog: &Catalog,
    credentials: &[flowsplice_core::authorization::TravelCredential],
) -> Catalog {
    let homes = catalog
        .homes
        .iter()
        .filter_map(|home| {
            let services = home
                .services
                .iter()
                .filter(|service| {
                    credentials.iter().any(|credential| {
                        credential.allows_service(&home.home_id, &service.id, service.protocol)
                    })
                })
                .cloned()
                .collect::<Vec<_>>();
            (!services.is_empty()).then(|| HomeCatalog {
                home_id: home.home_id.clone(),
                home_alias: home.home_alias.clone(),
                services,
            })
        })
        .collect();
    Catalog {
        generation: catalog.generation,
        homes,
    }
}

async fn record_authorization_ack(state: &Arc<State>, node: String, generation: u64) {
    let current = state.authorization.read().await.snapshot().generation;
    if generation <= current {
        let mut acknowledgements = state.authorization_acks.lock().await;
        let acknowledged = acknowledgements.entry(node.clone()).or_default();
        *acknowledged = (*acknowledged).max(generation);
        info!(event = "travel_authorization_ack", %node, generation, "node acknowledged Travel authorization state");
    }
}

async fn broadcast_authorization(state: &Arc<State>, snapshot: TravelAuthorizationSnapshot) {
    state.authorization_tx.send_replace(snapshot.generation);
    let message = ControlMessage::TravelAuthorizationSnapshot {
        snapshot: snapshot.clone(),
    };
    let relays = state.relay_txs.lock().await.clone();
    let mut publishes = JoinSet::new();
    for (relay_id, relay) in relays {
        let message = message.clone();
        publishes.spawn(async move {
            let sent = timeout(Duration::from_secs(5), relay.send(message)).await;
            (format!("Relay {relay_id}"), sent)
        });
    }
    let homes = state.homes.lock().await.senders();
    for (home_id, home) in homes {
        let message = message.clone();
        publishes.spawn(async move {
            let sent = timeout(Duration::from_secs(5), home.send(message)).await;
            (format!("Home {home_id}"), sent)
        });
    }
    while let Some(result) = publishes.join_next().await {
        match result {
            Ok((_node, Ok(Ok(())))) => {}
            Ok((node, _)) => warn!(%node, "failed to publish Travel authorization state"),
            Err(error) => warn!(%error, "authorization publication task failed"),
        }
    }
    info!(
        event = "travel_authorization_published",
        generation = snapshot.generation,
        revoked = snapshot.revocations.len(),
        credentials = snapshot.credentials.len(),
        "published Travel authorization state"
    );
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        time::{Duration, Instant},
    };

    use flowsplice_core::{
        deployment::{DeploymentTrust, HomeEndpointTrust},
        protocol::{HomeCatalog, RelayEndpoint, Role, Service, ServiceProtocol},
    };
    use tokio::sync::{mpsc, watch};
    use uuid::Uuid;

    use super::{
        ConfiguredHome, HomeRegistry, HomeSession, PendingRelayGrant, RelayDirectoryRegistry,
        TRAVEL_SESSION_LEASE, TravelSessionRegistry, take_matching_pending_grant,
        trusted_home_management_pins, validate_homes, validate_metric_ownership,
        validate_statistics_time_window,
    };

    fn home_session(session_id: Uuid) -> HomeSession {
        let (tx, _) = mpsc::channel(1);
        let (shutdown, _) = watch::channel(false);
        HomeSession {
            session_id,
            tx,
            shutdown,
        }
    }

    fn home_catalog(home_id: &str, target: &str) -> HomeCatalog {
        HomeCatalog {
            home_id: home_id.to_owned(),
            home_alias: home_id.to_owned(),
            services: vec![Service {
                id: "same-service".to_owned(),
                alias: "Same name".to_owned(),
                protocol: ServiceProtocol::Tcp,
                target: target.to_owned(),
            }],
        }
    }

    fn deployment_trust() -> DeploymentTrust {
        DeploymentTrust {
            version: 1,
            deployment_id: "deployment-1".to_owned(),
            generation: 1,
            not_before_unix_secs: 1,
            not_after_unix_secs: u64::MAX,
            management_ca_certificate_pem: String::new(),
            business_ca_certificate_pem: String::new(),
            server_control_keys: Vec::new(),
            home_endpoints: vec![
                HomeEndpointTrust {
                    home_id: "home-1".to_owned(),
                    management_spki_pins: vec!["11".repeat(32)],
                    business_spki_pins: vec!["33".repeat(32)],
                },
                HomeEndpointTrust {
                    home_id: "home-2".to_owned(),
                    management_spki_pins: vec!["22".repeat(32)],
                    business_spki_pins: vec!["44".repeat(32)],
                },
            ],
            travel_authorities: Vec::new(),
        }
    }

    #[test]
    fn configured_homes_require_unique_deployment_trusted_ids() {
        let homes = vec![
            ConfiguredHome {
                id: "home-1".to_owned(),
            },
            ConfiguredHome {
                id: "home-2".to_owned(),
            },
        ];
        let trust = deployment_trust();
        assert!(validate_homes(&homes, &trust).is_ok());
        assert!(validate_homes(&[], &trust).is_err());
        assert!(validate_homes(&[ConfiguredHome { id: String::new() }], &trust).is_err());
        assert!(
            validate_homes(
                &[
                    ConfiguredHome {
                        id: "home-1".to_owned(),
                    },
                    ConfiguredHome {
                        id: "home-1".to_owned(),
                    },
                ],
                &trust
            )
            .is_err()
        );
        assert!(
            validate_homes(
                &[ConfiguredHome {
                    id: "home-3".to_owned(),
                }],
                &trust
            )
            .is_err()
        );
        assert_eq!(
            trusted_home_management_pins(&trust, "home-1").unwrap_or_default(),
            &["11".repeat(32)]
        );
        assert!(trusted_home_management_pins(&trust, "home-3").is_err());
        assert!(
            serde_json::from_str::<ConfiguredHome>(r#"{"id":"home-1","spki_pins":["obsolete"]}"#)
                .is_err()
        );
    }

    #[test]
    fn relay_directory_uses_authenticated_spki_and_stable_generation() {
        let mut registry = RelayDirectoryRegistry::default();
        let relay_two = RelayEndpoint {
            id: "relay-2".to_owned(),
            management_addr: "192.0.2.2:8443".to_owned(),
            data_public_addr: "192.0.2.2:8444".to_owned(),
            management_spki_sha256: "22".repeat(32),
        };
        let first = registry.register(relay_two.clone());
        assert_eq!(first.generation, 1);
        assert_eq!(first.relays, vec![relay_two.clone()]);

        let unchanged = registry.register(relay_two);
        assert_eq!(unchanged.generation, 1);

        let relay_one = RelayEndpoint {
            id: "relay-1".to_owned(),
            management_addr: "192.0.2.1:8443".to_owned(),
            data_public_addr: "192.0.2.1:8444".to_owned(),
            management_spki_sha256: "11".repeat(32),
        };
        let complete = registry.register(relay_one);
        assert_eq!(complete.generation, 2);
        assert_eq!(complete.relays[0].id, "relay-1");
        assert_eq!(complete.relays[1].id, "relay-2");
        assert_eq!(complete.relays[1].management_spki_sha256, "22".repeat(32));
    }

    #[test]
    fn home_registry_keeps_identically_named_services_isolated_by_home() {
        let mut registry = HomeRegistry::default();
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let (_, first_catalog) = registry.register(
            home_catalog("home-1", "127.0.0.1:1001"),
            home_session(first_id),
        );
        let Some(first_catalog) = first_catalog else {
            panic!("first Home did not publish a catalog");
        };
        assert_eq!(first_catalog.generation, 1);
        let (_, second_catalog) = registry.register(
            home_catalog("home-2", "127.0.0.1:1001"),
            home_session(second_id),
        );
        let Some(catalog) = second_catalog else {
            panic!("second Home did not publish a catalog");
        };
        assert_eq!(catalog.generation, 2);
        assert_eq!(catalog.homes.len(), 2);
        assert!(registry.sender("home-1").is_some());
        assert!(registry.sender("home-2").is_some());
        assert!(registry.sender("missing").is_none());
    }

    #[test]
    fn stale_home_disconnect_cannot_remove_replacement_session() {
        let mut registry = HomeRegistry::default();
        let old_id = Uuid::new_v4();
        let new_id = Uuid::new_v4();
        registry.register(
            home_catalog("home-1", "127.0.0.1:1001"),
            home_session(old_id),
        );
        let (_, unchanged) = registry.register(
            home_catalog("home-1", "127.0.0.1:1001"),
            home_session(new_id),
        );
        assert!(unchanged.is_none());
        assert!(registry.remove("home-1", old_id).is_none());
        assert!(registry.sender("home-1").is_some());
        let Some(catalog) = registry.remove("home-1", new_id) else {
            panic!("current Home session did not remove its catalog");
        };
        assert!(catalog.homes.is_empty());
    }

    #[test]
    fn first_travel_session_wins_and_same_process_routes_are_allowed() {
        let now = Instant::now();
        let first_session = Uuid::new_v4();
        let later_session = Uuid::new_v4();
        let mut registry = TravelSessionRegistry::default();

        assert!(
            registry
                .authorize("travel-1", first_session, Some(Uuid::new_v4()), now)
                .is_ok()
        );
        assert!(
            registry
                .authorize("travel-1", first_session, None, now)
                .is_ok()
        );
        assert!(
            registry
                .authorize("travel-1", first_session, Some(Uuid::new_v4()), now)
                .is_ok()
        );
        assert!(
            registry
                .authorize("travel-1", later_session, Some(Uuid::new_v4()), now)
                .is_err()
        );
    }

    #[test]
    fn expired_travel_session_allows_a_new_process_to_login() {
        let now = Instant::now();
        let mut registry = TravelSessionRegistry::default();
        assert!(
            registry
                .authorize("travel-1", Uuid::new_v4(), Some(Uuid::new_v4()), now)
                .is_ok()
        );

        assert!(
            registry
                .authorize(
                    "travel-1",
                    Uuid::new_v4(),
                    Some(Uuid::new_v4()),
                    now + TRAVEL_SESSION_LEASE + Duration::from_millis(1),
                )
                .is_ok()
        );
    }

    #[test]
    fn releasing_the_last_lease_allows_a_new_process_to_login_immediately() {
        let now = Instant::now();
        let old_session = Uuid::new_v4();
        let old_lease = Uuid::new_v4();
        let mut registry = TravelSessionRegistry::default();
        assert!(
            registry
                .authorize("travel-1", old_session, Some(old_lease), now)
                .is_ok()
        );
        assert!(registry.release("travel-1", old_session, old_lease));
        assert!(
            registry
                .authorize("travel-1", Uuid::new_v4(), Some(Uuid::new_v4()), now,)
                .is_ok()
        );
    }

    #[test]
    fn a_stale_or_wrong_release_cannot_end_the_active_session() {
        let now = Instant::now();
        let session = Uuid::new_v4();
        let lease = Uuid::new_v4();
        let mut registry = TravelSessionRegistry::default();
        assert!(
            registry
                .authorize("travel-1", session, Some(lease), now)
                .is_ok()
        );

        assert!(!registry.release("travel-1", Uuid::new_v4(), lease));
        assert!(!registry.release("travel-1", session, Uuid::new_v4()));
        let Some(active) = registry.sessions.get("travel-1") else {
            panic!("stale release unexpectedly removed the active Travel session");
        };
        assert_eq!(active.session_id, session);
    }

    #[test]
    fn route_connection_cannot_create_a_travel_login() {
        let mut registry = TravelSessionRegistry::default();
        assert!(
            registry
                .authorize("travel-1", Uuid::new_v4(), None, Instant::now())
                .is_err()
        );
    }

    #[test]
    fn session_registry_is_keyed_by_travel_identity_not_one_grant() {
        let now = Instant::now();
        let mut registry = TravelSessionRegistry::default();
        let session_id = Uuid::new_v4();
        assert!(
            registry
                .authorize("travel-1", session_id, Some(Uuid::new_v4()), now,)
                .is_ok()
        );
        assert!(
            registry
                .authorize("travel-1", session_id, None, now)
                .is_ok()
        );
        assert_eq!(registry.sessions.len(), 1);
    }

    #[test]
    fn relay_work_readiness_is_bound_to_session_work_id_expiry_and_one_use() {
        let request_id = Uuid::new_v4();
        let work_id = Uuid::new_v4();
        let (home, _) = mpsc::channel(1);
        let now = Instant::now();
        let mut grants = HashMap::from([(
            request_id,
            PendingRelayGrant {
                relay_id: "relay-1".to_owned(),
                work_id,
                credential_id: Uuid::new_v4(),
                home,
                open_message: flowsplice_core::protocol::ControlMessage::Heartbeat { nonce: 1 },
                expires: now + Duration::from_secs(30),
            },
        )]);

        assert!(
            take_matching_pending_grant(&mut grants, "relay-2", request_id, work_id, now,).is_err()
        );
        assert!(grants.contains_key(&request_id));
        assert!(
            take_matching_pending_grant(&mut grants, "relay-1", request_id, Uuid::new_v4(), now,)
                .is_err()
        );
        assert!(grants.contains_key(&request_id));
        assert!(
            take_matching_pending_grant(&mut grants, "relay-1", request_id, work_id, now,).is_ok()
        );
        assert!(
            take_matching_pending_grant(&mut grants, "relay-1", request_id, work_id, now,).is_err()
        );

        let expired_request = Uuid::new_v4();
        let (home, _) = mpsc::channel(1);
        grants.insert(
            expired_request,
            PendingRelayGrant {
                relay_id: "relay-1".to_owned(),
                work_id,
                credential_id: Uuid::new_v4(),
                home,
                open_message: flowsplice_core::protocol::ControlMessage::Heartbeat { nonce: 2 },
                expires: now,
            },
        );
        assert!(
            take_matching_pending_grant(&mut grants, "relay-1", expired_request, work_id, now,)
                .is_err()
        );
        assert!(!grants.contains_key(&expired_request));
    }

    #[test]
    fn statistics_time_window_and_metric_ownership_are_role_bound() {
        let now = 100 * 24 * 60 * 60;
        assert!(validate_statistics_time_window(now, now).is_ok());
        assert!(validate_statistics_time_window(now + 300, now).is_ok());
        assert!(validate_statistics_time_window(now + 600, now).is_err());
        assert!(validate_statistics_time_window(now - 90 * 24 * 60 * 60, now).is_ok());
        assert!(validate_statistics_time_window(now - 90 * 24 * 60 * 60 - 300, now).is_err());

        assert!(validate_metric_ownership(Role::Travel, "delivered_download_bytes").is_ok());
        assert!(validate_metric_ownership(Role::Travel, "carrier_winner").is_ok());
        assert!(validate_metric_ownership(Role::Relay, "relay_transport_upload_bytes").is_ok());
        assert!(validate_metric_ownership(Role::Home, "delivered_upload_bytes").is_ok());
        assert!(validate_metric_ownership(Role::Home, "target_failure").is_ok());
        assert!(validate_metric_ownership(Role::Travel, "delivered_upload_bytes").is_err());
        assert!(validate_metric_ownership(Role::Relay, "home_flow_accepted").is_err());
        assert!(validate_metric_ownership(Role::Home, "relay_transport_upload_bytes").is_err());
        assert!(validate_metric_ownership(Role::Server, "server_control_bytes").is_err());
    }
}
