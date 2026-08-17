#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
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
    frame::{read_json, write_json},
    init_crypto,
    protocol::{Catalog, ControlMessage, Role},
    route::{RouteSide, read_preface, verify_preface, write_preface},
    tls::{peer_identity, require_peer, server_acceptor, validate_spki_pins},
};
use serde::Deserialize;
#[cfg(not(target_os = "linux"))]
use tokio::io::copy_bidirectional;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, RwLock, mpsc, oneshot},
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

#[derive(Default)]
struct State {
    server_tx: Mutex<Option<mpsc::Sender<ControlMessage>>>,
    catalog: RwLock<Catalog>,
    requests: Mutex<HashMap<Uuid, oneshot::Sender<Result<ServerGrant, String>>>>,
    routes: Mutex<HashMap<Uuid, PendingRoute>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_crypto();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flowsplice_relay=info".into()),
        )
        .init();

    let args = Args::parse();
    let config: Config = load_toml(&args.config)?;
    validate_spki_pins(&config.server_spki_pins, "server")?;
    validate_spki_pins(&config.travel_spki_pins, "travel")?;
    let state = Arc::new(State::default());
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

async fn handle_server(
    stream: TlsStream<TcpStream>,
    server_id: String,
    config: &Config,
    state: Arc<State>,
) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    match read_json::<_, ControlMessage>(&mut reader, CONTROL_FRAME_LIMIT).await? {
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
    *state.server_tx.lock().await = Some(tx);
    info!(%server_id, "server connected to relay");

    loop {
        tokio::select! {
            outgoing = rx.recv() => {
                let Some(outgoing) = outgoing else { bail!("server writer channel closed"); };
                write_json(&mut writer, &outgoing, CONTROL_FRAME_LIMIT).await?;
            }
            incoming = read_json::<_, ControlMessage>(&mut reader, CONTROL_FRAME_LIMIT) => {
                match incoming? {
                    ControlMessage::Catalog { catalog } => {
                        *state.catalog.write().await = catalog;
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
        }
    }
}

async fn handle_travel(
    mut stream: TlsStream<TcpStream>,
    travel_id: String,
    config: &Config,
    state: Arc<State>,
) -> Result<()> {
    match read_json::<_, ControlMessage>(&mut stream, CONTROL_FRAME_LIMIT).await? {
        ControlMessage::Hello { role, id } if role == Role::Travel && id == travel_id => {}
        _ => bail!("travel HELLO does not match its certificate"),
    }
    write_json(
        &mut stream,
        &ControlMessage::Hello {
            role: Role::Relay,
            id: config.id.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    write_json(
        &mut stream,
        &ControlMessage::Catalog {
            catalog: state.catalog.read().await.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;

    loop {
        let message = read_json::<_, ControlMessage>(&mut stream, CONTROL_FRAME_LIMIT).await?;
        match message {
            ControlMessage::RouteRequest {
                request_id,
                travel_id: declared_id,
            } if declared_id == travel_id => {
                if state.routes.lock().await.len() >= config.max_pending_routes {
                    write_json(
                        &mut stream,
                        &ControlMessage::RouteDenied {
                            request_id,
                            reason: "relay pending-route limit reached".to_owned(),
                        },
                        CONTROL_FRAME_LIMIT,
                    )
                    .await?;
                    continue;
                }
                let result = request_server_route(request_id, &travel_id, &state).await;
                match result {
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
                                expires: Instant::now()
                                    + Duration::from_secs(config.route_ttl_secs),
                            },
                        );
                        write_json(
                            &mut stream,
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
                            &mut stream,
                            &ControlMessage::RouteDenied { request_id, reason },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
                    }
                }
            }
            ControlMessage::Heartbeat { nonce } => {
                write_json(
                    &mut stream,
                    &ControlMessage::HeartbeatAck { nonce },
                    CONTROL_FRAME_LIMIT,
                )
                .await?;
            }
            _ => bail!("unexpected message from travel agent"),
        }
    }
}

async fn request_server_route(
    request_id: Uuid,
    travel_id: &str,
    state: &Arc<State>,
) -> Result<ServerGrant, String> {
    let server = state
        .server_tx
        .lock()
        .await
        .clone()
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
