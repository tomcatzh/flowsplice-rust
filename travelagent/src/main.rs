#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{self, IsTerminal},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::constant_time::verify_slices_are_equal;
use axum::{
    Json, Router,
    extract::{Request, State},
    http::StatusCode,
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::get,
};
use clap::{Parser, Subcommand};
use embedded_spa::{EmbeddedSpa, EmbeddedSpaConfig};
use flowsplice_core::{
    CONTROL_FRAME_LIMIT, DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    authorization::unix_time_secs,
    config::load_toml,
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{
        Catalog, ControlMessage, DataFrame, RelayDirectory, RelayEndpoint, Role, ServiceProtocol,
        TravelConnectionPurpose,
    },
    route::{RouteSide, write_preface},
    tls::{
        client_connector_with_private_key, peer_identity, require_peer, server_name,
        validate_spki_pins,
    },
};
use flowsplice_enrollment::{
    TravelEnrollmentResponse, create_enrollment_request, install_enrollment_response,
    key::{is_encrypted_private_key, load_private_key},
    load_json,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Mutex, RwLock, Semaphore, mpsc},
    task::JoinSet,
    time::{interval, sleep, timeout},
};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

mod tcp_flow;

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

static SPA: LazyLock<EmbeddedSpa<WebAssets>> = LazyLock::new(|| {
    EmbeddedSpa::new(EmbeddedSpaConfig::default())
        .unwrap_or_else(|error| panic!("invalid embedded Travel Agent UI: {error}"))
});

#[derive(Parser)]
struct Cli {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "travelagent.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    EnrollInit(EnrollInitArgs),
    EnrollImport(EnrollImportArgs),
}

#[derive(clap::Args)]
struct EnrollInitArgs {
    #[arg(long)]
    travel_id: String,
    #[arg(long)]
    output_dir: PathBuf,
    #[arg(long, hide = true)]
    test_password_file: Option<PathBuf>,
}

#[derive(clap::Args)]
struct EnrollImportArgs {
    #[arg(long)]
    enrollment_dir: PathBuf,
    #[arg(long)]
    response: PathBuf,
    #[arg(long)]
    management_ca: PathBuf,
    #[arg(long)]
    business_ca: PathBuf,
    #[arg(long, hide = true)]
    test_password_file: Option<PathBuf>,
}

#[derive(Clone, Deserialize)]
struct Config {
    id: String,
    seed_relays: Vec<RelayEndpoint>,
    homes: Vec<ConfiguredHome>,
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
    #[serde(default = "default_carrier_heartbeat")]
    carrier_heartbeat_secs: u64,
    #[serde(default = "default_carrier_timeout")]
    carrier_timeout_secs: u64,
    #[serde(default = "default_carrier_race_timeout")]
    carrier_race_timeout_secs: u64,
    #[serde(default = "default_carrier_recovery_timeout")]
    carrier_recovery_timeout_secs: u64,
    #[serde(default = "default_carrier_reevaluate")]
    carrier_reevaluate_secs: u64,
    #[serde(default = "default_max_carrier_reevaluate")]
    max_carrier_reevaluate_secs: u64,
    #[serde(default = "default_max_unacked_bytes")]
    max_unacked_bytes: usize,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredHome {
    id: String,
    server_name: String,
    #[serde(default)]
    spki_pins: Vec<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Mapping {
    home_id: String,
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

const fn default_carrier_heartbeat() -> u64 {
    2
}

const fn default_carrier_timeout() -> u64 {
    8
}

const fn default_carrier_race_timeout() -> u64 {
    10
}

const fn default_carrier_recovery_timeout() -> u64 {
    90
}

const fn default_carrier_reevaluate() -> u64 {
    60
}

const fn default_max_carrier_reevaluate() -> u64 {
    900
}

const fn default_max_unacked_bytes() -> usize {
    1_048_576
}

#[derive(Clone)]
struct AppState {
    config: Arc<Config>,
    session_id: Uuid,
    tls: Arc<TlsMaterial>,
    catalog: Arc<RwLock<Catalog>>,
    directory: Arc<RwLock<RelayDirectory>>,
    started: Instant,
    active_flows: Arc<std::sync::atomic::AtomicUsize>,
    permits: Arc<Semaphore>,
    flow_relays: Arc<Mutex<HashMap<Uuid, String>>>,
}

#[derive(Clone)]
struct TlsMaterial {
    management_connector: TlsConnector,
    business_connector: TlsConnector,
}

#[derive(Serialize)]
struct StatusResponse {
    ok: bool,
    travel_id: String,
    uptime_secs: u64,
    active_flows: usize,
    catalog_generation: u64,
    relay_directory_generation: u64,
    active_relays: Vec<String>,
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
        .with_ansi(io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flowsplice_travelagent=info".into()),
        )
        .init();
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        return run_command(command);
    }
    let config: Config = load_toml(&cli.config)?;
    validate_config(&config)?;
    let (management_key, business_key) = load_runtime_private_keys(&config)?;
    let tls = Arc::new(TlsMaterial {
        management_connector: client_connector_with_private_key(
            &config.management_cert,
            management_key,
            &config.management_ca,
        )?,
        business_connector: client_connector_with_private_key(
            &config.business_cert,
            business_key,
            &config.business_ca,
        )?,
    });
    let permits = Arc::new(Semaphore::new(config.max_active_flows));
    let state = AppState {
        config: Arc::new(config),
        session_id: Uuid::new_v4(),
        tls,
        catalog: Arc::new(RwLock::new(Catalog::default())),
        directory: Arc::new(RwLock::new(RelayDirectory::default())),
        started: Instant::now(),
        active_flows: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        permits,
        flow_relays: Arc::new(Mutex::new(HashMap::new())),
    };

    let mut tasks = JoinSet::new();
    tasks.spawn(run_catalog_subscription(state.clone()));
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

fn run_command(command: Command) -> Result<()> {
    match command {
        Command::EnrollInit(args) => {
            let password = if let Some(path) = args.test_password_file.as_deref() {
                test_password(path)?
            } else {
                prompt_new_private_key_password()?
            };
            if password.len() < 12 {
                bail!("private-key password must contain at least 12 characters");
            }
            let request = create_enrollment_request(
                &args.travel_id,
                password.as_bytes(),
                &args.output_dir,
                unix_time_secs()?,
            )?;
            println!(
                "created Travel enrollment request {} ({})",
                request.request_id,
                args.output_dir.display()
            );
            Ok(())
        }
        Command::EnrollImport(args) => {
            let password = if let Some(path) = args.test_password_file.as_deref() {
                test_password(path)?
            } else {
                Zeroizing::new(rpassword::prompt_password("Travel private-key password: ")?)
            };
            if password.is_empty() {
                bail!("private-key password must not be empty");
            }
            let response: TravelEnrollmentResponse = load_json(&args.response)?;
            let credential = install_enrollment_response(
                &args.enrollment_dir,
                &response,
                &args.management_ca,
                &args.business_ca,
                password.as_bytes(),
                unix_time_secs()?,
            )?;
            println!(
                "installed Travel credential {} for {}",
                credential.credential_id, credential.travel_id
            );
            Ok(())
        }
    }
}

fn prompt_new_private_key_password() -> Result<Zeroizing<String>> {
    let password = Zeroizing::new(rpassword::prompt_password(
        "New Travel private-key password: ",
    )?);
    if password.len() < 12 {
        bail!("private-key password must contain at least 12 characters");
    }
    let confirmation = Zeroizing::new(rpassword::prompt_password("Confirm password: ")?);
    if password.as_bytes() != confirmation.as_bytes() {
        bail!("private-key passwords do not match");
    }
    Ok(password)
}

fn load_runtime_private_keys(
    config: &Config,
) -> Result<(
    rustls_pki_types::PrivateKeyDer<'static>,
    rustls_pki_types::PrivateKeyDer<'static>,
)> {
    let management_encrypted = is_encrypted_private_key(&config.management_key)?;
    let business_encrypted = is_encrypted_private_key(&config.business_key)?;
    if management_encrypted != business_encrypted {
        bail!("Travel management and business keys must use the same protection mode");
    }
    if !management_encrypted {
        if env::var("FLOWSPLICE_ALLOW_UNENCRYPTED_TEST_KEYS").as_deref() != Ok("1") {
            bail!("unencrypted Travel private keys are forbidden");
        }
        return Ok((
            load_private_key(&config.management_key, None, true)?,
            load_private_key(&config.business_key, None, true)?,
        ));
    }
    let password = runtime_password()?;
    Ok((
        load_private_key(&config.management_key, Some(password.as_bytes()), false)?,
        load_private_key(&config.business_key, Some(password.as_bytes()), false)?,
    ))
}

fn runtime_password() -> Result<Zeroizing<String>> {
    if let Some(path) = env::var_os("FLOWSPLICE_TEST_PRIVATE_KEY_PASSWORD_FILE") {
        if env::var("FLOWSPLICE_ALLOW_TEST_PASSWORD_FILE").as_deref() != Ok("1") {
            bail!("test private-key password files are disabled");
        }
        return read_password_file(Path::new(&path));
    }
    let password = Zeroizing::new(rpassword::prompt_password("Travel private-key password: ")?);
    if password.is_empty() {
        bail!("private-key password must not be empty");
    }
    Ok(password)
}

fn test_password(path: &Path) -> Result<Zeroizing<String>> {
    if env::var("FLOWSPLICE_ALLOW_TEST_PASSWORD_FILE").as_deref() != Ok("1") {
        bail!("--test-password-file is disabled outside the explicit test environment");
    }
    read_password_file(path)
}

fn read_password_file(path: &Path) -> Result<Zeroizing<String>> {
    let mut password = Zeroizing::new(
        fs::read_to_string(path)
            .with_context(|| format!("failed to read test password file {}", path.display()))?,
    );
    while password.ends_with('\n') || password.ends_with('\r') {
        password.pop();
    }
    if password.is_empty() {
        return Err(anyhow!("test private-key password file must not be empty"));
    }
    Ok(password)
}

fn validate_config(config: &Config) -> Result<()> {
    validate_spki_pins(&config.relay_spki_pins, "relay")?;
    let mut home_ids = HashSet::new();
    for home in &config.homes {
        if home.id.is_empty() || home.server_name.is_empty() || !home_ids.insert(home.id.as_str()) {
            bail!("Home ids must be non-empty and unique, with non-empty TLS server names");
        }
        validate_spki_pins(&home.spki_pins, &format!("Home {}", home.id))?;
        let _ = server_name(&home.server_name)?;
    }
    if config.homes.is_empty() {
        bail!("at least one Home Agent is required");
    }
    if config.seed_relays.is_empty() {
        bail!("at least one seed relay is required");
    }
    let mut relay_ids = HashSet::new();
    for relay in &config.seed_relays {
        if relay.id.is_empty()
            || relay.management_addr.is_empty()
            || relay.server_name.is_empty()
            || !relay_ids.insert(&relay.id)
        {
            bail!("seed relay ids must be non-empty and unique");
        }
    }
    if config.carrier_heartbeat_secs == 0
        || config.carrier_timeout_secs <= config.carrier_heartbeat_secs
        || config.carrier_race_timeout_secs == 0
        || config.carrier_recovery_timeout_secs <= config.carrier_race_timeout_secs
        || config.carrier_reevaluate_secs == 0
        || config.max_carrier_reevaluate_secs < config.carrier_reevaluate_secs
        || config.max_unacked_bytes < MAX_DATA_PAYLOAD
    {
        bail!("carrier timeout, reevaluation, or unacknowledged-data limits are invalid");
    }
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
        if mapping.service_id.is_empty() || !home_ids.contains(mapping.home_id.as_str()) {
            bail!("every mapping must name a configured Home and a non-empty service");
        }
        let bind: SocketAddr = mapping.bind.parse().context("invalid mapping bind")?;
        if !config.allow_remote_listen && !bind.ip().is_loopback() {
            bail!("mapping binds must be loopback unless allow_remote_listen is true");
        }
        if !services.insert((&mapping.home_id, &mapping.service_id, mapping.protocol)) {
            bail!("mapping Home/service/protocol tuples must be unique");
        }
        if !binds.insert(bind) {
            bail!("mapping bind addresses must be unique");
        }
    }
    Ok(())
}

async fn run_catalog_subscription(state: AppState) -> Result<()> {
    loop {
        let mut connected = false;
        for relay in relay_candidates(&state).await {
            match open_management(&state, &relay, TravelConnectionPurpose::Catalog).await {
                Ok((stream, directory, catalog)) => {
                    update_directory(&state, directory).await;
                    *state.catalog.write().await = catalog;
                    info!(relay_id = %relay.id, relay = %relay.management_addr, "catalog subscription connected");
                    connected = true;
                    if let Err(error) = run_catalog_session(&state, &relay, stream).await {
                        warn!(relay_id = %relay.id, %error, "catalog subscription disconnected");
                    }
                    break;
                }
                Err(error) => {
                    warn!(relay_id = %relay.id, %error, "catalog subscription attempt failed");
                }
            }
        }
        if !connected {
            warn!("all catalog subscription candidates failed");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

async fn run_catalog_session(
    state: &AppState,
    relay: &RelayEndpoint,
    stream: TlsStream<TcpStream>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    let mut heartbeat = interval(Duration::from_secs(10));
    let mut nonce = 0_u64;
    let mut last_received = Instant::now();
    loop {
        tokio::select! {
            message = reader.read::<ControlMessage>() => {
                last_received = Instant::now();
                match message? {
                    ControlMessage::Catalog { catalog } => {
                        *state.catalog.write().await = catalog;
                    }
                    ControlMessage::RelayDirectory { directory } => {
                        update_directory(state, directory).await;
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(&mut writer, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
                    _ => bail!("unexpected message on catalog subscription"),
                }
            }
            _ = heartbeat.tick() => {
                if last_received.elapsed() > Duration::from_secs(30) {
                    bail!("relay {} catalog heartbeat timed out", relay.id);
                }
                nonce = nonce.wrapping_add(1);
                write_json(&mut writer, &ControlMessage::Heartbeat { nonce }, CONTROL_FRAME_LIMIT).await?;
            }
        }
    }
}

async fn open_management(
    state: &AppState,
    relay: &RelayEndpoint,
    purpose: TravelConnectionPurpose,
) -> Result<(TlsStream<TcpStream>, RelayDirectory, Catalog)> {
    let config = &state.config;
    let socket = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        TcpStream::connect(&relay.management_addr),
    )
    .await
    .context("relay TCP connection timed out")??;
    socket.set_nodelay(true)?;
    let mut stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        state
            .tls
            .management_connector
            .connect(server_name(&relay.server_name)?, socket),
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
    write_json(
        &mut stream,
        &ControlMessage::TravelHello {
            id: config.id.clone(),
            session_id: state.session_id,
            purpose,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    let mut reader = JsonFrameReader::new(&mut stream, CONTROL_FRAME_LIMIT);
    let setup_timeout = Duration::from_secs(config.handshake_timeout_secs);
    match reader
        .read_with_timeout::<ControlMessage>(setup_timeout)
        .await?
    {
        ControlMessage::TravelHelloAccepted { relay_id } if relay_id == relay.id => {}
        ControlMessage::TravelHelloDenied { reason } => {
            bail!("Travel session rejected by Relay {}: {reason}", relay.id);
        }
        _ => bail!("relay sent an invalid Travel HELLO response"),
    }
    let mut directory = None;
    let mut catalog = None;
    while directory.is_none() || catalog.is_none() {
        match reader
            .read_with_timeout::<ControlMessage>(setup_timeout)
            .await?
        {
            ControlMessage::RelayDirectory { directory: value } => directory = Some(value),
            ControlMessage::Catalog { catalog: value } => catalog = Some(value),
            _ => bail!("relay did not send initial directory and catalog"),
        }
    }
    drop(reader);
    Ok((
        stream,
        directory.ok_or_else(|| anyhow::anyhow!("missing relay directory"))?,
        catalog.ok_or_else(|| anyhow::anyhow!("missing catalog"))?,
    ))
}

async fn request_route(
    state: &AppState,
    relay: &RelayEndpoint,
    home_id: &str,
) -> Result<RouteGrant> {
    let config = &state.config;
    let (stream, directory, catalog) =
        open_management(state, relay, TravelConnectionPurpose::Route).await?;
    update_directory(state, directory).await;
    *state.catalog.write().await = catalog;
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    let request_id = Uuid::new_v4();
    write_json(
        &mut writer,
        &ControlMessage::TravelRouteRequest {
            request_id,
            travel_id: config.id.clone(),
            travel_session_id: state.session_id,
            home_id: home_id.to_owned(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    timeout(Duration::from_secs(10), async {
        loop {
            match reader.read::<ControlMessage>().await? {
                ControlMessage::RouteGrant {
                    request_id: response_id,
                    route_id,
                    route_secret,
                    data_addr,
                } if response_id == request_id => {
                    return Ok(RouteGrant {
                        route_id,
                        route_secret,
                        data_addr,
                    });
                }
                ControlMessage::RouteDenied {
                    request_id: response_id,
                    reason,
                } if response_id == request_id => bail!("route denied: {reason}"),
                ControlMessage::Catalog { catalog } => {
                    *state.catalog.write().await = catalog;
                }
                ControlMessage::RelayDirectory { directory } => {
                    update_directory(state, directory).await;
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
                _ => bail!("invalid route response"),
            }
        }
    })
    .await
    .context("route request timed out")?
}

struct BusinessCarrier {
    carrier_id: Uuid,
    relay_id: String,
    stream: TlsStream<TcpStream>,
    home_receive_offset: u64,
    home_send_offset: u64,
}

async fn open_business_on(
    state: &AppState,
    relay: &RelayEndpoint,
    flow_id: Uuid,
    carrier_id: Uuid,
    service_id: &str,
    protocol: ServiceProtocol,
    home_id: &str,
) -> Result<BusinessCarrier> {
    let config = &state.config;
    let home = config
        .homes
        .iter()
        .find(|home| home.id == home_id)
        .ok_or_else(|| anyhow!("Home {home_id} is not configured"))?;
    let grant = request_route(state, relay, home_id).await?;
    let mut socket = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        TcpStream::connect(&grant.data_addr),
    )
    .await
    .context("relay data connection timed out")??;
    socket.set_nodelay(true)?;
    write_preface(
        &mut socket,
        RouteSide::Travel,
        grant.route_id,
        &grant.route_secret,
    )
    .await?;
    let mut stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        state
            .tls
            .business_connector
            .connect(server_name(&home.server_name)?, socket),
    )
    .await
    .context("business TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Home, Some(&home.id), &home.spki_pins)?;
    write_json(
        &mut stream,
        &DataFrame::Open {
            flow_id,
            carrier_id,
            service_id: service_id.to_owned(),
            protocol,
        },
        DATA_FRAME_LIMIT,
    )
    .await?;
    match JsonFrameReader::new(&mut stream, DATA_FRAME_LIMIT)
        .read_with_timeout::<DataFrame>(Duration::from_secs(config.handshake_timeout_secs))
        .await?
    {
        DataFrame::OpenOk {
            flow_id: response_flow,
            carrier_id: response_carrier,
            receive_offset,
            send_offset,
        } if response_flow == flow_id && response_carrier == carrier_id => Ok(BusinessCarrier {
            carrier_id,
            relay_id: relay.id.clone(),
            stream,
            home_receive_offset: receive_offset,
            home_send_offset: send_offset,
        }),
        DataFrame::OpenError { reason, .. } => bail!("home rejected carrier: {reason}"),
        _ => bail!("invalid carrier OPEN response"),
    }
}

async fn relay_candidates(state: &AppState) -> Vec<RelayEndpoint> {
    let directory = state.directory.read().await.clone();
    let source = if directory.relays.is_empty() {
        state.config.seed_relays.clone()
    } else {
        directory.relays
    };
    let mut seen = HashSet::new();
    source
        .into_iter()
        .filter(|relay| seen.insert(relay.id.clone()))
        .collect()
}

async fn update_directory(state: &AppState, directory: RelayDirectory) {
    let mut current = state.directory.write().await;
    if directory.generation >= current.generation && !directory.relays.is_empty() {
        if *current != directory {
            let relay_ids: Vec<_> = directory
                .relays
                .iter()
                .map(|relay| relay.id.as_str())
                .collect();
            info!(
                event = "relay_directory_updated",
                generation = directory.generation,
                relay_count = directory.relays.len(),
                ?relay_ids,
                "travel updated relay directory"
            );
        }
        *current = directory;
    }
}

async fn run_tcp_listener(state: AppState, mapping: Mapping) -> Result<()> {
    let listener = TcpListener::bind(&mapping.bind).await?;
    info!(home_id = %mapping.home_id, service_id = %mapping.service_id, address = %mapping.bind, "local TCP mapping ready");
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
            if let Err(error) = run_tcp_flow(&state, &mapping, local).await {
                warn!(%peer, home_id = %mapping.home_id, service_id = %mapping.service_id, %error, "TCP flow closed");
            }
        });
    }
}

async fn run_tcp_flow(state: &AppState, mapping: &Mapping, local: TcpStream) -> Result<()> {
    tcp_flow::run(state.clone(), mapping.clone(), local).await
}

async fn run_udp_listener(state: AppState, mapping: Mapping) -> Result<()> {
    let socket = Arc::new(UdpSocket::bind(&mapping.bind).await?);
    info!(home_id = %mapping.home_id, service_id = %mapping.service_id, address = %mapping.bind, "local UDP mapping ready");
    let associations: Arc<Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut buffer = vec![0_u8; 65_507];
    loop {
        let (count, peer) = socket.recv_from(&mut buffer).await?;
        let mut bytes = buffer[..count].to_vec();
        let existing = associations.lock().await.get(&peer).cloned();
        if let Some(tx) = existing {
            match tx.try_send(bytes) {
                Ok(()) => continue,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    warn!(%peer, "dropping UDP datagram for a saturated association");
                    continue;
                }
                Err(mpsc::error::TrySendError::Closed(returned)) => {
                    bytes = returned;
                    let mut current = associations.lock().await;
                    if current
                        .get(&peer)
                        .is_some_and(|candidate| candidate.same_channel(&tx))
                    {
                        current.remove(&peer);
                    }
                }
            }
        }
        let (tx, rx) = mpsc::channel(64);
        let Ok(permit) = Arc::clone(&state.permits).try_acquire_owned() else {
            warn!(%peer, "travel active-flow limit reached");
            continue;
        };
        tx.try_send(bytes)
            .map_err(|_| anyhow::anyhow!("new UDP association channel unexpectedly unavailable"))?;
        associations.lock().await.insert(peer, tx.clone());
        let socket = Arc::clone(&socket);
        let associations = Arc::clone(&associations);
        let state = state.clone();
        let mapping = mapping.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let _guard = FlowGuard::new(Arc::clone(&state.active_flows));
            if let Err(error) = run_udp_association(&state, &mapping, socket, peer, rx).await {
                warn!(%peer, home_id = %mapping.home_id, service_id = %mapping.service_id, %error, "UDP association closed");
            }
            let mut current = associations.lock().await;
            if current
                .get(&peer)
                .is_some_and(|candidate| candidate.same_channel(&tx))
            {
                current.remove(&peer);
            }
        });
    }
}

async fn run_udp_association(
    state: &AppState,
    mapping: &Mapping,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    mut outgoing: mpsc::Receiver<Vec<u8>>,
) -> Result<()> {
    let config = &state.config;
    let flow_id = Uuid::new_v4();
    let mut opened = None;
    for relay in relay_candidates(state).await {
        let carrier_id = Uuid::new_v4();
        match open_business_on(
            state,
            &relay,
            flow_id,
            carrier_id,
            &mapping.service_id,
            ServiceProtocol::Udp,
            &mapping.home_id,
        )
        .await
        {
            Ok(carrier) => {
                opened = Some(carrier.stream);
                break;
            }
            Err(error) => warn!(relay_id = %relay.id, %error, "UDP carrier attempt failed"),
        }
    }
    let business = opened.ok_or_else(|| anyhow::anyhow!("all UDP carrier attempts failed"))?;
    let (mut reader, mut writer) = tokio::io::split(business);
    let mut reader = JsonFrameReader::new(&mut reader, DATA_FRAME_LIMIT);
    let mut send_sequence = 0_u64;
    let mut receive_sequence = 0_u64;
    loop {
        tokio::select! {
            datagram = timeout(Duration::from_secs(config.udp_idle_secs), outgoing.recv()) => {
                let Some(bytes) = datagram.context("UDP association idle timeout")? else { return Ok(()); };
                write_json(&mut writer, &DataFrame::Datagram { flow_id, sequence: send_sequence, bytes }, DATA_FRAME_LIMIT).await?;
                send_sequence = send_sequence.wrapping_add(1);
            }
            frame = timeout(Duration::from_secs(config.udp_idle_secs), reader.read::<DataFrame>()) => {
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
        .route("/relays", get(api_relays))
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
    let directory_generation = state.directory.read().await.generation;
    let mut active_relays: Vec<_> = state.flow_relays.lock().await.values().cloned().collect();
    active_relays.sort();
    active_relays.dedup();
    Json(StatusResponse {
        ok: true,
        travel_id: state.config.id.clone(),
        uptime_secs: state.started.elapsed().as_secs(),
        active_flows: state.active_flows.load(Ordering::Relaxed),
        catalog_generation: generation,
        relay_directory_generation: directory_generation,
        active_relays,
        mappings: state.config.mappings.clone(),
    })
}

async fn api_catalog(State(state): State<AppState>) -> Json<Catalog> {
    Json(state.catalog.read().await.clone())
}

async fn api_relays(State(state): State<AppState>) -> Json<RelayDirectory> {
    Json(state.directory.read().await.clone())
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
        .is_some_and(|token| {
            verify_slices_are_equal(token.as_bytes(), expected.as_bytes()).is_ok()
        });
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
