#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
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
    authorization::{TravelAuthorizationSnapshot, TrustedTravelAuthority, unix_time_secs},
    config::load_toml,
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{Catalog, ControlMessage, HomeCatalog, RelayDirectory, RelayEndpoint, Role},
    route::{RouteSide, read_preface, verify_preface},
    tls::{
        client_connector, peer_identity, require_peer, server_acceptor, server_name,
        validate_spki_pins,
    },
};
use serde::Deserialize;
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, mpsc, watch},
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
    data_listens: Vec<String>,
    relays: Vec<RelayEndpoint>,
    cert: PathBuf,
    key: PathBuf,
    management_ca: PathBuf,
    homes: Vec<ConfiguredHome>,
    #[serde(default)]
    relay_spki_pins: Vec<String>,
    travel_authorities: Vec<TrustedTravelAuthority>,
    travel_credentials: PathBuf,
    travel_revocations: PathBuf,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout_secs: u64,
    #[serde(default = "default_work_ttl")]
    work_ttl_secs: u64,
    #[serde(default = "default_max_pending_work")]
    max_pending_work: usize,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredHome {
    id: String,
    #[serde(default)]
    spki_pins: Vec<String>,
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

const TRAVEL_SESSION_LEASE: Duration = Duration::from_secs(45);

struct PendingWork {
    credential_id: Uuid,
    home_id: String,
    secret: Vec<u8>,
    expires: Instant,
    home: Option<TcpStream>,
    relay: Option<TcpStream>,
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
}

struct State {
    homes: Mutex<HomeRegistry>,
    relay_txs: Mutex<HashMap<String, mpsc::Sender<ControlMessage>>>,
    travel_sessions: Mutex<TravelSessionRegistry>,
    pending: Mutex<HashMap<Uuid, PendingWork>>,
    authorization: RwLock<ServerAuthorization>,
    authorization_acks: Mutex<HashMap<String, u64>>,
    authorization_tx: watch::Sender<u64>,
}

impl State {
    fn new(authorization: ServerAuthorization) -> Self {
        let (authorization_tx, _) = watch::channel(authorization.snapshot().generation);
        Self {
            homes: Mutex::new(HomeRegistry::default()),
            relay_txs: Mutex::new(HashMap::new()),
            travel_sessions: Mutex::new(TravelSessionRegistry::default()),
            pending: Mutex::new(HashMap::new()),
            authorization: RwLock::new(authorization),
            authorization_acks: Mutex::new(HashMap::new()),
            authorization_tx,
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
    let authorization = ServerAuthorization::load(
        config.travel_authorities.clone(),
        config.travel_credentials.clone(),
        config.travel_revocations.clone(),
    )?;
    let state = Arc::new(State::new(authorization));

    let control = run_home_listeners(config.clone(), Arc::clone(&state));
    let data = run_data_listeners(config.clone(), Arc::clone(&state));
    let relay = run_relay_connectors(config.clone(), Arc::clone(&state));
    let cleanup = cleanup_pending(Arc::clone(&state));
    tokio::try_join!(control, data, relay, cleanup)?;
    Ok(())
}

fn validate_config(config: &Config) -> Result<()> {
    if config.id.is_empty() {
        bail!("server id must be non-empty");
    }
    validate_listen(&config.control_listen, "control")?;
    validate_listens(&config.data_listens, "data")?;
    validate_homes(&config.homes)?;
    validate_spki_pins(&config.relay_spki_pins, "relay")?;
    ServerAuthorization::validate(
        config.travel_authorities.clone(),
        config.travel_credentials.clone(),
        config.travel_revocations.clone(),
    )?;
    validate_relays(&config.relays)?;
    if config.handshake_timeout_secs == 0
        || config.work_ttl_secs == 0
        || config.max_pending_work == 0
    {
        bail!("server timeout and pending-work limits must be positive");
    }
    let _ = server_acceptor(&config.cert, &config.key, &config.management_ca)?;
    let _ = client_connector(&config.cert, &config.key, &config.management_ca)?;
    for relay in &config.relays {
        let _ = server_name(&relay.server_name)?;
    }
    Ok(())
}

fn validate_homes(homes: &[ConfiguredHome]) -> Result<()> {
    if homes.is_empty() {
        bail!("at least one Home Agent is required");
    }
    let mut ids = std::collections::HashSet::new();
    for home in homes {
        if home.id.is_empty() || !ids.insert(&home.id) {
            bail!("Home ids must be non-empty and unique");
        }
        validate_spki_pins(&home.spki_pins, &format!("Home {}", home.id))?;
    }
    Ok(())
}

fn validate_listens(listens: &[String], label: &str) -> Result<()> {
    if listens.is_empty() {
        bail!("at least one {label} listener is required");
    }
    let mut unique = std::collections::HashSet::new();
    for address in listens {
        if address.is_empty() || !unique.insert(address) {
            bail!("{label} listener addresses must be non-empty and unique");
        }
        address
            .parse::<std::net::SocketAddr>()
            .with_context(|| format!("invalid {label} listener {address}"))?;
    }
    Ok(())
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

fn validate_relays(relays: &[RelayEndpoint]) -> Result<()> {
    if relays.is_empty() {
        bail!("at least one relay is required");
    }
    let mut ids = std::collections::HashSet::new();
    for relay in relays {
        if relay.id.is_empty()
            || relay.management_addr.is_empty()
            || relay.server_name.is_empty()
            || !ids.insert(&relay.id)
        {
            bail!("relay ids must be non-empty and unique, with non-empty addresses and names");
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
    run_home_accept_loop(listener, address, acceptor, config, state).await
}

async fn run_home_accept_loop(
    listener: TcpListener,
    address: String,
    acceptor: tokio_rustls::TlsAcceptor,
    config: Config,
    state: Arc<State>,
) -> Result<()> {
    info!(%address, "home control listener ready");
    loop {
        let (socket, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let state = Arc::clone(&state);
        let config = config.clone();
        let timeout_duration = Duration::from_secs(config.handshake_timeout_secs);
        tokio::spawn(async move {
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
    require_peer(
        &identity,
        Role::Home,
        Some(&configured_home.id),
        &configured_home.spki_pins,
    )?;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    let setup_timeout = Duration::from_secs(config.handshake_timeout_secs);
    match reader
        .read_with_timeout::<ControlMessage>(setup_timeout)
        .await?
    {
        ControlMessage::Hello { role, id } if role == Role::Home && id == identity.id => {}
        _ => bail!("home HELLO does not match its certificate"),
    }
    write_json(
        &mut writer,
        &ControlMessage::Hello {
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
    if let Some(catalog) = catalog {
        publish_catalog(&state, catalog).await;
    }
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
                            let catalog = state.homes.lock().await.update_catalog(home, session_id);
                            if let Some(catalog) = catalog {
                                publish_catalog(&state, catalog).await;
                            }
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
            .pending
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
    let catalog = state.homes.lock().await.remove(home_id, session_id);
    if let Some(catalog) = catalog {
        state
            .authorization_acks
            .lock()
            .await
            .remove(&format!("home:{home_id}"));
        publish_catalog(state, catalog).await;
    }
}

async fn publish_catalog(state: &Arc<State>, catalog: Catalog) {
    let relays = state.relay_txs.lock().await.clone();
    for (relay_id, relay) in relays {
        if relay
            .send(ControlMessage::Catalog {
                catalog: catalog.clone(),
            })
            .await
            .is_err()
        {
            warn!(%relay_id, "failed to publish catalog to relay");
        }
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
    relay: RelayEndpoint,
    state: Arc<State>,
) -> Result<()> {
    let connector = client_connector(&config.cert, &config.key, &config.management_ca)?;
    let dns_name = server_name(&relay.server_name)?;
    loop {
        let result = async {
            let socket = TcpStream::connect(&relay.management_addr).await?;
            socket.set_nodelay(true)?;
            let stream = timeout(
                Duration::from_secs(config.handshake_timeout_secs),
                connector.connect(dns_name.clone(), socket),
            )
            .await
            .context("relay TLS handshake timed out")??;
            let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
            require_peer(
                &identity,
                Role::Relay,
                Some(&relay.id),
                &config.relay_spki_pins,
            )?;
            run_relay_session(stream, &config, &relay, &state).await
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
    relay: &RelayEndpoint,
    state: &Arc<State>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    write_json(
        &mut writer,
        &ControlMessage::Hello {
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
        ControlMessage::Hello { role, id } if role == Role::Relay && id == relay.id => {}
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
    tx.send(ControlMessage::RelayDirectory {
        directory: RelayDirectory {
            generation: 1,
            relays: config.relays.clone(),
        },
    })
    .await?;
    info!(
        event = "relay_directory_published",
        relay_id = %relay.id,
        generation = 1,
        relay_count = config.relays.len(),
        "published relay directory to relay"
    );
    let catalog = state.homes.lock().await.catalog();
    tx.send(ControlMessage::Catalog { catalog }).await?;
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
                            Ok(()) => {
                                write_json(
                                    &mut writer,
                                    &ControlMessage::TravelSessionAccepted { request_id },
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
                            state,
                            &mut writer,
                        )
                        .await?;
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(&mut writer, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
                    ControlMessage::TravelAuthorizationAck { generation } => {
                        record_authorization_ack(state, format!("relay:{}", relay.id), generation).await;
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
    if home.is_none() {
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
    }

    let work_id = Uuid::new_v4();
    let mut secret = vec![0_u8; 32];
    SystemRandom::new()
        .fill(&mut secret)
        .map_err(|_| anyhow!("AWS-LC random generation failed"))?;
    let mut pending = state.pending.lock().await;
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
        work_id,
        PendingWork {
            credential_id: request.credential,
            home_id: request.home.clone(),
            secret: secret.clone(),
            expires: Instant::now() + Duration::from_secs(config.work_ttl_secs),
            home: None,
            relay: None,
        },
    );
    drop(pending);
    let home = home.ok_or_else(|| anyhow!("home disappeared during route setup"))?;
    home.send(ControlMessage::OpenWork {
        work_id,
        work_secret: secret.clone(),
        credential_id: request.credential,
    })
    .await?;
    write_json(
        writer,
        &ControlMessage::ServerRouteGrant {
            request_id: request.request,
            work_id,
            work_secret: secret,
            credential_id: request.credential,
            home_id: request.home,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    Ok(())
}

async fn run_data_listeners(config: Config, state: Arc<State>) -> Result<()> {
    let mut listeners = Vec::with_capacity(config.data_listens.len());
    for address in &config.data_listens {
        let listener = TcpListener::bind(address)
            .await
            .with_context(|| format!("failed to bind data listener {address}"))?;
        listeners.push((address.clone(), listener));
    }
    let mut tasks = JoinSet::new();
    for (address, listener) in listeners {
        let state = Arc::clone(&state);
        tasks.spawn(async move { run_data_accept_loop(listener, address, state).await });
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    bail!("all data listeners stopped")
}

async fn run_data_accept_loop(
    listener: TcpListener,
    address: String,
    state: Arc<State>,
) -> Result<()> {
    info!(%address, "data pairing listener ready");
    loop {
        let (mut socket, peer) = listener.accept().await?;
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            let result = async {
                let (preface, mac) = timeout(Duration::from_secs(10), read_preface(&mut socket))
                    .await
                    .context("route preface timed out")??;
                let pair = {
                    let mut pending = state.pending.lock().await;
                    let entry = pending
                        .get_mut(&preface.id)
                        .ok_or_else(|| anyhow!("unknown or expired work id"))?;
                    if !verify_preface(preface, &mac, &entry.secret) {
                        bail!("invalid work preface MAC");
                    }
                    match preface.side {
                        RouteSide::Home if entry.home.is_none() => entry.home = Some(socket),
                        RouteSide::Relay if entry.relay.is_none() => entry.relay = Some(socket),
                        _ => bail!("duplicate or invalid work side"),
                    }
                    if entry.home.is_some() && entry.relay.is_some() {
                        let mut completed = pending
                            .remove(&preface.id)
                            .ok_or_else(|| anyhow!("pending work vanished"))?;
                        Some((
                            completed.credential_id,
                            completed.home_id,
                            completed
                                .home
                                .take()
                                .ok_or_else(|| anyhow!("missing home"))?,
                            completed
                                .relay
                                .take()
                                .ok_or_else(|| anyhow!("missing relay"))?,
                        ))
                    } else {
                        None
                    }
                };
                if let Some((credential_id, home_id, mut home, mut relay)) = pair {
                    info!(work_id = %preface.id, %credential_id, %home_id, "paired opaque server work sockets");
                    tokio::select! {
                        result = copy_bidirectional(&mut home, &mut relay) => {
                            let _ = result?;
                        }
                        () = wait_until_credential_inactive(&state, credential_id) => {
                            info!(event = "revoked_work_closed", work_id = %preface.id, %credential_id, "closed Server data work for inactive Travel credential");
                        }
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                warn!(%peer, %error, "data work connection rejected");
            }
        });
    }
}

async fn cleanup_pending(state: Arc<State>) -> Result<()> {
    let mut timer = interval(Duration::from_secs(1));
    loop {
        timer.tick().await;
        let now = Instant::now();
        state.pending.lock().await.retain(|work_id, pending| {
            let keep = pending.expires > now;
            if !keep {
                warn!(%work_id, "expired incomplete work pairing");
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
) -> std::result::Result<(), String> {
    let now = unix_time_secs().map_err(|error| error.to_string())?;
    {
        let authorization = state.authorization.read().await;
        let credential = authorization
            .verified()
            .credential(credential_id)
            .ok_or_else(|| "unknown Travel credential".to_owned())?;
        if credential.travel_id != travel_id {
            return Err("Travel credential identity does not match".to_owned());
        }
        if !authorization.verified().is_active(credential_id, now) {
            return Err("Travel credential is revoked, expired, or not yet valid".to_owned());
        }
        if home_id.is_some_and(|home_id| !credential.allows_home(home_id)) {
            return Err("Travel credential is not authorized for the requested Home".to_owned());
        }
    }
    state.travel_sessions.lock().await.authorize(
        travel_id,
        travel_session_id,
        lease_id,
        Instant::now(),
    )
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
    for (relay_id, relay) in relays {
        if relay.send(message.clone()).await.is_err() {
            warn!(%relay_id, "failed to publish Travel authorization state to Relay");
        }
    }
    let homes = state.homes.lock().await.senders();
    for (home_id, home) in homes {
        if home.send(message.clone()).await.is_err() {
            warn!(%home_id, "failed to publish Travel authorization state to Home");
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

async fn wait_until_credential_inactive(state: &Arc<State>, credential_id: Uuid) {
    let mut authorization_updates = state.authorization_tx.subscribe();
    loop {
        let remaining = {
            let Ok(now) = unix_time_secs() else { return };
            let authorization = state.authorization.read().await;
            if !authorization.verified().is_active(credential_id, now) {
                return;
            }
            let Some(credential) = authorization.verified().credential(credential_id) else {
                return;
            };
            Duration::from_secs(credential.not_after_unix_secs.saturating_sub(now).max(1))
        };
        tokio::select! {
            updated = authorization_updates.changed() => {
                if updated.is_err() {
                    return;
                }
            }
            () = sleep(remaining) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use flowsplice_core::protocol::{HomeCatalog, Service, ServiceProtocol};
    use tokio::sync::{mpsc, watch};
    use uuid::Uuid;

    use super::{
        ConfiguredHome, HomeRegistry, HomeSession, TRAVEL_SESSION_LEASE, TravelSessionRegistry,
        validate_homes, validate_listens,
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

    #[test]
    fn accepts_distinct_ipv4_and_ipv6_listeners() {
        let listens = vec!["192.0.2.1:7444".to_owned(), "[2001:db8::1]:7444".to_owned()];
        assert!(validate_listens(&listens, "data").is_ok());
    }

    #[test]
    fn rejects_empty_duplicate_and_malformed_listeners() {
        assert!(validate_listens(&[], "data").is_err());
        assert!(
            validate_listens(
                &["127.0.0.1:7444".to_owned(), "127.0.0.1:7444".to_owned()],
                "data"
            )
            .is_err()
        );
        assert!(validate_listens(&["not-an-address".to_owned()], "data").is_err());
    }

    #[test]
    fn configured_homes_require_unique_ids_and_independent_pins() {
        let pin = "11".repeat(32);
        let homes = vec![
            ConfiguredHome {
                id: "home-1".to_owned(),
                spki_pins: vec![pin.clone()],
            },
            ConfiguredHome {
                id: "home-2".to_owned(),
                spki_pins: vec!["22".repeat(32)],
            },
        ];
        assert!(validate_homes(&homes).is_ok());
        assert!(validate_homes(&[]).is_err());
        assert!(
            validate_homes(&[
                ConfiguredHome {
                    id: "home-1".to_owned(),
                    spki_pins: vec![pin.clone()],
                },
                ConfiguredHome {
                    id: "home-1".to_owned(),
                    spki_pins: vec![pin],
                },
            ])
            .is_err()
        );
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
}
