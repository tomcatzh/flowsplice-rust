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
    authorization::unix_time_secs,
    config::load_toml,
    deployment::{
        DeploymentTrust, SignedControlSnapshot, SignedDeploymentTrust, VerifiedControlSnapshot,
    },
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{
        Catalog, ControlMessage, DataFrame, RelayDirectory, Role, ServiceProtocol,
        TravelConnectionPurpose,
    },
    route::{RouteSide, write_preface},
    tls::{
        identity_client_connector_with_private_key, identity_server_name, peer_identity,
        require_peer,
    },
};
use flowsplice_enrollment::{
    DEPLOYMENT_TRUST_FILE, TravelEnrollmentResponse, create_enrollment_request,
    install_enrollment_response,
    key::{
        MIN_PRIVATE_KEY_PASSWORD_CHARACTERS, PrivateKeyRotationTarget, is_encrypted_private_key,
        load_private_key, recover_private_key_password_rotation, rotate_private_key_passwords,
    },
    load_json,
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
enum Command {
    EnrollInit(EnrollInitArgs),
    EnrollImport(EnrollImportArgs),
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

#[derive(Clone, Deserialize)]
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
    ui_listen: String,
    #[cfg(feature = "e2e-remote-ui")]
    #[serde(default)]
    test_allow_remote_listen: bool,
    #[cfg(feature = "e2e-remote-ui")]
    #[serde(default)]
    test_admin_token: Option<String>,
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

#[derive(Clone)]
struct RelayCandidate {
    expected_id: Option<String>,
    management_addr: String,
    management_spki_sha256: Option<String>,
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
    control_trust_state_path: Arc<PathBuf>,
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
    let deployment_root_public_key = embedded_deployment_root_public_key()?.to_owned();
    let management_identity = local_certificate_identity(&config.management_cert)?;
    require_peer(&management_identity, Role::Travel, Some(&config.id), &[])?;
    let control_trust_state_path =
        enrollment_sibling(&config.management_cert, CONTROL_TRUST_STATE_FILE);
    let (control_trust_state, deployment_trust, cached_control_snapshot) =
        load_initial_control_trust_state(
            &config,
            &deployment_root_public_key,
            &control_trust_state_path,
            &management_identity.spki_sha256,
        )?;
    if recover_private_key_password_rotation(&travel_key_targets(&config))? {
        info!("completed interrupted Travel private-key password rotation");
    }
    let (management_key, business_key) = load_runtime_private_keys(&config)?;
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
        control_trust_state_path: Arc::new(control_trust_state_path),
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
    }
}

fn embedded_deployment_root_public_key() -> Result<&'static str> {
    option_env!("FLOWSPLICE_DEPLOYMENT_ROOT_PUBLIC_KEY")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("this Travel binary has no embedded deployment root public key"))
}

fn enrollment_sibling(certificate: &Path, file_name: &str) -> PathBuf {
    certificate
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .join(file_name)
}

fn load_initial_control_trust_state(
    config: &Config,
    deployment_root_public_key: &str,
    state_path: &Path,
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
    let mut state = if state_path.exists() {
        flowsplice_core::authorization::load_json(state_path)?
    } else {
        ControlTrustState::new()
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
        flowsplice_core::authorization::store_json_atomic(state_path, &state)?;
    }

    let cached = state.cached_snapshot.as_ref().and_then(|snapshot| {
        match snapshot.verify(deployment_root_public_key, now) {
            Ok(verified)
                if require_control_snapshot_subject(
                    &verified,
                    &config.id,
                    management_spki_sha256,
                )
                .is_ok()
                    && verified.trust.generation == state.trust_generation
                    && verified.trust_digest_sha256
                        == state.trust_digest_sha256.as_deref().unwrap_or_default()
                    && verified.payload.signer_epoch == state.signer_epoch
                    && verified.payload.generation == state.snapshot_generation
                    && verified.digest_sha256
                        == state.snapshot_digest_sha256.as_deref().unwrap_or_default() =>
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
    let runtime_trust = cached
        .as_ref()
        .map_or(trust, |snapshot| snapshot.trust.clone());
    configured_homes_are_trusted(&config.homes, &runtime_trust)?;
    Ok((state, runtime_trust, cached))
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
        for relay in relay_candidates(&state).await {
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
        }
    }
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
        trusted_home_business_pins(&trust, &home.id)?.to_vec()
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
    let source = if directory.relays.is_empty() {
        state
            .config
            .seed_relays
            .iter()
            .map(|relay| RelayCandidate {
                expected_id: None,
                management_addr: relay.management_addr.clone(),
                management_spki_sha256: None,
            })
            .collect::<Vec<_>>()
    } else {
        directory
            .relays
            .into_iter()
            .map(|relay| RelayCandidate {
                expected_id: Some(relay.id),
                management_addr: relay.management_addr,
                management_spki_sha256: Some(relay.management_spki_sha256),
            })
            .collect()
    };
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
    configured_homes_are_trusted(&state.config.homes, &verified.trust)?;
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
        flowsplice_core::authorization::store_json_atomic(
            &state.control_trust_state_path,
            &proposed,
        )?;
        *acceptance = proposed;
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

fn configured_homes_are_trusted(homes: &[ConfiguredHome], trust: &DeploymentTrust) -> Result<()> {
    for home in homes {
        trust.home_endpoint(&home.id)?;
    }
    Ok(())
}

fn trusted_home_business_pins<'a>(
    trust: &'a DeploymentTrust,
    home_id: &str,
) -> Result<&'a [String]> {
    Ok(&trust.home_endpoint(home_id)?.business_spki_pins)
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
                opened = Some(carrier.stream);
                break;
            }
            Err(error) => warn!(relay = relay.label(), %error, "UDP carrier attempt failed"),
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

    use super::{
        ConfiguredHome, ControlTrustState, configured_home_ids, configured_homes_are_trusted,
        local_ui_request_allowed, require_authenticated_relay_in_snapshot,
        require_control_snapshot_subject, trusted_home_business_pins,
    };

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
                &snapshot.trust
            )
            .is_ok()
        );
        assert!(
            configured_homes_are_trusted(
                &[ConfiguredHome {
                    id: "home-2".to_owned(),
                }],
                &snapshot.trust
            )
            .is_err()
        );
        assert_eq!(
            trusted_home_business_pins(&snapshot.trust, "home-1").unwrap_or_default(),
            &["22".repeat(32)]
        );
        assert!(trusted_home_business_pins(&snapshot.trust, "home-2").is_err());
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
}
