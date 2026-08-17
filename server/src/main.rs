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
    config::load_toml,
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{Catalog, ControlMessage, RelayDirectory, RelayEndpoint, Role},
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

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "server.toml")]
    config: PathBuf,
}

#[derive(Clone, Deserialize)]
struct Config {
    id: String,
    control_listen: String,
    data_listen: String,
    relays: Vec<RelayEndpoint>,
    cert: PathBuf,
    key: PathBuf,
    management_ca: PathBuf,
    home_id: String,
    #[serde(default)]
    home_spki_pins: Vec<String>,
    #[serde(default)]
    relay_spki_pins: Vec<String>,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout_secs: u64,
    #[serde(default = "default_work_ttl")]
    work_ttl_secs: u64,
    #[serde(default = "default_max_pending_work")]
    max_pending_work: usize,
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

struct PendingWork {
    secret: Vec<u8>,
    expires: Instant,
    home: Option<TcpStream>,
    relay: Option<TcpStream>,
}

struct HomeSession {
    session_id: Uuid,
    tx: mpsc::Sender<ControlMessage>,
    shutdown: watch::Sender<bool>,
}

#[derive(Default)]
struct State {
    catalog: RwLock<Catalog>,
    home_session: Mutex<Option<HomeSession>>,
    relay_txs: Mutex<HashMap<String, mpsc::Sender<ControlMessage>>>,
    pending: Mutex<HashMap<Uuid, PendingWork>>,
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
    validate_spki_pins(&config.home_spki_pins, "home")?;
    validate_spki_pins(&config.relay_spki_pins, "relay")?;
    validate_relays(&config.relays)?;
    let state = Arc::new(State::default());

    let control = run_home_listener(config.clone(), Arc::clone(&state));
    let data = run_data_listener(config.clone(), Arc::clone(&state));
    let relay = run_relay_connectors(config, Arc::clone(&state));
    let cleanup = cleanup_pending(state);

    tokio::try_join!(control, data, relay, cleanup)?;
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

async fn run_home_listener(config: Config, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&config.control_listen)
        .await
        .with_context(|| format!("failed to bind home control {}", config.control_listen))?;
    let acceptor = server_acceptor(&config.cert, &config.key, &config.management_ca)?;
    info!(address = %config.control_listen, "home control listener ready");
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

async fn handle_home(
    stream: TlsStream<TcpStream>,
    state: Arc<State>,
    config: &Config,
) -> Result<()> {
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(
        &identity,
        Role::Home,
        Some(&config.home_id),
        &config.home_spki_pins,
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

    let ControlMessage::HomeRegister { catalog } = reader
        .read_with_timeout::<ControlMessage>(setup_timeout)
        .await?
    else {
        bail!("home did not register a catalog after HELLO");
    };
    if catalog.home_id != identity.id {
        bail!("catalog home id does not match the authenticated home");
    }
    publish_catalog(&state, catalog.clone()).await;

    let (tx, mut rx) = mpsc::channel::<ControlMessage>(32);
    let session_id = Uuid::new_v4();
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    let previous = state.home_session.lock().await.replace(HomeSession {
        session_id,
        tx: tx.clone(),
        shutdown,
    });
    if let Some(previous) = previous {
        warn!(
            old_session_id = %previous.session_id,
            new_session_id = %session_id,
            home_id = %identity.id,
            "superseding existing Home session"
        );
        let _ = previous.shutdown.send(true);
    }
    let writer_task = tokio::spawn(async move {
        while let Some(message) = rx.recv().await {
            write_json(&mut writer, &message, CONTROL_FRAME_LIMIT).await?;
        }
        Ok::<_, std::io::Error>(())
    });

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
                        ControlMessage::HomeRegister { catalog } => {
                            if catalog.home_id != identity.id {
                                bail!("catalog home id does not match the authenticated home");
                            }
                            publish_catalog(&state, catalog).await;
                        }
                        ControlMessage::Heartbeat { nonce } => {
                            tx.send(ControlMessage::HeartbeatAck { nonce }).await?;
                        }
                        ControlMessage::HeartbeatAck { .. } => {}
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
    clear_home_session(&state, session_id).await;
    result
}

async fn clear_home_session(state: &Arc<State>, session_id: Uuid) {
    let mut current = state.home_session.lock().await;
    if current
        .as_ref()
        .is_some_and(|session| session.session_id == session_id)
    {
        *current = None;
    }
}

async fn publish_catalog(state: &Arc<State>, catalog: Catalog) {
    *state.catalog.write().await = catalog.clone();
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
        sleep(Duration::from_secs(1)).await;
    }
}

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

    let (tx, mut rx) = mpsc::channel::<ControlMessage>(64);
    state
        .relay_txs
        .lock()
        .await
        .insert(relay.id.clone(), tx.clone());
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
    let catalog = state.catalog.read().await.clone();
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
                    ControlMessage::RouteRequest { request_id, travel_id: _ } => {
                        handle_route_request(request_id, config, state, &mut writer).await?;
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(&mut writer, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
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
    request_id: Uuid,
    config: &Config,
    state: &Arc<State>,
    writer: &mut W,
) -> Result<()> {
    let home = state
        .home_session
        .lock()
        .await
        .as_ref()
        .map(|session| session.tx.clone());
    if home.is_none() {
        write_json(
            writer,
            &ControlMessage::RouteDenied {
                request_id,
                reason: "home agent is unavailable".to_owned(),
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
                request_id,
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
    })
    .await?;
    write_json(
        writer,
        &ControlMessage::ServerRouteGrant {
            request_id,
            work_id,
            work_secret: secret,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    Ok(())
}

async fn run_data_listener(config: Config, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&config.data_listen)
        .await
        .with_context(|| format!("failed to bind data listener {}", config.data_listen))?;
    info!(address = %config.data_listen, "data pairing listener ready");
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
                if let Some((mut home, mut relay)) = pair {
                    info!(work_id = %preface.id, "paired opaque server work sockets");
                    let _ = copy_bidirectional(&mut home, &mut relay).await?;
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
    }
}
