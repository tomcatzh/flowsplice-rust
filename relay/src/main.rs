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
    protocol::{Catalog, ControlMessage, RelayDirectory, Role},
    route::{RouteSide, read_preface, verify_preface, write_preface},
    tls::{peer_identity, require_peer, server_acceptor, validate_spki_pins},
};
use serde::Deserialize;
#[cfg(not(target_os = "linux"))]
use tokio::io::copy_bidirectional;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, mpsc, oneshot, watch},
    time::{interval, timeout},
};
use tokio_rustls::server::TlsStream;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "relay.toml")]
    config: PathBuf,
}

#[derive(Clone, Deserialize)]
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
    #[serde(default)]
    server_spki_pins: Vec<String>,
    #[serde(default)]
    travel_spki_pins: Vec<String>,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout_secs: u64,
    #[serde(default = "default_route_ttl")]
    route_ttl_secs: u64,
    #[serde(default = "default_max_pending_routes")]
    max_pending_routes: usize,
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

struct ServerGrant {
    work_id: Uuid,
    work_secret: Vec<u8>,
}

struct PendingRoute {
    work_id: Uuid,
    route_secret: Vec<u8>,
    work_secret: Vec<u8>,
    expires: Instant,
}

struct ServerSession {
    session_id: Uuid,
    tx: mpsc::Sender<ControlMessage>,
    shutdown: watch::Sender<bool>,
}

struct State {
    server_session: Mutex<Option<ServerSession>>,
    catalog_tx: watch::Sender<Catalog>,
    directory_tx: watch::Sender<RelayDirectory>,
    requests: Mutex<HashMap<Uuid, oneshot::Sender<Result<ServerGrant, String>>>>,
    routes: Mutex<HashMap<Uuid, PendingRoute>>,
}

impl State {
    fn new() -> Self {
        let (catalog_tx, _) = watch::channel(Catalog::default());
        let (directory_tx, _) = watch::channel(RelayDirectory::default());
        Self {
            server_session: Mutex::new(None),
            catalog_tx,
            directory_tx,
            requests: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
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
    validate_spki_pins(&config.server_spki_pins, "server")?;
    validate_spki_pins(&config.travel_spki_pins, "travel")?;
    let state = Arc::new(State::new());
    tokio::try_join!(
        run_management(config.clone(), Arc::clone(&state)),
        run_data(config, Arc::clone(&state)),
        cleanup_routes(state),
    )?;
    Ok(())
}

async fn run_management(config: Config, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&config.management_listen)
        .await
        .with_context(|| format!("failed to bind management {}", config.management_listen))?;
    let acceptor = server_acceptor(&config.cert, &config.key, &config.management_ca)?;
    info!(address = %config.management_listen, "relay management listener ready");
    loop {
        let (socket, peer) = listener.accept().await?;
        let acceptor = acceptor.clone();
        let config = config.clone();
        let state = Arc::clone(&state);
        tokio::spawn(async move {
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
                        require_peer(&identity, Role::Travel, None, &config.travel_spki_pins)?;
                        handle_travel(stream, identity.id, &config, state).await
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
                        ControlMessage::Catalog { catalog } => {
                            state.catalog_tx.send_replace(catalog);
                        }
                        ControlMessage::RelayDirectory { directory } => {
                            info!(
                                event = "relay_directory_received",
                                generation = directory.generation,
                                relay_count = directory.relays.len(),
                                "relay received directory from server"
                            );
                            state.directory_tx.send_replace(directory);
                        }
                        ControlMessage::ServerRouteGrant { request_id, work_id, work_secret } => {
                            if let Some(waiter) = state.requests.lock().await.remove(&request_id) {
                                let _ = waiter.send(Ok(ServerGrant { work_id, work_secret }));
                            }
                        }
                        ControlMessage::RouteDenied { request_id, reason } => {
                            if let Some(waiter) = state.requests.lock().await.remove(&request_id) {
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
    travel_id: String,
    config: &Config,
    state: Arc<State>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    match reader
        .read_with_timeout::<ControlMessage>(Duration::from_secs(config.handshake_timeout_secs))
        .await?
    {
        ControlMessage::Hello { role, id } if role == Role::Travel && id == travel_id => {}
        _ => bail!("travel HELLO does not match its certificate"),
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
    let mut catalog_rx = state.catalog_tx.subscribe();
    let mut directory_rx = state.directory_tx.subscribe();
    let initial_directory = directory_rx.borrow().clone();
    info!(
        event = "relay_directory_forwarded",
        %travel_id,
        generation = initial_directory.generation,
        relay_count = initial_directory.relays.len(),
        "relay forwarded initial directory to travel"
    );
    write_json(
        &mut writer,
        &ControlMessage::RelayDirectory {
            directory: initial_directory,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    let initial_catalog = catalog_rx.borrow().clone();
    write_json(
        &mut writer,
        &ControlMessage::Catalog {
            catalog: initial_catalog,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;

    let mut liveness = interval(Duration::from_secs(10));
    let mut last_received = Instant::now();
    loop {
        tokio::select! {
            message = reader.read::<ControlMessage>() => {
                last_received = Instant::now();
                match message? {
                    ControlMessage::RouteRequest {
                        request_id,
                        travel_id: declared_id,
                    } if declared_id == travel_id => {
                        handle_travel_route(request_id, &travel_id, config, &state, &mut writer)
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
            changed = catalog_rx.changed() => {
                changed.map_err(|_| anyhow!("catalog publisher closed"))?;
                let catalog = catalog_rx.borrow_and_update().clone();
                write_json(
                    &mut writer,
                    &ControlMessage::Catalog {
                        catalog,
                    },
                    CONTROL_FRAME_LIMIT,
                )
                .await?;
            }
            changed = directory_rx.changed() => {
                changed.map_err(|_| anyhow!("directory publisher closed"))?;
                let directory = directory_rx.borrow_and_update().clone();
                write_json(
                    &mut writer,
                    &ControlMessage::RelayDirectory { directory },
                    CONTROL_FRAME_LIMIT,
                )
                .await?;
            }
            _ = liveness.tick() => {
                if last_received.elapsed() > Duration::from_secs(30) {
                    bail!("travel management heartbeat timed out");
                }
            }
        }
    }
}

async fn handle_travel_route<W: tokio::io::AsyncWrite + Unpin>(
    request_id: Uuid,
    travel_id: &str,
    config: &Config,
    state: &Arc<State>,
    writer: &mut W,
) -> Result<()> {
    if state.routes.lock().await.len() >= config.max_pending_routes {
        write_json(
            writer,
            &ControlMessage::RouteDenied {
                request_id,
                reason: "relay pending-route limit reached".to_owned(),
            },
            CONTROL_FRAME_LIMIT,
        )
        .await?;
        return Ok(());
    }
    match request_server_route(request_id, travel_id, state).await {
        Ok(grant) => {
            let route_id = Uuid::new_v4();
            let mut route_secret = vec![0_u8; 32];
            SystemRandom::new()
                .fill(&mut route_secret)
                .map_err(|_| anyhow!("AWS-LC random generation failed"))?;
            state.routes.lock().await.insert(
                route_id,
                PendingRoute {
                    work_id: grant.work_id,
                    route_secret: route_secret.clone(),
                    work_secret: grant.work_secret,
                    expires: Instant::now() + Duration::from_secs(config.route_ttl_secs),
                },
            );
            write_json(
                writer,
                &ControlMessage::RouteGrant {
                    request_id,
                    route_id,
                    route_secret,
                    data_addr: config.data_public_addr.clone(),
                },
                CONTROL_FRAME_LIMIT,
            )
            .await?;
        }
        Err(reason) => {
            write_json(
                writer,
                &ControlMessage::RouteDenied { request_id, reason },
                CONTROL_FRAME_LIMIT,
            )
            .await?;
        }
    }
    Ok(())
}

async fn request_server_route(
    request_id: Uuid,
    travel_id: &str,
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
    state.requests.lock().await.insert(request_id, tx);
    if server
        .send(ControlMessage::RouteRequest {
            request_id,
            travel_id: travel_id.to_owned(),
        })
        .await
        .is_err()
    {
        state.requests.lock().await.remove(&request_id);
        return Err("server connection closed".to_owned());
    }
    match timeout(Duration::from_secs(10), rx).await {
        Ok(Ok(result)) => result,
        Ok(Err(_)) => Err("route response channel closed".to_owned()),
        Err(_) => {
            state.requests.lock().await.remove(&request_id);
            Err("route request timed out".to_owned())
        }
    }
}

async fn run_data(config: Config, state: Arc<State>) -> Result<()> {
    let listener = TcpListener::bind(&config.data_listen)
        .await
        .with_context(|| format!("failed to bind relay data {}", config.data_listen))?;
    info!(address = %config.data_listen, "relay data listener ready");
    loop {
        let (mut travel, peer) = listener.accept().await?;
        let config = config.clone();
        let state = Arc::clone(&state);
        tokio::spawn(async move {
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
                if !verify_preface(preface, &mac, &route.route_secret) {
                    bail!("invalid route ticket MAC");
                }
                let mut server = TcpStream::connect(&config.server_data_addr).await?;
                write_preface(
                    &mut server,
                    RouteSide::Relay,
                    route.work_id,
                    &route.work_secret,
                )
                .await?;
                info!(route_id = %preface.id, work_id = %route.work_id, "relay entered opaque forwarding");
                zero_copy_or_portable(&mut travel, &mut server).await?;
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
