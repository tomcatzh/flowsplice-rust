#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, HashSet},
    future::pending,
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::Parser;
use embedded_spa::{EmbeddedSpa, EmbeddedSpaConfig};
use flowsplice_core::{
    CONTROL_FRAME_LIMIT, DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    config::load_toml,
    frame::{read_json, write_json},
    init_crypto,
    protocol::{Catalog, ControlMessage, DataFrame, Role, ServiceProtocol},
    route::{RouteSide, write_preface},
    tls::{client_connector, peer_identity, require_peer, server_name, validate_spki_pins},
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Mutex, RwLock, Semaphore, mpsc},
    task::JoinSet,
    time::{sleep, timeout},
};
use tokio_rustls::client::TlsStream;
use tracing::{info, warn};
use uuid::Uuid;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

static SPA: LazyLock<EmbeddedSpa<WebAssets>> = LazyLock::new(|| {
    EmbeddedSpa::new(EmbeddedSpaConfig::default())
        .unwrap_or_else(|error| panic!("invalid embedded Travel Agent UI: {error}"))
});

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "travelagent.toml")]
    config: PathBuf,
}

#[derive(Clone, Deserialize)]
struct Config {
    id: String,
    relay_management_addr: String,
    relay_server_name: String,
    relay_id: String,
    home_server_name: String,
    home_id: String,
    management_cert: PathBuf,
    management_key: PathBuf,
    management_ca: PathBuf,
    business_cert: PathBuf,
    business_key: PathBuf,
    business_ca: PathBuf,
    ui_listen: String,
    #[serde(default)]
    relay_spki_pins: Vec<String>,
    #[serde(default)]
    home_spki_pins: Vec<String>,
    #[serde(default)]
    allow_remote_listen: bool,
    #[serde(default)]
    admin_token: Option<String>,
    mappings: Vec<Mapping>,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout_secs: u64,
    #[serde(default = "default_udp_idle")]
    udp_idle_secs: u64,
    #[serde(default = "default_max_active_flows")]
    max_active_flows: usize,
}

#[derive(Clone, Deserialize, Serialize)]
struct Mapping {
    service_id: String,
    protocol: ServiceProtocol,
    bind: String,
}

const fn default_handshake_timeout() -> u64 {
    10
}

const fn default_udp_idle() -> u64 {
    60
}

const fn default_max_active_flows() -> usize {
    128
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    catalog: Arc<RwLock<Catalog>>,
    started: Instant,
    active_flows: Arc<std::sync::atomic::AtomicUsize>,
    permits: Arc<Semaphore>,
}

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    travel_id: String,
    uptime_secs: u64,
    active_flows: usize,
    catalog_generation: u64,
    mappings: Vec<Mapping>,
}

struct RouteGrant {
    route_id: Uuid,
    route_secret: Vec<u8>,
    data_addr: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_crypto();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flowsplice_travelagent=info".into()),
        )
        .init();
    let args = Args::parse();
    let config: Config = load_toml(&args.config)?;
    validate_config(&config)?;
    let permits = Arc::new(Semaphore::new(config.max_active_flows));
    let state = AppState {
        config: Arc::new(config),
        catalog: Arc::new(RwLock::new(Catalog::default())),
        started: Instant::now(),
        active_flows: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        permits,
    };

    let mut tasks = JoinSet::new();
    tasks.spawn(refresh_catalog(state.clone()));
    tasks.spawn(run_ui(state.clone()));
    for mapping in state.config.mappings.clone() {
        let state = state.clone();
        match mapping.protocol {
            ServiceProtocol::Tcp => tasks.spawn(run_tcp_listener(state, mapping)),
            ServiceProtocol::Udp => tasks.spawn(run_udp_listener(state, mapping)),
        };
    }
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}

fn validate_config(config: &Config) -> Result<()> {
    validate_spki_pins(&config.relay_spki_pins, "relay")?;
    validate_spki_pins(&config.home_spki_pins, "home")?;
    let ui_addr: SocketAddr = config.ui_listen.parse().context("invalid ui_listen")?;
    if !config.allow_remote_listen && !ui_addr.ip().is_loopback() {
        bail!("ui_listen must be loopback unless allow_remote_listen is true");
    }
    if !ui_addr.ip().is_loopback()
        && config
            .admin_token
            .as_deref()
            .is_none_or(|token| token.len() < 32)
    {
        bail!("a non-loopback UI requires admin_token with at least 32 characters");
    }
    let mut services = HashSet::new();
    let mut binds = HashSet::new();
    for mapping in &config.mappings {
        let bind: SocketAddr = mapping.bind.parse().context("invalid mapping bind")?;
        if !config.allow_remote_listen && !bind.ip().is_loopback() {
            bail!("mapping binds must be loopback unless allow_remote_listen is true");
        }
        if !services.insert((&mapping.service_id, mapping.protocol)) {
            bail!("mapping service/protocol pairs must be unique");
        }
        if !binds.insert(bind) {
            bail!("mapping bind addresses must be unique");
        }
    }
    Ok(())
}

async fn refresh_catalog(state: AppState) -> Result<()> {
    loop {
        match open_management(&state.config).await {
            Ok((mut stream, catalog)) => {
                *state.catalog.write().await = catalog;
                let _ = write_json(
                    &mut stream,
                    &ControlMessage::Heartbeat { nonce: 1 },
                    CONTROL_FRAME_LIMIT,
                )
                .await;
            }
            Err(error) => warn!(%error, "catalog refresh failed"),
        }
        sleep(Duration::from_secs(5)).await;
    }
}

async fn open_management(config: &Config) -> Result<(TlsStream<TcpStream>, Catalog)> {
    let connector = client_connector(
        &config.management_cert,
        &config.management_key,
        &config.management_ca,
    )?;
    let socket = TcpStream::connect(&config.relay_management_addr).await?;
    let mut stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        connector.connect(server_name(&config.relay_server_name)?, socket),
    )
    .await
    .context("relay TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(
        &identity,
        Role::Relay,
        Some(&config.relay_id),
        &config.relay_spki_pins,
    )?;
    write_json(
        &mut stream,
        &ControlMessage::Hello {
            role: Role::Travel,
            id: config.id.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    match read_json::<_, ControlMessage>(&mut stream, CONTROL_FRAME_LIMIT).await? {
        ControlMessage::Hello { role, id } if role == Role::Relay && id == config.relay_id => {}
        _ => bail!("relay sent an invalid HELLO"),
    }
    let ControlMessage::Catalog { catalog } =
        read_json::<_, ControlMessage>(&mut stream, CONTROL_FRAME_LIMIT).await?
    else {
        bail!("relay did not send a catalog");
    };
    Ok((stream, catalog))
}

async fn request_route(config: &Config) -> Result<RouteGrant> {
    let (mut stream, _) = open_management(config).await?;
    let request_id = Uuid::new_v4();
    write_json(
        &mut stream,
        &ControlMessage::RouteRequest {
            request_id,
            travel_id: config.id.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    match timeout(
        Duration::from_secs(10),
        read_json::<_, ControlMessage>(&mut stream, CONTROL_FRAME_LIMIT),
    )
    .await
    .context("route request timed out")??
    {
        ControlMessage::RouteGrant {
            request_id: response_id,
            route_id,
            route_secret,
            data_addr,
        } if response_id == request_id => Ok(RouteGrant {
            route_id,
            route_secret,
            data_addr,
        }),
        ControlMessage::RouteDenied {
            request_id: response_id,
            reason,
        } if response_id == request_id => bail!("route denied: {reason}"),
        _ => bail!("invalid route response"),
    }
}

async fn open_business(config: &Config) -> Result<TlsStream<TcpStream>> {
    let grant = request_route(config).await?;
    let mut socket = TcpStream::connect(&grant.data_addr).await?;
    write_preface(
        &mut socket,
        RouteSide::Travel,
        grant.route_id,
        &grant.route_secret,
    )
    .await?;
    let connector = client_connector(
        &config.business_cert,
        &config.business_key,
        &config.business_ca,
    )?;
    let stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        connector.connect(server_name(&config.home_server_name)?, socket),
    )
    .await
    .context("business TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(
        &identity,
        Role::Home,
        Some(&config.home_id),
        &config.home_spki_pins,
    )?;
    Ok(stream)
}

async fn run_tcp_listener(state: AppState, mapping: Mapping) -> Result<()> {
    let listener = TcpListener::bind(&mapping.bind).await?;
    info!(service_id = %mapping.service_id, address = %mapping.bind, "local TCP mapping ready");
    loop {
        let (local, peer) = listener.accept().await?;
        let Ok(permit) = Arc::clone(&state.permits).try_acquire_owned() else {
            warn!(%peer, "travel active-flow limit reached");
            continue;
        };
        let state = state.clone();
        let mapping = mapping.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _guard = FlowGuard::new(Arc::clone(&state.active_flows));
            if let Err(error) = run_tcp_flow(&state.config, &mapping, local).await {
                warn!(%peer, service_id = %mapping.service_id, %error, "TCP flow closed");
            }
        });
    }
}

async fn run_tcp_flow(config: &Config, mapping: &Mapping, local: TcpStream) -> Result<()> {
    let mut business = open_business(config).await?;
    let flow_id = Uuid::new_v4();
    write_json(
        &mut business,
        &DataFrame::Open {
            flow_id,
            service_id: mapping.service_id.clone(),
            protocol: ServiceProtocol::Tcp,
        },
        DATA_FRAME_LIMIT,
    )
    .await?;
    match read_json::<_, DataFrame>(&mut business, DATA_FRAME_LIMIT).await? {
        DataFrame::OpenOk { flow_id: id } if id == flow_id => {}
        DataFrame::OpenError { reason, .. } => bail!("home rejected TCP flow: {reason}"),
        _ => bail!("invalid TCP OPEN response"),
    }
    pump_tcp(business, local, flow_id).await
}

async fn pump_tcp(business: TlsStream<TcpStream>, local: TcpStream, flow_id: Uuid) -> Result<()> {
    let (mut tls_reader, mut tls_writer) = tokio::io::split(business);
    let (mut local_reader, mut local_writer) = local.into_split();
    let mut send_offset = 0_u64;
    let mut receive_offset = 0_u64;
    let mut local_eof = false;
    let mut remote_eof = false;
    let mut buffer = vec![0_u8; MAX_DATA_PAYLOAD];
    while !(local_eof && remote_eof) {
        tokio::select! {
            read = async {
                if local_eof { pending::<std::io::Result<usize>>().await } else { local_reader.read(&mut buffer).await }
            } => {
                let count = read?;
                if count == 0 {
                    local_eof = true;
                    write_json(&mut tls_writer, &DataFrame::Fin { flow_id, final_offset: send_offset }, DATA_FRAME_LIMIT).await?;
                } else {
                    write_json(&mut tls_writer, &DataFrame::Data { flow_id, offset: send_offset, bytes: buffer[..count].to_vec() }, DATA_FRAME_LIMIT).await?;
                    send_offset += count as u64;
                }
            }
            frame = read_json::<_, DataFrame>(&mut tls_reader, DATA_FRAME_LIMIT) => {
                match frame? {
                    DataFrame::Data { flow_id: id, offset, bytes } if id == flow_id && offset == receive_offset && bytes.len() <= MAX_DATA_PAYLOAD => {
                        local_writer.write_all(&bytes).await?;
                        receive_offset += bytes.len() as u64;
                        write_json(&mut tls_writer, &DataFrame::Ack { flow_id, next_offset: receive_offset }, DATA_FRAME_LIMIT).await?;
                    }
                    DataFrame::Fin { flow_id: id, final_offset } if id == flow_id && final_offset == receive_offset => {
                        local_writer.shutdown().await?;
                        remote_eof = true;
                    }
                    DataFrame::Ack { flow_id: id, .. } if id == flow_id => {}
                    DataFrame::Close { flow_id: id, reason } if id == flow_id => bail!("home closed flow: {reason}"),
                    _ => bail!("invalid TCP flow frame"),
                }
            }
        }
    }
    Ok(())
}

async fn run_udp_listener(state: AppState, mapping: Mapping) -> Result<()> {
    let socket = Arc::new(UdpSocket::bind(&mapping.bind).await?);
    info!(service_id = %mapping.service_id, address = %mapping.bind, "local UDP mapping ready");
    let associations: Arc<Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut buffer = vec![0_u8; 65_507];
    loop {
        let (count, peer) = socket.recv_from(&mut buffer).await?;
        let bytes = buffer[..count].to_vec();
        let existing = associations.lock().await.get(&peer).cloned();
        if let Some(tx) = existing {
            let _ = tx.send(bytes).await;
            continue;
        }
        let (tx, rx) = mpsc::channel(64);
        let Ok(permit) = Arc::clone(&state.permits).try_acquire_owned() else {
            warn!(%peer, "travel active-flow limit reached");
            continue;
        };
        tx.send(bytes).await?;
        associations.lock().await.insert(peer, tx);
        let socket = Arc::clone(&socket);
        let associations = Arc::clone(&associations);
        let state = state.clone();
        let mapping = mapping.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _guard = FlowGuard::new(Arc::clone(&state.active_flows));
            if let Err(error) = run_udp_association(&state.config, &mapping, socket, peer, rx).await
            {
                warn!(%peer, service_id = %mapping.service_id, %error, "UDP association closed");
            }
            associations.lock().await.remove(&peer);
        });
    }
}

async fn run_udp_association(
    config: &Config,
    mapping: &Mapping,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    mut outgoing: mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let mut business = open_business(config).await?;
    let flow_id = Uuid::new_v4();
    write_json(
        &mut business,
        &DataFrame::Open {
            flow_id,
            service_id: mapping.service_id.clone(),
            protocol: ServiceProtocol::Udp,
        },
        DATA_FRAME_LIMIT,
    )
    .await?;
    match read_json::<_, DataFrame>(&mut business, DATA_FRAME_LIMIT).await? {
        DataFrame::OpenOk { flow_id: id } if id == flow_id => {}
        DataFrame::OpenError { reason, .. } => bail!("home rejected UDP flow: {reason}"),
        _ => bail!("invalid UDP OPEN response"),
    }
    let (mut reader, mut writer) = tokio::io::split(business);
    let mut send_sequence = 0_u64;
    let mut receive_sequence = 0_u64;
    loop {
        tokio::select! {
            datagram = timeout(Duration::from_secs(config.udp_idle_secs), outgoing.recv()) => {
                let Some(bytes) = datagram.context("UDP association idle timeout")? else { return Ok(()); };
                write_json(&mut writer, &DataFrame::Datagram { flow_id, sequence: send_sequence, bytes }, DATA_FRAME_LIMIT).await?;
                send_sequence = send_sequence.wrapping_add(1);
            }
            frame = timeout(Duration::from_secs(config.udp_idle_secs), read_json::<_, DataFrame>(&mut reader, DATA_FRAME_LIMIT)) => {
                match frame.context("UDP association idle timeout")?? {
                    DataFrame::Datagram { flow_id: id, sequence, bytes } if id == flow_id && sequence >= receive_sequence && bytes.len() <= 65_507 => {
                        if sequence == receive_sequence {
                            socket.send_to(&bytes, peer).await?;
                            receive_sequence = receive_sequence.wrapping_add(1);
                        }
                    }
                    DataFrame::Close { flow_id: id, .. } if id == flow_id => return Ok(()),
                    _ => bail!("invalid UDP flow frame"),
                }
            }
        }
    }
}

async fn run_ui(state: AppState) -> Result<()> {
    let api = Router::new()
        .route("/status", get(api_status))
        .route("/catalog", get(api_catalog))
        .fallback(|| async { StatusCode::NOT_FOUND });
    let app = Router::new()
        .nest("/api", api)
        .fallback(serve_spa)
        .with_state(state.clone())
        .layer(from_fn_with_state(state.clone(), authorize_ui));
    let listener = TcpListener::bind(&state.config.ui_listen).await?;
    info!(address = %state.config.ui_listen, "Travel Agent UI ready");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn api_status(State(state): State<AppState>) -> Json<StatusResponse> {
    use std::sync::atomic::Ordering;
    let generation = state.catalog.read().await.generation;
    Json(StatusResponse {
        ok: true,
        travel_id: state.config.id.clone(),
        uptime_secs: state.started.elapsed().as_secs(),
        active_flows: state.active_flows.load(Ordering::Relaxed),
        catalog_generation: generation,
        mappings: state.config.mappings.clone(),
    })
}

async fn api_catalog(State(state): State<AppState>) -> Json<Catalog> {
    Json(state.catalog.read().await.clone())
}

async fn serve_spa(request: Request) -> Response {
    SPA.serve(request)
}

async fn authorize_ui(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let is_loopback = state
        .config
        .ui_listen
        .parse::<SocketAddr>()
        .is_ok_and(|address| address.ip().is_loopback());
    if is_loopback {
        return next.run(request).await;
    }
    let expected = state.config.admin_token.as_deref().unwrap_or_default();
    let authorized = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| token == expected);
    if authorized {
        next.run(request).await
    } else {
        let mut response = StatusCode::UNAUTHORIZED.into_response();
        response.headers_mut().insert(
            axum::http::header::WWW_AUTHENTICATE,
            axum::http::HeaderValue::from_static("Bearer"),
        );
        response
    }
}

struct FlowGuard(Arc<std::sync::atomic::AtomicUsize>);

impl FlowGuard {
    fn new(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        use std::sync::atomic::Ordering;
        counter.fetch_add(1, Ordering::Relaxed);
        Self(counter)
    }
}

impl Drop for FlowGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}
