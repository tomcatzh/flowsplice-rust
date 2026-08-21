#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env, fs,
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    digest,
    rand::{SecureRandom, SystemRandom},
    signature::EcdsaKeyPair,
};
use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{Method, StatusCode, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::{Parser, Subcommand};
use embedded_spa::{EmbeddedSpa, EmbeddedSpaConfig};
use flowsplice_core::{
    CONTROL_FRAME_LIMIT, DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    authorization::unix_time_secs,
    config::load_toml,
    deployment::{
        DeploymentTrust, SignedControlSnapshot, SignedDeploymentTrust, VerifiedControlSnapshot,
    },
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{
        CONTROL_PROTOCOL_VERSION, Catalog, ControlMessage, DataFrame, RelayDirectory, Role,
        ServiceProtocol, TravelConnectionPurpose, bootstrap_verification_code,
    },
    route::{RouteSide, write_preface},
    statistics::statistics_signing_key,
    tls::{
        identity_client_connector_with_private_key, identity_from_certificate_pem,
        identity_server_auth_connector_from_ca_pem, identity_server_name, peer_identity,
        require_peer,
    },
};
use flowsplice_enrollment::{
    BUSINESS_CA_FILE, BUSINESS_CERT_FILE, BUSINESS_KEY_FILE, DEPLOYMENT_TRUST_FILE,
    MANAGEMENT_CA_FILE, MANAGEMENT_CERT_FILE, MANAGEMENT_KEY_FILE, MAX_REQUEST_AGE_SECS,
    REQUEST_FILE, TravelEnrollmentRequest, TravelEnrollmentResponse, create_enrollment_request,
    install_enrollment_response,
    key::{
        MIN_PRIVATE_KEY_PASSWORD_CHARACTERS, PrivateKeyRotationTarget, is_encrypted_private_key,
        load_private_key, recover_private_key_password_rotation, rotate_private_key_passwords,
    },
    load_json, validate_enrollment_response,
};
use flowsplice_storage::{
    LocalStatistics, MetricPoint, MetricRollup, StateStore, Table, WriteBatch,
    summarize_metric_points,
};
use rust_embed::RustEmbed;
use rustls_pki_types::{CertificateDer, pem::PemObject};
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
#[allow(clippy::enum_variant_names)]
enum Command {
    EnrollInit(EnrollInitArgs),
    EnrollImport(EnrollImportArgs),
    EnrollRemote(EnrollRemoteArgs),
}

#[derive(clap::Args)]
struct EnrollInitArgs {
    #[arg(long)]
    travel_id: String,
    #[arg(long)]
    enrollment_dir: PathBuf,
    #[arg(long, hide = true)]
    test_password_file: Option<PathBuf>,
}

#[derive(clap::Args)]
struct EnrollImportArgs {
    #[arg(long)]
    enrollment_dir: PathBuf,
    #[arg(long)]
    response: PathBuf,
    #[arg(long, hide = true)]
    test_password_file: Option<PathBuf>,
}

#[derive(clap::Args)]
struct EnrollRemoteArgs {
    #[arg(long)]
    travel_id: String,
    #[arg(long)]
    home_id: String,
    #[arg(long)]
    install_dir: PathBuf,
    #[arg(long = "bootstrap-relay")]
    bootstrap_relays: Vec<String>,
    #[arg(long, default_value = "127.0.0.1:9080")]
    ui_listen: String,
    #[arg(long, default_value_t = 900)]
    wait_timeout_secs: u64,
    #[cfg(feature = "e2e-remote-ui")]
    #[arg(long, hide = true)]
    test_allow_remote_listen: bool,
    #[cfg(feature = "e2e-remote-ui")]
    #[arg(long, hide = true)]
    test_admin_token: Option<String>,
    #[arg(long, hide = true)]
    test_password_file: Option<PathBuf>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    id: String,
    seed_relays: Vec<SeedRelay>,
    homes: Vec<ConfiguredHome>,
    management_cert: PathBuf,
    management_key: PathBuf,
    management_ca: PathBuf,
    business_cert: PathBuf,
    business_key: PathBuf,
    business_ca: PathBuf,
    state_store: PathBuf,
    enrollment_work_dir: PathBuf,
    ui_listen: String,
    #[cfg(feature = "e2e-remote-ui")]
    #[serde(default)]
    test_allow_remote_listen: bool,
    #[cfg(feature = "e2e-remote-ui")]
    #[serde(default)]
    test_admin_token: Option<String>,
    #[serde(default)]
    mappings: Vec<Mapping>,
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
struct SeedRelay {
    management_addr: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RelayCandidate {
    expected_id: Option<String>,
    management_addr: String,
    management_spki_sha256: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RelayHistoryRecord {
    version: u32,
    deployment_id: String,
    relay_id: String,
    management_addr: String,
    management_spki_sha256: String,
    first_seen_unix_secs: u64,
    last_seen_unix_secs: u64,
    last_directory_generation: u64,
    last_success_unix_secs: Option<u64>,
    last_failure_unix_secs: Option<u64>,
    consecutive_failures: u32,
    current_member: bool,
    operator_disabled: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteEnrollmentOutboxRecord {
    version: u32,
    home_id: String,
    enrollment_dir: PathBuf,
    request: TravelEnrollmentRequest,
    response: Option<TravelEnrollmentResponse>,
    created_at_unix_secs: u64,
    last_attempt_unix_secs: Option<u64>,
    restart_required: bool,
    #[serde(default)]
    installed_credential_id: Option<Uuid>,
    #[serde(default)]
    installed_at_unix_secs: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct BootstrapEnrollmentState {
    version: u32,
    home_id: String,
    request_id: Uuid,
    retrieval_token_hex: String,
}

#[derive(Serialize)]
struct InstalledTravelConfig {
    id: String,
    management_cert: PathBuf,
    management_key: PathBuf,
    management_ca: PathBuf,
    business_cert: PathBuf,
    business_key: PathBuf,
    business_ca: PathBuf,
    state_store: PathBuf,
    enrollment_work_dir: PathBuf,
    ui_listen: String,
    #[cfg(feature = "e2e-remote-ui")]
    test_allow_remote_listen: bool,
    #[cfg(feature = "e2e-remote-ui")]
    test_admin_token: Option<String>,
    homes: Vec<InstalledHome>,
    seed_relays: Vec<SeedRelayOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    mappings: Vec<Mapping>,
}

#[derive(Serialize)]
struct InstalledHome {
    id: String,
}

#[derive(Serialize)]
struct SeedRelayOutput {
    management_addr: String,
}

impl RelayCandidate {
    fn label(&self) -> &str {
        self.expected_id.as_deref().unwrap_or(&self.management_addr)
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfiguredHome {
    id: String,
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

const fn default_max_active_carriers() -> usize {
    512
}

const fn default_max_carriers_per_flow() -> usize {
    16
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

const CONTROL_TRUST_STATE_FILE: &str = "control-trust-state.json";
const CONTROL_STATE_KEY: &[u8] = b"control";
const LEGACY_CONTROL_STATE_DIGEST_KEY: &[u8] = b"legacy_control_state_sha256";
const ACTIVE_IDENTITY_DIR_KEY: &[u8] = b"active_identity_dir";
const REMOTE_ENROLLMENT_VERSION: u32 = 1;
const MAX_REMOTE_ENROLLMENT_OUTBOX_RECORDS: usize = 64;
const REMOTE_ENROLLMENT_INSTALLED_RETENTION_SECS: u64 = 24 * 60 * 60;
const RELAY_HISTORY_VERSION: u32 = 1;
const CONTROL_TRUST_STATE_OBJECT_TYPE: &str = "flowsplice.travel_control_high_water";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ControlTrustState {
    object_type: String,
    deployment_id: Option<String>,
    trust_generation: u64,
    trust_digest_sha256: Option<String>,
    signer_epoch: u64,
    snapshot_generation: u64,
    snapshot_digest_sha256: Option<String>,
    cached_snapshot: Option<SignedControlSnapshot>,
}

impl ControlTrustState {
    fn new() -> Self {
        Self {
            object_type: CONTROL_TRUST_STATE_OBJECT_TYPE.to_owned(),
            deployment_id: None,
            trust_generation: 0,
            trust_digest_sha256: None,
            signer_epoch: 0,
            snapshot_generation: 0,
            snapshot_digest_sha256: None,
            cached_snapshot: None,
        }
    }

    fn validate_shape(&self) -> Result<()> {
        if self.object_type != CONTROL_TRUST_STATE_OBJECT_TYPE {
            bail!("unsupported Travel control high-water state");
        }
        if self.snapshot_generation == 0
            && (self.signer_epoch != 0
                || self.snapshot_digest_sha256.is_some()
                || self.cached_snapshot.is_some())
        {
            bail!("Travel control high-water state is inconsistent");
        }
        Ok(())
    }

    fn accept(&mut self, snapshot: &VerifiedControlSnapshot) -> Result<bool> {
        self.validate_shape()?;
        if self
            .deployment_id
            .as_deref()
            .is_some_and(|deployment_id| deployment_id != snapshot.trust.deployment_id)
        {
            bail!("control snapshot belongs to a different deployment");
        }
        if snapshot.trust.generation < self.trust_generation {
            bail!("deployment trust generation rollback detected");
        }
        if snapshot.trust.generation == self.trust_generation
            && self.trust_digest_sha256.is_some()
            && self.trust_digest_sha256.as_deref() != Some(&snapshot.trust_digest_sha256)
        {
            bail!("conflicting deployment trust documents use the same generation");
        }
        if snapshot.payload.signer_epoch < self.signer_epoch {
            bail!("Server control signer epoch rollback detected");
        }
        if snapshot.payload.signer_epoch == self.signer_epoch
            && snapshot.payload.generation < self.snapshot_generation
        {
            return Ok(false);
        }
        if snapshot.payload.signer_epoch == self.signer_epoch
            && snapshot.payload.generation == self.snapshot_generation
        {
            if self.snapshot_digest_sha256.as_deref() != Some(&snapshot.digest_sha256) {
                bail!("conflicting control snapshots use the same generation");
            }
            return Ok(false);
        }
        self.deployment_id = Some(snapshot.trust.deployment_id.clone());
        self.trust_generation = snapshot.trust.generation;
        self.trust_digest_sha256 = Some(snapshot.trust_digest_sha256.clone());
        self.signer_epoch = snapshot.payload.signer_epoch;
        self.snapshot_generation = snapshot.payload.generation;
        self.snapshot_digest_sha256 = Some(snapshot.digest_sha256.clone());
        Ok(true)
    }
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
    carrier_permits: Arc<Semaphore>,
    flow_relays: Arc<Mutex<HashMap<Uuid, String>>>,
    key_operation: Arc<Mutex<()>>,
    sensitive_operation: Arc<Semaphore>,
    deployment_root_public_key: Arc<String>,
    deployment_trust: Arc<RwLock<DeploymentTrust>>,
    management_spki_sha256: Arc<String>,
    state_store: Arc<StateStore>,
    statistics: Arc<LocalStatistics>,
    statistics_signer: Arc<EcdsaKeyPair>,
    statistics_certificate_pem: Arc<String>,
    relay_history: Arc<RwLock<Vec<RelayHistoryRecord>>>,
    control_trust_state: Arc<Mutex<ControlTrustState>>,
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
    private_key_password_rotation_available: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RotatePrivateKeyPasswordRequest {
    current_password: String,
    new_password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRemoteEnrollmentRequest {
    home_id: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRemoteEnrollmentRequest {
    request_id: Uuid,
    password: String,
}

#[derive(Serialize)]
struct RemoteEnrollmentStatus {
    request_id: Uuid,
    home_id: String,
    created_at_unix_secs: u64,
    response_received: bool,
    restart_required: bool,
}

#[derive(Serialize)]
struct InstallRemoteEnrollmentResponse {
    request_id: Uuid,
    credential_id: Uuid,
    restart_required: bool,
}

#[derive(Serialize)]
struct RotatePrivateKeyPasswordResponse {
    rotated_keys: usize,
}

#[derive(Serialize)]
struct ApiError {
    error: String,
}

type ApiResult<T> = std::result::Result<Json<T>, (StatusCode, Json<ApiError>)>;

struct RouteGrant {
    route_id: Uuid,
    route_secret: Vec<u8>,
    data_addr: String,
}

#[allow(clippy::too_many_lines)]
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
        return run_command(command).await;
    }
    let mut config: Config = load_toml(&cli.config)?;
    validate_config(&config)?;
    let state_store = StateStore::open(&config.state_store)?;
    apply_active_identity_directory(&mut config, &state_store)?;
    let deployment_root_public_key = embedded_deployment_root_public_key()?.to_owned();
    let management_identity = local_certificate_identity(&config.management_cert)?;
    require_peer(&management_identity, Role::Travel, Some(&config.id), &[])?;
    let legacy_control_trust_state_path =
        enrollment_sibling(&config.management_cert, CONTROL_TRUST_STATE_FILE);
    let (control_trust_state, deployment_trust, cached_control_snapshot) =
        load_initial_control_trust_state(
            &config,
            &deployment_root_public_key,
            &state_store,
            &legacy_control_trust_state_path,
            &management_identity.spki_sha256,
        )?;
    let relay_history = load_relay_history(&state_store)?;
    if recover_private_key_password_rotation(&travel_key_targets(&config))? {
        info!("completed interrupted Travel private-key password rotation");
    }
    let (management_key, business_key) = load_runtime_private_keys(&config)?;
    let statistics_signer = statistics_signing_key(&management_key)?;
    let statistics_certificate_pem = fs::read_to_string(&config.management_cert)
        .context("failed to read Travel statistics signing certificate")?;
    let statistics = LocalStatistics::new(state_store.clone());
    let tls = Arc::new(TlsMaterial {
        management_connector: identity_client_connector_with_private_key(
            &config.management_cert,
            management_key,
            &config.management_ca,
        )?,
        business_connector: identity_client_connector_with_private_key(
            &config.business_cert,
            business_key,
            &config.business_ca,
        )?,
    });
    let permits = Arc::new(Semaphore::new(config.max_active_flows));
    let carrier_permits = Arc::new(Semaphore::new(config.max_active_carriers));
    let state = AppState {
        config: Arc::new(config),
        session_id: Uuid::new_v4(),
        tls,
        catalog: Arc::new(RwLock::new(
            cached_control_snapshot
                .as_ref()
                .map_or_else(Catalog::default, |snapshot| {
                    snapshot.payload.catalog.clone()
                }),
        )),
        directory: Arc::new(RwLock::new(
            cached_control_snapshot
                .as_ref()
                .map_or_else(RelayDirectory::default, |snapshot| {
                    snapshot.payload.relay_directory.clone()
                }),
        )),
        started: Instant::now(),
        active_flows: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        permits,
        carrier_permits,
        flow_relays: Arc::new(Mutex::new(HashMap::new())),
        key_operation: Arc::new(Mutex::new(())),
        sensitive_operation: Arc::new(Semaphore::new(1)),
        deployment_root_public_key: Arc::new(deployment_root_public_key),
        deployment_trust: Arc::new(RwLock::new(deployment_trust)),
        management_spki_sha256: Arc::new(management_identity.spki_sha256),
        state_store: Arc::new(state_store),
        statistics: Arc::new(statistics),
        statistics_signer: Arc::new(statistics_signer),
        statistics_certificate_pem: Arc::new(statistics_certificate_pem),
        relay_history: Arc::new(RwLock::new(relay_history)),
        control_trust_state: Arc::new(Mutex::new(control_trust_state)),
    };

    let mut tasks = JoinSet::new();
    tasks.spawn(run_catalog_subscription(state.clone()));
    tasks.spawn(run_ui(state.clone()));
    tasks.spawn(monitor_trust_expiry(state.clone()));
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

async fn monitor_trust_expiry(state: AppState) -> Result<()> {
    loop {
        if unix_time_secs()? >= state.deployment_trust.read().await.not_after_unix_secs {
            bail!("deployment trust expired; refusing to continue");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn run_command(command: Command) -> Result<()> {
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
                &args.enrollment_dir,
                unix_time_secs()?,
            )?;
            println!(
                "created Travel enrollment request {} ({})",
                request.request_id,
                args.enrollment_dir.display()
            );
            Ok(())
        }
        Command::EnrollImport(args) => {
            let deployment_root_public_key = embedded_deployment_root_public_key()?;
            let password = if let Some(path) = args.test_password_file.as_deref() {
                test_password(path)?
            } else {
                Zeroizing::new(rpassword::prompt_password("Travel private-key password: ")?)
            };
            if password.is_empty() {
                bail!("private-key password must not be empty");
            }
            let response: TravelEnrollmentResponse = load_json(&args.response)?;
            let trust = response
                .deployment_trust
                .verify(deployment_root_public_key, unix_time_secs()?)?;
            let control_trust_state_path = args.enrollment_dir.join(CONTROL_TRUST_STATE_FILE);
            let mut control_trust_state = if control_trust_state_path.exists() {
                flowsplice_core::authorization::load_json(&control_trust_state_path)?
            } else {
                ControlTrustState::new()
            };
            control_trust_state.validate_shape()?;
            if control_trust_state
                .deployment_id
                .as_deref()
                .is_some_and(|deployment_id| deployment_id != trust.deployment_id)
            {
                bail!("Enrollment Response belongs to a different deployment");
            }
            if trust.generation < control_trust_state.trust_generation {
                bail!("Enrollment Response would roll back deployment trust");
            }
            let trust_digest_sha256 = response.deployment_trust.payload_digest_sha256()?;
            if trust.generation == control_trust_state.trust_generation
                && control_trust_state.trust_digest_sha256.is_some()
                && control_trust_state.trust_digest_sha256.as_deref() != Some(&trust_digest_sha256)
            {
                bail!("Enrollment Response conflicts with installed deployment trust");
            }
            let credential = install_enrollment_response(
                &args.enrollment_dir,
                &response,
                deployment_root_public_key,
                password.as_bytes(),
                unix_time_secs()?,
            )?;
            control_trust_state.deployment_id = Some(trust.deployment_id);
            control_trust_state.trust_generation = trust.generation;
            control_trust_state.trust_digest_sha256 = Some(trust_digest_sha256);
            flowsplice_core::authorization::store_json_atomic(
                &control_trust_state_path,
                &control_trust_state,
            )?;
            println!(
                "installed Travel credential {} for {}",
                credential.credential_id, credential.travel_id
            );
            Ok(())
        }
        Command::EnrollRemote(args) => run_remote_enrollment(args).await,
    }
}

fn embedded_deployment_root_public_key() -> Result<&'static str> {
    option_env!("FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("this Travel binary has no embedded deployment root public key"))
}

fn embedded_management_ca_certificate() -> Result<&'static str> {
    option_env!("FLOWSPLICE_MANAGEMENT_CA_CERTIFICATE_PEM")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("this Travel binary has no embedded bootstrap management CA"))
}

fn embedded_bootstrap_relays() -> Vec<String> {
    option_env!("FLOWSPLICE_BOOTSTRAP_RELAYS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

#[allow(clippy::too_many_lines)]
async fn run_remote_enrollment(args: EnrollRemoteArgs) -> Result<()> {
    if args.wait_timeout_secs == 0 {
        bail!("wait-timeout-secs must be positive");
    }
    let _: SocketAddr = args
        .ui_listen
        .parse()
        .context("invalid Travel UI listener")?;
    let root_public_key = embedded_deployment_root_public_key()?;
    let management_ca = embedded_management_ca_certificate()?;
    let mut bootstrap_relays = if args.bootstrap_relays.is_empty() {
        embedded_bootstrap_relays()
    } else {
        args.bootstrap_relays.clone()
    };
    bootstrap_relays.sort();
    bootstrap_relays.dedup();
    if bootstrap_relays.is_empty() {
        bail!("no bootstrap Relay is embedded or provided");
    }
    for relay in &bootstrap_relays {
        if !valid_connect_address(relay) {
            bail!("invalid bootstrap Relay address {relay}");
        }
    }

    if args.install_dir.exists() && !args.install_dir.is_dir() {
        bail!(
            "Travel install path is not a directory: {}",
            args.install_dir.display()
        );
    }
    fs::create_dir_all(&args.install_dir).with_context(|| {
        format!(
            "failed to create Travel install directory {}",
            args.install_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&args.install_dir, fs::Permissions::from_mode(0o700))?;
    }
    let install_root = args
        .install_dir
        .canonicalize()
        .context("failed to resolve Travel install directory")?;
    let enrollment_dir = install_root.join("cert");
    let bootstrap_state_path = install_root.join("bootstrap-enrollment.json");
    let config_path = install_root.join("travelagent.toml");
    if config_path.exists() {
        bail!(
            "Travel configuration already exists: {}",
            config_path.display()
        );
    }
    let password = if let Some(path) = args.test_password_file.as_deref() {
        test_password(path)?
    } else {
        prompt_new_private_key_password()?
    };
    if password.chars().count() < MIN_PRIVATE_KEY_PASSWORD_CHARACTERS {
        bail!(
            "private-key password must contain at least {MIN_PRIVATE_KEY_PASSWORD_CHARACTERS} characters"
        );
    }

    let (request, retrieval_token) = if bootstrap_state_path.exists() {
        let state: BootstrapEnrollmentState = load_json(&bootstrap_state_path)?;
        if state.version != REMOTE_ENROLLMENT_VERSION || state.home_id != args.home_id {
            bail!("existing first-enrollment state conflicts with the requested Home");
        }
        let request: TravelEnrollmentRequest = load_json(&enrollment_dir.join(REQUEST_FILE))?;
        if request.request_id != state.request_id || request.travel_id != args.travel_id {
            bail!("existing first-enrollment state conflicts with the requested Travel id");
        }
        let token = hex::decode(&state.retrieval_token_hex)
            .context("first-enrollment retrieval token is invalid")?;
        if token.len() != 32 {
            bail!("first-enrollment retrieval token has an invalid length");
        }
        (request, token)
    } else {
        if enrollment_dir.exists() {
            bail!(
                "enrollment directory exists without resumable bootstrap state: {}",
                enrollment_dir.display()
            );
        }
        let request = create_enrollment_request(
            &args.travel_id,
            password.as_bytes(),
            &enrollment_dir,
            unix_time_secs()?,
        )?;
        let mut token = vec![0_u8; 32];
        SystemRandom::new()
            .fill(&mut token)
            .map_err(|_| anyhow!("failed to generate first-enrollment retrieval token"))?;
        flowsplice_enrollment::write_json_private(
            &bootstrap_state_path,
            &BootstrapEnrollmentState {
                version: REMOTE_ENROLLMENT_VERSION,
                home_id: args.home_id.clone(),
                request_id: request.request_id,
                retrieval_token_hex: hex::encode(&token),
            },
        )?;
        (request, token)
    };
    let request_json = serde_json::to_vec(&request)?;
    let verification_code = bootstrap_verification_code(&request_json, &retrieval_token);
    println!("first enrollment request: {}", request.request_id);
    println!("Home verification code: {verification_code}");
    println!(
        "Open the local Home page, compare this code, select scope/validity, and approve with the Home issuance password."
    );

    let connector = identity_server_auth_connector_from_ca_pem(management_ca)?;
    let deadline = Instant::now() + Duration::from_secs(args.wait_timeout_secs);
    let mut last_error = None;
    let (response, mut seed_relays) = 'outer: loop {
        for relay in &bootstrap_relays {
            match poll_bootstrap_relay(
                relay,
                &connector,
                &request,
                &args.home_id,
                &retrieval_token,
                &request_json,
            )
            .await
            {
                Ok(Some(result)) => break 'outer result,
                Ok(None) => {
                    last_error = None;
                }
                Err(error) => {
                    last_error = Some(format!("{relay}: {error}"));
                }
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for Home approval{}",
                last_error
                    .as_deref()
                    .map_or_else(String::new, |error| format!(": {error}"))
            );
        }
        sleep(Duration::from_secs(2)).await;
    };

    let (credential, trust) =
        validate_enrollment_response(&response, root_public_key, unix_time_secs()?)?;
    if trust.management_ca_certificate_pem.trim() != management_ca.trim() {
        info!("deployment trust rotated the bootstrap management CA during enrollment");
    }
    let installed = install_enrollment_response(
        &enrollment_dir,
        &response,
        root_public_key,
        password.as_bytes(),
        unix_time_secs()?,
    )?;
    if installed != credential {
        bail!("installed first-enrollment credential changed after verification");
    }
    seed_relays.retain(|relay| valid_connect_address(relay));
    if seed_relays.is_empty() {
        seed_relays = bootstrap_relays;
    }
    seed_relays.sort();
    seed_relays.dedup();

    let state_store_path = install_root.join("state/travel-state.redb");
    let generated = InstalledTravelConfig {
        id: args.travel_id.clone(),
        management_cert: enrollment_dir.join(MANAGEMENT_CERT_FILE),
        management_key: enrollment_dir.join(MANAGEMENT_KEY_FILE),
        management_ca: enrollment_dir.join(MANAGEMENT_CA_FILE),
        business_cert: enrollment_dir.join(BUSINESS_CERT_FILE),
        business_key: enrollment_dir.join(BUSINESS_KEY_FILE),
        business_ca: enrollment_dir.join(BUSINESS_CA_FILE),
        state_store: state_store_path.clone(),
        enrollment_work_dir: install_root.join("state/enrollment"),
        ui_listen: args.ui_listen,
        #[cfg(feature = "e2e-remote-ui")]
        test_allow_remote_listen: args.test_allow_remote_listen,
        #[cfg(feature = "e2e-remote-ui")]
        test_admin_token: args.test_admin_token,
        homes: vec![InstalledHome {
            id: args.home_id.clone(),
        }],
        seed_relays: seed_relays
            .into_iter()
            .map(|management_addr| SeedRelayOutput { management_addr })
            .collect(),
        mappings: Vec::new(),
    };
    let encoded = toml::to_string_pretty(&generated).context("failed to encode Travel config")?;
    let mut config_file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config_path)
        .with_context(|| format!("failed to create {}", config_path.display()))?;
    config_file.write_all(encoded.as_bytes())?;
    config_file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600))?;
    }
    let store = StateStore::open(&state_store_path)?;
    store.apply_immediate(WriteBatch::new().put_json(
        Table::EnrollmentOutbox,
        request.request_id.as_bytes().to_vec(),
        &RemoteEnrollmentOutboxRecord {
            version: REMOTE_ENROLLMENT_VERSION,
            home_id: args.home_id,
            enrollment_dir,
            request,
            response: Some(response),
            created_at_unix_secs: unix_time_secs()?,
            last_attempt_unix_secs: None,
            restart_required: true,
            installed_credential_id: Some(credential.credential_id),
            installed_at_unix_secs: Some(unix_time_secs()?),
        },
    )?)?;
    fs::remove_file(&bootstrap_state_path).with_context(|| {
        format!(
            "failed to remove completed bootstrap state {}",
            bootstrap_state_path.display()
        )
    })?;
    println!("installed Travel credential {}", credential.credential_id);
    println!("configuration: {}", config_path.display());
    println!(
        "start: flowsplice-travelagent --config {}",
        config_path.display()
    );
    Ok(())
}

fn valid_connect_address(value: &str) -> bool {
    if value.parse::<SocketAddr>().is_ok() {
        return true;
    }
    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    !host.trim().is_empty() && port.parse::<u16>().is_ok_and(|port| port > 0)
}

async fn poll_bootstrap_relay(
    relay: &str,
    connector: &TlsConnector,
    request: &TravelEnrollmentRequest,
    home_id: &str,
    retrieval_token: &[u8],
    request_json: &[u8],
) -> Result<Option<(TravelEnrollmentResponse, Vec<String>)>> {
    let socket = timeout(Duration::from_secs(10), TcpStream::connect(relay))
        .await
        .context("bootstrap Relay TCP connection timed out")??;
    socket.set_nodelay(true)?;
    let mut stream = timeout(
        Duration::from_secs(10),
        connector.connect(identity_server_name()?, socket),
    )
    .await
    .context("bootstrap Relay TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Relay, None, &[])?;
    write_json(
        &mut stream,
        &ControlMessage::BootstrapEnrollmentSubmit {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            request_id: request.request_id,
            travel_id: request.travel_id.clone(),
            home_id: home_id.to_owned(),
            retrieval_token: retrieval_token.to_vec(),
            request_json: request_json.to_vec(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    let result = JsonFrameReader::new(&mut stream, CONTROL_FRAME_LIMIT)
        .read_with_timeout::<ControlMessage>(Duration::from_secs(20))
        .await?;
    let ControlMessage::BootstrapEnrollmentResult {
        request_id,
        accepted,
        response_json,
        seed_relays,
        error,
    } = result
    else {
        bail!("bootstrap Relay returned an unexpected response");
    };
    if request_id != request.request_id {
        bail!("bootstrap Relay returned the wrong request id");
    }
    if !accepted {
        bail!(
            "first enrollment was rejected: {}",
            error.unwrap_or_else(|| "unspecified error".to_owned())
        );
    }
    response_json
        .map(|bytes| {
            serde_json::from_slice(&bytes)
                .context("first enrollment response is invalid")
                .map(|response| (response, seed_relays))
        })
        .transpose()
}

fn enrollment_sibling(certificate: &Path, file_name: &str) -> PathBuf {
    certificate
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(file_name)
}

fn apply_active_identity_directory(config: &mut Config, state_store: &StateStore) -> Result<()> {
    let Some(bytes) = state_store.get(Table::Metadata, ACTIVE_IDENTITY_DIR_KEY)? else {
        return Ok(());
    };
    let directory = PathBuf::from(
        String::from_utf8(bytes).context("active Travel identity directory is not UTF-8")?,
    );
    if !directory.is_dir() {
        bail!(
            "active Travel identity directory {} is unavailable",
            directory.display()
        );
    }
    config.management_cert = directory.join(MANAGEMENT_CERT_FILE);
    config.management_key = directory.join(MANAGEMENT_KEY_FILE);
    config.management_ca = directory.join(MANAGEMENT_CA_FILE);
    config.business_cert = directory.join(BUSINESS_CERT_FILE);
    config.business_key = directory.join(BUSINESS_KEY_FILE);
    config.business_ca = directory.join(BUSINESS_CA_FILE);
    info!(path = %directory.display(), "selected remotely enrolled Travel identity; restart activation complete");
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn load_initial_control_trust_state(
    config: &Config,
    deployment_root_public_key: &str,
    state_store: &StateStore,
    legacy_state_path: &Path,
    management_spki_sha256: &str,
) -> Result<(
    ControlTrustState,
    DeploymentTrust,
    Option<VerifiedControlSnapshot>,
)> {
    let trust_path = enrollment_sibling(&config.management_cert, DEPLOYMENT_TRUST_FILE);
    let signed: SignedDeploymentTrust = load_json(&trust_path)?;
    let now = unix_time_secs()?;
    let trust = signed.verify(deployment_root_public_key, now)?;
    let trust_digest = signed.payload_digest_sha256()?;
    let legacy_bytes = legacy_state_path
        .exists()
        .then(|| {
            fs::read(legacy_state_path).with_context(|| {
                format!(
                    "failed to read legacy control state {}",
                    legacy_state_path.display()
                )
            })
        })
        .transpose()?;
    let legacy_digest = legacy_bytes.as_deref().map(sha256_hex);
    let stored_state = state_store.get_json(Table::TravelControlState, CONTROL_STATE_KEY)?;
    let migrating_legacy = stored_state.is_none() && legacy_bytes.is_some();
    let mut state = match stored_state {
        Some(state) => state,
        None if migrating_legacy => serde_json::from_slice(
            legacy_bytes
                .as_deref()
                .ok_or_else(|| anyhow!("legacy control state disappeared during migration"))?,
        )
        .context("legacy control state is invalid")?,
        None => ControlTrustState::new(),
    };
    state.validate_shape()?;
    if state
        .deployment_id
        .as_deref()
        .is_some_and(|deployment_id| deployment_id != trust.deployment_id)
    {
        bail!("installed deployment trust belongs to a different deployment");
    }
    if trust.generation == state.trust_generation
        && state.trust_digest_sha256.is_some()
        && state.trust_digest_sha256.as_deref() != Some(&trust_digest)
    {
        bail!("installed deployment trust conflicts with the durable trust high-water mark");
    }
    if trust.generation > state.trust_generation || state.deployment_id.is_none() {
        state.deployment_id = Some(trust.deployment_id.clone());
        state.trust_generation = trust.generation;
        state.trust_digest_sha256 = Some(trust_digest);
    }

    let snapshot_matches_high_water = |verified: &VerifiedControlSnapshot| {
        verified.trust.generation == state.trust_generation
            && verified.trust_digest_sha256
                == state.trust_digest_sha256.as_deref().unwrap_or_default()
            && verified.payload.signer_epoch == state.signer_epoch
            && verified.payload.generation == state.snapshot_generation
            && verified.digest_sha256 == state.snapshot_digest_sha256.as_deref().unwrap_or_default()
    };
    let cached = state.cached_snapshot.as_ref().and_then(|snapshot| {
        match snapshot.verify(deployment_root_public_key, now) {
            Ok(verified)
                if require_control_snapshot_subject(
                    &verified,
                    &config.id,
                    management_spki_sha256,
                )
                .is_ok()
                    && snapshot_matches_high_water(&verified) =>
            {
                Some(verified)
            }
            Ok(_) => {
                warn!("ignored cached control snapshot inconsistent with its durable high-water mark");
                None
            }
            Err(error) => {
                warn!(%error, "cached control snapshot is unusable; waiting for a fresh signed state");
                None
            }
        }
    });
    let historical_migration_snapshot = migrating_legacy
        .then_some(state.cached_snapshot.as_ref())
        .flatten()
        .and_then(|snapshot| match snapshot.verify_at_issuance_for_migration(deployment_root_public_key) {
            Ok(verified)
                if require_control_snapshot_subject(
                    &verified,
                    &config.id,
                    management_spki_sha256,
                )
                .is_ok()
                    && snapshot_matches_high_water(&verified) =>
            {
                Some(verified)
            }
            Ok(_) => {
                warn!("did not migrate Relay history from a legacy snapshot inconsistent with its high-water mark");
                None
            }
            Err(error) => {
                warn!(%error, "did not migrate Relay history from an invalid legacy snapshot");
                None
            }
        });
    let runtime_trust = cached
        .as_ref()
        .map_or(trust, |snapshot| snapshot.trust.clone());
    if let Some(snapshot) = cached.as_ref() {
        configured_homes_are_trusted(
            &config.homes,
            &runtime_trust,
            &snapshot.payload.catalog,
            now,
        )?;
    }
    let mut batch = WriteBatch::new().put_json(
        Table::TravelControlState,
        CONTROL_STATE_KEY.to_vec(),
        &state,
    )?;
    if let Some(snapshot) = cached.as_ref().or(historical_migration_snapshot.as_ref()) {
        for record in relay_history_records(snapshot, now) {
            batch = batch.put_json(
                Table::RelayHistory,
                record.relay_id.as_bytes().to_vec(),
                &record,
            )?;
        }
    }
    if migrating_legacy {
        batch = batch.put(
            Table::Metadata,
            LEGACY_CONTROL_STATE_DIGEST_KEY.to_vec(),
            legacy_digest
                .as_deref()
                .ok_or_else(|| anyhow!("legacy control state digest is missing"))?
                .as_bytes()
                .to_vec(),
        );
    }
    state_store.apply_immediate(batch)?;
    let stored: ControlTrustState = state_store
        .get_json(Table::TravelControlState, CONTROL_STATE_KEY)?
        .ok_or_else(|| anyhow!("redb control state vanished after durable commit"))?;
    if stored != state {
        bail!("redb control state readback differs from committed value");
    }
    let recorded_legacy_digest =
        state_store.get(Table::Metadata, LEGACY_CONTROL_STATE_DIGEST_KEY)?;
    let legacy_is_committed = legacy_digest
        .as_deref()
        .is_some_and(|digest| recorded_legacy_digest.as_deref() == Some(digest.as_bytes()));
    if legacy_is_committed {
        fs::remove_file(legacy_state_path).with_context(|| {
            format!(
                "failed to remove migrated legacy control state {}",
                legacy_state_path.display()
            )
        })?;
        info!(path = %legacy_state_path.display(), "removed committed legacy Travel control state after redb migration");
    } else if legacy_bytes.is_some() {
        warn!(path = %legacy_state_path.display(), "left an unrecognized legacy Travel control state file untouched");
    }
    Ok((state, runtime_trust, cached))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, bytes).as_ref())
}

fn load_relay_history(store: &StateStore) -> Result<Vec<RelayHistoryRecord>> {
    let mut records = Vec::new();
    for (key, value) in store.scan_prefix(Table::RelayHistory, b"")? {
        let decoded = serde_json::from_slice::<RelayHistoryRecord>(&value);
        match decoded {
            Ok(record) if valid_relay_history_record(&key, &record) => {
                records.push(record);
            }
            Ok(_) | Err(_) => {
                warn!(?key, "ignored malformed optional Relay history record");
            }
        }
    }
    let mut address_owners = HashMap::<String, (String, String)>::new();
    let mut conflicting_addresses = HashSet::new();
    for record in &records {
        let identity = (
            record.relay_id.clone(),
            record.management_spki_sha256.clone(),
        );
        if address_owners
            .insert(record.management_addr.clone(), identity.clone())
            .is_some_and(|existing| existing != identity)
        {
            conflicting_addresses.insert(record.management_addr.clone());
        }
    }
    records.retain(|record| !conflicting_addresses.contains(&record.management_addr));
    Ok(records)
}

fn valid_relay_history_record(key: &[u8], record: &RelayHistoryRecord) -> bool {
    let valid_address = record
        .management_addr
        .rsplit_once(':')
        .is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|port| port > 0)
        });
    record.version == RELAY_HISTORY_VERSION
        && key == record.relay_id.as_bytes()
        && !record.deployment_id.is_empty()
        && !record.relay_id.is_empty()
        && valid_address
        && record.management_spki_sha256.len() == 64
        && record
            .management_spki_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
        && record.first_seen_unix_secs <= record.last_seen_unix_secs
        && record.last_directory_generation > 0
}

fn relay_history_records(snapshot: &VerifiedControlSnapshot, now: u64) -> Vec<RelayHistoryRecord> {
    snapshot
        .payload
        .relay_directory
        .relays
        .iter()
        .map(|relay| RelayHistoryRecord {
            version: RELAY_HISTORY_VERSION,
            deployment_id: snapshot.trust.deployment_id.clone(),
            relay_id: relay.id.clone(),
            management_addr: relay.management_addr.clone(),
            management_spki_sha256: relay.management_spki_sha256.clone(),
            first_seen_unix_secs: now,
            last_seen_unix_secs: now,
            last_directory_generation: snapshot.payload.relay_directory.generation,
            last_success_unix_secs: None,
            last_failure_unix_secs: None,
            consecutive_failures: 0,
            current_member: true,
            operator_disabled: false,
        })
        .collect()
}

fn local_certificate_identity(path: &Path) -> Result<flowsplice_core::tls::PeerIdentity> {
    let certificates = CertificateDer::pem_file_iter(path)
        .with_context(|| format!("failed to open certificate {}", path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse certificate {}", path.display()))?;
    peer_identity(Some(&certificates))
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

fn travel_key_targets(config: &Config) -> [PrivateKeyRotationTarget<'_>; 2] {
    [
        PrivateKeyRotationTarget {
            label: "Travel management",
            path: &config.management_key,
        },
        PrivateKeyRotationTarget {
            label: "Travel business",
            path: &config.business_key,
        },
    ]
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
    let home_ids = configured_home_ids(&config.homes)?;
    if config.state_store.as_os_str().is_empty() {
        bail!("state_store must be non-empty");
    }
    if config.enrollment_work_dir.as_os_str().is_empty() {
        bail!("enrollment_work_dir must be non-empty");
    }
    if config.seed_relays.is_empty() {
        bail!("at least one seed relay is required");
    }
    let mut seed_relays = HashSet::new();
    for relay in &config.seed_relays {
        if relay.management_addr.is_empty() || !seed_relays.insert(&relay.management_addr) {
            bail!("seed Relay addresses must be non-empty and unique");
        }
    }
    if config.carrier_heartbeat_secs == 0
        || config.carrier_timeout_secs <= config.carrier_heartbeat_secs
        || config.carrier_race_timeout_secs == 0
        || config.carrier_recovery_timeout_secs <= config.carrier_race_timeout_secs
        || config.carrier_reevaluate_secs == 0
        || config.max_carrier_reevaluate_secs < config.carrier_reevaluate_secs
        || config.max_unacked_bytes < MAX_DATA_PAYLOAD
        || config.max_unacked_bytes > u32::MAX as usize
        || config.max_active_flows == 0
        || config.max_active_carriers == 0
        || config.max_carriers_per_flow == 0
        || config.max_carriers_per_flow > config.max_active_carriers
    {
        bail!("carrier timeout, reevaluation, or unacknowledged-data limits are invalid");
    }
    let ui_addr: SocketAddr = config.ui_listen.parse().context("invalid ui_listen")?;
    if ui_addr != SocketAddr::from(([127, 0, 0, 1], ui_addr.port()))
        && !test_remote_ui_enabled(config)
    {
        bail!("Travel UI must listen directly on 127.0.0.1");
    }
    let mut services = HashSet::new();
    let mut binds = HashSet::new();
    for mapping in &config.mappings {
        if mapping.service_id.is_empty() || !home_ids.contains(mapping.home_id.as_str()) {
            bail!("every mapping must name a configured Home and a non-empty service");
        }
        let bind: SocketAddr = mapping.bind.parse().context("invalid mapping bind")?;
        if bind.ip() != std::net::Ipv4Addr::LOCALHOST && !test_remote_ui_enabled(config) {
            bail!("Travel mapping listeners must bind directly to 127.0.0.1");
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

#[cfg(feature = "e2e-remote-ui")]
fn test_remote_ui_enabled(config: &Config) -> bool {
    config.test_allow_remote_listen
        && config
            .test_admin_token
            .as_deref()
            .is_some_and(|token| token.len() >= 32)
}

#[cfg(not(feature = "e2e-remote-ui"))]
const fn test_remote_ui_enabled(_config: &Config) -> bool {
    false
}

fn configured_home_ids(homes: &[ConfiguredHome]) -> Result<HashSet<&str>> {
    if homes.is_empty() {
        bail!("at least one Home Agent is required");
    }
    let mut home_ids = HashSet::new();
    for home in homes {
        if home.id.is_empty() || !home_ids.insert(home.id.as_str()) {
            bail!("Home ids must be non-empty and unique");
        }
    }
    Ok(home_ids)
}

async fn run_catalog_subscription(state: AppState) -> Result<()> {
    loop {
        let mut connected = false;
        for relay in bootstrap_candidates(&state).await {
            match open_management(&state, &relay, TravelConnectionPurpose::Catalog).await {
                Ok((stream, relay_id, relay_spki)) => {
                    info!(%relay_id, relay = %relay.management_addr, "catalog subscription connected");
                    connected = true;
                    if let Err(error) =
                        run_catalog_session(&state, &relay_id, &relay_spki, stream).await
                    {
                        warn!(%relay_id, %error, "catalog subscription disconnected");
                    }
                    break;
                }
                Err(error) => {
                    if let Err(storage_error) = record_relay_failure(&state, &relay).await {
                        warn!(%storage_error, "failed to persist Relay history failure");
                    }
                    warn!(relay = relay.label(), %error, "catalog subscription attempt failed");
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
    relay_id: &str,
    relay_spki: &str,
    stream: TlsStream<TcpStream>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    let mut heartbeat = interval(Duration::from_secs(10));
    let mut statistics_tick = interval(Duration::from_secs(5));
    let mut report_keys = HashMap::<String, Vec<u8>>::new();
    let mut enrollment_inflight = None::<(Uuid, Instant)>;
    let mut nonce = 0_u64;
    let mut last_received = Instant::now();
    loop {
        tokio::select! {
            message = reader.read::<ControlMessage>() => {
                last_received = Instant::now();
                match message? {
                    ControlMessage::ControlSnapshot { snapshot } => {
                        apply_control_snapshot(state, snapshot, relay_id, relay_spki).await?;
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(&mut writer, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
                    ControlMessage::StatisticsReportAck { digest_sha256, accepted, error } => {
                        if accepted {
                            if let Some(key) = report_keys.remove(&digest_sha256) {
                                let statistics = Arc::clone(&state.statistics);
                                let digest = digest_sha256.clone();
                                tokio::task::spawn_blocking(move || statistics.acknowledge_report(&key, &digest))
                                    .await
                                    .context("Travel statistics acknowledgement task failed")??;
                            }
                        } else {
                            warn!(?error, %digest_sha256, "Server rejected Travel statistics report");
                        }
                    }
                    ControlMessage::RemoteEnrollmentResult { request_id, accepted, response_json, error } => {
                        apply_remote_enrollment_result(
                            state,
                            request_id,
                            accepted,
                            response_json,
                            error,
                        )
                        .await?;
                        if enrollment_inflight.is_some_and(|(id, _)| id == request_id) {
                            enrollment_inflight = None;
                        }
                    }
                    ControlMessage::RemoteEnrollmentInstalledAck { request_id, accepted, error } => {
                        if accepted {
                            acknowledge_remote_enrollment_install(state, request_id).await?;
                        } else {
                            warn!(%request_id, ?error, "Server rejected remote enrollment install acknowledgement");
                        }
                        if enrollment_inflight.is_some_and(|(id, _)| id == request_id) {
                            enrollment_inflight = None;
                        }
                    }
                    _ => bail!("unexpected message on catalog subscription"),
                }
            }
            _ = heartbeat.tick() => {
                if last_received.elapsed() > Duration::from_secs(30) {
                    bail!("relay {relay_id} catalog heartbeat timed out");
                }
                nonce = nonce.wrapping_add(1);
                write_json(&mut writer, &ControlMessage::Heartbeat { nonce }, CONTROL_FRAME_LIMIT).await?;
            }
            _ = statistics_tick.tick() => {
                flush_and_send_travel_statistics(state, &mut writer, &mut report_keys).await?;
                if enrollment_inflight
                    .is_some_and(|(_, started)| started.elapsed() > Duration::from_secs(30))
                {
                    enrollment_inflight = None;
                }
                if enrollment_inflight.is_none()
                    && let Some((request_id, message)) = next_remote_enrollment_message(state).await?
                {
                    write_json(&mut writer, &message, CONTROL_FRAME_LIMIT).await?;
                    enrollment_inflight = Some((request_id, Instant::now()));
                }
            }
        }
    }
}

async fn next_remote_enrollment_message(
    state: &AppState,
) -> Result<Option<(Uuid, ControlMessage)>> {
    let store = Arc::clone(&state.state_store);
    let travel_id = state.config.id.clone();
    let travel_session_id = state.session_id;
    let management_spki_sha256 = Arc::clone(&state.management_spki_sha256);
    tokio::task::spawn_blocking(move || {
        let now = unix_time_secs()?;
        prune_remote_enrollment_outbox(&store, now)?;
        for (key, value) in store.scan_prefix(Table::EnrollmentOutbox, b"")? {
            let Ok(mut record) = serde_json::from_slice::<RemoteEnrollmentOutboxRecord>(&value)
            else {
                warn!(?key, "ignored malformed remote enrollment outbox record");
                continue;
            };
            if record.version != REMOTE_ENROLLMENT_VERSION
                || record
                    .last_attempt_unix_secs
                    .is_some_and(|attempt| now.saturating_sub(attempt) < 10)
            {
                continue;
            }
            let request_id = record.request.request_id;
            let message = if let (Some(response), Some(credential_id)) =
                (record.response.as_ref(), record.installed_credential_id)
            {
                let installed_identity =
                    identity_from_certificate_pem(&response.management_certificate_pem)?;
                if installed_identity.spki_sha256 != *management_spki_sha256 {
                    continue;
                }
                ControlMessage::RemoteEnrollmentInstalled {
                    request_id,
                    travel_id: travel_id.clone(),
                    travel_session_id,
                    credential_id,
                    home_id: record.home_id.clone(),
                }
            } else if record.response.is_none() && !record.restart_required {
                ControlMessage::TravelEnrollmentSubmit {
                    request_id,
                    travel_id: travel_id.clone(),
                    travel_session_id,
                    home_id: record.home_id.clone(),
                    request_json: serde_json::to_vec(&record.request)?,
                }
            } else {
                continue;
            };
            record.last_attempt_unix_secs = Some(now);
            store.apply_immediate(WriteBatch::new().put_json(
                Table::EnrollmentOutbox,
                key,
                &record,
            )?)?;
            return Ok(Some((request_id, message)));
        }
        Ok(None)
    })
    .await
    .context("Travel enrollment outbox query task failed")?
}

fn prune_remote_enrollment_outbox(store: &StateStore, now: u64) -> Result<()> {
    let mut batch = WriteBatch::new();
    for (key, value) in store.scan_prefix(Table::EnrollmentOutbox, b"")? {
        let Ok(record) = serde_json::from_slice::<RemoteEnrollmentOutboxRecord>(&value) else {
            continue;
        };
        let expired = remote_enrollment_outbox_expired(
            record.created_at_unix_secs,
            record.installed_at_unix_secs,
            now,
        );
        if expired {
            batch = batch.delete(Table::EnrollmentOutbox, key);
        }
    }
    if batch.is_empty() {
        Ok(())
    } else {
        store.apply_immediate(batch)
    }
}

fn remote_enrollment_outbox_expired(
    created_at_unix_secs: u64,
    installed_at_unix_secs: Option<u64>,
    now: u64,
) -> bool {
    installed_at_unix_secs.map_or_else(
        || now.saturating_sub(created_at_unix_secs) >= MAX_REQUEST_AGE_SECS,
        |installed_at| {
            now.saturating_sub(installed_at) >= REMOTE_ENROLLMENT_INSTALLED_RETENTION_SECS
        },
    )
}

async fn acknowledge_remote_enrollment_install(state: &AppState, request_id: Uuid) -> Result<()> {
    let store = Arc::clone(&state.state_store);
    tokio::task::spawn_blocking(move || {
        let record = store
            .get_json::<RemoteEnrollmentOutboxRecord>(
                Table::EnrollmentOutbox,
                request_id.as_bytes(),
            )?
            .ok_or_else(|| anyhow!("unknown remote enrollment install acknowledgement"))?;
        if record.installed_credential_id.is_none() {
            bail!("remote enrollment was not installed");
        }
        store.apply_immediate(
            WriteBatch::new().delete(Table::EnrollmentOutbox, request_id.as_bytes().to_vec()),
        )
    })
    .await
    .context("Travel enrollment install acknowledgement task failed")??;
    info!(%request_id, "Home acknowledged installed remote enrollment");
    Ok(())
}

async fn apply_remote_enrollment_result(
    state: &AppState,
    request_id: Uuid,
    accepted: bool,
    response_json: Option<Vec<u8>>,
    error: Option<String>,
) -> Result<()> {
    if !accepted {
        warn!(%request_id, ?error, "remote enrollment submission was rejected");
        return Ok(());
    }
    let Some(response_json) = response_json else {
        info!(%request_id, "remote enrollment request is awaiting Home approval");
        return Ok(());
    };
    let response: TravelEnrollmentResponse =
        serde_json::from_slice(&response_json).context("remote enrollment response is invalid")?;
    if response.approval.request.request_id != request_id {
        bail!("remote enrollment response has the wrong request id");
    }
    validate_enrollment_response(
        &response,
        &state.deployment_root_public_key,
        unix_time_secs()?,
    )?;
    let store = Arc::clone(&state.state_store);
    tokio::task::spawn_blocking(move || {
        let mut record = store
            .get_json::<RemoteEnrollmentOutboxRecord>(
                Table::EnrollmentOutbox,
                request_id.as_bytes(),
            )?
            .ok_or_else(|| anyhow!("remote enrollment response has no local request"))?;
        if record.request != response.approval.request {
            bail!("remote enrollment response does not match the local request");
        }
        if let Some(existing) = record.response.as_ref() {
            if existing == &response {
                return Ok(());
            }
            bail!("conflicting remote enrollment response for the same request");
        }
        record.response = Some(response);
        store.apply_immediate(WriteBatch::new().put_json(
            Table::EnrollmentOutbox,
            request_id.as_bytes().to_vec(),
            &record,
        )?)
    })
    .await
    .context("Travel enrollment response commit task failed")??;
    info!(%request_id, "received and verified remote enrollment response");
    Ok(())
}

async fn flush_and_send_travel_statistics<W: tokio::io::AsyncWrite + Unpin>(
    state: &AppState,
    writer: &mut W,
    report_keys: &mut HashMap<String, Vec<u8>>,
) -> Result<()> {
    let statistics = Arc::clone(&state.statistics);
    let deployment_id = state.deployment_trust.read().await.deployment_id.clone();
    let reporter_id = state.config.id.clone();
    let certificate_pem = Arc::clone(&state.statistics_certificate_pem);
    let signer = Arc::clone(&state.statistics_signer);
    tokio::task::spawn_blocking(move || {
        statistics.flush_and_stage(
            &deployment_id,
            Role::Travel,
            &reporter_id,
            &certificate_pem,
            &signer,
        )
    })
    .await
    .context("Travel statistics flush task failed")??;
    let statistics = Arc::clone(&state.statistics);
    let reports = tokio::task::spawn_blocking(move || statistics.pending_reports(16))
        .await
        .context("Travel statistics outbox task failed")??;
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

async fn open_management(
    state: &AppState,
    relay: &RelayCandidate,
    purpose: TravelConnectionPurpose,
) -> Result<(TlsStream<TcpStream>, String, String)> {
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
            .connect(identity_server_name()?, socket),
    )
    .await
    .context("relay TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    let allowed_spki = relay.management_spki_sha256.as_slice();
    require_peer(
        &identity,
        Role::Relay,
        relay.expected_id.as_deref(),
        allowed_spki,
    )?;
    write_json(
        &mut stream,
        &ControlMessage::TravelHello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
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
        ControlMessage::TravelHelloAccepted { relay_id } if relay_id == identity.id => {}
        ControlMessage::TravelHelloDenied { reason } => {
            bail!("Travel session rejected by Relay {}: {reason}", identity.id);
        }
        _ => bail!("relay sent an invalid Travel HELLO response"),
    }
    let ControlMessage::ControlSnapshot { snapshot } = reader
        .read_with_timeout::<ControlMessage>(setup_timeout)
        .await?
    else {
        bail!("relay did not send an initial signed control snapshot");
    };
    drop(reader);
    apply_control_snapshot(state, snapshot, &identity.id, &identity.spki_sha256).await?;
    if let Err(error) = record_relay_success(state, &identity.id).await {
        warn!(%error, relay_id = %identity.id, "failed to persist Relay history success");
    }
    Ok((stream, identity.id, identity.spki_sha256))
}

async fn request_route(
    state: &AppState,
    relay: &RelayCandidate,
    home_id: &str,
) -> Result<(RouteGrant, String)> {
    let config = &state.config;
    let (stream, relay_id, relay_spki) =
        open_management(state, relay, TravelConnectionPurpose::Route).await?;
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
                    return Ok((
                        RouteGrant {
                            route_id,
                            route_secret,
                            data_addr,
                        },
                        relay_id,
                    ));
                }
                ControlMessage::RouteDenied {
                    request_id: response_id,
                    reason,
                } if response_id == request_id => bail!("route denied: {reason}"),
                ControlMessage::ControlSnapshot { snapshot } => {
                    apply_control_snapshot(state, snapshot, &relay_id, &relay_spki).await?;
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
    relay: &RelayCandidate,
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
    let home_spki_pins = {
        let trust = state.deployment_trust.read().await;
        let catalog = state.catalog.read().await;
        let endpoint_credential = catalog
            .homes
            .iter()
            .find(|candidate| candidate.home_id == home.id)
            .and_then(|home| home.endpoint_credential.as_ref());
        trusted_home_business_pins(&trust, &home.id, endpoint_credential, unix_time_secs()?)?
    };
    let (grant, relay_id) = request_route(state, relay, home_id).await?;
    let mut socket = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        TcpStream::connect(&grant.data_addr),
    )
    .await
    .context("relay data connection timed out")??;
    socket.set_nodelay(true)?;
    timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        write_preface(
            &mut socket,
            RouteSide::Travel,
            grant.route_id,
            &grant.route_secret,
        ),
    )
    .await
    .context("relay data preface timed out")??;
    let mut stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        state
            .tls
            .business_connector
            .connect(identity_server_name()?, socket),
    )
    .await
    .context("business TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Home, Some(&home.id), &home_spki_pins)?;
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
            relay_id,
            stream,
            home_receive_offset: receive_offset,
            home_send_offset: send_offset,
        }),
        DataFrame::OpenError { reason, .. } => bail!("home rejected carrier: {reason}"),
        _ => bail!("invalid carrier OPEN response"),
    }
}

async fn relay_candidates(state: &AppState) -> Vec<RelayCandidate> {
    let directory = state.directory.read().await.clone();
    if directory.relays.is_empty() {
        return bootstrap_candidates(state).await;
    }
    signed_directory_candidates(directory)
}

fn signed_directory_candidates(directory: RelayDirectory) -> Vec<RelayCandidate> {
    directory
        .relays
        .into_iter()
        .map(|relay| RelayCandidate {
            expected_id: Some(relay.id),
            management_addr: relay.management_addr,
            management_spki_sha256: Some(relay.management_spki_sha256),
        })
        .collect()
}

async fn bootstrap_candidates(state: &AppState) -> Vec<RelayCandidate> {
    let directory = state.directory.read().await.clone();
    let deployment_id = state.deployment_trust.read().await.deployment_id.clone();
    let history = state.relay_history.read().await.clone();
    bootstrap_candidate_pool(
        directory,
        &deployment_id,
        history,
        &state.config.seed_relays,
    )
}

fn bootstrap_candidate_pool(
    directory: RelayDirectory,
    deployment_id: &str,
    mut history: Vec<RelayHistoryRecord>,
    seeds: &[SeedRelay],
) -> Vec<RelayCandidate> {
    history.sort_by(|left, right| {
        right
            .current_member
            .cmp(&left.current_member)
            .then_with(|| {
                right
                    .last_success_unix_secs
                    .cmp(&left.last_success_unix_secs)
            })
            .then_with(|| right.last_seen_unix_secs.cmp(&left.last_seen_unix_secs))
    });
    let mut source = directory
        .relays
        .into_iter()
        .map(|relay| RelayCandidate {
            expected_id: Some(relay.id),
            management_addr: relay.management_addr,
            management_spki_sha256: Some(relay.management_spki_sha256),
        })
        .collect::<Vec<_>>();
    source.extend(
        history
            .into_iter()
            .filter(|relay| relay.deployment_id == deployment_id && !relay.operator_disabled)
            .map(|relay| RelayCandidate {
                expected_id: Some(relay.relay_id),
                management_addr: relay.management_addr,
                management_spki_sha256: Some(relay.management_spki_sha256),
            }),
    );
    source.extend(seeds.iter().map(|relay| RelayCandidate {
        expected_id: None,
        management_addr: relay.management_addr.clone(),
        management_spki_sha256: None,
    }));
    let mut seen = HashSet::new();
    source
        .into_iter()
        .filter(|relay| {
            seen.insert(
                relay
                    .expected_id
                    .clone()
                    .unwrap_or_else(|| relay.management_addr.clone()),
            )
        })
        .collect()
}

async fn apply_control_snapshot(
    state: &AppState,
    snapshot: SignedControlSnapshot,
    authenticated_relay_id: &str,
    authenticated_relay_spki: &str,
) -> Result<()> {
    let verified = snapshot.verify(&state.deployment_root_public_key, unix_time_secs()?)?;
    require_control_snapshot_subject(&verified, &state.config.id, &state.management_spki_sha256)?;
    configured_homes_are_trusted(
        &state.config.homes,
        &verified.trust,
        &verified.payload.catalog,
        unix_time_secs()?,
    )?;
    require_authenticated_relay_in_snapshot(
        &verified,
        authenticated_relay_id,
        authenticated_relay_spki,
    )?;
    let mut acceptance = state.control_trust_state.lock().await;
    let mut proposed = acceptance.clone();
    let changed = proposed.accept(&verified)?;
    if changed {
        proposed.cached_snapshot = Some(snapshot);
        let now = unix_time_secs()?;
        let mut history = state.relay_history.write().await;
        let mut proposed_history = history.clone();
        for record in &mut proposed_history {
            record.current_member = false;
        }
        for endpoint in &verified.payload.relay_directory.relays {
            if let Some(record) = proposed_history
                .iter_mut()
                .find(|record| record.relay_id == endpoint.id)
            {
                record.management_addr.clone_from(&endpoint.management_addr);
                record
                    .management_spki_sha256
                    .clone_from(&endpoint.management_spki_sha256);
                record.last_seen_unix_secs = now;
                record.last_directory_generation = verified.payload.relay_directory.generation;
                record.current_member = true;
                record.operator_disabled = false;
            } else {
                proposed_history.push(RelayHistoryRecord {
                    version: RELAY_HISTORY_VERSION,
                    deployment_id: verified.trust.deployment_id.clone(),
                    relay_id: endpoint.id.clone(),
                    management_addr: endpoint.management_addr.clone(),
                    management_spki_sha256: endpoint.management_spki_sha256.clone(),
                    first_seen_unix_secs: now,
                    last_seen_unix_secs: now,
                    last_directory_generation: verified.payload.relay_directory.generation,
                    last_success_unix_secs: None,
                    last_failure_unix_secs: None,
                    consecutive_failures: 0,
                    current_member: true,
                    operator_disabled: false,
                });
            }
        }
        let mut batch = WriteBatch::new().put_json(
            Table::TravelControlState,
            CONTROL_STATE_KEY.to_vec(),
            &proposed,
        )?;
        for record in &proposed_history {
            batch = batch.put_json(
                Table::RelayHistory,
                record.relay_id.as_bytes().to_vec(),
                record,
            )?;
        }
        let store = Arc::clone(&state.state_store);
        tokio::task::spawn_blocking(move || store.apply_immediate(batch))
            .await
            .context("Travel redb commit task failed")??;
        *acceptance = proposed;
        *history = proposed_history;
    }
    drop(acceptance);
    if !changed {
        return Ok(());
    }
    *state.deployment_trust.write().await = verified.trust;
    let directory = verified.payload.relay_directory;
    let catalog = verified.payload.catalog;
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
        "travel updated authenticated Relay directory"
    );
    *state.directory.write().await = directory;
    *state.catalog.write().await = catalog;
    Ok(())
}

async fn record_relay_success(state: &AppState, relay_id: &str) -> Result<()> {
    let now = unix_time_secs()?;
    let mut history = state.relay_history.write().await;
    let Some(record) = history
        .iter_mut()
        .find(|record| record.relay_id == relay_id)
    else {
        return Ok(());
    };
    record.last_success_unix_secs = Some(now);
    record.consecutive_failures = 0;
    let record = record.clone();
    let batch = WriteBatch::new().put_json(
        Table::RelayHistory,
        record.relay_id.as_bytes().to_vec(),
        &record,
    )?;
    let store = Arc::clone(&state.state_store);
    tokio::task::spawn_blocking(move || store.apply_immediate(batch))
        .await
        .context("Relay success history commit task failed")??;
    Ok(())
}

async fn record_relay_failure(state: &AppState, relay: &RelayCandidate) -> Result<()> {
    let now = unix_time_secs()?;
    let mut history = state.relay_history.write().await;
    let Some(record) = history.iter_mut().find(|record| {
        relay
            .expected_id
            .as_ref()
            .map_or(record.management_addr == relay.management_addr, |id| {
                record.relay_id == *id
            })
    }) else {
        return Ok(());
    };
    record.last_failure_unix_secs = Some(now);
    record.consecutive_failures = record.consecutive_failures.saturating_add(1);
    let record = record.clone();
    let batch = WriteBatch::new().put_json(
        Table::RelayHistory,
        record.relay_id.as_bytes().to_vec(),
        &record,
    )?;
    let store = Arc::clone(&state.state_store);
    tokio::task::spawn_blocking(move || store.apply_immediate(batch))
        .await
        .context("Relay failure history commit task failed")??;
    Ok(())
}

fn require_control_snapshot_subject(
    verified: &VerifiedControlSnapshot,
    travel_id: &str,
    management_spki_sha256: &str,
) -> Result<()> {
    if verified.payload.travel_id != travel_id
        || !verified
            .payload
            .travel_management_spki_sha256
            .eq_ignore_ascii_case(management_spki_sha256)
    {
        bail!("control snapshot is bound to a different Travel identity");
    }
    Ok(())
}

fn configured_homes_are_trusted(
    homes: &[ConfiguredHome],
    trust: &DeploymentTrust,
    catalog: &Catalog,
    now: u64,
) -> Result<()> {
    for home in homes {
        let catalog_home = catalog
            .homes
            .iter()
            .find(|candidate| candidate.home_id == home.id);
        trust.resolve_home_endpoint(
            &home.id,
            catalog_home.and_then(|home| home.endpoint_credential.as_ref()),
            now,
        )?;
    }
    Ok(())
}

fn trusted_home_business_pins(
    trust: &DeploymentTrust,
    home_id: &str,
    endpoint_credential: Option<&flowsplice_core::deployment::SignedHomeEndpointCredential>,
    now: u64,
) -> Result<Vec<String>> {
    Ok(trust
        .resolve_home_endpoint(home_id, endpoint_credential, now)?
        .business_spki_pins)
}

fn require_authenticated_relay_in_snapshot(
    verified: &VerifiedControlSnapshot,
    authenticated_relay_id: &str,
    authenticated_relay_spki: &str,
) -> Result<()> {
    let authenticated_endpoint = verified
        .payload
        .relay_directory
        .relays
        .iter()
        .find(|endpoint| endpoint.id == authenticated_relay_id)
        .ok_or_else(|| anyhow!("authenticated Relay is absent from the signed Server directory"))?;
    if !authenticated_endpoint
        .management_spki_sha256
        .eq_ignore_ascii_case(authenticated_relay_spki)
    {
        bail!("authenticated Relay SPKI conflicts with the signed Server directory");
    }
    Ok(())
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
                opened = Some(carrier);
                break;
            }
            Err(error) => warn!(relay = relay.label(), %error, "UDP carrier attempt failed"),
        }
    }
    let carrier = opened.ok_or_else(|| anyhow::anyhow!("all UDP carrier attempts failed"))?;
    let relay_id = carrier.relay_id.clone();
    let business = carrier.stream;
    let (mut reader, mut writer) = tokio::io::split(business);
    let mut reader = JsonFrameReader::new(&mut reader, DATA_FRAME_LIMIT);
    let mut send_sequence = 0_u64;
    let mut receive_sequence = 0_u64;
    loop {
        tokio::select! {
            datagram = timeout(Duration::from_secs(config.udp_idle_secs), outgoing.recv()) => {
                let Some(bytes) = datagram.context("UDP association idle timeout")? else { return Ok(()); };
                record_travel_udp_metric(state, mapping, &relay_id, "travel_flow_upload_observed_bytes", bytes.len() as u64);
                write_json(&mut writer, &DataFrame::Datagram { flow_id, sequence: send_sequence, bytes }, DATA_FRAME_LIMIT).await?;
                send_sequence = send_sequence.wrapping_add(1);
            }
            frame = timeout(Duration::from_secs(config.udp_idle_secs), reader.read::<DataFrame>()) => {
                match frame.context("UDP association idle timeout")?? {
                    DataFrame::Datagram { flow_id: id, sequence, bytes } if id == flow_id && sequence >= receive_sequence && bytes.len() <= 65_507 => {
                        if sequence == receive_sequence {
                            socket.send_to(&bytes, peer).await?;
                            record_travel_udp_metric(state, mapping, &relay_id, "delivered_download_datagram_bytes", bytes.len() as u64);
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

fn record_travel_udp_metric(
    state: &AppState,
    mapping: &Mapping,
    relay_id: &str,
    family: &str,
    value: u64,
) {
    let mut dimensions = BTreeMap::new();
    dimensions.insert("home_id".to_owned(), mapping.home_id.clone());
    dimensions.insert("service_id".to_owned(), mapping.service_id.clone());
    dimensions.insert("protocol".to_owned(), "udp".to_owned());
    dimensions.insert("mapping".to_owned(), mapping.bind.clone());
    dimensions.insert("relay_id".to_owned(), relay_id.to_owned());
    if let Ok(now) = unix_time_secs() {
        state
            .statistics
            .record(now, family, dimensions, value, None);
    }
}

async fn run_ui(state: AppState) -> Result<()> {
    let api = Router::new()
        .route("/status", get(api_status))
        .route("/catalog", get(api_catalog))
        .route("/relays", get(api_relays))
        .route("/statistics", get(api_statistics))
        .route(
            "/enrollment",
            get(api_remote_enrollments).post(api_create_remote_enrollment),
        )
        .route("/enrollment/install", post(api_install_remote_enrollment))
        .route(
            "/private-key-password",
            post(api_rotate_private_key_password),
        )
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
        private_key_password_rotation_available: travel_password_rotation_is_local(&state.config),
    })
}

async fn api_catalog(State(state): State<AppState>) -> Json<Catalog> {
    Json(state.catalog.read().await.clone())
}

async fn api_relays(State(state): State<AppState>) -> Json<RelayDirectory> {
    Json(state.directory.read().await.clone())
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
struct TravelStatisticsResponse {
    period: String,
    from_unix_secs: u64,
    to_unix_secs: u64,
    dropped_events: u64,
    active_flows: usize,
    overview: Vec<MetricRollup>,
    breakdowns: Vec<MetricRollup>,
    relay_discovery: Vec<RelayDiscoveryStatus>,
    points: Vec<MetricPoint>,
}

#[derive(Serialize)]
struct RelayDiscoveryStatus {
    relay_id: Option<String>,
    management_addr: String,
    configured_seed: bool,
    learned: bool,
    current_member: bool,
    last_seen_unix_secs: Option<u64>,
    last_success_unix_secs: Option<u64>,
    last_failure_unix_secs: Option<u64>,
    consecutive_failures: u32,
}

async fn api_statistics(
    State(state): State<AppState>,
    Query(query): Query<StatisticsQuery>,
) -> Json<TravelStatisticsResponse> {
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
    let history = state.relay_history.read().await.clone();
    let mut relay_discovery = history
        .into_iter()
        .map(|record| RelayDiscoveryStatus {
            relay_id: Some(record.relay_id),
            configured_seed: state
                .config
                .seed_relays
                .iter()
                .any(|seed| seed.management_addr == record.management_addr),
            management_addr: record.management_addr,
            learned: true,
            current_member: record.current_member,
            last_seen_unix_secs: Some(record.last_seen_unix_secs),
            last_success_unix_secs: record.last_success_unix_secs,
            last_failure_unix_secs: record.last_failure_unix_secs,
            consecutive_failures: record.consecutive_failures,
        })
        .collect::<Vec<_>>();
    for seed in &state.config.seed_relays {
        if !relay_discovery
            .iter()
            .any(|relay| relay.management_addr == seed.management_addr)
        {
            relay_discovery.push(RelayDiscoveryStatus {
                relay_id: None,
                management_addr: seed.management_addr.clone(),
                configured_seed: true,
                learned: false,
                current_member: false,
                last_seen_unix_secs: None,
                last_success_unix_secs: None,
                last_failure_unix_secs: None,
                consecutive_failures: 0,
            });
        }
    }
    relay_discovery.sort_by(|left, right| left.management_addr.cmp(&right.management_addr));
    Json(TravelStatisticsResponse {
        period: query.period,
        from_unix_secs: from,
        to_unix_secs: now,
        dropped_events: state.statistics.dropped_events(),
        active_flows: state
            .active_flows
            .load(std::sync::atomic::Ordering::Relaxed),
        overview: summarize_metric_points(&points, false),
        breakdowns: summarize_metric_points(&points, true),
        relay_discovery,
        points,
    })
}

async fn api_remote_enrollments(
    State(state): State<AppState>,
) -> ApiResult<Vec<RemoteEnrollmentStatus>> {
    let store = Arc::clone(&state.state_store);
    tokio::task::spawn_blocking(move || {
        let mut statuses = store
            .scan_prefix(Table::EnrollmentOutbox, b"")?
            .into_iter()
            .map(|(_, value)| {
                let record: RemoteEnrollmentOutboxRecord = serde_json::from_slice(&value)
                    .context("Travel enrollment outbox contains an invalid record")?;
                Ok(RemoteEnrollmentStatus {
                    request_id: record.request.request_id,
                    home_id: record.home_id,
                    created_at_unix_secs: record.created_at_unix_secs,
                    response_received: record.response.is_some(),
                    restart_required: record.restart_required,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        statuses.sort_by_key(|status| status.created_at_unix_secs);
        Ok(statuses)
    })
    .await
    .context("Travel enrollment outbox query task failed")
    .and_then(|result| result)
    .map(Json)
    .map_err(api_error)
}

async fn api_create_remote_enrollment(
    State(state): State<AppState>,
    Json(request): Json<CreateRemoteEnrollmentRequest>,
) -> ApiResult<RemoteEnrollmentStatus> {
    create_remote_enrollment(&state, request)
        .await
        .map(Json)
        .map_err(api_error)
}

async fn create_remote_enrollment(
    state: &AppState,
    request: CreateRemoteEnrollmentRequest,
) -> Result<RemoteEnrollmentStatus> {
    if !state
        .config
        .homes
        .iter()
        .any(|home| home.id == request.home_id)
    {
        bail!("remote enrollment target Home is not configured");
    }
    let password = Zeroizing::new(request.password);
    if password.chars().count() < MIN_PRIVATE_KEY_PASSWORD_CHARACTERS {
        bail!(
            "private-key password must contain at least {MIN_PRIVATE_KEY_PASSWORD_CHARACTERS} characters"
        );
    }
    let _sensitive_permit = Arc::clone(&state.sensitive_operation)
        .try_acquire_owned()
        .map_err(|_| anyhow!("another sensitive Travel operation is already running"))?;
    let work_root = state.config.enrollment_work_dir.clone();
    let travel_id = state.config.id.clone();
    let home_id = request.home_id;
    let store = Arc::clone(&state.state_store);
    tokio::task::spawn_blocking(move || {
        let now = unix_time_secs()?;
        prune_remote_enrollment_outbox(&store, now)?;
        let queued = store.scan_prefix(Table::EnrollmentOutbox, b"")?.len();
        if !remote_enrollment_capacity_available(
            queued,
            MAX_REMOTE_ENROLLMENT_OUTBOX_RECORDS,
        ) {
            bail!(
                "remote enrollment outbox capacity of {MAX_REMOTE_ENROLLMENT_OUTBOX_RECORDS} is exhausted"
            );
        }
        fs::create_dir_all(&work_root).with_context(|| {
            format!(
                "failed to create enrollment work directory {}",
                work_root.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&work_root, fs::Permissions::from_mode(0o700))?;
        }
        let enrollment_dir = work_root.join(format!("request-{}", Uuid::new_v4()));
        let request = create_enrollment_request(
            &travel_id,
            password.as_bytes(),
            &enrollment_dir,
            unix_time_secs()?,
        )?;
        let record = RemoteEnrollmentOutboxRecord {
            version: REMOTE_ENROLLMENT_VERSION,
            home_id: home_id.clone(),
            enrollment_dir,
            request: request.clone(),
            response: None,
            created_at_unix_secs: now,
            last_attempt_unix_secs: None,
            restart_required: false,
            installed_credential_id: None,
            installed_at_unix_secs: None,
        };
        store.apply_immediate(WriteBatch::new().put_json(
            Table::EnrollmentOutbox,
            request.request_id.as_bytes().to_vec(),
            &record,
        )?)?;
        Ok(RemoteEnrollmentStatus {
            request_id: request.request_id,
            home_id,
            created_at_unix_secs: record.created_at_unix_secs,
            response_received: false,
            restart_required: false,
        })
    })
    .await
    .context("Travel remote enrollment creation task failed")?
}

fn remote_enrollment_capacity_available(current: usize, maximum: usize) -> bool {
    current < maximum
}

async fn api_install_remote_enrollment(
    State(state): State<AppState>,
    Json(request): Json<InstallRemoteEnrollmentRequest>,
) -> ApiResult<InstallRemoteEnrollmentResponse> {
    install_remote_enrollment(&state, request)
        .await
        .map(Json)
        .map_err(api_error)
}

async fn install_remote_enrollment(
    state: &AppState,
    request: InstallRemoteEnrollmentRequest,
) -> Result<InstallRemoteEnrollmentResponse> {
    let _sensitive_permit = Arc::clone(&state.sensitive_operation)
        .try_acquire_owned()
        .map_err(|_| anyhow!("another sensitive Travel operation is already running"))?;
    let password = Zeroizing::new(request.password);
    if password.is_empty() {
        bail!("private-key password must not be empty");
    }
    let request_id = request.request_id;
    let store = Arc::clone(&state.state_store);
    let root_public_key = Arc::clone(&state.deployment_root_public_key);
    tokio::task::spawn_blocking(move || {
        let mut record = store
            .get_json::<RemoteEnrollmentOutboxRecord>(
                Table::EnrollmentOutbox,
                request_id.as_bytes(),
            )?
            .ok_or_else(|| anyhow!("unknown remote enrollment request"))?;
        let response = record
            .response
            .as_ref()
            .ok_or_else(|| anyhow!("remote enrollment is still awaiting Home approval"))?;
        let credential = install_enrollment_response(
            &record.enrollment_dir,
            response,
            &root_public_key,
            password.as_bytes(),
            unix_time_secs()?,
        )?;
        record.restart_required = true;
        record.installed_credential_id = Some(credential.credential_id);
        record.installed_at_unix_secs = Some(unix_time_secs()?);
        record.last_attempt_unix_secs = None;
        store.apply_immediate(
            WriteBatch::new()
                .put_json(
                    Table::EnrollmentOutbox,
                    request_id.as_bytes().to_vec(),
                    &record,
                )?
                .put(
                    Table::Metadata,
                    ACTIVE_IDENTITY_DIR_KEY.to_vec(),
                    record.enrollment_dir.to_string_lossy().as_bytes().to_vec(),
                ),
        )?;
        Ok(InstallRemoteEnrollmentResponse {
            request_id,
            credential_id: credential.credential_id,
            restart_required: true,
        })
    })
    .await
    .context("Travel remote enrollment installation task failed")?
}

async fn api_rotate_private_key_password(
    State(state): State<AppState>,
    Json(request): Json<RotatePrivateKeyPasswordRequest>,
) -> ApiResult<RotatePrivateKeyPasswordResponse> {
    rotate_travel_private_key_password(&state, request)
        .await
        .map(Json)
        .map_err(api_error)
}

async fn rotate_travel_private_key_password(
    state: &AppState,
    request: RotatePrivateKeyPasswordRequest,
) -> Result<RotatePrivateKeyPasswordResponse> {
    if !travel_password_rotation_is_local(&state.config) {
        bail!("Travel private-key password rotation is available only on a loopback UI");
    }
    if !is_encrypted_private_key(&state.config.management_key)?
        || !is_encrypted_private_key(&state.config.business_key)?
    {
        bail!("password rotation is unavailable for unencrypted test Travel keys");
    }
    let _sensitive_permit = Arc::clone(&state.sensitive_operation)
        .try_acquire_owned()
        .map_err(|_| anyhow!("another sensitive Travel operation is already running"))?;
    let current_password = Zeroizing::new(request.current_password);
    let new_password = Zeroizing::new(request.new_password);
    if new_password.chars().count() < MIN_PRIVATE_KEY_PASSWORD_CHARACTERS {
        bail!(
            "new private-key password must contain at least {MIN_PRIVATE_KEY_PASSWORD_CHARACTERS} characters"
        );
    }
    let config = Arc::clone(&state.config);
    let key_operation = state.key_operation.lock().await;
    tokio::task::spawn_blocking(move || {
        rotate_private_key_passwords(
            &travel_key_targets(&config),
            current_password.as_str(),
            new_password.as_str(),
        )
    })
    .await
    .context("Travel private-key password rotation task failed")??;
    drop(key_operation);
    info!(rotated_keys = 2, "rotated Travel private-key password");
    Ok(RotatePrivateKeyPasswordResponse { rotated_keys: 2 })
}

fn travel_password_rotation_is_local(config: &Config) -> bool {
    config
        .ui_listen
        .parse::<SocketAddr>()
        .is_ok_and(|address| address.ip() == std::net::Ipv4Addr::LOCALHOST)
        || test_remote_ui_enabled(config)
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

async fn authorize_ui(State(state): State<AppState>, request: Request, next: Next) -> Response {
    if local_ui_request_allowed(&request, &state.config.ui_listen) {
        return next.run(request).await;
    }
    #[cfg(feature = "e2e-remote-ui")]
    if test_remote_ui_enabled(&state.config) {
        use aws_lc_rs::constant_time::verify_slices_are_equal;
        let expected = state.config.test_admin_token.as_deref().unwrap_or_default();
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

#[cfg(test)]
mod tests {
    use axum::{body::Body, extract::Request, http::Method};
    use flowsplice_core::{
        authorization::TrustedTravelAuthority,
        deployment::{
            ControlSnapshotPayload, DeploymentTrust, HomeEndpointTrust, VerifiedControlSnapshot,
        },
        protocol::{Catalog, RelayDirectory, RelayEndpoint},
    };
    use flowsplice_storage::{StateStore, Table, WriteBatch};

    use super::{
        ConfiguredHome, ControlTrustState, RELAY_HISTORY_VERSION, RelayHistoryRecord, SeedRelay,
        bootstrap_candidate_pool, configured_home_ids, configured_homes_are_trusted,
        load_relay_history, local_ui_request_allowed, remote_enrollment_capacity_available,
        remote_enrollment_outbox_expired, require_authenticated_relay_in_snapshot,
        require_control_snapshot_subject, signed_directory_candidates, trusted_home_business_pins,
    };

    #[test]
    fn remote_enrollment_outbox_has_bounded_capacity_and_retention() {
        assert!(remote_enrollment_capacity_available(63, 64));
        assert!(!remote_enrollment_capacity_available(64, 64));
        assert!(!remote_enrollment_outbox_expired(100, None, 100));
        assert!(remote_enrollment_outbox_expired(
            100,
            None,
            100 + flowsplice_enrollment::MAX_REQUEST_AGE_SECS
        ));
        assert!(!remote_enrollment_outbox_expired(
            1,
            Some(200),
            200 + 24 * 60 * 60 - 1
        ));
        assert!(remote_enrollment_outbox_expired(
            1,
            Some(200),
            200 + 24 * 60 * 60
        ));
    }

    #[test]
    fn travel_ui_requires_exact_loopback_host_and_origin() -> anyhow::Result<()> {
        let get = Request::builder()
            .uri("http://127.0.0.1:9080/")
            .header("host", "127.0.0.1:9080")
            .body(Body::empty())?;
        assert!(local_ui_request_allowed(&get, "127.0.0.1:9080"));

        let bad_site = Request::builder()
            .uri("http://127.0.0.1:9080/")
            .header("host", "127.0.0.1:9080")
            .header("sec-fetch-site", "cross-site")
            .body(Body::empty())?;
        assert!(!local_ui_request_allowed(&bad_site, "127.0.0.1:9080"));

        let post = Request::builder()
            .method(Method::POST)
            .uri("http://127.0.0.1:9080/api/keys/password")
            .header("host", "127.0.0.1:9080")
            .header("origin", "http://127.0.0.1:9080")
            .body(Body::empty())?;
        assert!(local_ui_request_allowed(&post, "127.0.0.1:9080"));

        let missing_origin = Request::builder()
            .method(Method::POST)
            .uri("http://127.0.0.1:9080/api/keys/password")
            .header("host", "127.0.0.1:9080")
            .body(Body::empty())?;
        assert!(!local_ui_request_allowed(&missing_origin, "127.0.0.1:9080"));
        Ok(())
    }

    fn verified(trust_generation: u64, generation: u64, digest: &str) -> VerifiedControlSnapshot {
        VerifiedControlSnapshot {
            trust: DeploymentTrust {
                version: 1,
                deployment_id: "deployment-1".to_owned(),
                generation: trust_generation,
                not_before_unix_secs: 1,
                not_after_unix_secs: u64::MAX,
                management_ca_certificate_pem: String::new(),
                business_ca_certificate_pem: String::new(),
                server_control_keys: Vec::new(),
                home_endpoints: Vec::new(),
                home_enrollment_authorities: Vec::new(),
                travel_authorities: Vec::<TrustedTravelAuthority>::new(),
            },
            trust_digest_sha256: format!("trust-{trust_generation}"),
            payload: ControlSnapshotPayload {
                version: 1,
                object_type: flowsplice_core::deployment::CONTROL_SNAPSHOT_OBJECT_TYPE.to_owned(),
                deployment_id: "deployment-1".to_owned(),
                server_id: "server-1".to_owned(),
                signer_epoch: 1,
                travel_id: "travel-1".to_owned(),
                travel_management_spki_sha256: "33".repeat(32),
                generation,
                issued_at_unix_secs: 1,
                expires_at_unix_secs: 2,
                relay_directory: RelayDirectory::default(),
                catalog: Catalog::default(),
            },
            digest_sha256: digest.to_owned(),
        }
    }

    #[test]
    fn control_trust_state_rejects_rollback_and_same_generation_conflicts() -> anyhow::Result<()> {
        let mut state = ControlTrustState::new();
        assert!(state.accept(&verified(3, 10, "aa")).unwrap_or(false));
        assert!(!state.accept(&verified(3, 10, "aa")).unwrap_or(true));
        assert!(state.accept(&verified(3, 10, "bb")).is_err());
        assert!(state.accept(&verified(2, 11, "cc")).is_err());
        assert!(!state.accept(&verified(4, 9, "dd")).unwrap_or(true));
        assert!(state.accept(&verified(4, 12, "ee")).unwrap_or(false));

        let encoded = serde_json::to_vec(&state)?;
        let mut restarted: ControlTrustState = serde_json::from_slice(&encoded)?;
        assert!(!restarted.accept(&verified(4, 11, "ff")).unwrap_or(true));
        let mut rotated = verified(5, 1, "gg");
        rotated.payload.signer_epoch = 2;
        assert!(restarted.accept(&rotated).unwrap_or(false));
        let mut downgraded = verified(6, 99, "hh");
        downgraded.payload.signer_epoch = 1;
        assert!(restarted.accept(&downgraded).is_err());
        Ok(())
    }

    #[test]
    fn seed_must_match_the_server_signed_relay_directory() {
        let mut snapshot = verified(1, 1, "aa");
        snapshot.payload.relay_directory.relays = vec![RelayEndpoint {
            id: "relay-1".to_owned(),
            management_addr: "relay.example:8443".to_owned(),
            data_public_addr: "relay.example:8444".to_owned(),
            management_spki_sha256: "11".repeat(32),
        }];

        assert!(
            require_authenticated_relay_in_snapshot(&snapshot, "relay-1", &"11".repeat(32)).is_ok()
        );
        assert!(
            require_authenticated_relay_in_snapshot(&snapshot, "relay-attacker", &"22".repeat(32))
                .is_err()
        );
        assert!(
            require_authenticated_relay_in_snapshot(&snapshot, "relay-1", &"22".repeat(32))
                .is_err()
        );
        assert!(require_control_snapshot_subject(&snapshot, "travel-1", &"33".repeat(32)).is_ok());
        assert!(require_control_snapshot_subject(&snapshot, "travel-2", &"33".repeat(32)).is_err());
    }

    #[test]
    fn configured_homes_are_resolved_from_deployment_trust() {
        let mut snapshot = verified(1, 1, "aa");
        snapshot.trust.home_endpoints = vec![HomeEndpointTrust {
            home_id: "home-1".to_owned(),
            management_spki_pins: vec!["11".repeat(32)],
            business_spki_pins: vec!["22".repeat(32)],
        }];
        assert!(
            configured_homes_are_trusted(
                &[ConfiguredHome {
                    id: "home-1".to_owned(),
                }],
                &snapshot.trust,
                &snapshot.payload.catalog,
                2,
            )
            .is_ok()
        );
        assert!(
            configured_homes_are_trusted(
                &[ConfiguredHome {
                    id: "home-2".to_owned(),
                }],
                &snapshot.trust,
                &snapshot.payload.catalog,
                2,
            )
            .is_err()
        );
        assert_eq!(
            trusted_home_business_pins(&snapshot.trust, "home-1", None, 2).unwrap_or_default(),
            vec!["22".repeat(32)]
        );
        assert!(trusted_home_business_pins(&snapshot.trust, "home-2", None, 2).is_err());
    }

    #[test]
    fn configured_home_ids_reject_empty_duplicate_and_legacy_pin_fields() {
        assert!(configured_home_ids(&[]).is_err());
        assert!(configured_home_ids(&[ConfiguredHome { id: String::new() }]).is_err());
        assert!(
            configured_home_ids(&[
                ConfiguredHome {
                    id: "home-1".to_owned(),
                },
                ConfiguredHome {
                    id: "home-1".to_owned(),
                },
            ])
            .is_err()
        );
        assert!(
            serde_json::from_str::<ConfiguredHome>(r#"{"id":"home-1","spki_pins":["obsolete"]}"#)
                .is_err()
        );
    }

    fn history_record(relay_id: &str, address: &str, deployment_id: &str) -> RelayHistoryRecord {
        RelayHistoryRecord {
            version: RELAY_HISTORY_VERSION,
            deployment_id: deployment_id.to_owned(),
            relay_id: relay_id.to_owned(),
            management_addr: address.to_owned(),
            management_spki_sha256: "11".repeat(32),
            first_seen_unix_secs: 100,
            last_seen_unix_secs: 200,
            last_directory_generation: 2,
            last_success_unix_secs: Some(190),
            last_failure_unix_secs: None,
            consecutive_failures: 0,
            current_member: false,
            operator_disabled: false,
        }
    }

    #[test]
    fn relay_history_ignores_malformed_keys_values_and_address_conflicts() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let store = StateStore::open(directory.path().join("travel.redb"))?;
        let valid = history_record("relay-1", "relay1.example:8443", "deployment-1");
        let mut bad_spki = history_record("relay-2", "relay2.example:8443", "deployment-1");
        bad_spki.management_spki_sha256 = "not-hex".to_owned();
        let wrong_key = history_record("relay-3", "relay3.example:8443", "deployment-1");
        let conflict_one = history_record("relay-4", "shared.example:8443", "deployment-1");
        let conflict_two = history_record("relay-5", "shared.example:8443", "deployment-1");
        store.apply_immediate(
            WriteBatch::new()
                .put_json(Table::RelayHistory, b"relay-1".to_vec(), &valid)?
                .put_json(Table::RelayHistory, b"relay-2".to_vec(), &bad_spki)?
                .put_json(Table::RelayHistory, b"wrong-key".to_vec(), &wrong_key)?
                .put_json(Table::RelayHistory, b"relay-4".to_vec(), &conflict_one)?
                .put_json(Table::RelayHistory, b"relay-5".to_vec(), &conflict_two)?,
        )?;
        let loaded = load_relay_history(&store)?;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].relay_id, "relay-1");
        Ok(())
    }

    #[test]
    fn historical_relays_bootstrap_but_only_current_directory_authorizes_business() {
        let current = RelayEndpoint {
            id: "relay-current".to_owned(),
            management_addr: "current.example:8443".to_owned(),
            data_public_addr: "current.example:8444".to_owned(),
            management_spki_sha256: "22".repeat(32),
        };
        let directory = RelayDirectory {
            generation: 4,
            relays: vec![current.clone()],
        };
        let mut stale_copy = history_record(
            "relay-current",
            "stale-current.example:8443",
            "deployment-1",
        );
        stale_copy.current_member = true;
        let removed = history_record("relay-removed", "removed.example:8443", "deployment-1");
        let wrong_deployment =
            history_record("relay-foreign", "foreign.example:8443", "deployment-2");
        let mut disabled =
            history_record("relay-disabled", "disabled.example:8443", "deployment-1");
        disabled.operator_disabled = true;
        let pool = bootstrap_candidate_pool(
            directory.clone(),
            "deployment-1",
            vec![stale_copy, removed, wrong_deployment, disabled],
            &[SeedRelay {
                management_addr: "seed.example:8443".to_owned(),
            }],
        );
        assert_eq!(pool.len(), 3);
        assert_eq!(pool[0].expected_id.as_deref(), Some("relay-current"));
        assert_eq!(pool[0].management_addr, "current.example:8443");
        assert_eq!(pool[1].expected_id.as_deref(), Some("relay-removed"));
        assert_eq!(pool[2].expected_id, None);

        let business = signed_directory_candidates(directory);
        assert_eq!(business.len(), 1);
        assert_eq!(business[0].expected_id.as_deref(), Some("relay-current"));
        assert!(
            business
                .iter()
                .all(|candidate| candidate.expected_id.as_deref() != Some("relay-removed"))
        );
    }
}
