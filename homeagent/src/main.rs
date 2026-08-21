#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, OpenOptions},
    io::Write,
    io::{self, IsTerminal},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use aws_lc_rs::{
    digest,
    rand::{SecureRandom, SystemRandom},
    signature::EcdsaKeyPair,
};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::{Parser, Subcommand};
use embedded_spa::{EmbeddedSpa, EmbeddedSpaConfig};
use flowsplice_core::{
    CONTROL_FRAME_LIMIT, DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    authorization::{
        AuthorizationCache, SignedTravelCredential, TravelAuthorizationSnapshot,
        TravelCredentialScope, TrustedTravelAuthority, VerifiedAuthorization, load_json,
        store_json_atomic, unix_time_secs,
    },
    config::load_toml,
    deployment::{DeploymentTrust, SignedDeploymentTrust, SignedHomeEndpointCredential},
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{
        CONTROL_PROTOCOL_VERSION, ControlMessage, DataFrame, HomeCatalog, Role, Service,
        ServiceProtocol, bootstrap_verification_code,
    },
    route::{RouteSide, write_preface},
    statistics::{statistics_dashboard_html, statistics_signing_key},
    tls::{
        client_connector, identity_server_auth_connector_from_ca_pem, identity_server_name,
        load_private_key as load_management_private_key, peer_identity, require_peer,
        server_acceptor, server_name, validate_spki_pins,
    },
};
use flowsplice_enrollment::home::{
    HOME_BUSINESS_CERT_FILE, HOME_BUSINESS_KEY_FILE, HOME_ENDPOINT_CREDENTIAL_FILE,
    HOME_MANAGEMENT_CERT_FILE, HOME_MANAGEMENT_KEY_FILE, HOME_REQUEST_FILE, HomeEnrollmentProfile,
    HomeEnrollmentRequest, HomeEnrollmentResponse, HomeIssuerMaterial,
    create_home_enrollment_request, install_home_enrollment_response, issue_home_enrollment,
    parse_home_enrollment_request, prepare_home_enrollment_approval,
};
use flowsplice_enrollment::{
    DEFAULT_VALID_DAYS, MAX_VALID_DAYS, TravelEnrollmentRequest, TravelEnrollmentResponse,
    issuer::{IssuerMaterial, ProtectedKey, issue_enrollment},
    key::{
        MIN_PRIVATE_KEY_PASSWORD_CHARACTERS, PrivateKeyRotationTarget,
        load_private_key as load_issuer_private_key, recover_private_key_password_rotation,
        rotate_private_key_passwords,
    },
    parse_enrollment_request, prepare_enrollment_approval,
};
use flowsplice_storage::{
    LocalStatistics, MetricPoint, MetricRollup, StateStore, Table, WriteBatch,
    summarize_metric_points,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::{Mutex, Semaphore, mpsc, oneshot, watch},
    time::{interval, sleep, timeout},
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector, client::TlsStream as ClientTlsStream,
    server::TlsStream as ServerTlsStream,
};
use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

mod tcp_flow;

mod issuance_ledger;

use issuance_ledger::{IssuanceLedger, IssuanceRecord, ledger_path};
use tcp_flow::{IncomingCarrier, TcpFlowRegistry};

#[derive(RustEmbed)]
#[folder = "web/dist/"]
struct WebAssets;

static SPA: LazyLock<EmbeddedSpa<WebAssets>> = LazyLock::new(|| {
    EmbeddedSpa::new(EmbeddedSpaConfig::default())
        .unwrap_or_else(|error| panic!("invalid embedded Home issuer UI: {error}"))
});

#[derive(Parser)]
#[command(version)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "homeagent.toml")]
    config: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize and enroll this Home with only the Server IP.
    Init {
        #[arg(long)]
        server: IpAddr,
    },
}

const HOME_BOOTSTRAP_STATE_VERSION: u32 = 1;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HomeBootstrapState {
    version: u32,
    request_id: Uuid,
    home_id: String,
    retrieval_token_hex: String,
}

#[derive(Serialize)]
struct InstalledHomeConfig {
    id: String,
    alias: String,
    server_control_addr: String,
    server_name: String,
    server_id: String,
    management_cert: PathBuf,
    management_key: PathBuf,
    management_ca: PathBuf,
    business_cert: PathBuf,
    business_key: PathBuf,
    business_ca: PathBuf,
    deployment_root_public_key: PathBuf,
    deployment_trust: PathBuf,
    endpoint_credential: PathBuf,
    server_spki_pins: Vec<String>,
    travel_authorization_cache: PathBuf,
    state_store: PathBuf,
    ui_listen: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    issuer: Option<InstalledIssuerConfig>,
    services: Vec<Service>,
}

#[derive(Serialize)]
struct InstalledIssuerConfig {
    management_ca_key: PathBuf,
    business_ca_key: PathBuf,
    default_valid_days: u32,
    home_authority: InstalledSigningAuthority,
    #[serde(skip_serializing_if = "Option::is_none")]
    global_authority: Option<InstalledSigningAuthority>,
    #[serde(skip_serializing_if = "Option::is_none")]
    home_enrollment_authority: Option<InstalledSigningAuthority>,
}

#[derive(Serialize)]
struct InstalledSigningAuthority {
    id: String,
    private_key: PathBuf,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    id: String,
    alias: String,
    server_control_addr: String,
    server_name: String,
    server_id: String,
    management_cert: PathBuf,
    management_key: PathBuf,
    management_ca: PathBuf,
    business_cert: PathBuf,
    business_key: PathBuf,
    business_ca: PathBuf,
    deployment_root_public_key: PathBuf,
    deployment_trust: PathBuf,
    #[serde(default)]
    endpoint_credential: Option<PathBuf>,
    #[serde(default)]
    server_spki_pins: Vec<String>,
    travel_authorization_cache: PathBuf,
    state_store: PathBuf,
    ui_listen: String,
    #[serde(default)]
    issuer: Option<IssuerConfig>,
    services: Vec<Service>,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout_secs: u64,
    #[serde(default = "default_udp_idle")]
    udp_idle_secs: u64,
    #[serde(default = "default_max_active_flows")]
    max_active_flows: usize,
    #[serde(default = "default_max_active_carriers")]
    max_active_carriers: usize,
    #[serde(default = "default_max_carriers_per_flow")]
    max_carriers_per_flow: usize,
    #[serde(default = "default_carrier_heartbeat")]
    carrier_heartbeat_secs: u64,
    #[serde(default = "default_carrier_timeout")]
    carrier_timeout_secs: u64,
    #[serde(default = "default_flow_detach_timeout")]
    flow_detach_timeout_secs: u64,
    #[serde(default = "default_max_unacked_bytes")]
    max_unacked_bytes: usize,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuerConfig {
    management_ca_key: PathBuf,
    business_ca_key: PathBuf,
    home_authority: SigningAuthorityConfig,
    #[serde(default)]
    global_authority: Option<SigningAuthorityConfig>,
    #[serde(default)]
    home_enrollment_authority: Option<SigningAuthorityConfig>,
    #[serde(default = "default_travel_valid_days")]
    default_valid_days: u32,
    #[cfg(feature = "e2e-remote-ui")]
    #[serde(default)]
    test_allow_remote_listen: bool,
    #[cfg(feature = "e2e-remote-ui")]
    #[serde(default)]
    test_admin_token: Option<String>,
    #[serde(default)]
    allow_unencrypted_test_keys: bool,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SigningAuthorityConfig {
    id: String,
    private_key: PathBuf,
}

const fn default_travel_valid_days() -> u32 {
    DEFAULT_VALID_DAYS
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

const fn default_max_active_carriers() -> usize {
    512
}

const fn default_max_carriers_per_flow() -> usize {
    16
}

const fn default_carrier_heartbeat() -> u64 {
    5
}

const fn default_carrier_timeout() -> u64 {
    30
}

const fn default_flow_detach_timeout() -> u64 {
    120
}

const fn default_max_unacked_bytes() -> usize {
    1_048_576
}

#[derive(Clone)]
struct TlsMaterial {
    management_connector: TlsConnector,
    business_acceptor: TlsAcceptor,
}

struct TravelAuthorizationState {
    tx: watch::Sender<Option<Arc<VerifiedAuthorization>>>,
    cache: Mutex<AuthorizationCache>,
    trust: Arc<DeploymentTrust>,
    deployment_id: Arc<String>,
}

impl TravelAuthorizationState {
    fn new(cache: AuthorizationCache, trust: DeploymentTrust) -> Arc<Self> {
        let (tx, _) = watch::channel(None);
        Arc::new(Self {
            tx,
            cache: Mutex::new(cache),
            deployment_id: Arc::new(trust.deployment_id.clone()),
            trust: Arc::new(trust),
        })
    }
}

enum IssuerControlRequest {
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

#[derive(Clone)]
struct IssuerAppState {
    config: Arc<Config>,
    issuer: Arc<IssuerConfig>,
    endpoint_credential: Option<Arc<SignedHomeEndpointCredential>>,
    authorization: Arc<TravelAuthorizationState>,
    control_tx: mpsc::Sender<IssuerControlRequest>,
    key_operation: Arc<Mutex<()>>,
    sensitive_operation: Arc<Semaphore>,
    issuance_ledger: Arc<Mutex<IssuanceLedger>>,
    statistics: HomeStatistics,
}

#[derive(Clone)]
struct HomeStatistics {
    store: Arc<StateStore>,
    local: Arc<LocalStatistics>,
    signer: Arc<EcdsaKeyPair>,
    certificate_pem: Arc<String>,
    deployment_id: Arc<String>,
    reporter_id: Arc<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssueRequest {
    request: TravelEnrollmentRequest,
    valid_days: Option<u32>,
    valid_minutes: Option<u32>,
    scope: TravelCredentialScope,
    password: String,
}

#[derive(Serialize)]
struct IssueResponse {
    generation: u64,
    enrollment: TravelEnrollmentResponse,
    reused: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeRequest {
    credential_id: Uuid,
    reason: String,
    password: String,
}

const REMOTE_ENROLLMENT_VERSION: u32 = 1;
const MAX_REMOTE_ENROLLMENT_INBOX_RECORDS: usize = 1024;
const REMOTE_ENROLLMENT_PENDING_RETENTION_SECS: u64 = 7 * 24 * 60 * 60;
const REMOTE_ENROLLMENT_INSTALLED_RETENTION_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteEnrollmentInboxRecord {
    version: u32,
    travel_id: String,
    home_id: String,
    received_at_unix_secs: u64,
    request: TravelEnrollmentRequest,
    response: Option<TravelEnrollmentResponse>,
    #[serde(default)]
    bootstrap_token_sha256: Option<String>,
    #[serde(default)]
    verification_code: Option<String>,
    #[serde(default)]
    installed_credential_id: Option<Uuid>,
    #[serde(default)]
    installed_at_unix_secs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveRemoteEnrollmentRequest {
    request_id: Uuid,
    valid_days: Option<u32>,
    valid_minutes: Option<u32>,
    scope: TravelCredentialScope,
    password: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HomeEnrollmentInboxRecord {
    version: u32,
    home_id: String,
    received_at_unix_secs: u64,
    request: HomeEnrollmentRequest,
    response: Option<HomeEnrollmentResponse>,
    retrieval_token_sha256: String,
    verification_code: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApproveHomeEnrollmentRequest {
    request_id: Uuid,
    profile: HomeEnrollmentProfile,
    valid_days: Option<u32>,
    password: String,
}

#[derive(Serialize)]
struct HomeEnrollmentStatus {
    request_id: Uuid,
    home_id: String,
    received_at_unix_secs: u64,
    approved: bool,
    verification_code: String,
    profile: Option<HomeEnrollmentProfile>,
}

#[derive(Serialize)]
struct ApproveHomeEnrollmentResponse {
    request_id: Uuid,
    home_id: String,
    profile: HomeEnrollmentProfile,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RotatePrivateKeyPasswordRequest {
    current_password: String,
    new_password: String,
}

#[derive(Serialize)]
struct RotatePrivateKeyPasswordResponse {
    rotated_keys: usize,
}

#[derive(Serialize)]
struct IssuerStatus {
    home_id: String,
    home_alias: String,
    default_valid_days: u32,
    global_authority_available: bool,
    home_enrollment_available: bool,
    private_key_password_rotation_available: bool,
    services: Vec<Service>,
}

#[derive(Serialize)]
struct IssuedCredentialStatus {
    credential_id: Uuid,
    travel_id: String,
    authority_id: String,
    scope: TravelCredentialScope,
    not_after_unix_secs: u64,
    revoked: bool,
    active: bool,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

type ApiResult<T> = std::result::Result<Json<T>, (StatusCode, Json<ApiError>)>;

#[allow(clippy::too_many_lines)]
#[tokio::main]
async fn main() -> Result<()> {
    init_crypto();
    tracing_subscriber::fmt()
        .with_ansi(io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flowsplice_homeagent=info".into()),
        )
        .init();
    let args = Args::parse();
    match args.command {
        Some(Command::Init { server }) => run_home_init(server).await,
        None => run_home_agent(args.config).await,
    }
}

#[allow(clippy::too_many_lines)]
async fn run_home_agent(config_path: PathBuf) -> Result<()> {
    let config: Config = load_toml(&config_path)?;
    validate_home_ui_config(&config)?;
    validate_services(&config.services)?;
    validate_spki_pins(&config.server_spki_pins, "server")?;
    let (_, deployment_trust) = load_home_trust(&config)?;
    let endpoint_credential = if let Some(path) = config.endpoint_credential.as_deref() {
        let signed: SignedHomeEndpointCredential = load_json(path)?;
        let endpoint = signed.verify(&deployment_trust, unix_time_secs()?)?;
        if endpoint.home_id != config.id {
            bail!("Home endpoint credential belongs to a different Home");
        }
        Some((signed, endpoint.not_after_unix_secs))
    } else {
        None
    };
    if let Some(issuer) = &config.issuer {
        validate_issuer_config(
            &config,
            issuer,
            endpoint_credential.as_ref().map(|(signed, _)| signed),
        )?;
        if !issuer.allow_unencrypted_test_keys
            && recover_private_key_password_rotation(&issuer_key_targets(issuer))?
        {
            info!("completed interrupted Home issuer private-key password rotation");
        }
    }
    if config.carrier_heartbeat_secs == 0
        || config.carrier_timeout_secs <= config.carrier_heartbeat_secs
        || config.flow_detach_timeout_secs <= config.carrier_timeout_secs
        || config.max_unacked_bytes < MAX_DATA_PAYLOAD
        || config.max_unacked_bytes > u32::MAX as usize
        || config.max_active_flows == 0
        || config.max_active_carriers == 0
        || config.max_carriers_per_flow == 0
        || config.max_carriers_per_flow > config.max_active_carriers
    {
        bail!("carrier/flow timeout or unacknowledged-data limits are invalid");
    }
    let state_store = Arc::new(StateStore::open(&config.state_store)?);
    let statistics = HomeStatistics {
        store: Arc::clone(&state_store),
        local: Arc::new(LocalStatistics::new(state_store.as_ref().clone())),
        signer: Arc::new(statistics_signing_key(&load_management_private_key(
            &config.management_key,
        )?)?),
        certificate_pem: Arc::new(
            std::fs::read_to_string(&config.management_cert)
                .context("failed to read Home statistics signing certificate")?,
        ),
        deployment_id: Arc::new(deployment_trust.deployment_id.clone()),
        reporter_id: Arc::new(config.id.clone()),
    };
    let tls = Arc::new(TlsMaterial {
        management_connector: client_connector(
            &config.management_cert,
            &config.management_key,
            &config.management_ca,
        )?,
        business_acceptor: server_acceptor(
            &config.business_cert,
            &config.business_key,
            &config.business_ca,
        )?,
    });
    let config = Arc::new(config);
    let authorization_cache = if config.travel_authorization_cache.exists() {
        load_json(&config.travel_authorization_cache)?
    } else {
        AuthorizationCache::default()
    };
    let authorization =
        TravelAuthorizationState::new(authorization_cache, deployment_trust.clone());
    let permits = Arc::new(Semaphore::new(config.max_active_flows));
    let tcp_flows = TcpFlowRegistry::new(
        Arc::clone(&permits),
        Duration::from_secs(config.carrier_heartbeat_secs),
        Duration::from_secs(config.carrier_timeout_secs),
        Duration::from_secs(config.flow_detach_timeout_secs),
        config.max_unacked_bytes,
        config.max_active_carriers,
        config.max_carriers_per_flow,
        Arc::clone(&statistics.local),
    );
    let (issuer_state, issuer_control_rx) = if let Some(issuer) = &config.issuer {
        let (issuer_control_tx, issuer_control_rx) = mpsc::channel(32);
        let issuance_ledger = IssuanceLedger::load(ledger_path(&issuer.management_ca_key)?)?;
        (
            Some(IssuerAppState {
                config: Arc::clone(&config),
                issuer: Arc::new(issuer.clone()),
                endpoint_credential: endpoint_credential
                    .as_ref()
                    .map(|(signed, _)| Arc::new(signed.clone())),
                authorization: Arc::clone(&authorization),
                control_tx: issuer_control_tx,
                key_operation: Arc::new(Mutex::new(())),
                sensitive_operation: Arc::new(Semaphore::new(1)),
                issuance_ledger: Arc::new(Mutex::new(issuance_ledger)),
                statistics: statistics.clone(),
            }),
            Some(issuer_control_rx),
        )
    } else {
        (None, None)
    };
    let control = run_control_loop(
        Arc::clone(&config),
        Arc::clone(&permits),
        Arc::clone(&tls),
        Arc::clone(&tcp_flows),
        Arc::clone(&authorization),
        issuer_control_rx,
        statistics.clone(),
    );
    let trust_expiry = monitor_trust_expiry(
        endpoint_credential
            .as_ref()
            .map_or(deployment_trust.not_after_unix_secs, |(_, not_after)| {
                deployment_trust.not_after_unix_secs.min(*not_after)
            }),
    );
    if let Some(issuer_state) = issuer_state {
        tokio::try_join!(control, run_issuer_ui(issuer_state), trust_expiry)?;
    } else {
        tokio::try_join!(
            control,
            run_statistics_only_ui(Arc::clone(&config), statistics),
            trust_expiry
        )?;
    }
    Ok(())
}

fn embedded_deployment_root_public_key() -> Result<&'static str> {
    option_env!("FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!("this Home binary has no embedded deployment root public key")
        })
}

fn embedded_management_ca_certificate() -> Result<&'static str> {
    option_env!("FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("this Home binary has no embedded management CA"))
}

fn embedded_server_id() -> &'static str {
    option_env!("FLOWSPLICE_SERVER_ID")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("server-1")
}

fn embedded_server_name() -> Result<&'static str> {
    let name = option_env!("FLOWSPLICE_SERVER_NAME")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("server.flowsplice");
    server_name(name).context("embedded Server certificate name is invalid")?;
    Ok(name)
}

fn embedded_server_port() -> Result<u16> {
    option_env!("FLOWSPLICE_SERVER_CONTROL_PORT")
        .unwrap_or("7443")
        .parse::<u16>()
        .context("embedded Server control port is invalid")
}

fn default_home_install_root() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("FLOWSPLICE_HOME_INSTALL_DIR") {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot determine the current user's home directory"))?;
    Ok(home.join("Library/Application Support/FlowSplice/Home"))
}

#[allow(clippy::too_many_lines)]
async fn run_home_init(server: IpAddr) -> Result<()> {
    let deployment_root_public_key = embedded_deployment_root_public_key()?;
    let management_ca = embedded_management_ca_certificate()?;
    let server_address = SocketAddr::new(server, embedded_server_port()?);
    let install_root = default_home_install_root()?;
    if install_root.exists() && !install_root.is_dir() {
        bail!(
            "Home install path is not a directory: {}",
            install_root.display()
        );
    }
    fs::create_dir_all(&install_root)?;
    #[cfg(unix)]
    fs::set_permissions(
        &install_root,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let install_root = install_root.canonicalize()?;
    let enrollment_directory = install_root.join("cert");
    let bootstrap_state_path = install_root.join("home-bootstrap.json");
    let config_path = install_root.join("homeagent.toml");
    if config_path.exists() {
        #[cfg(target_os = "macos")]
        {
            let installed: Config = load_toml(&config_path)?;
            if installed.server_control_addr != server_address.to_string() {
                bail!(
                    "Home is already initialized for a different Server: {}",
                    installed.server_control_addr
                );
            }
            install_and_start_macos_home(&install_root, &config_path, &installed.id)?;
            if bootstrap_state_path.exists() {
                let bootstrap: HomeBootstrapState = load_json(&bootstrap_state_path)?;
                if bootstrap.home_id != installed.id {
                    bail!("installed Home configuration conflicts with bootstrap state");
                }
                fs::remove_file(&bootstrap_state_path)?;
            }
            println!("Home is already initialized: {}", config_path.display());
            println!("Home page: http://{}/", installed.ui_listen);
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        bail!("Home is already initialized: {}", config_path.display());
    }
    let (request, retrieval_token) = if bootstrap_state_path.exists() {
        let state: HomeBootstrapState = load_json(&bootstrap_state_path)?;
        if state.version != HOME_BOOTSTRAP_STATE_VERSION {
            bail!("existing Home bootstrap state has an unsupported version");
        }
        let request: HomeEnrollmentRequest =
            load_json(&enrollment_directory.join(HOME_REQUEST_FILE))?;
        if request.request_id != state.request_id || request.home_id != state.home_id {
            bail!("existing Home bootstrap state conflicts with its local request");
        }
        let token = hex::decode(&state.retrieval_token_hex)
            .context("Home bootstrap retrieval token is invalid")?;
        if token.len() != 32 {
            bail!("Home bootstrap retrieval token has an invalid length");
        }
        (request, token)
    } else {
        if enrollment_directory.exists() {
            bail!(
                "Home enrollment directory exists without resumable bootstrap state: {}",
                enrollment_directory.display()
            );
        }
        let suffix = Uuid::new_v4().simple().to_string();
        let home_id = format!("home-{}", &suffix[..12]);
        let request =
            create_home_enrollment_request(&home_id, &enrollment_directory, unix_time_secs()?)?;
        let mut token = vec![0_u8; 32];
        SystemRandom::new()
            .fill(&mut token)
            .map_err(|_| anyhow::anyhow!("failed to generate Home bootstrap retrieval token"))?;
        flowsplice_enrollment::write_json_private(
            &bootstrap_state_path,
            &HomeBootstrapState {
                version: HOME_BOOTSTRAP_STATE_VERSION,
                request_id: request.request_id,
                home_id: request.home_id.clone(),
                retrieval_token_hex: hex::encode(&token),
            },
        )?;
        (request, token)
    };
    let request_json = serde_json::to_vec(&request)?;
    let verification_code = bootstrap_verification_code(&request_json, &retrieval_token);
    println!("Home id: {}", request.home_id);
    println!("Home verification code: {verification_code}");
    println!("Waiting for approval on any online global Home page...");
    io::stdout()
        .flush()
        .context("failed to flush Home enrollment status")?;
    let connector = identity_server_auth_connector_from_ca_pem(management_ca)?;
    let mut last_error = None;
    let (response, server_spki_pin) = loop {
        match poll_home_bootstrap_server(
            server_address,
            &connector,
            &request,
            &retrieval_token,
            &request_json,
        )
        .await
        {
            Ok(Some(response)) => break response,
            Ok(None) => last_error = None,
            Err(error) => {
                let current = error.to_string();
                if last_error.as_deref() != Some(current.as_str()) {
                    println!("Still waiting: {current}");
                }
                last_error = Some(current);
            }
        }
        sleep(Duration::from_secs(2)).await;
    };
    let (endpoint, _) = flowsplice_enrollment::home::validate_home_enrollment_response(
        &response,
        deployment_root_public_key,
        unix_time_secs()?,
    )?;
    if endpoint.home_id != request.home_id {
        bail!("approved Home identity changed during enrollment");
    }
    install_home_enrollment_response(
        &enrollment_directory,
        &response,
        deployment_root_public_key,
        unix_time_secs()?,
    )?;
    write_or_verify_private(
        &enrollment_directory.join("deployment-root.pub"),
        format!("{}\n", deployment_root_public_key.trim()).as_bytes(),
    )?;
    let issuer = installed_issuer_config(&install_root, &response);
    let ui_port = option_env!("FLOWSPLICE_HOME_UI_PORT")
        .unwrap_or("9082")
        .parse::<u16>()
        .context("embedded Home UI port is invalid")?;
    let suffix = request
        .home_id
        .strip_prefix("home-")
        .unwrap_or(&request.home_id);
    let generated = InstalledHomeConfig {
        id: request.home_id.clone(),
        alias: format!("Home {suffix}"),
        server_control_addr: server_address.to_string(),
        server_name: embedded_server_name()?.to_owned(),
        server_id: embedded_server_id().to_owned(),
        management_cert: enrollment_directory.join(HOME_MANAGEMENT_CERT_FILE),
        management_key: enrollment_directory.join(HOME_MANAGEMENT_KEY_FILE),
        management_ca: enrollment_directory.join(flowsplice_enrollment::MANAGEMENT_CA_FILE),
        business_cert: enrollment_directory.join(HOME_BUSINESS_CERT_FILE),
        business_key: enrollment_directory.join(HOME_BUSINESS_KEY_FILE),
        business_ca: enrollment_directory.join(flowsplice_enrollment::BUSINESS_CA_FILE),
        deployment_root_public_key: enrollment_directory.join("deployment-root.pub"),
        deployment_trust: enrollment_directory.join(flowsplice_enrollment::DEPLOYMENT_TRUST_FILE),
        endpoint_credential: enrollment_directory.join(HOME_ENDPOINT_CREDENTIAL_FILE),
        server_spki_pins: vec![server_spki_pin],
        travel_authorization_cache: install_root.join("state/travel-authorization-cache.json"),
        state_store: install_root.join("state/home-state.redb"),
        ui_listen: format!("127.0.0.1:{ui_port}"),
        issuer,
        services: Vec::new(),
    };
    let encoded = toml::to_string_pretty(&generated).context("failed to encode Home TOML")?;
    write_new_private(&config_path, encoded.as_bytes())?;
    let _ = StateStore::open(&generated.state_store)?;
    #[cfg(target_os = "macos")]
    install_and_start_macos_home(&install_root, &config_path, &request.home_id)?;
    fs::remove_file(&bootstrap_state_path)?;
    println!(
        "Home enrollment installed with profile {:?}",
        response.approval.profile
    );
    println!("configuration: {}", config_path.display());
    println!("Home page: http://127.0.0.1:{ui_port}/");
    #[cfg(not(target_os = "macos"))]
    println!(
        "start: flowsplice-homeagent --config {}",
        config_path.display()
    );
    Ok(())
}

async fn poll_home_bootstrap_server(
    address: SocketAddr,
    connector: &TlsConnector,
    request: &HomeEnrollmentRequest,
    retrieval_token: &[u8],
    request_json: &[u8],
) -> Result<Option<(HomeEnrollmentResponse, String)>> {
    let socket = timeout(Duration::from_secs(10), TcpStream::connect(address))
        .await
        .context("Server bootstrap TCP connection timed out")??;
    let mut stream = timeout(
        Duration::from_secs(10),
        connector.connect(identity_server_name()?, socket),
    )
    .await
    .context("Server bootstrap TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Server, Some(embedded_server_id()), &[])?;
    write_json(
        &mut stream,
        &ControlMessage::HomeBootstrapEnrollmentSubmit {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id: request.request_id,
            home_id: request.home_id.clone(),
            retrieval_token: retrieval_token.to_vec(),
            request_json: request_json.to_vec(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    let response = JsonFrameReader::new(&mut stream, CONTROL_FRAME_LIMIT)
        .read_with_timeout::<ControlMessage>(Duration::from_secs(25))
        .await?;
    let ControlMessage::HomeBootstrapEnrollmentResult {
        request_id,
        accepted,
        response_json,
        error,
    } = response
    else {
        bail!("Server returned an unexpected Home enrollment response");
    };
    if request_id != request.request_id {
        bail!("Server returned the wrong Home enrollment request id");
    }
    if !accepted {
        bail!(error.unwrap_or_else(|| "Home enrollment was rejected".to_owned()));
    }
    response_json
        .map(|json| {
            let response = serde_json::from_slice(&json)
                .context("Home enrollment response JSON is invalid")?;
            Ok((response, identity.spki_sha256.clone()))
        })
        .transpose()
}

fn installed_issuer_config(
    install_root: &Path,
    response: &HomeEnrollmentResponse,
) -> Option<InstalledIssuerConfig> {
    let Some(bundle) = &response.issuer_bundle else {
        return None;
    };
    let issuer_root = install_root.join("issuer");
    Some(InstalledIssuerConfig {
        management_ca_key: issuer_root
            .join(flowsplice_enrollment::home::HOME_ISSUER_MANAGEMENT_CA_KEY_FILE),
        business_ca_key: issuer_root
            .join(flowsplice_enrollment::home::HOME_ISSUER_BUSINESS_CA_KEY_FILE),
        default_valid_days: DEFAULT_VALID_DAYS,
        home_authority: InstalledSigningAuthority {
            id: bundle.home_authority_id.clone(),
            private_key: issuer_root
                .join(flowsplice_enrollment::home::HOME_ISSUER_HOME_AUTHORITY_KEY_FILE),
        },
        global_authority: bundle
            .global_authority_id
            .as_ref()
            .map(|id| InstalledSigningAuthority {
                id: id.clone(),
                private_key: issuer_root
                    .join(flowsplice_enrollment::home::HOME_ISSUER_GLOBAL_AUTHORITY_KEY_FILE),
            }),
        home_enrollment_authority: bundle.home_enrollment_authority_key_pem.as_ref().map(|_| {
            InstalledSigningAuthority {
                id: response.approval.authority_id.clone(),
                private_key: issuer_root
                    .join(flowsplice_enrollment::home::HOME_ISSUER_ENROLLMENT_AUTHORITY_KEY_FILE),
            }
        }),
    })
}

fn write_new_private(path: &Path, data: &[u8]) -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

fn write_or_verify_private(path: &Path, data: &[u8]) -> Result<()> {
    if path.exists() {
        if fs::read(path)? != data {
            bail!("refusing to replace conflicting file {}", path.display());
        }
        return Ok(());
    }
    write_new_private(path, data)
}

#[cfg(target_os = "macos")]
fn install_and_start_macos_home(
    install_root: &Path,
    config_path: &Path,
    home_id: &str,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let current_executable = std::env::current_exe()?.canonicalize()?;
    let bin_directory = install_root.join("bin");
    fs::create_dir_all(&bin_directory)?;
    let executable = bin_directory.join("flowsplice-homeagent");
    if current_executable != executable {
        write_or_verify_private(&executable, &fs::read(&current_executable)?)?;
    }
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o755))?;
    let user_home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("cannot determine macOS user home"))?;
    let launch_agents = user_home.join("Library/LaunchAgents");
    fs::create_dir_all(&launch_agents)?;
    let label = format!("io.zxf.flowsplice.homeagent.{home_id}");
    let plist_path = launch_agents.join(format!("{label}.plist"));
    let xml_escape = |value: &Path| {
        value
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{}</string><string>--config</string><string>{}</string></array><key>WorkingDirectory</key><string>{}</string><key>RunAtLoad</key><true/><key>KeepAlive</key><true/><key>StandardOutPath</key><string>{}/homeagent.log</string><key>StandardErrorPath</key><string>{}/homeagent-error.log</string></dict></plist>\n",
        xml_escape(&executable),
        xml_escape(config_path),
        xml_escape(install_root),
        xml_escape(install_root),
        xml_escape(install_root),
    );
    write_or_verify_private(&plist_path, plist.as_bytes())?;
    let domain = format!("gui/{}", rustix::process::geteuid().as_raw());
    let service = format!("{domain}/{label}");
    let loaded = std::process::Command::new("launchctl")
        .arg("print")
        .arg(&service)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?
        .success();
    if !loaded {
        let status = std::process::Command::new("launchctl")
            .arg("bootstrap")
            .arg(&domain)
            .arg(&plist_path)
            .status()?;
        if !status.success() {
            bail!("launchctl could not install the initialized Home service");
        }
    }
    let status = std::process::Command::new("launchctl")
        .arg("kickstart")
        .arg("-k")
        .arg(&service)
        .status()?;
    if !status.success() {
        bail!("launchctl could not start the initialized Home service");
    }
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

async fn run_control_loop(
    config: Arc<Config>,
    permits: Arc<Semaphore>,
    tls: Arc<TlsMaterial>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization: Arc<TravelAuthorizationState>,
    mut issuer_control_rx: Option<mpsc::Receiver<IssuerControlRequest>>,
    statistics: HomeStatistics,
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
        )
        .await
        {
            warn!(%error, "server control disconnected; reconnecting");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn validate_issuer_config(
    config: &Config,
    issuer: &IssuerConfig,
    endpoint_credential: Option<&SignedHomeEndpointCredential>,
) -> Result<()> {
    let listen = config
        .ui_listen
        .parse::<SocketAddr>()
        .context("invalid Home UI listen address")?;
    if listen != SocketAddr::from(([127, 0, 0, 1], listen.port()))
        && !test_remote_issuer_enabled(issuer)
    {
        bail!("Home issuer UI must listen directly on 127.0.0.1");
    }
    if issuer.default_valid_days == 0 || issuer.default_valid_days > MAX_VALID_DAYS {
        bail!("Travel validity must be between 1 and {MAX_VALID_DAYS} days");
    }
    let (_, trust) = load_home_trust(config)?;
    if std::fs::read_to_string(&config.management_ca)? != trust.management_ca_certificate_pem
        || std::fs::read_to_string(&config.business_ca)? != trust.business_ca_certificate_pem
    {
        bail!("Home issuer CA certificates do not match deployment trust");
    }
    let endpoint_credentials = endpoint_credential.into_iter().cloned().collect::<Vec<_>>();
    let authorities =
        trust.travel_authorities_with_home_delegations(&endpoint_credentials, unix_time_secs()?)?;
    validate_signing_authority(config, &authorities, &issuer.home_authority, false)?;
    if let Some(authority) = &issuer.global_authority {
        validate_signing_authority(config, &authorities, authority, true)?;
        if issuer.home_enrollment_authority.is_none() {
            bail!("global Home issuer must configure Home enrollment authority material");
        }
    }
    if let Some(authority) = &issuer.home_enrollment_authority {
        let trusted = trust
            .home_enrollment_authorities
            .iter()
            .find(|trusted| trusted.id == authority.id)
            .ok_or_else(|| {
                anyhow::anyhow!("Home enrollment authority {} is not trusted", authority.id)
            })?;
        let dynamically_global = authorities.iter().any(|authority| {
            matches!(authority, TrustedTravelAuthority::Global { home_id, .. } if home_id == &config.id)
        });
        if trusted.issuer_home_id != config.id && !dynamically_global {
            bail!("Home enrollment authority is not assigned to this Home");
        }
    }
    if issuer.allow_unencrypted_test_keys
        && std::env::var("FLOWSPLICE_ALLOW_UNENCRYPTED_TEST_KEYS").as_deref() != Ok("1")
    {
        bail!("unencrypted issuer keys are disabled outside the explicit test environment");
    }
    Ok(())
}

fn validate_home_ui_config(config: &Config) -> Result<()> {
    if config.state_store.as_os_str().is_empty() {
        bail!("state_store must be non-empty");
    }
    let listen = config
        .ui_listen
        .parse::<SocketAddr>()
        .context("invalid Home UI listen address")?;
    if !listen.ip().is_loopback()
        && !config
            .issuer
            .as_ref()
            .is_some_and(test_remote_issuer_enabled)
    {
        bail!("Home UI must listen directly on a loopback address");
    }
    Ok(())
}

#[cfg(feature = "e2e-remote-ui")]
fn test_remote_issuer_enabled(issuer: &IssuerConfig) -> bool {
    issuer.test_allow_remote_listen
        && issuer
            .test_admin_token
            .as_deref()
            .is_some_and(|token| token.len() >= 32)
}

#[cfg(not(feature = "e2e-remote-ui"))]
const fn test_remote_issuer_enabled(_issuer: &IssuerConfig) -> bool {
    false
}

fn load_home_trust(config: &Config) -> Result<(String, DeploymentTrust)> {
    let root_public_key = std::fs::read_to_string(&config.deployment_root_public_key)
        .context("failed to read deployment root public key")?;
    let signed: SignedDeploymentTrust = load_json(&config.deployment_trust)?;
    let trust = signed.verify(root_public_key.trim(), unix_time_secs()?)?;
    Ok((root_public_key.trim().to_owned(), trust))
}

fn validate_signing_authority(
    config: &Config,
    authorities: &[TrustedTravelAuthority],
    signing: &SigningAuthorityConfig,
    global: bool,
) -> Result<()> {
    let trusted = authorities
        .iter()
        .find(|authority| authority.id() == signing.id)
        .ok_or_else(|| anyhow::anyhow!("issuer authority {} is not trusted", signing.id))?;
    let correct_kind = matches!(
        (global, trusted),
        (true, TrustedTravelAuthority::Global { .. })
            | (false, TrustedTravelAuthority::Home { .. })
    );
    if !correct_kind || trusted.home_id() != Some(config.id.as_str()) {
        bail!(
            "issuer authority {} does not match this Home's trusted authority",
            signing.id
        );
    }
    Ok(())
}

fn issuer_key_targets(issuer: &IssuerConfig) -> Vec<PrivateKeyRotationTarget<'_>> {
    let mut targets = vec![
        PrivateKeyRotationTarget {
            label: "management CA",
            path: &issuer.management_ca_key,
        },
        PrivateKeyRotationTarget {
            label: "business CA",
            path: &issuer.business_ca_key,
        },
        PrivateKeyRotationTarget {
            label: "Home authorization",
            path: &issuer.home_authority.private_key,
        },
    ];
    if let Some(authority) = &issuer.global_authority {
        targets.push(PrivateKeyRotationTarget {
            label: "global authorization",
            path: &authority.private_key,
        });
    }
    if let Some(authority) = &issuer.home_enrollment_authority {
        targets.push(PrivateKeyRotationTarget {
            label: "Home enrollment authorization",
            path: &authority.private_key,
        });
    }
    targets
}

fn validate_services(services: &[Service]) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for service in services {
        if service.id.is_empty() || !ids.insert(&service.id) {
            bail!("service ids must be non-empty and unique");
        }
        let valid_target = service.target.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0)
        });
        if !valid_target {
            bail!("service {} target must be host:port", service.id);
        }
    }
    Ok(())
}

async fn run_control(
    config: Arc<Config>,
    permits: Arc<Semaphore>,
    tls: Arc<TlsMaterial>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization: Arc<TravelAuthorizationState>,
    issuer_control_rx: &mut Option<mpsc::Receiver<IssuerControlRequest>>,
    statistics: HomeStatistics,
) -> Result<()> {
    let socket = TcpStream::connect(&config.server_control_addr).await?;
    let stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        tls.management_connector
            .connect(server_name(&config.server_name)?, socket),
    )
    .await
    .context("server TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(
        &identity,
        Role::Server,
        Some(&config.server_id),
        &config.server_spki_pins,
    )?;
    run_control_session(
        stream,
        config,
        permits,
        tls,
        tcp_flows,
        authorization,
        issuer_control_rx,
        statistics,
    )
    .await
}

#[allow(clippy::too_many_lines)]
#[allow(clippy::too_many_arguments)]
async fn run_control_session(
    stream: ClientTlsStream<TcpStream>,
    config: Arc<Config>,
    permits: Arc<Semaphore>,
    tls: Arc<TlsMaterial>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization: Arc<TravelAuthorizationState>,
    issuer_control_rx: &mut Option<mpsc::Receiver<IssuerControlRequest>>,
    statistics: HomeStatistics,
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
    info!(server = %config.server_control_addr, "home agent registered");

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
                        ensure_credential_active(&authorization.tx.subscribe(), credential_id)?;
                        let config = Arc::clone(&config);
                        let tls = Arc::clone(&tls);
                        let permits = Arc::clone(&permits);
                        let tcp_flows = Arc::clone(&tcp_flows);
                        let authorization_rx = authorization.tx.subscribe();
                        tokio::spawn(async move {
                            if let Err(error) = run_work(config, tls, permits, tcp_flows, authorization_rx, credential_id, work_id, work_secret, relay_id.clone(), relay_data_addr, expires_at_unix_secs).await {
                                warn!(%work_id, %relay_id, %error, "home direct Relay work failed");
                            }
                        });
                    }
                    ControlMessage::RemoteEnrollmentSubmit {
                        request_id,
                        travel_id,
                        home_id,
                        request_json,
                        ..
                    } => {
                        let result = persist_remote_enrollment_request(
                            &statistics,
                            &config,
                            request_id,
                            travel_id,
                            home_id,
                            request_json,
                            None,
                        )
                        .await;
                        let (accepted, response_json, error) = match result {
                            Ok(response) => (
                                true,
                                response
                                    .map(|response| serde_json::to_vec(&response))
                                    .transpose()?,
                                None,
                            ),
                            Err(error) => (false, None, Some(error.to_string())),
                        };
                        write_json(
                            &mut writer,
                            &ControlMessage::RemoteEnrollmentResult {
                                request_id,
                                accepted,
                                response_json,
                                error,
                            },
                            CONTROL_FRAME_LIMIT,
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
                        let result = if protocol_version == CONTROL_PROTOCOL_VERSION {
                            persist_remote_enrollment_request(
                                &statistics,
                                &config,
                                request_id,
                                travel_id,
                                home_id,
                                request_json,
                                Some(retrieval_token),
                            )
                            .await
                        } else {
                            Err(anyhow::anyhow!("unsupported bootstrap protocol version"))
                        };
                        let (accepted, response_json, error) = match result {
                            Ok(response) => (
                                true,
                                response
                                    .map(|response| serde_json::to_vec(&response))
                                    .transpose()?,
                                None,
                            ),
                            Err(error) => (false, None, Some(error.to_string())),
                        };
                        write_json(
                            &mut writer,
                            &ControlMessage::RemoteEnrollmentResult {
                                request_id,
                                accepted,
                                response_json,
                                error,
                            },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
                    }
                    ControlMessage::HomeEnrollmentSubmit {
                        request_id,
                        home_id,
                        retrieval_token,
                        request_json,
                    } => {
                        let result = persist_home_enrollment_request(
                            &statistics,
                            request_id,
                            home_id,
                            retrieval_token,
                            request_json,
                        )
                        .await;
                        let (accepted, response_json, error) = match result {
                            Ok(response) => (
                                true,
                                response
                                    .map(|response| serde_json::to_vec(&response))
                                    .transpose()?,
                                None,
                            ),
                            Err(error) => (false, None, Some(error.to_string())),
                        };
                        write_json(
                            &mut writer,
                            &ControlMessage::HomeEnrollmentResult {
                                request_id,
                                accepted,
                                response_json,
                                error,
                            },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
                    }
                    ControlMessage::RemoteEnrollmentInstalled {
                        request_id,
                        travel_id,
                        credential_id,
                        home_id,
                        ..
                    } => {
                        let result = acknowledge_remote_enrollment_installed(
                            &statistics,
                            &config,
                            request_id,
                            &travel_id,
                            credential_id,
                            &home_id,
                        )
                        .await;
                        write_json(
                            &mut writer,
                            &ControlMessage::RemoteEnrollmentInstalledAck {
                                request_id,
                                accepted: result.is_ok(),
                                error: result.err().map(|error| error.to_string()),
                            },
                            CONTROL_FRAME_LIMIT,
                        )
                        .await?;
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
                                tokio::task::spawn_blocking(move || local.acknowledge_report(&key, &digest))
                                    .await
                                    .context("Home statistics acknowledgement task failed")??;
                            }
                        } else {
                            warn!(?error, %digest_sha256, "Server rejected Home statistics report");
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
                flush_and_send_home_statistics(&statistics, &mut writer, &mut report_keys).await?;
                if last_enrollment_prune.elapsed() >= Duration::from_secs(60) {
                    prune_remote_enrollment_inbox(&statistics).await?;
                    last_enrollment_prune = Instant::now();
                }
            }
        }
    }
}

async fn persist_remote_enrollment_request(
    statistics: &HomeStatistics,
    config: &Config,
    request_id: Uuid,
    travel_id: String,
    home_id: String,
    request_json: Vec<u8>,
    bootstrap_token: Option<Vec<u8>>,
) -> Result<Option<TravelEnrollmentResponse>> {
    if request_id.is_nil() || request_json.is_empty() || request_json.len() > 512 * 1024 {
        bail!("remote enrollment request is missing or oversized");
    }
    if home_id != config.id {
        bail!("remote enrollment request targets a different Home");
    }
    let request: TravelEnrollmentRequest =
        serde_json::from_slice(&request_json).context("remote enrollment request is invalid")?;
    if request.request_id != request_id || request.travel_id != travel_id {
        bail!("remote enrollment request does not match authenticated transport identity");
    }
    if bootstrap_token
        .as_ref()
        .is_some_and(|token| token.len() != 32)
    {
        bail!("first enrollment retrieval token has an invalid length");
    }
    parse_enrollment_request(&request, unix_time_secs()?)?;
    let bootstrap_token_sha256 = bootstrap_token
        .as_ref()
        .map(|token| hex::encode(digest::digest(&digest::SHA256, token).as_ref()));
    let verification_code = bootstrap_token
        .as_ref()
        .map(|token| bootstrap_verification_code(&request_json, token));
    let store = Arc::clone(&statistics.store);
    tokio::task::spawn_blocking(move || {
        let now = unix_time_secs()?;
        prune_remote_enrollment_inbox_store(&store, now)?;
        let key = request_id.as_bytes();
        if let Some(existing) =
            store.get_json::<RemoteEnrollmentInboxRecord>(Table::EnrollmentInbox, key)?
        {
            if existing.version != REMOTE_ENROLLMENT_VERSION
                || existing.travel_id != travel_id
                || existing.home_id != home_id
                || existing.request != request
                || existing.bootstrap_token_sha256 != bootstrap_token_sha256
            {
                bail!("conflicting reuse of remote enrollment request id");
            }
            return Ok(existing.response);
        }
        if !remote_enrollment_capacity_available(
            store.scan_prefix(Table::EnrollmentInbox, b"")?.len(),
            MAX_REMOTE_ENROLLMENT_INBOX_RECORDS,
        ) {
            bail!(
                "remote enrollment inbox capacity of {MAX_REMOTE_ENROLLMENT_INBOX_RECORDS} is exhausted"
            );
        }
        let record = RemoteEnrollmentInboxRecord {
            version: REMOTE_ENROLLMENT_VERSION,
            travel_id,
            home_id,
            received_at_unix_secs: now,
            request,
            response: None,
            bootstrap_token_sha256,
            verification_code,
            installed_credential_id: None,
            installed_at_unix_secs: None,
        };
        store.apply_immediate(WriteBatch::new().put_json(
            Table::EnrollmentInbox,
            request_id.as_bytes().to_vec(),
            &record,
        )?)?;
        Ok(None)
    })
    .await
    .context("Home enrollment inbox task failed")?
}

async fn persist_home_enrollment_request(
    statistics: &HomeStatistics,
    request_id: Uuid,
    home_id: String,
    retrieval_token: Vec<u8>,
    request_json: Vec<u8>,
) -> Result<Option<HomeEnrollmentResponse>> {
    if request_id.is_nil()
        || home_id.is_empty()
        || retrieval_token.len() != 32
        || request_json.is_empty()
        || request_json.len() > 512 * 1024
    {
        bail!("Home enrollment request is invalid or oversized");
    }
    let request: HomeEnrollmentRequest =
        serde_json::from_slice(&request_json).context("Home enrollment request is invalid")?;
    if request.request_id != request_id || request.home_id != home_id {
        bail!("Home enrollment transport identity does not match its request");
    }
    parse_home_enrollment_request(&request, unix_time_secs()?)?;
    let retrieval_token_sha256 =
        hex::encode(digest::digest(&digest::SHA256, &retrieval_token).as_ref());
    let verification_code = bootstrap_verification_code(&request_json, &retrieval_token);
    let store = Arc::clone(&statistics.store);
    tokio::task::spawn_blocking(move || {
        let key = request_id.as_bytes();
        if let Some(existing) =
            store.get_json::<HomeEnrollmentInboxRecord>(Table::HomeEnrollmentInbox, key)?
        {
            if existing.version != flowsplice_enrollment::home::HOME_ENROLLMENT_VERSION
                || existing.home_id != home_id
                || existing.request != request
                || existing.retrieval_token_sha256 != retrieval_token_sha256
            {
                bail!("conflicting reuse of Home enrollment request id");
            }
            return Ok(existing.response);
        }
        if store.scan_prefix(Table::HomeEnrollmentInbox, b"")?.len()
            >= MAX_REMOTE_ENROLLMENT_INBOX_RECORDS
        {
            bail!("Home enrollment inbox capacity is exhausted");
        }
        let record = HomeEnrollmentInboxRecord {
            version: flowsplice_enrollment::home::HOME_ENROLLMENT_VERSION,
            home_id,
            received_at_unix_secs: unix_time_secs()?,
            request,
            response: None,
            retrieval_token_sha256,
            verification_code,
        };
        store.apply_immediate(WriteBatch::new().put_json(
            Table::HomeEnrollmentInbox,
            request_id.as_bytes().to_vec(),
            &record,
        )?)?;
        Ok(None)
    })
    .await
    .context("Home enrollment inbox task failed")?
}

async fn acknowledge_remote_enrollment_installed(
    statistics: &HomeStatistics,
    config: &Config,
    request_id: Uuid,
    travel_id: &str,
    credential_id: Uuid,
    home_id: &str,
) -> Result<()> {
    if home_id != config.id {
        bail!("remote enrollment acknowledgement targets a different Home");
    }
    let store = Arc::clone(&statistics.store);
    let travel_id = travel_id.to_owned();
    let home_id = home_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let mut record = store
            .get_json::<RemoteEnrollmentInboxRecord>(Table::EnrollmentInbox, request_id.as_bytes())?
            .ok_or_else(|| anyhow::anyhow!("unknown remote enrollment request"))?;
        if record.travel_id != travel_id || record.home_id != home_id {
            bail!("remote enrollment acknowledgement identity does not match the inbox");
        }
        if let Some(installed) = record.installed_credential_id {
            if installed != credential_id {
                bail!("conflicting installed credential acknowledgement");
            }
            return Ok(());
        }
        let response = record
            .response
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("remote enrollment has no approved response"))?;
        if response.approval.request.request_id != request_id
            || response.approval.request.travel_id != travel_id
            || response.approval.credential_id != credential_id
        {
            bail!("installed credential does not match the approved enrollment response");
        }
        record.response = None;
        record.verification_code = None;
        record.installed_credential_id = Some(credential_id);
        record.installed_at_unix_secs = Some(unix_time_secs()?);
        store.apply_immediate(WriteBatch::new().put_json(
            Table::EnrollmentInbox,
            request_id.as_bytes().to_vec(),
            &record,
        )?)
    })
    .await
    .context("Home enrollment acknowledgement task failed")?
}

async fn prune_remote_enrollment_inbox(statistics: &HomeStatistics) -> Result<()> {
    let store = Arc::clone(&statistics.store);
    tokio::task::spawn_blocking(move || {
        let now = unix_time_secs()?;
        prune_remote_enrollment_inbox_store(&store, now)
    })
    .await
    .context("Home enrollment inbox retention task failed")?
}

fn prune_remote_enrollment_inbox_store(store: &StateStore, now: u64) -> Result<()> {
    let mut batch = WriteBatch::new();
    for (key, value) in store.scan_prefix(Table::EnrollmentInbox, b"")? {
        let Ok(record) = serde_json::from_slice::<RemoteEnrollmentInboxRecord>(&value) else {
            continue;
        };
        let expired = remote_enrollment_inbox_expired(
            record.received_at_unix_secs,
            record.installed_at_unix_secs,
            now,
        );
        if expired {
            batch = batch.delete(Table::EnrollmentInbox, key);
        }
    }
    for (key, value) in store.scan_prefix(Table::HomeEnrollmentInbox, b"")? {
        let Ok(record) = serde_json::from_slice::<HomeEnrollmentInboxRecord>(&value) else {
            continue;
        };
        let retention = if record.response.is_some() {
            REMOTE_ENROLLMENT_INSTALLED_RETENTION_SECS
        } else {
            REMOTE_ENROLLMENT_PENDING_RETENTION_SECS
        };
        if now.saturating_sub(record.received_at_unix_secs) >= retention {
            batch = batch.delete(Table::HomeEnrollmentInbox, key);
        }
    }
    if batch.is_empty() {
        Ok(())
    } else {
        store.apply_immediate(batch)
    }
}

fn remote_enrollment_inbox_expired(
    received_at_unix_secs: u64,
    installed_at_unix_secs: Option<u64>,
    now: u64,
) -> bool {
    installed_at_unix_secs.map_or_else(
        || now.saturating_sub(received_at_unix_secs) >= REMOTE_ENROLLMENT_PENDING_RETENTION_SECS,
        |installed_at| {
            now.saturating_sub(installed_at) >= REMOTE_ENROLLMENT_INSTALLED_RETENTION_SECS
        },
    )
}

fn remote_enrollment_capacity_available(current: usize, maximum: usize) -> bool {
    current < maximum
}

async fn flush_and_send_home_statistics<W: tokio::io::AsyncWrite + Unpin>(
    statistics: &HomeStatistics,
    writer: &mut W,
    report_keys: &mut HashMap<String, Vec<u8>>,
) -> Result<()> {
    let local = Arc::clone(&statistics.local);
    let deployment_id = Arc::clone(&statistics.deployment_id);
    let reporter_id = Arc::clone(&statistics.reporter_id);
    let certificate_pem = Arc::clone(&statistics.certificate_pem);
    let signer = Arc::clone(&statistics.signer);
    let flush_result = tokio::task::spawn_blocking(move || {
        local.flush_and_stage(
            &deployment_id,
            Role::Home,
            &reporter_id,
            &certificate_pem,
            &signer,
        )
    })
    .await
    .context("Home statistics flush task failed")?;
    if let Err(error) = flush_result {
        warn!(%error, "Home statistics write failed; business processing remains active and pending deltas were retained");
    }
    let local = Arc::clone(&statistics.local);
    let reports = tokio::task::spawn_blocking(move || local.pending_reports(16))
        .await
        .context("Home statistics outbox task failed")??;
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

#[allow(clippy::too_many_arguments)]
async fn run_work(
    config: Arc<Config>,
    tls: Arc<TlsMaterial>,
    permits: Arc<Semaphore>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization_rx: watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    expected_credential_id: Uuid,
    work_id: Uuid,
    work_secret: Vec<u8>,
    relay_id: String,
    relay_data_addr: String,
    expires_at_unix_secs: u64,
) -> Result<()> {
    if unix_time_secs()? >= expires_at_unix_secs {
        bail!("direct Relay work expired before Home connection");
    }
    let socket = timeout(Duration::from_secs(config.handshake_timeout_secs), async {
        let mut socket = TcpStream::connect(&relay_data_addr).await?;
        write_preface(&mut socket, RouteSide::Home, work_id, &work_secret).await?;
        Ok::<_, anyhow::Error>(socket)
    })
    .await
    .with_context(|| format!("Relay {relay_id} direct work setup timed out"))??;
    let mut stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        tls.business_acceptor.accept(socket),
    )
    .await
    .context("business TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Travel, None, &[])?;
    let credential =
        authorize_business_identity(&identity, &authorization_rx, expected_credential_id)?;
    let credential_id = credential.credential_id;
    let not_after_unix_secs = credential
        .not_after_unix_secs
        .min(identity.not_after_unix_secs);

    let open = JsonFrameReader::new(&mut stream, DATA_FRAME_LIMIT)
        .read_with_timeout::<DataFrame>(Duration::from_secs(config.handshake_timeout_secs))
        .await?;
    let DataFrame::Open {
        flow_id,
        carrier_id,
        service_id,
        protocol,
    } = open
    else {
        bail!("first business frame must be OPEN");
    };
    let service = config
        .services
        .iter()
        .find(|candidate| candidate.id == service_id && candidate.protocol == protocol)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown or mismatched service"))?;
    if !credential.allows_service(&config.id, &service.id, service.protocol) {
        bail!("Travel credential is not authorized for this logical service");
    }

    match protocol {
        ServiceProtocol::Tcp => {
            tcp_flows
                .attach(
                    credential_id,
                    identity.id,
                    flow_id,
                    service,
                    IncomingCarrier {
                        carrier_id,
                        relay_id,
                        stream,
                        global_permit: None,
                        flow_permit: None,
                    },
                    not_after_unix_secs,
                )
                .await
        }
        ServiceProtocol::Udp => {
            let _permit = permits
                .try_acquire_owned()
                .map_err(|_| anyhow::anyhow!("home active-flow limit reached"))?;
            serve_udp(
                stream,
                flow_id,
                carrier_id,
                &service,
                config.udp_idle_secs,
                authorization_rx,
                credential_id,
                not_after_unix_secs,
                tcp_flows.statistics(),
                identity.id,
                relay_id,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_udp(
    stream: ServerTlsStream<TcpStream>,
    flow_id: Uuid,
    carrier_id: Uuid,
    service: &Service,
    idle_secs: u64,
    mut authorization_rx: watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    credential_id: Uuid,
    not_after_unix_secs: u64,
    statistics: Arc<LocalStatistics>,
    travel_id: String,
    relay_id: String,
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    let target_started = Instant::now();
    socket.connect(&service.target).await?;
    let target_latency_ms = u64::try_from(target_started.elapsed().as_millis()).unwrap_or(u64::MAX);
    record_home_udp_metric_sample(
        &statistics,
        &travel_id,
        service,
        &relay_id,
        "target_connection_latency_ms",
        target_latency_ms,
        Some(target_latency_ms),
    );
    record_home_udp_metric(
        &statistics,
        &travel_id,
        service,
        &relay_id,
        "home_flow_accepted",
        1,
    );
    let (mut tls_reader, mut tls_writer) = tokio::io::split(stream);
    let mut tls_reader = JsonFrameReader::new(&mut tls_reader, DATA_FRAME_LIMIT);
    write_json(
        &mut tls_writer,
        &DataFrame::OpenOk {
            flow_id,
            carrier_id,
            receive_offset: 0,
            send_offset: 0,
        },
        DATA_FRAME_LIMIT,
    )
    .await?;
    let mut send_sequence = 0_u64;
    let mut receive_sequence = 0_u64;
    let mut buffer = vec![0_u8; 65_507];
    loop {
        tokio::select! {
            response = timeout(Duration::from_secs(idle_secs), socket.recv(&mut buffer)) => {
                let count = response.context("UDP association idle timeout")??;
                record_home_udp_metric(&statistics, &travel_id, service, &relay_id, "home_flow_download_observed_datagram_bytes", count as u64);
                write_json(&mut tls_writer, &DataFrame::Datagram { flow_id, sequence: send_sequence, bytes: buffer[..count].to_vec() }, DATA_FRAME_LIMIT).await?;
                send_sequence = send_sequence.wrapping_add(1);
            }
            frame = timeout(Duration::from_secs(idle_secs), tls_reader.read::<DataFrame>()) => {
                match frame.context("UDP association idle timeout")?? {
                    DataFrame::Datagram { flow_id: id, sequence, bytes } if id == flow_id && sequence >= receive_sequence && bytes.len() <= 65_507 => {
                        if sequence == receive_sequence {
                            socket.send(&bytes).await?;
                            record_home_udp_metric(&statistics, &travel_id, service, &relay_id, "delivered_upload_datagram_bytes", bytes.len() as u64);
                            receive_sequence = receive_sequence.wrapping_add(1);
                        }
                    }
                    DataFrame::Close { flow_id: id, .. } if id == flow_id => return Ok(()),
                    _ => bail!("invalid UDP flow frame"),
                }
            }
            changed = authorization_rx.changed() => {
                changed.map_err(|_| anyhow::anyhow!("Travel authorization publisher closed"))?;
                ensure_credential_active(&authorization_rx, credential_id)?;
            }
            () = sleep_until_unix(not_after_unix_secs) => {
                bail!("Travel credential expired");
            }
        }
    }
}

fn record_home_udp_metric(
    statistics: &LocalStatistics,
    travel_id: &str,
    service: &Service,
    relay_id: &str,
    family: &str,
    value: u64,
) {
    record_home_udp_metric_sample(
        statistics, travel_id, service, relay_id, family, value, None,
    );
}

fn record_home_udp_metric_sample(
    statistics: &LocalStatistics,
    travel_id: &str,
    service: &Service,
    relay_id: &str,
    family: &str,
    value: u64,
    histogram_sample: Option<u64>,
) {
    let mut dimensions = BTreeMap::new();
    dimensions.insert("travel_id".to_owned(), travel_id.to_owned());
    dimensions.insert("service_id".to_owned(), service.id.clone());
    dimensions.insert("protocol".to_owned(), "udp".to_owned());
    dimensions.insert("relay_id".to_owned(), relay_id.to_owned());
    if let Ok(now) = unix_time_secs() {
        statistics.record(now, family, dimensions, value, histogram_sample);
    }
}

fn authorize_business_identity(
    identity: &flowsplice_core::tls::PeerIdentity,
    authorization_rx: &watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    credential_id: Uuid,
) -> Result<flowsplice_core::authorization::TravelCredential> {
    let now = unix_time_secs()?;
    let authorization = authorization_rx
        .borrow()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Travel authorization has not synchronized from Server"))?;
    Ok(authorization
        .authorize_business_credential(identity, credential_id, now)?
        .clone())
}

fn ensure_credential_active(
    authorization_rx: &watch::Receiver<Option<Arc<VerifiedAuthorization>>>,
    credential_id: Uuid,
) -> Result<u64> {
    let now = unix_time_secs()?;
    let authorization = authorization_rx
        .borrow()
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Travel authorization has not synchronized from Server"))?;
    if !authorization.is_active(credential_id, now) {
        bail!("Travel credential is revoked, expired, or not yet valid");
    }
    authorization
        .credential(credential_id)
        .map(|credential| credential.not_after_unix_secs)
        .ok_or_else(|| anyhow::anyhow!("unknown Travel credential"))
}

async fn sleep_until_unix(not_after_unix_secs: u64) {
    let now = unix_time_secs().unwrap_or(not_after_unix_secs);
    tokio::time::sleep(Duration::from_secs(not_after_unix_secs.saturating_sub(now))).await;
}

async fn apply_authorization_snapshot(
    state: &TravelAuthorizationState,
    tcp_flows: &TcpFlowRegistry,
    config: &Config,
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
    info!(
        event = "travel_authorization_applied",
        generation,
        revoked = snapshot.revocations.len(),
        credentials = snapshot.credentials.len(),
        "Home applied Travel authorization state without restart"
    );
    Ok(generation)
}

async fn run_issuer_ui(state: IssuerAppState) -> Result<()> {
    let api = Router::new()
        .route("/status", get(api_issuer_status))
        .route("/credentials", get(api_issued_credentials))
        .route("/statistics", get(api_home_statistics))
        .route("/enrollment/pending", get(api_pending_remote_enrollments))
        .route("/enrollment/approve", post(api_approve_remote_enrollment))
        .route(
            "/home-enrollment/pending",
            get(api_pending_home_enrollments),
        )
        .route(
            "/home-enrollment/approve",
            post(api_approve_home_enrollment),
        )
        .route("/issue", post(api_issue))
        .route("/revoke", post(api_revoke))
        .route(
            "/private-key-password",
            post(api_rotate_private_key_password),
        )
        .fallback(|| async { StatusCode::NOT_FOUND });
    #[cfg(feature = "e2e-remote-ui")]
    let api = api.route(
        "/test/statistics-flush-failures",
        get(api_statistics_flush_fault_status).post(api_inject_statistics_flush_faults),
    );
    let app = Router::new()
        .nest("/api", api)
        .fallback(serve_spa)
        .with_state(state.clone())
        .layer(from_fn_with_state(state.clone(), authorize_issuer_ui));
    let listener = tokio::net::TcpListener::bind(&state.config.ui_listen).await?;
    info!(address = %state.config.ui_listen, "Home UI ready");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(feature = "e2e-remote-ui")]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StatisticsFlushFaultRequest {
    failures: u64,
}

#[cfg(feature = "e2e-remote-ui")]
#[derive(Serialize)]
struct StatisticsFlushFaultResponse {
    remaining: u64,
}

#[cfg(feature = "e2e-remote-ui")]
async fn api_statistics_flush_fault_status(
    State(state): State<IssuerAppState>,
) -> Json<StatisticsFlushFaultResponse> {
    Json(StatisticsFlushFaultResponse {
        remaining: state.statistics.local.injected_flush_failures_remaining(),
    })
}

#[cfg(feature = "e2e-remote-ui")]
async fn api_inject_statistics_flush_faults(
    State(state): State<IssuerAppState>,
    Json(request): Json<StatisticsFlushFaultRequest>,
) -> ApiResult<StatisticsFlushFaultResponse> {
    if request.failures > 16 {
        return Err(api_error(anyhow::anyhow!(
            "statistics flush failure count exceeds the E2E limit"
        )));
    }
    state
        .statistics
        .local
        .inject_flush_failures(request.failures);
    Ok(Json(StatisticsFlushFaultResponse {
        remaining: request.failures,
    }))
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
struct HomeStatisticsResponse {
    period: String,
    from_unix_secs: u64,
    to_unix_secs: u64,
    dropped_events: u64,
    overview: Vec<MetricRollup>,
    breakdowns: Vec<MetricRollup>,
    points: Vec<MetricPoint>,
}

async fn api_home_statistics(
    State(state): State<IssuerAppState>,
    axum::extract::Query(query): axum::extract::Query<StatisticsQuery>,
) -> Json<HomeStatisticsResponse> {
    home_statistics_response(&state.statistics, query).await
}

async fn home_statistics_response(
    statistics: &HomeStatistics,
    query: StatisticsQuery,
) -> Json<HomeStatisticsResponse> {
    let now = unix_time_secs().unwrap_or_default();
    let duration = match query.period.as_str() {
        "week" => 7 * 24 * 60 * 60,
        "month" => 31 * 24 * 60 * 60,
        "year" => 366 * 24 * 60 * 60,
        _ => 24 * 60 * 60,
    };
    let from = now.saturating_sub(duration);
    let local = Arc::clone(&statistics.local);
    let points = tokio::task::spawn_blocking(move || local.query(from, now))
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    Json(HomeStatisticsResponse {
        period: query.period,
        from_unix_secs: from,
        to_unix_secs: now,
        dropped_events: statistics.local.dropped_events(),
        overview: summarize_metric_points(&points, false),
        breakdowns: summarize_metric_points(&points, true),
        points,
    })
}

async fn run_statistics_only_ui(config: Arc<Config>, statistics: HomeStatistics) -> Result<()> {
    #[derive(Clone)]
    struct StatisticsOnlyState {
        statistics: HomeStatistics,
    }
    async fn api(
        State(state): State<StatisticsOnlyState>,
        axum::extract::Query(query): axum::extract::Query<StatisticsQuery>,
    ) -> Json<HomeStatisticsResponse> {
        home_statistics_response(&state.statistics, query).await
    }
    async fn page() -> axum::response::Html<String> {
        axum::response::Html(statistics_dashboard_html(
            "Home statistics",
            "Delivered business traffic, target outcomes, and Relay paths. This Home is serving-only; issuer operations are unavailable.",
            false,
        ))
    }
    let state = StatisticsOnlyState { statistics };
    let app = Router::new()
        .route("/", get(page))
        .route("/api/statistics", get(api))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(&config.ui_listen).await?;
    info!(address = %config.ui_listen, "serving-only Home statistics UI ready");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn api_issuer_status(State(state): State<IssuerAppState>) -> Json<IssuerStatus> {
    Json(IssuerStatus {
        home_id: state.config.id.clone(),
        home_alias: state.config.alias.clone(),
        default_valid_days: state.issuer.default_valid_days,
        global_authority_available: state.issuer.global_authority.is_some(),
        home_enrollment_available: state.issuer.home_enrollment_authority.is_some(),
        private_key_password_rotation_available: issuer_password_rotation_is_local(
            &state.config,
            &state.issuer,
        ),
        services: state.config.services.clone(),
    })
}

#[derive(Serialize)]
struct RemoteEnrollmentStatus {
    request_id: Uuid,
    travel_id: String,
    home_id: String,
    received_at_unix_secs: u64,
    approved: bool,
    bootstrap: bool,
    verification_code: Option<String>,
}

async fn api_pending_remote_enrollments(
    State(state): State<IssuerAppState>,
) -> ApiResult<Vec<RemoteEnrollmentStatus>> {
    let store = Arc::clone(&state.statistics.store);
    tokio::task::spawn_blocking(move || {
        let mut records = store
            .scan_prefix(Table::EnrollmentInbox, b"")?
            .into_iter()
            .map(|(_, value)| {
                serde_json::from_slice::<RemoteEnrollmentInboxRecord>(&value)
                    .context("Home enrollment inbox contains an invalid record")
            })
            .collect::<Result<Vec<_>>>()?;
        records.sort_by_key(|record| record.received_at_unix_secs);
        Ok(records
            .into_iter()
            .filter(|record| record.installed_credential_id.is_none())
            .map(|record| RemoteEnrollmentStatus {
                request_id: record.request.request_id,
                travel_id: record.travel_id,
                home_id: record.home_id,
                received_at_unix_secs: record.received_at_unix_secs,
                approved: record.response.is_some(),
                bootstrap: record.bootstrap_token_sha256.is_some(),
                verification_code: record.verification_code,
            })
            .collect())
    })
    .await
    .context("Home enrollment inbox query task failed")
    .and_then(|result| result)
    .map(Json)
    .map_err(api_error)
}

async fn api_approve_remote_enrollment(
    State(state): State<IssuerAppState>,
    Json(request): Json<ApproveRemoteEnrollmentRequest>,
) -> ApiResult<IssueResponse> {
    let result = approve_remote_enrollment(&state, request).await;
    record_issuer_operation(&state, "issuer_enrollment_approval", result.is_ok());
    result.map(Json).map_err(api_error)
}

async fn api_pending_home_enrollments(
    State(state): State<IssuerAppState>,
) -> ApiResult<Vec<HomeEnrollmentStatus>> {
    if state.issuer.global_authority.is_none() || state.issuer.home_enrollment_authority.is_none() {
        return Err(api_error(anyhow::anyhow!(
            "this Home is not a global Home enrollment issuer"
        )));
    }
    let store = Arc::clone(&state.statistics.store);
    tokio::task::spawn_blocking(move || {
        let mut records = store
            .scan_prefix(Table::HomeEnrollmentInbox, b"")?
            .into_iter()
            .map(|(_, value)| {
                serde_json::from_slice::<HomeEnrollmentInboxRecord>(&value)
                    .context("Home enrollment inbox contains an invalid record")
            })
            .collect::<Result<Vec<_>>>()?;
        records.sort_by_key(|record| record.received_at_unix_secs);
        Ok(records
            .into_iter()
            .map(|record| HomeEnrollmentStatus {
                request_id: record.request.request_id,
                home_id: record.home_id,
                received_at_unix_secs: record.received_at_unix_secs,
                approved: record.response.is_some(),
                verification_code: record.verification_code,
                profile: record.response.map(|response| response.approval.profile),
            })
            .collect())
    })
    .await
    .context("Home enrollment inbox query task failed")
    .and_then(|result| result)
    .map(Json)
    .map_err(api_error)
}

async fn api_approve_home_enrollment(
    State(state): State<IssuerAppState>,
    Json(request): Json<ApproveHomeEnrollmentRequest>,
) -> ApiResult<ApproveHomeEnrollmentResponse> {
    let result = approve_home_enrollment(&state, request).await;
    record_issuer_operation(&state, "issuer_home_enrollment_approval", result.is_ok());
    result.map(Json).map_err(api_error)
}

async fn approve_home_enrollment(
    state: &IssuerAppState,
    request: ApproveHomeEnrollmentRequest,
) -> Result<ApproveHomeEnrollmentResponse> {
    if state.issuer.global_authority.is_none() {
        bail!("only a global Home issuer may approve new Homes");
    }
    let enrollment_authority = state
        .issuer
        .home_enrollment_authority
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("Home enrollment authority is not configured"))?;
    let store = Arc::clone(&state.statistics.store);
    let request_id = request.request_id;
    let mut record = tokio::task::spawn_blocking(move || {
        store
            .get_json::<HomeEnrollmentInboxRecord>(
                Table::HomeEnrollmentInbox,
                request_id.as_bytes(),
            )?
            .ok_or_else(|| anyhow::anyhow!("unknown Home enrollment request"))
    })
    .await
    .context("Home enrollment inbox read task failed")??;
    if let Some(existing) = &record.response {
        if existing.approval.profile != request.profile {
            bail!("Home enrollment was already approved with a different profile");
        }
        return Ok(ApproveHomeEnrollmentResponse {
            request_id,
            home_id: record.home_id,
            profile: request.profile,
        });
    }
    let valid_days = request
        .valid_days
        .unwrap_or(state.issuer.default_valid_days);
    if valid_days == 0 || valid_days > MAX_VALID_DAYS {
        bail!("Home validity must be between 1 and {MAX_VALID_DAYS} days");
    }
    let password = Zeroizing::new(request.password);
    if password.is_empty() && !state.issuer.allow_unencrypted_test_keys {
        bail!("issuer password must not be empty");
    }
    let approval = prepare_home_enrollment_approval(
        record.request.clone(),
        u64::from(valid_days) * 86_400,
        enrollment_authority.id.clone(),
        request.profile,
        unix_time_secs()?,
    )?;
    let protected = |path| ProtectedKey {
        path,
        password: Some(password.as_bytes()),
        allow_unencrypted: state.issuer.allow_unencrypted_test_keys,
    };
    let deployment_root_public_key =
        std::fs::read_to_string(&state.config.deployment_root_public_key)?;
    let deployment_trust: SignedDeploymentTrust = load_json(&state.config.deployment_trust)?;
    let material = HomeIssuerMaterial {
        deployment_trust: &deployment_trust,
        deployment_root_public_key: deployment_root_public_key.trim(),
        management_ca_certificate: &state.config.management_ca,
        management_ca_key: protected(&state.issuer.management_ca_key),
        business_ca_certificate: &state.config.business_ca,
        business_ca_key: protected(&state.issuer.business_ca_key),
        home_enrollment_authority_key: protected(&enrollment_authority.private_key),
    };
    let _sensitive_permit = Arc::clone(&state.sensitive_operation)
        .try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("another sensitive issuer operation is already running"))?;
    let key_operation = state.key_operation.lock().await;
    recover_private_key_password_rotation(&issuer_key_targets(&state.issuer))?;
    let response = issue_home_enrollment(approval, &material, unix_time_secs()?)?;
    drop(key_operation);
    record.response = Some(response);
    let home_id = record.home_id.clone();
    let store = Arc::clone(&state.statistics.store);
    tokio::task::spawn_blocking(move || {
        store.apply_immediate(WriteBatch::new().put_json(
            Table::HomeEnrollmentInbox,
            request_id.as_bytes().to_vec(),
            &record,
        )?)
    })
    .await
    .context("Home enrollment response commit task failed")??;
    Ok(ApproveHomeEnrollmentResponse {
        request_id,
        home_id,
        profile: request.profile,
    })
}

async fn approve_remote_enrollment(
    state: &IssuerAppState,
    request: ApproveRemoteEnrollmentRequest,
) -> Result<IssueResponse> {
    let store = Arc::clone(&state.statistics.store);
    let request_id = request.request_id;
    let record = tokio::task::spawn_blocking(move || {
        store
            .get_json::<RemoteEnrollmentInboxRecord>(Table::EnrollmentInbox, request_id.as_bytes())?
            .ok_or_else(|| anyhow::anyhow!("unknown remote enrollment request"))
    })
    .await
    .context("Home enrollment inbox read task failed")??;
    if record.travel_id != record.request.travel_id || record.home_id != state.config.id {
        bail!("remote enrollment inbox record has an invalid identity binding");
    }
    let issued = issue_from_home(
        state,
        IssueRequest {
            request: record.request.clone(),
            valid_days: request.valid_days,
            valid_minutes: request.valid_minutes,
            scope: request.scope,
            password: request.password,
        },
    )
    .await?;
    let mut updated = record;
    updated.response = Some(issued.enrollment.clone());
    let store = Arc::clone(&state.statistics.store);
    tokio::task::spawn_blocking(move || {
        store.apply_immediate(WriteBatch::new().put_json(
            Table::EnrollmentInbox,
            request_id.as_bytes().to_vec(),
            &updated,
        )?)
    })
    .await
    .context("Home enrollment response commit task failed")??;
    Ok(issued)
}

async fn api_issued_credentials(
    State(state): State<IssuerAppState>,
) -> Json<Vec<IssuedCredentialStatus>> {
    let now = unix_time_secs().unwrap_or_default();
    let authority_ids = [
        Some(state.issuer.home_authority.id.as_str()),
        state
            .issuer
            .global_authority
            .as_ref()
            .map(|authority| authority.id.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect::<std::collections::HashSet<_>>();
    let Some(authorization) = state.authorization.tx.borrow().clone() else {
        return Json(Vec::new());
    };
    let mut credentials = authorization
        .credentials()
        .filter(|credential| authority_ids.contains(credential.authority_id.as_str()))
        .map(|credential| IssuedCredentialStatus {
            credential_id: credential.credential_id,
            travel_id: credential.travel_id.clone(),
            authority_id: credential.authority_id.clone(),
            scope: credential.scope.clone(),
            not_after_unix_secs: credential.not_after_unix_secs,
            revoked: authorization
                .revoked_credentials()
                .contains(&credential.credential_id),
            active: authorization.is_active(credential.credential_id, now),
        })
        .collect::<Vec<_>>();
    credentials.sort_by(|left, right| {
        left.travel_id
            .cmp(&right.travel_id)
            .then_with(|| left.credential_id.cmp(&right.credential_id))
    });
    Json(credentials)
}

async fn api_issue(
    State(state): State<IssuerAppState>,
    Json(request): Json<IssueRequest>,
) -> ApiResult<IssueResponse> {
    let result = issue_from_home(&state, request).await;
    record_issuer_operation(&state, "issuer_credential_issuance", result.is_ok());
    result.map(Json).map_err(api_error)
}

fn record_issuer_operation(state: &IssuerAppState, family: &str, accepted: bool) {
    let mut dimensions = BTreeMap::new();
    dimensions.insert(
        "result".to_owned(),
        if accepted { "accepted" } else { "rejected" }.to_owned(),
    );
    if let Ok(now) = unix_time_secs() {
        state
            .statistics
            .local
            .record(now, family, dimensions, 1, None);
    }
}

async fn issue_from_home(state: &IssuerAppState, request: IssueRequest) -> Result<IssueResponse> {
    let IssueRequest {
        request,
        valid_days,
        valid_minutes,
        scope,
        password,
    } = request;
    let valid_for_secs =
        requested_validity_secs(valid_days, valid_minutes, state.issuer.default_valid_days)?;
    validate_requested_scope(&state.config, &state.issuer, &scope)?;
    let authority = match &scope {
        TravelCredentialScope::Global => state
            .issuer
            .global_authority
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("global super authorization is not configured"))?,
        TravelCredentialScope::Home { .. } | TravelCredentialScope::Service { .. } => {
            &state.issuer.home_authority
        }
    };
    let mut ledger = state.issuance_ledger.lock().await;
    if let Some(record) = ledger.find(&request)? {
        if !record.matches_intent(&authority.id, &scope, valid_for_secs) {
            bail!(
                "this enrollment request was already used for a different authorization; create a new enrollment request"
            );
        }
        ensure_record_not_revoked(state, &record)?;
        let generation = if let Some(generation) = record.published_generation() {
            generation
        } else {
            let generation = publish_enrollment(state, record.enrollment()).await?;
            ledger.mark_published(record.credential_id(), generation)?;
            generation
        };
        return Ok(IssueResponse {
            generation,
            enrollment: record.enrollment().clone(),
            reused: true,
        });
    }

    let _sensitive_permit = Arc::clone(&state.sensitive_operation)
        .try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("another sensitive issuer operation is already running"))?;
    let password = Zeroizing::new(password);
    if password.is_empty() && !state.issuer.allow_unencrypted_test_keys {
        bail!("private-key password must not be empty");
    }
    let approval = prepare_enrollment_approval(
        request.clone(),
        valid_for_secs,
        authority.id.clone(),
        scope.clone(),
        unix_time_secs()?,
    )?;
    let protected = |path| ProtectedKey {
        path,
        password: Some(password.as_bytes()),
        allow_unencrypted: state.issuer.allow_unencrypted_test_keys,
    };
    let deployment_root_public_key =
        std::fs::read_to_string(&state.config.deployment_root_public_key)
            .context("failed to read deployment root public key")?;
    let deployment_trust: SignedDeploymentTrust = load_json(&state.config.deployment_trust)?;
    let material = IssuerMaterial {
        deployment_trust: &deployment_trust,
        deployment_root_public_key: deployment_root_public_key.trim(),
        home_endpoint_credential: state.endpoint_credential.as_deref(),
        management_ca_certificate: &state.config.management_ca,
        management_ca_key: protected(&state.issuer.management_ca_key),
        business_ca_certificate: &state.config.business_ca,
        business_ca_key: protected(&state.issuer.business_ca_key),
        travel_authority_key: protected(&authority.private_key),
    };
    let key_operation = state.key_operation.lock().await;
    recover_private_key_password_rotation(&issuer_key_targets(&state.issuer))?;
    let enrollment = issue_enrollment(approval, &material, unix_time_secs()?)?;
    drop(key_operation);
    let record =
        ledger.insert_pending(&request, &authority.id, &scope, valid_for_secs, enrollment)?;
    let generation = publish_enrollment(state, record.enrollment()).await?;
    ledger.mark_published(record.credential_id(), generation)?;
    Ok(IssueResponse {
        generation,
        enrollment: record.enrollment().clone(),
        reused: false,
    })
}

fn ensure_record_not_revoked(state: &IssuerAppState, record: &IssuanceRecord) -> Result<()> {
    let Some(authorization) = state.authorization.tx.borrow().clone() else {
        return Ok(());
    };
    if authorization.credential(record.credential_id()).is_some()
        && !authorization.is_active(record.credential_id(), unix_time_secs()?)
    {
        bail!(
            "this enrollment intent was already issued, but its credential is no longer active; create a new enrollment request"
        );
    }
    Ok(())
}

async fn publish_enrollment(
    state: &IssuerAppState,
    enrollment: &TravelEnrollmentResponse,
) -> Result<u64> {
    let (response_tx, response_rx) = oneshot::channel();
    state
        .control_tx
        .send(IssuerControlRequest::Publish {
            credential: enrollment.signed_credential.clone(),
            response: response_tx,
        })
        .await
        .map_err(|_| anyhow::anyhow!("Home control connection is unavailable"))?;
    timeout(Duration::from_secs(15), response_rx)
        .await
        .context("Server did not confirm the signed credential")?
        .context("Home control connection closed before publication")?
        .map_err(anyhow::Error::msg)
}

async fn api_rotate_private_key_password(
    State(state): State<IssuerAppState>,
    Json(request): Json<RotatePrivateKeyPasswordRequest>,
) -> ApiResult<RotatePrivateKeyPasswordResponse> {
    rotate_issuer_private_key_password(&state, request)
        .await
        .map(Json)
        .map_err(api_error)
}

async fn rotate_issuer_private_key_password(
    state: &IssuerAppState,
    request: RotatePrivateKeyPasswordRequest,
) -> Result<RotatePrivateKeyPasswordResponse> {
    if !issuer_password_rotation_is_local(&state.config, &state.issuer) {
        bail!("Home private-key password rotation is available only on a loopback issuer UI");
    }
    if state.issuer.allow_unencrypted_test_keys {
        bail!("password rotation is unavailable for unencrypted test issuer keys");
    }
    let _sensitive_permit = Arc::clone(&state.sensitive_operation)
        .try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("another sensitive issuer operation is already running"))?;
    let current_password = Zeroizing::new(request.current_password);
    let new_password = Zeroizing::new(request.new_password);
    if new_password.chars().count() < MIN_PRIVATE_KEY_PASSWORD_CHARACTERS {
        bail!(
            "new private-key password must contain at least {MIN_PRIVATE_KEY_PASSWORD_CHARACTERS} characters"
        );
    }
    let rotated_keys = issuer_key_targets(&state.issuer).len();
    let issuer = Arc::clone(&state.issuer);
    let key_operation = state.key_operation.lock().await;
    tokio::task::spawn_blocking(move || {
        rotate_private_key_passwords(
            &issuer_key_targets(&issuer),
            current_password.as_str(),
            new_password.as_str(),
        )
    })
    .await
    .context("Home issuer private-key password rotation task failed")??;
    drop(key_operation);
    info!(rotated_keys, "rotated Home issuer private-key password");
    Ok(RotatePrivateKeyPasswordResponse { rotated_keys })
}

fn issuer_password_rotation_is_local(config: &Config, issuer: &IssuerConfig) -> bool {
    config
        .ui_listen
        .parse::<SocketAddr>()
        .is_ok_and(|address| address.ip().is_loopback())
        || test_remote_issuer_enabled(issuer)
}

fn requested_validity_secs(
    valid_days: Option<u32>,
    valid_minutes: Option<u32>,
    default_valid_days: u32,
) -> Result<u64> {
    if valid_days.is_some() && valid_minutes.is_some() {
        bail!("specify either valid_days or valid_minutes, not both");
    }
    if let Some(minutes) = valid_minutes {
        let seconds = u64::from(minutes)
            .checked_mul(60)
            .ok_or_else(|| anyhow::anyhow!("Travel validity overflow"))?;
        if minutes == 0 || seconds > u64::from(MAX_VALID_DAYS) * 24 * 60 * 60 {
            bail!("Travel validity must be between 1 minute and {MAX_VALID_DAYS} days");
        }
        return Ok(seconds);
    }
    let days = valid_days.unwrap_or(default_valid_days);
    if days == 0 || days > MAX_VALID_DAYS {
        bail!("Travel validity must be between 1 and {MAX_VALID_DAYS} days");
    }
    Ok(u64::from(days) * 24 * 60 * 60)
}

fn validate_requested_scope(
    config: &Config,
    issuer: &IssuerConfig,
    scope: &TravelCredentialScope,
) -> Result<()> {
    match scope {
        TravelCredentialScope::Global => {
            if issuer.global_authority.is_none() {
                bail!("global super authorization is not configured");
            }
        }
        TravelCredentialScope::Home { home_id } if home_id == &config.id => {}
        TravelCredentialScope::Service {
            home_id,
            service_id,
            protocol,
        } if home_id == &config.id
            && config
                .services
                .iter()
                .any(|service| service.id == *service_id && service.protocol == *protocol) => {}
        TravelCredentialScope::Home { .. } | TravelCredentialScope::Service { .. } => {
            bail!("ordinary Home authority may sign only this Home's configured services");
        }
    }
    Ok(())
}

async fn api_revoke(
    State(state): State<IssuerAppState>,
    Json(request): Json<RevokeRequest>,
) -> ApiResult<serde_json::Value> {
    let result = revoke_from_home(&state, request).await;
    record_issuer_operation(&state, "issuer_revocation", result.is_ok());
    result
}

async fn revoke_from_home(
    state: &IssuerAppState,
    request: RevokeRequest,
) -> ApiResult<serde_json::Value> {
    let reason = request.reason.trim();
    if reason.is_empty() || reason.len() > 256 {
        return Err(api_error(anyhow::anyhow!(
            "revocation reason must contain 1 to 256 bytes"
        )));
    }
    let _sensitive_permit = Arc::clone(&state.sensitive_operation)
        .try_acquire_owned()
        .map_err(|_| {
            api_error(anyhow::anyhow!(
                "another sensitive issuer operation is already running"
            ))
        })?;
    let password = Zeroizing::new(request.password);
    if password.is_empty() && !state.issuer.allow_unencrypted_test_keys {
        return Err(api_error(anyhow::anyhow!(
            "issuer password must not be empty"
        )));
    }
    let authorization =
        state.authorization.tx.borrow().clone().ok_or_else(|| {
            api_error(anyhow::anyhow!("Travel authorization has not synchronized"))
        })?;
    let credential = authorization
        .credential(request.credential_id)
        .ok_or_else(|| api_error(anyhow::anyhow!("unknown Travel credential")))?;
    let authority_key = if credential.authority_id == state.issuer.home_authority.id {
        state.issuer.home_authority.private_key.clone()
    } else if let Some(authority) = state
        .issuer
        .global_authority
        .as_ref()
        .filter(|authority| authority.id == credential.authority_id)
    {
        authority.private_key.clone()
    } else {
        return Err(api_error(anyhow::anyhow!(
            "credential was not issued by this Home"
        )));
    };
    let allow_unencrypted = state.issuer.allow_unencrypted_test_keys;
    tokio::task::spawn_blocking(move || {
        load_issuer_private_key(&authority_key, Some(password.as_bytes()), allow_unencrypted)
            .map(|_| ())
    })
    .await
    .map_err(|_| api_error(anyhow::anyhow!("issuer password verification task failed")))?
    .map_err(api_error)?;
    let (response_tx, response_rx) = oneshot::channel();
    state
        .control_tx
        .send(IssuerControlRequest::Revoke {
            credential_id: request.credential_id,
            reason: reason.to_owned(),
            response: response_tx,
        })
        .await
        .map_err(|_| api_error(anyhow::anyhow!("Home control connection is unavailable")))?;
    let generation = timeout(Duration::from_secs(15), response_rx)
        .await
        .map_err(|_| api_error(anyhow::anyhow!("Server did not confirm the revocation")))?
        .map_err(|_| {
            api_error(anyhow::anyhow!(
                "Home control connection closed before revocation"
            ))
        })?
        .map_err(|error| api_error(anyhow::Error::msg(error)))?;
    Ok(Json(serde_json::json!({ "generation": generation })))
}

fn api_error(error: impl Into<anyhow::Error>) -> (StatusCode, Json<ApiError>) {
    let error = error.into();
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: error.to_string(),
        }),
    )
}

async fn serve_spa(request: Request) -> Response {
    SPA.serve(request)
}

async fn authorize_issuer_ui(
    State(state): State<IssuerAppState>,
    request: Request,
    next: Next,
) -> Response {
    if local_ui_request_allowed(&request, &state.config.ui_listen) {
        return next.run(request).await;
    }
    #[cfg(feature = "e2e-remote-ui")]
    if test_remote_issuer_enabled(&state.issuer) {
        use aws_lc_rs::constant_time::verify_slices_are_equal;
        let expected = state.issuer.test_admin_token.as_deref().unwrap_or_default();
        let authorized = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| {
                verify_slices_are_equal(token.as_bytes(), expected.as_bytes()).is_ok()
            });
        if authorized {
            return next.run(request).await;
        }
    }
    StatusCode::FORBIDDEN.into_response()
}

fn local_ui_request_allowed(request: &Request, listen: &str) -> bool {
    let Ok(address) = listen.parse::<SocketAddr>() else {
        return false;
    };
    if address.ip() != std::net::Ipv4Addr::LOCALHOST {
        return false;
    }
    let authority = address.to_string();
    if request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        != Some(authority.as_str())
    {
        return false;
    }
    if let Some(site) = request
        .headers()
        .get("sec-fetch-site")
        .and_then(|value| value.to_str().ok())
        && !matches!(site, "same-origin" | "none")
    {
        return false;
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        let expected = format!("http://{authority}");
        if request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            != Some(expected.as_str())
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, extract::Request, http::Method};

    use super::{
        Config, local_ui_request_allowed, remote_enrollment_capacity_available,
        remote_enrollment_inbox_expired, requested_validity_secs,
    };
    use anyhow::Result;

    #[test]
    fn issuer_validity_supports_default_days_and_short_lived_minutes() -> Result<()> {
        assert_eq!(
            requested_validity_secs(None, None, 365)?,
            365 * 24 * 60 * 60
        );
        assert_eq!(requested_validity_secs(None, Some(30), 365)?, 30 * 60);
        assert!(requested_validity_secs(Some(1), Some(30), 365).is_err());
        assert!(requested_validity_secs(None, Some(0), 365).is_err());
        Ok(())
    }

    #[test]
    fn remote_enrollment_inbox_has_bounded_capacity_and_retention() {
        assert!(remote_enrollment_capacity_available(1023, 1024));
        assert!(!remote_enrollment_capacity_available(1024, 1024));
        assert!(!remote_enrollment_inbox_expired(100, None, 100));
        assert!(remote_enrollment_inbox_expired(
            100,
            None,
            100 + 7 * 24 * 60 * 60
        ));
        assert!(!remote_enrollment_inbox_expired(
            1,
            Some(200),
            200 + 24 * 60 * 60 - 1
        ));
        assert!(remote_enrollment_inbox_expired(
            1,
            Some(200),
            200 + 24 * 60 * 60
        ));
    }

    #[test]
    fn issuer_ui_requires_exact_loopback_host_and_origin() -> Result<()> {
        let get = Request::builder()
            .uri("http://127.0.0.1:9081/")
            .header("host", "127.0.0.1:9081")
            .body(Body::empty())?;
        assert!(local_ui_request_allowed(&get, "127.0.0.1:9081"));

        let bad_host = Request::builder()
            .uri("http://attacker.invalid/")
            .header("host", "attacker.invalid")
            .body(Body::empty())?;
        assert!(!local_ui_request_allowed(&bad_host, "127.0.0.1:9081"));

        let post = Request::builder()
            .method(Method::POST)
            .uri("http://127.0.0.1:9081/api/issue")
            .header("host", "127.0.0.1:9081")
            .header("origin", "http://127.0.0.1:9081")
            .body(Body::empty())?;
        assert!(local_ui_request_allowed(&post, "127.0.0.1:9081"));

        let missing_origin = Request::builder()
            .method(Method::POST)
            .uri("http://127.0.0.1:9081/api/issue")
            .header("host", "127.0.0.1:9081")
            .body(Body::empty())?;
        assert!(!local_ui_request_allowed(&missing_origin, "127.0.0.1:9081"));
        Ok(())
    }

    #[test]
    fn secondary_home_configuration_does_not_require_an_issuer() -> Result<()> {
        let config: Config = toml::from_str(
            r#"
id = "home-2"
alias = "Secondary Home"
server_control_addr = "127.0.0.1:7443"
server_name = "server.invalid"
server_id = "server-1"
management_cert = "home.crt"
management_key = "home.key"
management_ca = "management-ca.crt"
business_cert = "home-business.crt"
business_key = "home-business.key"
business_ca = "business-ca.crt"
deployment_root_public_key = "deployment-root.pub"
deployment_trust = "deployment-trust.json"
travel_authorization_cache = "authorization-cache.json"
state_store = "home-state.redb"
ui_listen = "127.0.0.1:9081"

[[services]]
id = "ssh"
alias = "SSH"
protocol = "tcp"
target = "127.0.0.1:22"
"#,
        )?;
        assert!(config.issuer.is_none());
        Ok(())
    }
}
