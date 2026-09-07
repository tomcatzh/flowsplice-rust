#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    env, fs,
    io::{Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

#[cfg(feature = "frontend")]
use std::{
    io::{self, IsTerminal},
    sync::LazyLock,
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    digest,
    rand::{SecureRandom, SystemRandom},
    signature::EcdsaKeyPair,
};
#[cfg(feature = "frontend")]
use axum::{
    Json, Router,
    extract::{Query, Request, State},
    http::{Method, StatusCode, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post},
};
#[cfg(feature = "frontend")]
use clap::{Parser, Subcommand};
#[cfg(feature = "frontend")]
use embedded_spa::{EmbeddedSpa, EmbeddedSpaConfig};
use flowsplice_core::{
    CONTROL_FRAME_LIMIT, DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    authorization::unix_time_secs,
    config::{load_toml, resolve_path},
    deployment::{
        DeploymentTrust, SignedControlSnapshot, SignedDeploymentTrust, VerifiedControlSnapshot,
        load_verified_deployment_trust,
    },
    frame::{DataFrameCodec, DataFrameReader, JsonFrameReader, write_data_frame, write_json},
    init_crypto,
    protocol::{
        CONTROL_PROTOCOL_VERSION, Catalog, ControlMessage, DATA_PROTOCOL_BINARY_V1, DataFrame,
        RelayDirectory, Role, ServiceProtocol, TravelConnectionPurpose,
        bootstrap_verification_code,
    },
    route::{RouteSide, write_preface},
    statistics::statistics_signing_key,
    tls::{
        bootstrap_discovery_connector, identity_client_connector_with_private_key,
        identity_from_certificate_pem, identity_server_auth_connector_from_ca_pem,
        identity_server_name, peer_identity, require_peer, verify_discovery_certificate,
    },
};
#[cfg(feature = "frontend")]
use flowsplice_enrollment::key::rotate_private_key_passwords;
use flowsplice_enrollment::{
    BUSINESS_CA_FILE, BUSINESS_CERT_FILE, BUSINESS_KEY_FILE, DEPLOYMENT_TRUST_FILE,
    MANAGEMENT_CA_FILE, MANAGEMENT_CERT_FILE, MANAGEMENT_KEY_FILE, MAX_REQUEST_AGE_SECS,
    REQUEST_FILE, TravelEnrollmentRequest, TravelEnrollmentResponse, create_enrollment_request,
    install_enrollment_response,
    key::{
        MIN_PRIVATE_KEY_PASSWORD_CHARACTERS, PrivateKeyRotationTarget, is_encrypted_private_key,
        load_private_key, recover_private_key_password_rotation,
    },
    load_json, validate_enrollment_response,
};
use flowsplice_storage::{LocalStatistics, MetricBatch, StateStore, Table, WriteBatch};
#[cfg(feature = "frontend")]
use flowsplice_storage::{MetricPoint, MetricRollup, summarize_metric_points};
#[cfg(feature = "frontend")]
use rust_embed::RustEmbed;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream, UdpSocket},
    sync::{Mutex, RwLock, Semaphore, mpsc, watch},
    task::{JoinHandle, JoinSet},
    time::{interval, sleep, timeout},
};
use tokio_rustls::{TlsConnector, client::TlsStream};
use tracing::{info, warn};
use uuid::Uuid;
use zeroize::Zeroizing;

pub mod business;
mod socket;
mod tcp_flow;
pub use socket::{ServiceBinding, SocketDatagrams, SocketStream};

#[cfg(test)]
mod review_regressions;

#[derive(RustEmbed)]
#[folder = "../../travelagent/web/dist/"]
#[cfg(feature = "frontend")]
struct WebAssets;

#[cfg(feature = "frontend")]
static SPA: LazyLock<EmbeddedSpa<WebAssets>> = LazyLock::new(|| {
    EmbeddedSpa::new(EmbeddedSpaConfig::default())
        .unwrap_or_else(|error| panic!("invalid embedded Travel Agent UI: {error}"))
});

#[derive(Parser)]
#[command(name = "flowsplice-travelagent", version)]
#[cfg(feature = "frontend")]
struct Cli {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "travelagent.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
#[allow(clippy::enum_variant_names)]
#[cfg(feature = "frontend")]
enum Command {
    EnrollRemote(EnrollRemoteArgs),
    CheckBootstrapConfig {
        #[arg(long, default_value = "travel-bootstrap.toml")]
        config: PathBuf,
    },
}

#[derive(clap::Args)]
#[cfg(feature = "frontend")]
struct EnrollRemoteArgs {
    #[arg(long)]
    travel_id: String,
    #[arg(long)]
    home_id: String,
    #[arg(long)]
    install_dir: PathBuf,
    /// Relay management address used for first contact, for example relay.example:8443.
    #[arg(
        long,
        required_unless_present = "bootstrap_config",
        conflicts_with = "bootstrap_config"
    )]
    relay: Option<String>,
    /// Legacy private bootstrap file accepted for compatibility with existing automation.
    #[arg(long, conflicts_with = "relay")]
    bootstrap_config: Option<PathBuf>,
    /// Trusted deployment public key supplied separately from the Relay (CLI discovery).
    #[arg(
        long,
        required_unless_present = "bootstrap_config",
        requires = "relay",
        conflicts_with = "bootstrap_config"
    )]
    deployment_root_public_key: Option<PathBuf>,
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
    #[serde(default)]
    relay_address_overrides: Vec<RelayAddressOverride>,
    homes: Vec<ConfiguredHome>,
    deployment_root_public_key: PathBuf,
    deployment_trust: PathBuf,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TravelBootstrapConfig {
    deployment_root_public_key: PathBuf,
    deployment_trust: PathBuf,
    #[serde(default)]
    bootstrap_relays: Vec<String>,
    #[serde(default)]
    relay_address_overrides: Vec<RelayAddressOverride>,
    ui_listen: String,
}

struct VerifiedTravelBootstrap {
    deployment_root_public_key: String,
    signed_trust: SignedDeploymentTrust,
    trust: DeploymentTrust,
    bootstrap_relays: Vec<String>,
    relay_address_overrides: Vec<RelayAddressOverride>,
    ui_listen: String,
}

/// Inputs for a complete first-device remote enrollment.
/// Required identity, trust and installation values are validated before enrollment.
#[derive(Default)]
pub struct RemoteEnrollmentOptions {
    pub travel_id: String,
    pub home_id: String,
    pub install_dir: PathBuf,
    pub bootstrap_config: Option<PathBuf>,
    /// Pretrusted key from a signed native resource or a private CLI input. Never from discovery.
    pub trusted_deployment_root_public_key: Option<String>,
    pub selected_relay: Option<String>,
    /// Listener written to a generated CLI config. Native clients may leave this unset.
    pub ui_listen: Option<String>,
    pub private_key_password: String,
    pub wait_timeout_secs: u64,
    #[cfg(feature = "e2e-remote-ui")]
    pub test_allow_remote_listen: bool,
    #[cfg(feature = "e2e-remote-ui")]
    pub test_admin_token: Option<String>,
}

/// Observable stages of first-device remote enrollment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEnrollmentPhase {
    Preparing,
    WaitingForApproval,
    Installed,
}

/// Progress snapshot emitted by the reusable enrollment operation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RemoteEnrollmentProgress {
    pub phase: RemoteEnrollmentPhase,
    pub travel_id: String,
    pub request_id: Option<Uuid>,
    pub verification_code: Option<String>,
    pub config_path: Option<PathBuf>,
    pub credential_id: Option<Uuid>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct SeedRelay {
    management_addr: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RelayAddressOverride {
    id: String,
    management_addr: String,
    data_addr: String,
    /// Discovery addresses are fallback hints; only explicit operator overrides outrank updates.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    bootstrap_hint: bool,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    business: Option<flowsplice_core::business::BusinessDescriptor>,
}

#[derive(Serialize)]
struct InstalledTravelConfig {
    id: String,
    deployment_root_public_key: PathBuf,
    deployment_trust: PathBuf,
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
    relay_address_overrides: Vec<RelayAddressOverride>,
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Mapping {
    pub home_id: String,
    pub service_id: String,
    pub protocol: ServiceProtocol,
    pub bind: String,
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
    10
}

const fn default_carrier_timeout() -> u64 {
    30
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
const TRAVEL_MAPPINGS_KEY: &[u8] = b"active";
const LEGACY_CONTROL_STATE_DIGEST_KEY: &[u8] = b"legacy_control_state_sha256";
const ACTIVE_IDENTITY_DIR_KEY: &[u8] = b"active_identity_dir";
const REMOTE_ENROLLMENT_VERSION: u32 = 1;
#[cfg(feature = "frontend")]
const MAX_REMOTE_ENROLLMENT_OUTBOX_RECORDS: usize = 64;
const REMOTE_ENROLLMENT_INSTALLED_RETENTION_SECS: u64 = 24 * 60 * 60;
const RELAY_HISTORY_VERSION: u32 = 1;
const MAX_ROUTE_EVENTS: usize = 256;
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
    uploaded_bytes: Arc<std::sync::atomic::AtomicU64>,
    downloaded_bytes: Arc<std::sync::atomic::AtomicU64>,
    connected_relays: Arc<RwLock<HashSet<String>>>,
    network_generation: watch::Sender<u64>,
    status_generation: watch::Sender<u64>,
    permits: Arc<Semaphore>,
    carrier_permits: Arc<Semaphore>,
    flow_relays: Arc<Mutex<HashMap<Uuid, String>>>,
    diagnostics: Arc<Mutex<DiagnosticsState>>,
    #[cfg(feature = "frontend")]
    key_operation: Arc<Mutex<()>>,
    #[cfg(feature = "frontend")]
    sensitive_operation: Arc<Semaphore>,
    deployment_root_public_key: Arc<String>,
    deployment_trust: Arc<RwLock<DeploymentTrust>>,
    management_spki_sha256: Arc<String>,
    state_store: Arc<StateStore>,
    enrollment_outbox_generation: Arc<std::sync::atomic::AtomicU64>,
    statistics: Arc<LocalStatistics>,
    statistics_signer: Arc<EcdsaKeyPair>,
    statistics_certificate_pem: Arc<String>,
    relay_history: Arc<RwLock<Vec<RelayHistoryRecord>>>,
    control_trust_state: Arc<Mutex<ControlTrustState>>,
    mappings: Arc<RwLock<Vec<Mapping>>>,
    mapping_tasks: Arc<Mutex<HashMap<String, MappingTask>>>,
    ready_mapping_listeners: Arc<RwLock<HashMap<String, Uuid>>>,
    mapping_operation: Arc<Mutex<()>>,
    blocking_operations: watch::Sender<usize>,
}

struct BlockingOperation(watch::Sender<usize>);

impl Drop for BlockingOperation {
    fn drop(&mut self) {
        self.0.send_modify(|count| *count -= 1);
    }
}

impl AppState {
    fn spawn_blocking<F, R>(&self, work: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        self.blocking_operations.send_modify(|count| *count += 1);
        let operation = BlockingOperation(self.blocking_operations.clone());
        tokio::task::spawn_blocking(move || {
            let _operation = operation;
            work()
        })
    }

    fn mark_status_changed(&self) {
        self.status_generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn mark_enrollment_outbox_changed(&self) {
        self.enrollment_outbox_generation
            .fetch_add(1, std::sync::atomic::Ordering::Release);
    }

    fn has_enrollment_outbox_work(&self) -> bool {
        self.enrollment_outbox_generation
            .load(std::sync::atomic::Ordering::Acquire)
            != 0
    }

    async fn set_mapping_listener_ready(&self, key: &str, token: Uuid, ready: bool) {
        let changed = {
            let mut listeners = self.ready_mapping_listeners.write().await;
            if ready {
                listeners.insert(key.to_owned(), token) != Some(token)
            } else if listeners.get(key) == Some(&token) {
                listeners.remove(key);
                true
            } else {
                false
            }
        };
        if changed {
            self.mark_status_changed();
        }
    }

    async fn begin_route_flow(&self, flow_id: Uuid, mapping: &Mapping) {
        let now = unix_time_secs().unwrap_or_default();
        let mut diagnostics = self.diagnostics.lock().await;
        diagnostics.flows.insert(
            flow_id,
            DiagnosticFlowState {
                mapping: mapping.clone(),
                started_at_unix_secs: now,
                selected_relay: None,
                last_switch_unix_secs: None,
                switch_count: 0,
                uploaded_bytes: 0,
                downloaded_bytes: 0,
                recovering: true,
            },
        );
        diagnostics.push_event(
            now,
            Some(flow_id),
            None,
            mapping.protocol,
            "flow",
            "started",
            None,
            None,
        );
        drop(diagnostics);
        self.mark_status_changed();
    }

    async fn select_route_relay(&self, flow_id: Uuid, relay_id: &str, latency_ms: Option<u64>) {
        let now = unix_time_secs().unwrap_or_default();
        let mut diagnostics = self.diagnostics.lock().await;
        let Some(flow) = diagnostics.flows.get_mut(&flow_id) else {
            return;
        };
        let protocol = flow.mapping.protocol;
        let previous = flow.selected_relay.replace(relay_id.to_owned());
        let switched = previous.as_deref().is_some_and(|value| value != relay_id);
        if switched {
            flow.switch_count = flow.switch_count.saturating_add(1);
            flow.last_switch_unix_secs = Some(now);
        }
        flow.recovering = false;
        diagnostics.push_event(
            now,
            Some(flow_id),
            Some(relay_id.to_owned()),
            protocol,
            "carrier",
            if switched { "switched" } else { "selected" },
            latency_ms,
            None,
        );
        drop(diagnostics);
        self.mark_status_changed();
    }

    async fn mark_route_recovering(&self, flow_id: Uuid, reason: Option<&str>) {
        let now = unix_time_secs().unwrap_or_default();
        let mut diagnostics = self.diagnostics.lock().await;
        let Some(flow) = diagnostics.flows.get_mut(&flow_id) else {
            return;
        };
        if flow.recovering {
            return;
        }
        flow.recovering = true;
        let protocol = flow.mapping.protocol;
        let relay_id = flow.selected_relay.clone();
        diagnostics.push_event(
            now,
            Some(flow_id),
            relay_id,
            protocol,
            "recovery",
            "started",
            None,
            reason.map(sanitized_route_reason),
        );
        drop(diagnostics);
        self.mark_status_changed();
    }

    async fn update_route_flow_counters(
        &self,
        flow_id: Uuid,
        uploaded_bytes: u64,
        downloaded_bytes: u64,
    ) {
        if let Some(flow) = self.diagnostics.lock().await.flows.get_mut(&flow_id) {
            flow.uploaded_bytes = uploaded_bytes;
            flow.downloaded_bytes = downloaded_bytes;
        }
    }

    async fn record_route_attempt(
        &self,
        flow_id: Uuid,
        relay_id: Option<String>,
        protocol: ServiceProtocol,
        outcome: &'static str,
        latency_ms: Option<u64>,
        reason: Option<&str>,
    ) {
        let now = unix_time_secs().unwrap_or_default();
        self.diagnostics.lock().await.push_event(
            now,
            Some(flow_id),
            relay_id,
            protocol,
            "route_attempt",
            outcome,
            latency_ms,
            reason.map(sanitized_route_reason),
        );
    }

    async fn finish_route_flow(
        &self,
        flow_id: Uuid,
        uploaded_bytes: u64,
        downloaded_bytes: u64,
        error: Option<&anyhow::Error>,
    ) {
        let now = unix_time_secs().unwrap_or_default();
        let mut diagnostics = self.diagnostics.lock().await;
        let Some(mut flow) = diagnostics.flows.remove(&flow_id) else {
            return;
        };
        flow.uploaded_bytes = uploaded_bytes;
        flow.downloaded_bytes = downloaded_bytes;
        diagnostics.push_event(
            now,
            Some(flow_id),
            flow.selected_relay,
            flow.mapping.protocol,
            "flow",
            if error.is_some() { "failed" } else { "closed" },
            None,
            error.map(|value| sanitized_route_reason(&value.to_string())),
        );
        drop(diagnostics);
        self.mark_status_changed();
    }
}

struct MappingTask {
    shutdown: watch::Sender<bool>,
    join: JoinHandle<()>,
}

enum PreparedMappingListener {
    Tcp(TcpListener),
    Udp(UdpSocket),
}

#[derive(Clone)]
struct TlsMaterial {
    management_connector: TlsConnector,
    business_connector: TlsConnector,
}

#[derive(Serialize)]
pub struct StatusResponse {
    pub ok: bool,
    pub online: bool,
    pub travel_id: String,
    pub uptime_secs: u64,
    pub active_flows: usize,
    pub catalog_generation: u64,
    pub relay_directory_generation: u64,
    pub active_relays: Vec<String>,
    pub session_uploaded_bytes: u64,
    pub session_downloaded_bytes: u64,
    pub mappings: Vec<Mapping>,
    pub private_key_password_rotation_available: bool,
}

#[derive(Serialize)]
pub struct StatusUpdate {
    pub generation: u64,
    pub status: StatusResponse,
}

#[derive(Clone, Debug)]
struct DiagnosticFlowState {
    mapping: Mapping,
    started_at_unix_secs: u64,
    selected_relay: Option<String>,
    last_switch_unix_secs: Option<u64>,
    switch_count: u32,
    uploaded_bytes: u64,
    downloaded_bytes: u64,
    recovering: bool,
}

#[derive(Default)]
struct DiagnosticsState {
    flows: HashMap<Uuid, DiagnosticFlowState>,
    events: VecDeque<RouteEvent>,
    next_event_id: u64,
}

impl DiagnosticsState {
    #[allow(clippy::too_many_arguments)]
    fn push_event(
        &mut self,
        timestamp_unix_secs: u64,
        flow_id: Option<Uuid>,
        relay_id: Option<String>,
        protocol: ServiceProtocol,
        phase: &'static str,
        outcome: &'static str,
        latency_ms: Option<u64>,
        reason: Option<String>,
    ) {
        self.next_event_id = self.next_event_id.wrapping_add(1);
        self.events.push_back(RouteEvent {
            id: self.next_event_id,
            timestamp_unix_secs,
            flow_id: flow_id.map(|value| value.to_string()),
            relay_id,
            protocol,
            phase: phase.to_owned(),
            outcome: outcome.to_owned(),
            latency_ms,
            reason,
        });
        while self.events.len() > MAX_ROUTE_EVENTS {
            self.events.pop_front();
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FlowRouteSnapshot {
    pub flow_id: String,
    pub home_id: String,
    pub service_id: String,
    pub protocol: ServiceProtocol,
    pub local_bind: String,
    pub selected_relay: Option<String>,
    pub started_at_unix_secs: u64,
    pub last_switch_unix_secs: Option<u64>,
    pub switch_count: u32,
    pub uploaded_bytes: u64,
    pub downloaded_bytes: u64,
    pub recovering: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RelayRouteSnapshot {
    pub relay_id: Option<String>,
    pub redacted_endpoint: String,
    pub current_member: bool,
    pub observation: String,
    pub active_flow_count: usize,
    pub last_seen_unix_secs: Option<u64>,
    pub last_success_unix_secs: Option<u64>,
    pub last_failure_unix_secs: Option<u64>,
    pub consecutive_failures: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ControlPlaneSnapshot {
    pub catalog_generation: u64,
    pub relay_directory_generation: u64,
    pub last_accepted_unix_secs: Option<u64>,
    pub catalog_subscription_relay: Option<String>,
    pub connected_relays: Vec<String>,
    pub directory_size: usize,
    pub degraded_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct RouteEvent {
    pub id: u64,
    pub timestamp_unix_secs: u64,
    pub flow_id: Option<String>,
    pub relay_id: Option<String>,
    pub protocol: ServiceProtocol,
    pub phase: String,
    pub outcome: String,
    pub latency_ms: Option<u64>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiagnosticsSnapshot {
    pub generated_at_unix_secs: u64,
    pub flows: Vec<FlowRouteSnapshot>,
    pub relays: Vec<RelayRouteSnapshot>,
    pub control_plane: ControlPlaneSnapshot,
    pub events: Vec<RouteEvent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "frontend")]
struct RotatePrivateKeyPasswordRequest {
    current_password: String,
    new_password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "frontend")]
struct CreateRemoteEnrollmentRequest {
    home_id: String,
    password: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "frontend")]
struct InstallRemoteEnrollmentRequest {
    request_id: Uuid,
    password: String,
}

#[derive(Serialize)]
#[cfg(feature = "frontend")]
struct RemoteEnrollmentStatus {
    request_id: Uuid,
    home_id: String,
    created_at_unix_secs: u64,
    response_received: bool,
    restart_required: bool,
}

#[derive(Serialize)]
#[cfg(feature = "frontend")]
struct InstallRemoteEnrollmentResponse {
    request_id: Uuid,
    credential_id: Uuid,
    restart_required: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "frontend")]
struct MappingIdentityRequest {
    home_id: String,
    service_id: String,
    protocol: ServiceProtocol,
}

#[derive(Serialize)]
#[cfg(feature = "frontend")]
struct RotatePrivateKeyPasswordResponse {
    rotated_keys: usize,
}

#[derive(Serialize)]
#[cfg(feature = "frontend")]
struct ApiError {
    error: String,
}

#[cfg(feature = "frontend")]
type ApiResult<T> = std::result::Result<Json<T>, (StatusCode, Json<ApiError>)>;

struct RouteGrant {
    route_id: Uuid,
    route_secret: Vec<u8>,
    data_addr: String,
}

#[allow(clippy::too_many_lines)]
/// Runs the existing command-line and embedded-Web Travel application.
///
/// # Errors
///
/// Returns an error when configuration, credentials, listeners, or a runtime task fails.
///
/// # Panics
///
/// Panics if Tokio cannot construct the process runtime required by `#[tokio::main]`.
#[tokio::main]
#[cfg(feature = "frontend")]
pub async fn run_cli() -> Result<()> {
    init_crypto();
    tracing_subscriber::fmt()
        .with_ansi(io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flowsplice_travel_core=info".into()),
        )
        .init();
    let cli = Cli::parse();
    if let Some(command) = cli.command {
        return run_command(command).await;
    }
    let state = load_app_state(&cli.config, None)?;

    start_initial_mapping_listeners(&state).await?;

    let mut tasks = JoinSet::new();
    tasks.spawn(run_catalog_subscription(state.clone()));
    tasks.spawn(run_ui(state.clone()));
    tasks.spawn(monitor_trust_expiry(state.clone()));
    while let Some(result) = tasks.join_next().await {
        result??;
    }
    Ok(())
}

fn load_app_state(config_path: &Path, supplied_password: Option<&str>) -> Result<AppState> {
    let (config, state_store, mappings) = load_app_config(config_path)?;
    let deployment_root_public_key = read_public_key(&config.deployment_root_public_key)?;
    let management_identity = local_certificate_identity(&config.management_cert)?;
    require_peer(&management_identity, Role::Travel, Some(&config.id), &[])?;
    let legacy_control_trust_state_path =
        enrollment_sibling(&config.management_cert, CONTROL_TRUST_STATE_FILE);
    let (control_trust_state, deployment_trust, cached_control_snapshot) =
        load_control_trust_state(
            &config,
            &state_store,
            &deployment_root_public_key,
            &legacy_control_trust_state_path,
            &management_identity.spki_sha256,
        )?;
    let relay_history = load_relay_history(&state_store)?;
    if recover_private_key_password_rotation(&travel_key_targets(&config))? {
        info!("completed interrupted Travel private-key password rotation");
    }
    let (management_key, business_key) = load_runtime_private_keys(&config, supplied_password)?;
    let statistics_signer = statistics_signing_key(&management_key)?;
    let statistics_certificate_pem = fs::read_to_string(&config.management_cert)
        .context("failed to read Travel statistics signing certificate")?;
    let (statistics, enrollment_outbox_pending_at_start) = load_statistics_state(&state_store)?;
    let tls = build_tls_material(
        &config.management_cert,
        management_key,
        &config.management_ca,
        &config.business_cert,
        business_key,
        &config.business_ca,
    )?;
    let permits = Arc::new(Semaphore::new(config.max_active_flows));
    let carrier_permits = Arc::new(Semaphore::new(config.max_active_carriers));
    Ok(AppState {
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
        uploaded_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        downloaded_bytes: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        connected_relays: Arc::new(RwLock::new(HashSet::new())),
        network_generation: watch::channel(0).0,
        status_generation: watch::channel(0).0,
        permits,
        carrier_permits,
        flow_relays: Arc::new(Mutex::new(HashMap::new())),
        diagnostics: Arc::new(Mutex::new(DiagnosticsState::default())),
        #[cfg(feature = "frontend")]
        key_operation: Arc::new(Mutex::new(())),
        #[cfg(feature = "frontend")]
        sensitive_operation: Arc::new(Semaphore::new(1)),
        deployment_root_public_key: Arc::new(deployment_root_public_key),
        deployment_trust: Arc::new(RwLock::new(deployment_trust)),
        management_spki_sha256: Arc::new(management_identity.spki_sha256),
        state_store: Arc::new(state_store),
        enrollment_outbox_generation: Arc::new(std::sync::atomic::AtomicU64::new(u64::from(
            enrollment_outbox_pending_at_start,
        ))),
        statistics: Arc::new(statistics),
        statistics_signer: Arc::new(statistics_signer),
        statistics_certificate_pem: Arc::new(statistics_certificate_pem),
        relay_history: Arc::new(RwLock::new(relay_history)),
        control_trust_state: Arc::new(Mutex::new(control_trust_state)),
        mappings: Arc::new(RwLock::new(mappings)),
        mapping_tasks: Arc::new(Mutex::new(HashMap::new())),
        ready_mapping_listeners: Arc::new(RwLock::new(HashMap::new())),
        mapping_operation: Arc::new(Mutex::new(())),
        blocking_operations: watch::channel(0).0,
    })
}

fn load_app_config(config_path: &Path) -> Result<(Config, StateStore, Vec<Mapping>)> {
    let mut config: Config = load_toml(config_path)?;
    validate_config(&config)?;
    let state_store = StateStore::open(&config.state_store)?;
    let mappings = load_runtime_mappings(&config, &state_store)?;
    apply_active_identity_directory(&mut config, &state_store)?;
    Ok((config, state_store, mappings))
}

fn read_public_key(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .context("failed to open deployment root public key")?
        .take(257)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 256 {
        bail!("deployment root public key is too large");
    }
    normalized_trusted_root(
        std::str::from_utf8(&bytes).context("invalid deployment root public key encoding")?,
    )
}

fn load_control_trust_state(
    config: &Config,
    state_store: &StateStore,
    deployment_root_public_key: &str,
    legacy_control_trust_state_path: &Path,
    management_spki_sha256: &str,
) -> Result<(
    ControlTrustState,
    DeploymentTrust,
    Option<VerifiedControlSnapshot>,
)> {
    load_initial_control_trust_state(
        config,
        deployment_root_public_key,
        state_store,
        legacy_control_trust_state_path,
        management_spki_sha256,
    )
}

fn load_statistics_state(state_store: &StateStore) -> Result<(LocalStatistics, bool)> {
    let statistics = LocalStatistics::new(state_store.clone());
    let enrollment_outbox_pending_at_start = !state_store
        .scan_prefix(Table::EnrollmentOutbox, b"")?
        .is_empty();
    Ok((statistics, enrollment_outbox_pending_at_start))
}

fn build_tls_material(
    management_cert: &Path,
    management_key: PrivateKeyDer<'static>,
    management_ca: &Path,
    business_cert: &Path,
    business_key: PrivateKeyDer<'static>,
    business_ca: &Path,
) -> Result<Arc<TlsMaterial>> {
    Ok(Arc::new(TlsMaterial {
        management_connector: identity_client_connector_with_private_key(
            management_cert,
            management_key,
            management_ca,
        )?,
        business_connector: identity_client_connector_with_private_key(
            business_cert,
            business_key,
            business_ca,
        )?,
    }))
}

/// A reusable, UI-independent Travel runtime.
///
/// Native shells own the lifecycle and supply a configuration path plus the private-key
/// password. The runtime owns control subscriptions, local mappings, traffic counters, and
/// orderly shutdown; no terminal input or embedded web server is started.
pub struct TravelCore {
    state: AppState,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    shutdown_lock: Mutex<()>,
    stopped: std::sync::atomic::AtomicBool,
    socket_mode: bool,
    socket_tasks: socket::SocketTasks,
}

impl Drop for TravelCore {
    fn drop(&mut self) {
        // Explicit shutdown drains these tasks. Drop still cancels them if the owner goes away.
        self.socket_tasks.shutdown.send_replace(true);
        for task in self.tasks.get_mut().drain(..) {
            task.abort();
        }
        if let Ok(mut mappings) = self.state.mapping_tasks.try_lock() {
            for (_, task) in mappings.drain() {
                task.shutdown.send_replace(true);
                task.join.abort();
            }
        }
    }
}

impl TravelCore {
    /// Starts the Travel runtime without any CLI or web adapter.
    ///
    /// # Errors
    ///
    /// Returns an error if configuration, state, credentials, or initial mapping listeners fail.
    pub async fn start(config_path: &Path, private_key_password: &str) -> Result<Self> {
        Self::start_inner(config_path, private_key_password, None, false).await
    }

    /// Starts a native runtime only if the installed identity belongs to the packaged deployment.
    ///
    /// # Errors
    /// Returns an error without changing the identity when the trust root differs or is invalid.
    pub async fn start_with_trusted_root(
        config_path: &Path,
        private_key_password: &str,
        trusted_root: &str,
    ) -> Result<Self> {
        let root = normalized_trusted_root(trusted_root)?;
        Self::start_inner(config_path, private_key_password, Some(root), false).await
    }

    /// Starts a business runtime that never opens local listeners, even if legacy mappings exist.
    ///
    /// The caller owns its active-use lifetime and must shut it down when the business view closes.
    ///
    /// # Errors
    /// Returns configuration, identity, trust or password errors.
    pub async fn start_in_process(
        config_path: &Path,
        private_key_password: &str,
        trusted_root: &str,
    ) -> Result<Self> {
        let root = normalized_trusted_root(trusted_root)?;
        Self::start_inner(config_path, private_key_password, Some(root), true).await
    }

    async fn start_inner(
        config_path: &Path,
        private_key_password: &str,
        trusted_root: Option<String>,
        socket_mode: bool,
    ) -> Result<Self> {
        init_crypto();
        let config_path = config_path.to_path_buf();
        let private_key_password = Zeroizing::new(private_key_password.to_owned());
        let state = tokio::task::spawn_blocking(move || {
            if let Some(root) = trusted_root {
                let config: Config = load_toml(&config_path)?;
                require_trusted_root(&read_public_key(&config.deployment_root_public_key)?, &root)?;
            }
            load_app_state(&config_path, Some(private_key_password.as_str()))
        })
        .await
        .context("Travel state loading task failed")??;
        if !socket_mode {
            start_initial_mapping_listeners(&state).await?;
        }
        let catalog_state = state.clone();
        let catalog = tokio::spawn(async move {
            if let Err(error) = run_catalog_subscription(catalog_state).await {
                warn!(%error, "Travel catalog task stopped");
            }
        });
        let trust_state = state.clone();
        let trust = tokio::spawn(async move {
            if let Err(error) = monitor_trust_expiry(trust_state).await {
                warn!(%error, "Travel trust monitor stopped");
            }
        });
        Ok(Self {
            state,
            tasks: Mutex::new(vec![catalog, trust]),
            shutdown_lock: Mutex::new(()),
            stopped: std::sync::atomic::AtomicBool::new(false),
            socket_mode,
            socket_tasks: socket::SocketTasks::default(),
        })
    }

    /// Returns the current runtime state for a native UI or status surface.
    pub async fn status(&self) -> StatusResponse {
        travel_status(&self.state).await
    }

    /// Waits until a status-relevant runtime transition occurs or the caller's refresh deadline
    /// expires, then returns one consistent snapshot. Byte counters intentionally do not wake an
    /// idle observer; shells use a short deadline while flows are active and a long one otherwise.
    pub async fn wait_for_status_change(
        &self,
        known_generation: u64,
        max_wait: Duration,
    ) -> StatusUpdate {
        let mut changes = self.state.status_generation.subscribe();
        let unchanged = *changes.borrow_and_update() == known_generation;
        if unchanged {
            let _ = timeout(max_wait, changes.changed()).await;
        }
        let generation = *changes.borrow_and_update();
        StatusUpdate {
            generation,
            status: travel_status(&self.state).await,
        }
    }

    /// Wakes native status observers so a platform lifecycle operation never waits for an idle
    /// refresh deadline to expire.
    pub fn wake_status_observers(&self) {
        self.state.mark_status_changed();
    }

    /// Returns the signed service catalog currently accepted by the runtime.
    pub async fn catalog(&self) -> Catalog {
        self.state.catalog.read().await.clone()
    }

    /// Returns the signed Relay directory currently accepted by the runtime.
    pub async fn relay_directory(&self) -> RelayDirectory {
        self.state.directory.read().await.clone()
    }

    /// Returns a consistent, privacy-bounded snapshot of current routes and observed Relay state.
    pub async fn diagnostics(&self) -> DiagnosticsSnapshot {
        diagnostics_snapshot(&self.state).await
    }

    /// Immediately retires connections created on the previous default network.
    ///
    /// Native shells call this after their platform reports that the default network changed or
    /// disappeared. Local listeners and flow state remain alive while control and business
    /// connections reconnect through the current route.
    pub fn notify_network_changed(&self) {
        self.state
            .network_generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
        self.state.mark_status_changed();
    }

    /// Creates or updates a local mapping with bind-before-commit semantics.
    ///
    /// # Errors
    ///
    /// Returns an error if the mapping is invalid, cannot bind, or cannot be persisted.
    pub async fn upsert_mapping(&self, mapping: Mapping) -> Result<Mapping> {
        let _lifecycle = self.shutdown_lock.lock().await;
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            bail!("Travel runtime is stopped");
        }
        if self.socket_mode {
            bail!("in-process Travel does not support local listeners");
        }
        upsert_mapping(&self.state, mapping).await
    }

    /// Deletes a local mapping and its listener.
    ///
    /// # Errors
    ///
    /// Returns an error if the mapping is unknown or the updated set cannot be persisted.
    pub async fn delete_mapping(
        &self,
        home_id: String,
        service_id: String,
        protocol: ServiceProtocol,
    ) -> Result<Mapping> {
        let _lifecycle = self.shutdown_lock.lock().await;
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            bail!("Travel runtime is stopped");
        }
        if self.socket_mode {
            bail!("in-process Travel does not manage local mappings");
        }
        delete_mapping(&self.state, home_id, service_id, protocol).await
    }

    /// Stops background subscriptions and all local mapping listeners.
    pub async fn shutdown(&self) {
        let _lifecycle = self.shutdown_lock.lock().await;
        if self.stopped.swap(true, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        self.state.mark_status_changed();
        self.socket_tasks.shutdown().await;
        let tasks = {
            let mut tasks = self.tasks.lock().await;
            std::mem::take(&mut *tasks)
        };
        for task in &tasks {
            task.abort();
        }
        for task in tasks {
            let _ = task.await;
        }
        let mappings = {
            let mut tasks = self.state.mapping_tasks.lock().await;
            tasks.drain().map(|(_, task)| task).collect::<Vec<_>>()
        };
        for task in mappings {
            stop_mapping_task(task).await;
        }
        let mut blocking = self.state.blocking_operations.subscribe();
        let _ = blocking.wait_for(|count| *count == 0).await;
        self.state.connected_relays.write().await.clear();
        self.state.flow_relays.lock().await.clear();
        self.state.diagnostics.lock().await.flows.clear();
        self.state.mark_status_changed();
        // Persist the final deltas even when the control connection is offline.
        if let Err(error) = flush_travel_statistics(&self.state).await {
            warn!(%error, "Travel statistics final flush failed");
        }
    }
}

async fn monitor_trust_expiry(state: AppState) -> Result<()> {
    loop {
        if unix_time_secs()? >= state.deployment_trust.read().await.not_after_unix_secs {
            bail!("deployment trust expired; refusing to continue");
        }
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

#[cfg(feature = "frontend")]
async fn run_command(command: Command) -> Result<()> {
    match command {
        Command::EnrollRemote(args) => run_remote_enrollment_cli(args).await,
        Command::CheckBootstrapConfig { config } => {
            let bootstrap = load_travel_bootstrap(&config)?;
            if bootstrap.bootstrap_relays.is_empty() {
                bail!("Travel bootstrap configuration has no Relay address");
            }
            println!(
                "Travel bootstrap configuration is valid for deployment {} generation {} with {} Relay(s)",
                bootstrap.trust.deployment_id,
                bootstrap.trust.generation,
                bootstrap.bootstrap_relays.len()
            );
            Ok(())
        }
    }
}

fn load_travel_bootstrap(path: &Path) -> Result<VerifiedTravelBootstrap> {
    let configured: TravelBootstrapConfig = load_toml(path)?;
    let deployment_root_public_key_path =
        resolve_path(path, &configured.deployment_root_public_key);
    let deployment_trust_path = resolve_path(path, &configured.deployment_trust);
    let (deployment_root_public_key, signed_trust, trust) = load_verified_deployment_trust(
        &deployment_root_public_key_path,
        &deployment_trust_path,
        unix_time_secs()?,
    )?;
    let ui_listen: SocketAddr = configured
        .ui_listen
        .parse()
        .context("invalid Travel bootstrap UI listener")?;
    if !ui_listen.ip().is_loopback() {
        bail!("Travel bootstrap UI must listen directly on a loopback address");
    }
    let mut bootstrap_relays = configured.bootstrap_relays;
    bootstrap_relays.sort();
    bootstrap_relays.dedup();
    for relay in &bootstrap_relays {
        if !valid_connect_address(relay) {
            bail!("invalid bootstrap Relay address {relay}");
        }
    }
    let mut override_ids = HashSet::new();
    for relay in &configured.relay_address_overrides {
        if relay.id.is_empty()
            || !override_ids.insert(relay.id.clone())
            || !valid_connect_address(&relay.management_addr)
            || !valid_connect_address(&relay.data_addr)
        {
            bail!("invalid or duplicate bootstrap Relay address override");
        }
    }
    Ok(VerifiedTravelBootstrap {
        deployment_root_public_key,
        signed_trust,
        trust,
        bootstrap_relays,
        relay_address_overrides: configured.relay_address_overrides,
        ui_listen: configured.ui_listen,
    })
}

fn validate_bootstrap_trust_continuity(
    baseline_signed: &SignedDeploymentTrust,
    baseline: &DeploymentTrust,
    candidate_signed: &SignedDeploymentTrust,
    candidate: &DeploymentTrust,
) -> Result<()> {
    if candidate.deployment_id != baseline.deployment_id {
        bail!("enrollment response belongs to a different deployment");
    }
    if candidate.generation < baseline.generation {
        bail!("enrollment response would roll back bootstrap deployment trust");
    }
    if candidate.generation == baseline.generation
        && candidate_signed.payload_digest_sha256()? != baseline_signed.payload_digest_sha256()?
    {
        bail!("enrollment response conflicts with bootstrap deployment trust");
    }
    Ok(())
}

#[cfg(feature = "frontend")]
async fn run_remote_enrollment_cli(args: EnrollRemoteArgs) -> Result<()> {
    let password = if let Some(path) = args.test_password_file.as_deref() {
        test_password(path)?
    } else {
        prompt_new_private_key_password()?
    };
    let ui_listen = if args.bootstrap_config.is_none() {
        #[cfg(feature = "e2e-remote-ui")]
        if args.test_allow_remote_listen {
            Some("0.0.0.0:9080".to_owned())
        } else {
            Some("127.0.0.1:9080".to_owned())
        }
        #[cfg(not(feature = "e2e-remote-ui"))]
        Some("127.0.0.1:9080".to_owned())
    } else {
        None
    };
    let options = RemoteEnrollmentOptions {
        travel_id: args.travel_id,
        home_id: args.home_id,
        install_dir: args.install_dir,
        bootstrap_config: args.bootstrap_config,
        trusted_deployment_root_public_key: args
            .deployment_root_public_key
            .as_deref()
            .map(read_public_key)
            .transpose()?,
        selected_relay: args.relay,
        ui_listen,
        private_key_password: password.to_string(),
        wait_timeout_secs: args.wait_timeout_secs,
        #[cfg(feature = "e2e-remote-ui")]
        test_allow_remote_listen: args.test_allow_remote_listen,
        #[cfg(feature = "e2e-remote-ui")]
        test_admin_token: args.test_admin_token,
    };
    enroll_remote(options, |progress| match progress.phase {
        RemoteEnrollmentPhase::Preparing => {}
        RemoteEnrollmentPhase::WaitingForApproval => {
            if let (Some(request_id), Some(code)) =
                (progress.request_id, progress.verification_code)
            {
                println!("first enrollment request: {request_id}");
                println!("Home verification code: {code}");
                println!(
                    "Open the local Home page, compare this code, select scope/validity, and approve with the Home issuance password."
                );
            }
        }
        RemoteEnrollmentPhase::Installed => {
            if let Some(credential_id) = progress.credential_id {
                println!("installed Travel credential {credential_id}");
            }
            if let Some(config_path) = progress.config_path {
                println!("configuration: {}", config_path.display());
                println!(
                    "start: flowsplice-travelagent --config {}",
                    config_path.display()
                );
            }
        }
    })
    .await
}

/// Completes first-device remote enrollment and atomically installs a runnable Travel identity.
///
/// The callback receives resumable progress snapshots and can safely update a native UI. Private
/// keys are generated inside `install_dir` and never included in the public enrollment request.
///
/// # Errors
///
/// Returns an error for invalid inputs or bootstrap trust, conflicting resumable state, network or
/// approval timeout, rejected enrollment, failed response verification, or failed durable install.
#[allow(clippy::too_many_lines)]
pub async fn enroll_remote<F>(options: RemoteEnrollmentOptions, on_progress: F) -> Result<()>
where
    F: Fn(RemoteEnrollmentProgress) + Send + Sync,
{
    enroll_remote_inner(options, None, on_progress).await
}

#[allow(clippy::too_many_lines)]
async fn enroll_remote_inner<F>(
    options: RemoteEnrollmentOptions,
    business: Option<flowsplice_core::business::BusinessDescriptor>,
    on_progress: F,
) -> Result<()>
where
    F: Fn(RemoteEnrollmentProgress) + Send + Sync,
{
    init_crypto();
    if options.wait_timeout_secs == 0 {
        bail!("wait-timeout-secs must be positive");
    }
    if options.home_id.is_empty() || options.home_id.len() > 128 {
        bail!("Home id must be non-empty and no longer than 128 bytes");
    }
    let password = Zeroizing::new(options.private_key_password);
    if password.chars().count() < MIN_PRIVATE_KEY_PASSWORD_CHARACTERS {
        bail!(
            "private-key password must contain at least {MIN_PRIVATE_KEY_PASSWORD_CHARACTERS} characters"
        );
    }
    on_progress(RemoteEnrollmentProgress {
        phase: RemoteEnrollmentPhase::Preparing,
        travel_id: options.travel_id.clone(),
        request_id: None,
        verification_code: None,
        config_path: None,
        credential_id: None,
    });
    let selected_relay = options
        .selected_relay
        .as_deref()
        .map(str::trim)
        .filter(|relay| !relay.is_empty())
        .map(str::to_owned);
    if let Some(relay) = selected_relay.as_deref()
        && !valid_connect_address(relay)
    {
        bail!("invalid selected Relay address {relay}");
    }
    let bootstrap = if let Some(config) = options.bootstrap_config.as_deref() {
        let bootstrap = load_travel_bootstrap(config)?;
        if let Some(root) = options.trusted_deployment_root_public_key.as_deref() {
            require_trusted_root(&bootstrap.deployment_root_public_key, root)?;
        }
        bootstrap
    } else {
        let relay = selected_relay
            .as_deref()
            .ok_or_else(|| anyhow!("remote enrollment requires a Relay address"))?;
        let root = options
            .trusted_deployment_root_public_key
            .as_deref()
            .ok_or_else(|| {
                anyhow!("a trusted deployment public key is required before Relay discovery")
            })?;
        discover_bootstrap_relay(relay, root).await?
    };
    let root_public_key = bootstrap.deployment_root_public_key.as_str();
    let business_target = business
        .as_ref()
        .map(|descriptor| descriptor.verify(&bootstrap.trust, unix_time_secs()?))
        .transpose()?;
    if business_target
        .as_ref()
        .is_some_and(|target| target.approving_home_id != options.home_id)
    {
        bail!("business descriptor does not match its approving Home");
    }
    let management_ca = &bootstrap.trust.management_ca_certificate_pem;
    let bootstrap_relays = bootstrap.bootstrap_relays.clone();
    let polling_relays = if let Some(selected) = selected_relay {
        vec![selected]
    } else {
        bootstrap_relays.clone()
    };
    if polling_relays.is_empty() {
        bail!("remote enrollment requires a Relay address");
    }
    if options.install_dir.exists() && !options.install_dir.is_dir() {
        bail!(
            "Travel install path is not a directory: {}",
            options.install_dir.display()
        );
    }
    fs::create_dir_all(&options.install_dir).with_context(|| {
        format!(
            "failed to create Travel install directory {}",
            options.install_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&options.install_dir, fs::Permissions::from_mode(0o700))?;
    }
    let install_root = options
        .install_dir
        .canonicalize()
        .context("failed to resolve Travel install directory")?;
    let enrollment_dir = install_root.join("cert");
    let bootstrap_state_path = install_root.join("bootstrap-enrollment.json");
    let config_path = install_root.join("travelagent.toml");
    let binding_path = install_root.join(flowsplice_enrollment::business::BUSINESS_BINDING_FILE);
    if binding_path.exists()
        && let Some(descriptor) = &business
    {
        let approved = business::load_binding(&config_path, root_public_key, descriptor)?;
        if approved.travel_id != options.travel_id {
            bail!("completed business installation belongs to a different Travel identity");
        }
        // Verify the supplied password and all installed runtime resources without starting I/O.
        drop(load_app_state(&config_path, Some(password.as_str()))?);
        if bootstrap_state_path.exists() {
            let pending: BootstrapEnrollmentState = load_json(&bootstrap_state_path)?;
            if pending.request_id != approved.request_id
                || pending.business.as_ref() != Some(descriptor)
            {
                bail!("completed business binding conflicts with leftover enrollment state");
            }
            fs::remove_file(&bootstrap_state_path)?;
        }
        let journal_path = install_root.join(business::INSTALL_JOURNAL_FILE);
        if journal_path.exists() {
            let pending: business::InstallJournal = load_json(&journal_path)?;
            if pending.response.request.request.request_id != approved.request_id
                || pending.response.request.descriptor != *descriptor
            {
                bail!("completed business binding conflicts with leftover installation journal");
            }
            fs::remove_file(journal_path)?;
        }
        on_progress(RemoteEnrollmentProgress {
            phase: RemoteEnrollmentPhase::Installed,
            travel_id: options.travel_id,
            request_id: Some(approved.request_id),
            verification_code: None,
            config_path: Some(config_path),
            credential_id: Some(approved.credential_id),
        });
        return Ok(());
    }
    if config_path.exists() && business.is_none() {
        bail!(
            "Travel configuration already exists: {}",
            config_path.display()
        );
    }
    let (request, retrieval_token) = if bootstrap_state_path.exists() {
        let state: BootstrapEnrollmentState = load_json(&bootstrap_state_path)?;
        if state.version != REMOTE_ENROLLMENT_VERSION || state.home_id != options.home_id {
            bail!("existing first-enrollment state conflicts with the requested Home");
        }
        if state.business != business {
            bail!("resumed enrollment changed the requested business descriptor");
        }
        let request: TravelEnrollmentRequest = load_json(&enrollment_dir.join(REQUEST_FILE))?;
        if request.request_id != state.request_id || request.travel_id != options.travel_id {
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
            &options.travel_id,
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
                home_id: options.home_id.clone(),
                request_id: request.request_id,
                retrieval_token_hex: hex::encode(&token),
                business: business.clone(),
            },
        )?;
        (request, token)
    };
    let request_id = request.request_id;
    let request_envelope = match &business {
        Some(descriptor) => flowsplice_enrollment::business::TravelRequestEnvelope::Business(
            Box::new(flowsplice_enrollment::business::BusinessTravelRequest {
                version: flowsplice_core::business::BUSINESS_VERSION,
                object_type: flowsplice_enrollment::business::BUSINESS_TRAVEL_REQUEST_TYPE
                    .to_owned(),
                request: request.clone(),
                descriptor: descriptor.clone(),
            }),
        ),
        None => flowsplice_enrollment::business::TravelRequestEnvelope::Legacy(request.clone()),
    };
    let request_json = serde_json::to_vec(&request_envelope)?;
    let verification_code = bootstrap_verification_code(&request_json, &retrieval_token);
    on_progress(RemoteEnrollmentProgress {
        phase: RemoteEnrollmentPhase::WaitingForApproval,
        travel_id: options.travel_id.clone(),
        request_id: Some(request_id),
        verification_code: Some(verification_code),
        config_path: None,
        credential_id: None,
    });

    let connector = identity_server_auth_connector_from_ca_pem(management_ca)?;
    let deadline = Instant::now() + Duration::from_secs(options.wait_timeout_secs);
    let mut last_error = None;
    let journal_path = install_root.join(business::INSTALL_JOURNAL_FILE);
    let journal: Option<business::InstallJournal> = if business.is_some() && journal_path.exists() {
        Some(load_json(&journal_path)?)
    } else {
        None
    };
    let (response, mut seed_relays) = if let Some(journal) = &journal {
        (
            flowsplice_enrollment::business::TravelResponseEnvelope::Business(Box::new(
                journal.response.clone(),
            )),
            Vec::new(),
        )
    } else {
        'outer: loop {
            for relay in &polling_relays {
                let expected_relay_id = bootstrap
                    .relay_address_overrides
                    .iter()
                    .find(|candidate| candidate.management_addr.as_str() == relay.as_str())
                    .map(|candidate| candidate.id.as_str());
                match poll_bootstrap_relay(
                    relay,
                    expected_relay_id,
                    &connector,
                    &request,
                    &options.home_id,
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
        }
    };

    let (credential, trust) = match (&request_envelope, &response) {
        (
            flowsplice_enrollment::business::TravelRequestEnvelope::Business(request),
            flowsplice_enrollment::business::TravelResponseEnvelope::Business(response),
        ) => {
            if **request != response.request {
                bail!("business enrollment response changed its complete request");
            }
            response.validate(root_public_key, unix_time_secs()?)?
        }
        (
            flowsplice_enrollment::business::TravelRequestEnvelope::Legacy(request),
            flowsplice_enrollment::business::TravelResponseEnvelope::Legacy(response),
        ) => {
            if request != &response.approval.request {
                bail!("enrollment response changed its request");
            }
            validate_enrollment_response(response, root_public_key, unix_time_secs()?)?
        }
        _ => bail!("enrollment response changed the requested business/legacy format"),
    };
    validate_bootstrap_trust_continuity(
        &bootstrap.signed_trust,
        &bootstrap.trust,
        &response.deployment_trust,
        &trust,
    )?;
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
    write_or_verify_private(
        &enrollment_dir.join("deployment-root.pub"),
        format!("{}\n", root_public_key.trim()).as_bytes(),
    )?;
    seed_relays.retain(|relay| valid_connect_address(relay));
    seed_relays.extend(polling_relays);
    seed_relays.sort();
    seed_relays.dedup();

    let state_store_path = install_root.join("state/travel-state.redb");
    let generated = InstalledTravelConfig {
        id: options.travel_id.clone(),
        deployment_root_public_key: enrollment_dir.join("deployment-root.pub"),
        deployment_trust: enrollment_dir.join(DEPLOYMENT_TRUST_FILE),
        management_cert: enrollment_dir.join(MANAGEMENT_CERT_FILE),
        management_key: enrollment_dir.join(MANAGEMENT_KEY_FILE),
        management_ca: enrollment_dir.join(MANAGEMENT_CA_FILE),
        business_cert: enrollment_dir.join(BUSINESS_CERT_FILE),
        business_key: enrollment_dir.join(BUSINESS_KEY_FILE),
        business_ca: enrollment_dir.join(BUSINESS_CA_FILE),
        state_store: state_store_path.clone(),
        enrollment_work_dir: install_root.join("state/enrollment"),
        ui_listen: options.ui_listen.unwrap_or(bootstrap.ui_listen),
        #[cfg(feature = "e2e-remote-ui")]
        test_allow_remote_listen: options.test_allow_remote_listen,
        #[cfg(feature = "e2e-remote-ui")]
        test_admin_token: options.test_admin_token,
        homes: vec![InstalledHome {
            id: business_target
                .as_ref()
                .map_or_else(|| options.home_id.clone(), |target| target.home_id.clone()),
        }],
        seed_relays: seed_relays
            .into_iter()
            .map(|management_addr| SeedRelayOutput { management_addr })
            .collect(),
        relay_address_overrides: bootstrap.relay_address_overrides,
    };
    let encoded = if let Some(journal) = &journal {
        journal.config_toml.clone()
    } else {
        toml::to_string_pretty(&generated).context("failed to encode Travel config")?
    };
    if let flowsplice_enrollment::business::TravelResponseEnvelope::Business(response) = &response {
        if journal.is_none() {
            business::write_atomic_private(
                &journal_path,
                &serde_json::to_vec_pretty(&business::InstallJournal {
                    response: (**response).clone(),
                    config_toml: encoded.clone(),
                })?,
            )?;
        }
        if config_path.exists() {
            if fs::read(&config_path)? != encoded.as_bytes() {
                bail!("existing business configuration conflicts with its pending installation");
            }
        } else {
            business::write_atomic_private(&config_path, encoded.as_bytes())?;
        }
    } else {
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
    }
    let store = StateStore::open(&state_store_path)?;
    store.apply_immediate(WriteBatch::new().put_json(
        Table::EnrollmentOutbox,
        request_id.as_bytes().to_vec(),
        &RemoteEnrollmentOutboxRecord {
            version: REMOTE_ENROLLMENT_VERSION,
            home_id: options.home_id,
            enrollment_dir,
            request,
            response: Some((*response).clone()),
            created_at_unix_secs: unix_time_secs()?,
            last_attempt_unix_secs: None,
            restart_required: true,
            installed_credential_id: Some(credential.credential_id),
            installed_at_unix_secs: Some(unix_time_secs()?),
        },
    )?)?;
    drop(store);
    if let flowsplice_enrollment::business::TravelResponseEnvelope::Business(response) = &response {
        // Initialize/read back durable trust high-water and validate keys before publishing completion.
        drop(load_app_state(&config_path, Some(password.as_str()))?);
        let marker = business::CompletedBinding {
            version: flowsplice_core::business::BUSINESS_VERSION,
            config_sha256: sha256_hex(encoded.as_bytes()),
            response: (**response).clone(),
        };
        business::validate_installed_binding(
            &config_path,
            &marker,
            root_public_key,
            &response.request.descriptor,
        )?;
        business::write_atomic_private(&binding_path, &serde_json::to_vec_pretty(&marker)?)?;
    }
    fs::remove_file(&bootstrap_state_path).with_context(|| {
        format!(
            "failed to remove completed bootstrap state {}",
            bootstrap_state_path.display()
        )
    })?;
    if journal_path.exists() && business.is_some() {
        fs::remove_file(&journal_path)?;
    }
    on_progress(RemoteEnrollmentProgress {
        phase: RemoteEnrollmentPhase::Installed,
        travel_id: options.travel_id,
        request_id: Some(request_id),
        verification_code: None,
        config_path: Some(config_path),
        credential_id: Some(credential.credential_id),
    });
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

fn write_or_verify_private(path: &Path, data: &[u8]) -> Result<()> {
    if path.exists() {
        if fs::read(path)? != data {
            bail!(
                "existing file conflicts with bootstrap configuration: {}",
                path.display()
            );
        }
        return Ok(());
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

fn normalized_trusted_root(root: &str) -> Result<String> {
    let root = root.trim();
    if root.len() != 130 || !root.starts_with("04") || !root.bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("invalid trusted deployment public key; expected a P-256 public point");
    }
    Ok(root.to_ascii_lowercase())
}

fn require_trusted_root(received: &str, trusted: &str) -> Result<String> {
    let trusted = normalized_trusted_root(trusted)?;
    if normalized_trusted_root(received)? != trusted {
        bail!("deployment trust mismatch; use the app package for this deployment");
    }
    Ok(trusted)
}

async fn discover_bootstrap_relay(
    relay: &str,
    trusted_root: &str,
) -> Result<VerifiedTravelBootstrap> {
    let trusted_root = normalized_trusted_root(trusted_root)?;
    let socket = timeout(Duration::from_secs(10), TcpStream::connect(relay))
        .await
        .context("Relay discovery TCP connection timed out")??;
    socket.set_nodelay(true)?;
    let connector = bootstrap_discovery_connector();
    let handshake = timeout(
        Duration::from_secs(10),
        connector.connect(identity_server_name()?, socket),
    )
    .await
    .context("Relay discovery TLS handshake timed out")?;
    let mut stream = handshake.context("Relay discovery TLS handshake failed")?;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Relay, None, &[])?;
    write_json(
        &mut stream,
        &ControlMessage::BootstrapDiscoveryRequest {
            protocol_version: CONTROL_PROTOCOL_VERSION,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    let result = JsonFrameReader::new(&mut stream, CONTROL_FRAME_LIMIT)
        .read_with_timeout::<ControlMessage>(Duration::from_secs(20))
        .await
        .context("Relay discovery response failed")?;
    let ControlMessage::BootstrapDiscoveryResult {
        protocol_version,
        deployment_root_public_key,
        deployment_trust_json,
        relay_data_addr,
    } = result
    else {
        bail!("Relay returned an unexpected discovery response");
    };
    if protocol_version != CONTROL_PROTOCOL_VERSION {
        bail!("Relay returned an unsupported discovery protocol version");
    }
    let deployment_root_public_key =
        require_trusted_root(&deployment_root_public_key, &trusted_root)?;
    let signed_trust: SignedDeploymentTrust = serde_json::from_slice(&deployment_trust_json)
        .context("Relay discovery returned invalid deployment trust")?;
    let trust = signed_trust.verify(&deployment_root_public_key, unix_time_secs()?)?;
    verify_discovery_certificate(
        stream.get_ref().1.peer_certificates(),
        &trust.management_ca_certificate_pem,
    )?;
    if trust.home_endpoints.is_empty() {
        bail!("discovered deployment trust has no Home");
    }
    if !valid_connect_address(&relay_data_addr) {
        bail!("Relay discovery returned an invalid data address");
    }
    Ok(VerifiedTravelBootstrap {
        deployment_root_public_key,
        signed_trust,
        trust,
        bootstrap_relays: vec![relay.to_owned()],
        relay_address_overrides: vec![RelayAddressOverride {
            id: identity.id,
            management_addr: relay.to_owned(),
            data_addr: relay_data_addr,
            bootstrap_hint: true,
        }],
        ui_listen: "127.0.0.1:0".to_owned(),
    })
}

async fn poll_bootstrap_relay(
    relay: &str,
    expected_relay_id: Option<&str>,
    connector: &TlsConnector,
    request: &TravelEnrollmentRequest,
    home_id: &str,
    retrieval_token: &[u8],
    request_json: &[u8],
) -> Result<
    Option<(
        flowsplice_enrollment::business::TravelResponseEnvelope,
        Vec<String>,
    )>,
> {
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
    require_peer(&identity, Role::Relay, expected_relay_id, &[])?;
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
    let signed: SignedDeploymentTrust = load_json(&config.deployment_trust)?;
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

#[cfg(feature = "frontend")]
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
    supplied_password: Option<&str>,
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
    let password = supplied_password.map_or_else(runtime_password, |password| {
        if password.is_empty() {
            bail!("private-key password must not be empty");
        }
        Ok(Zeroizing::new(password.to_owned()))
    })?;
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
    #[cfg(not(feature = "frontend"))]
    bail!("a supplied private-key password is required without the frontend feature");
    #[cfg(feature = "frontend")]
    {
        let password = Zeroizing::new(rpassword::prompt_password("Travel private-key password: ")?);
        if password.is_empty() {
            bail!("private-key password must not be empty");
        }
        Ok(password)
    }
}

#[cfg(feature = "frontend")]
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
    configured_home_ids(&config.homes)?;
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
        if !valid_connect_address(&relay.management_addr)
            || !seed_relays.insert(&relay.management_addr)
        {
            bail!("seed Relay addresses must be non-empty and unique");
        }
    }
    let mut override_ids = HashSet::new();
    for relay in &config.relay_address_overrides {
        if relay.id.is_empty()
            || !override_ids.insert(&relay.id)
            || !valid_connect_address(&relay.management_addr)
            || !valid_connect_address(&relay.data_addr)
        {
            bail!("Relay address overrides must have unique ids and valid addresses");
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
    if !ui_addr.ip().is_loopback() && !test_remote_ui_enabled(config) {
        bail!("Travel UI must listen directly on a loopback address");
    }
    validate_mapping_set(config, &config.mappings)?;
    Ok(())
}

fn validate_mapping_set(config: &Config, mappings: &[Mapping]) -> Result<()> {
    let home_ids = configured_home_ids(&config.homes)?;
    let mut services = HashSet::new();
    let mut binds = HashSet::new();
    for mapping in mappings {
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

fn load_runtime_mappings(config: &Config, store: &StateStore) -> Result<Vec<Mapping>> {
    if let Some(mappings) =
        store.get_json::<Vec<Mapping>>(Table::TravelMappings, TRAVEL_MAPPINGS_KEY)?
    {
        validate_mapping_set(config, &mappings)?;
        return Ok(mappings);
    }
    let mappings = config.mappings.clone();
    persist_runtime_mappings(store, &mappings)?;
    Ok(mappings)
}

fn persist_runtime_mappings(store: &StateStore, mappings: &[Mapping]) -> Result<()> {
    store.apply_immediate(WriteBatch::new().put_json(
        Table::TravelMappings,
        TRAVEL_MAPPINGS_KEY.to_vec(),
        &mappings,
    )?)
}

fn mapping_key(mapping: &Mapping) -> String {
    let protocol = match mapping.protocol {
        ServiceProtocol::Tcp => "tcp",
        ServiceProtocol::Udp => "udp",
    };
    format!("{}\0{}\0{protocol}", mapping.home_id, mapping.service_id)
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
    let mut network_changes = state.network_generation.subscribe();
    let mut retry_backoff = Duration::from_secs(1);
    loop {
        let mut connected = false;
        for relay in bootstrap_candidates(&state).await {
            match open_management(&state, &relay, TravelConnectionPurpose::Catalog).await {
                Ok((stream, relay_id, relay_spki)) => {
                    info!(%relay_id, relay = %relay.management_addr, "catalog subscription connected");
                    connected = true;
                    let relay_became_connected = state
                        .connected_relays
                        .write()
                        .await
                        .insert(relay_id.clone());
                    if relay_became_connected {
                        state.mark_status_changed();
                    }
                    if let Err(error) =
                        run_catalog_session(&state, &relay_id, &relay_spki, stream).await
                    {
                        warn!(%relay_id, %error, "catalog subscription disconnected");
                    }
                    if state.connected_relays.write().await.remove(&relay_id) {
                        state.mark_status_changed();
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
        let delay = jittered_retry_delay(
            if connected {
                Duration::from_secs(1)
            } else {
                retry_backoff
            },
            uuid_seed(state.session_id) ^ *network_changes.borrow(),
        );
        tokio::select! {
            () = sleep(delay) => {
                retry_backoff = if connected {
                    Duration::from_secs(1)
                } else {
                    retry_backoff.saturating_mul(2).min(Duration::from_secs(60))
                };
            }
            changed = network_changes.changed() => {
                changed.context("network change notifier stopped")?;
                retry_backoff = Duration::from_secs(1);
            }
        }
    }
}

fn jittered_retry_delay(base: Duration, seed: u64) -> Duration {
    let spread = (base.as_millis() / 5).max(1);
    let mixed = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(17);
    let offset = u128::from(mixed) % (spread.saturating_mul(2) + 1);
    let millis = base
        .as_millis()
        .saturating_sub(spread)
        .saturating_add(offset);
    Duration::from_millis(u64::try_from(millis).unwrap_or(u64::MAX))
}

fn uuid_seed(id: Uuid) -> u64 {
    let value = id.as_u128();
    let low = u64::try_from(value & u128::from(u64::MAX)).unwrap_or_default();
    let high = u64::try_from(value >> 64).unwrap_or_default();
    low ^ high
}

async fn run_catalog_session(
    state: &AppState,
    relay_id: &str,
    relay_spki: &str,
    stream: TlsStream<TcpStream>,
) -> Result<()> {
    let mut network_changes = state.network_generation.subscribe();
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    let mut heartbeat = interval(Duration::from_secs(10));
    let mut statistics_tick = interval(Duration::from_secs(5));
    let mut enrollment_tick = interval(Duration::from_secs(5));
    let mut report_keys = HashMap::<String, Vec<u8>>::new();
    let mut enrollment_inflight = None::<(Uuid, Instant)>;
    let mut enrollment_generation_checked = 0_u64;
    let mut enrollment_pending = state.has_enrollment_outbox_work();
    // Each connection gets one fresh outbox scan, including ACK loss in an earlier session.
    let mut statistics_pending = true;
    let mut nonce = 0_u64;
    let mut last_received = Instant::now();
    loop {
        tokio::select! {
            changed = network_changes.changed() => {
                changed.context("network change notifier stopped")?;
                bail!("default network changed");
            }
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
                                state.spawn_blocking(move || statistics.acknowledge_report(&key, &digest))
                                    .await
                                    .context("Travel statistics acknowledgement task failed")??;
                            }
                        } else {
                            report_keys.remove(&digest_sha256);
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
                flush_and_send_travel_statistics(
                    state,
                    &mut writer,
                    &mut report_keys,
                    &mut statistics_pending,
                )
                .await?;
            }
            _ = enrollment_tick.tick() => {
                poll_remote_enrollment(
                    state,
                    &mut writer,
                    &mut enrollment_pending,
                    &mut enrollment_generation_checked,
                    &mut enrollment_inflight,
                )
                .await?;
            }
        }
    }
}

async fn poll_remote_enrollment<W: tokio::io::AsyncWrite + Unpin>(
    state: &AppState,
    writer: &mut W,
    known_pending: &mut bool,
    checked_generation: &mut u64,
    in_flight: &mut Option<(Uuid, Instant)>,
) -> Result<()> {
    if in_flight.is_some_and(|(_, started)| started.elapsed() > Duration::from_secs(30)) {
        *in_flight = None;
    }
    let outbox_generation = state
        .enrollment_outbox_generation
        .load(std::sync::atomic::Ordering::Acquire);
    if !enrollment_outbox_scan_needed(
        *known_pending,
        outbox_generation,
        *checked_generation,
        in_flight.is_some(),
    ) {
        return Ok(());
    }
    let poll = next_remote_enrollment_message(state).await?;
    *known_pending = poll.pending;
    if !poll.pending {
        *checked_generation = outbox_generation;
    }
    if let Some((request_id, message)) = poll.message {
        write_json(writer, &message, CONTROL_FRAME_LIMIT).await?;
        *in_flight = Some((request_id, Instant::now()));
    }
    Ok(())
}

fn enrollment_outbox_scan_needed(
    known_pending: bool,
    current_generation: u64,
    checked_generation: u64,
    in_flight: bool,
) -> bool {
    !in_flight && (known_pending || current_generation != checked_generation)
}

struct EnrollmentOutboxPoll {
    message: Option<(Uuid, ControlMessage)>,
    pending: bool,
}

async fn next_remote_enrollment_message(state: &AppState) -> Result<EnrollmentOutboxPoll> {
    let store = Arc::clone(&state.state_store);
    let travel_id = state.config.id.clone();
    let travel_session_id = state.session_id;
    let management_spki_sha256 = Arc::clone(&state.management_spki_sha256);
    state
        .spawn_blocking(move || {
            let now = unix_time_secs()?;
            let mut batch = WriteBatch::new();
            let mut pending = false;
            let mut selected = None;
            for (key, value) in store.scan_prefix(Table::EnrollmentOutbox, b"")? {
                let Ok(mut record) = serde_json::from_slice::<RemoteEnrollmentOutboxRecord>(&value)
                else {
                    warn!(?key, "ignored malformed remote enrollment outbox record");
                    continue;
                };
                if remote_enrollment_outbox_expired(
                    record.created_at_unix_secs,
                    record.installed_at_unix_secs,
                    now,
                ) {
                    batch = batch.delete(Table::EnrollmentOutbox, key);
                    continue;
                }
                if record.version != REMOTE_ENROLLMENT_VERSION {
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
                pending = true;
                if selected.is_some()
                    || record
                        .last_attempt_unix_secs
                        .is_some_and(|attempt| now.saturating_sub(attempt) < 10)
                {
                    continue;
                }
                record.last_attempt_unix_secs = Some(now);
                batch = batch.put_json(Table::EnrollmentOutbox, key, &record)?;
                selected = Some((request_id, message));
            }
            if !batch.is_empty() {
                store.apply_immediate(batch)?;
            }
            Ok(EnrollmentOutboxPoll {
                message: selected,
                pending,
            })
        })
        .await
        .context("Travel enrollment outbox query task failed")?
}

#[cfg(feature = "frontend")]
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
    state
        .spawn_blocking(move || {
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
    state.mark_enrollment_outbox_changed();
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
    state
        .spawn_blocking(move || {
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
    state.mark_enrollment_outbox_changed();
    info!(%request_id, "received and verified remote enrollment response");
    Ok(())
}

async fn flush_travel_statistics(state: &AppState) -> Result<usize> {
    let statistics = Arc::clone(&state.statistics);
    let deployment_id = state.deployment_trust.read().await.deployment_id.clone();
    let reporter_id = state.config.id.clone();
    let certificate_pem = Arc::clone(&state.statistics_certificate_pem);
    let signer = Arc::clone(&state.statistics_signer);
    let staged = state
        .spawn_blocking(move || {
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
    Ok(staged)
}

async fn flush_and_send_travel_statistics<W: tokio::io::AsyncWrite + Unpin>(
    state: &AppState,
    writer: &mut W,
    report_keys: &mut HashMap<String, Vec<u8>>,
    known_pending: &mut bool,
) -> Result<()> {
    let staged = flush_travel_statistics(state).await?;
    if !statistics_outbox_scan_needed(staged, *known_pending, !report_keys.is_empty()) {
        return Ok(());
    }
    let statistics = Arc::clone(&state.statistics);
    let reports = state
        .spawn_blocking(move || statistics.pending_reports(16))
        .await
        .context("Travel statistics outbox task failed")??;
    *known_pending = !reports.is_empty();
    for (key, report) in reports {
        let digest = report.digest_sha256()?;
        if report_keys.contains_key(&digest) {
            continue;
        }
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

fn statistics_outbox_scan_needed(staged: usize, known_pending: bool, in_flight: bool) -> bool {
    staged > 0 || (known_pending && !in_flight)
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
    data_codec: DataFrameCodec,
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
    let addresses =
        relay_data_addresses(&relay_id, &grant.data_addr, &config.relay_address_overrides);
    let mut connected = None;
    let mut last_error = None;
    for address in addresses {
        match timeout(
            Duration::from_secs(config.handshake_timeout_secs),
            TcpStream::connect(address),
        )
        .await
        {
            Ok(Ok(socket)) => {
                connected = Some(socket);
                break;
            }
            Ok(Err(error)) => last_error = Some(anyhow!(error)),
            Err(_) => last_error = Some(anyhow!("relay data connection timed out")),
        }
    }
    let mut socket =
        connected.ok_or_else(|| last_error.unwrap_or_else(|| anyhow!("no Relay data address")))?;
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
            data_protocol_version: DATA_PROTOCOL_BINARY_V1,
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
            data_protocol_version,
        } if response_flow == flow_id && response_carrier == carrier_id => Ok(BusinessCarrier {
            carrier_id,
            relay_id,
            stream,
            home_receive_offset: receive_offset,
            home_send_offset: send_offset,
            data_codec: DataFrameCodec::negotiate(data_protocol_version),
        }),
        DataFrame::OpenError { reason, .. } => bail!("home rejected carrier: {reason}"),
        _ => bail!("invalid carrier OPEN response"),
    }
}

async fn relay_candidates(state: &AppState) -> Vec<RelayCandidate> {
    let directory = state.directory.read().await.clone();
    signed_directory_candidates(directory, &state.config.relay_address_overrides)
}

fn signed_directory_candidates(
    directory: RelayDirectory,
    overrides: &[RelayAddressOverride],
) -> Vec<RelayCandidate> {
    directory
        .relays
        .into_iter()
        .flat_map(|relay| {
            relay_management_candidates(
                relay.id,
                relay.management_addr,
                relay.management_spki_sha256,
                overrides,
            )
        })
        .collect()
}

fn relay_data_addresses<'a>(
    relay_id: &str,
    signed: &'a str,
    overrides: &'a [RelayAddressOverride],
) -> Vec<&'a str> {
    let Some(address) = overrides.iter().find(|entry| entry.id == relay_id) else {
        return vec![signed];
    };
    if address.data_addr == signed {
        return vec![signed];
    }
    if address.bootstrap_hint {
        vec![signed, &address.data_addr]
    } else {
        vec![&address.data_addr, signed]
    }
}

fn relay_management_candidates(
    id: String,
    signed: String,
    pin: String,
    overrides: &[RelayAddressOverride],
) -> Vec<RelayCandidate> {
    let address = overrides.iter().find(|entry| entry.id == id);
    let signed_candidate = RelayCandidate {
        expected_id: Some(id),
        management_addr: signed,
        management_spki_sha256: Some(pin),
    };
    let Some(address) =
        address.filter(|entry| entry.management_addr != signed_candidate.management_addr)
    else {
        return vec![signed_candidate];
    };
    let override_candidate = RelayCandidate {
        management_addr: address.management_addr.clone(),
        ..signed_candidate.clone()
    };
    if address.bootstrap_hint {
        vec![signed_candidate, override_candidate]
    } else {
        vec![override_candidate, signed_candidate]
    }
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
        &state.config.relay_address_overrides,
    )
}

fn bootstrap_candidate_pool(
    directory: RelayDirectory,
    deployment_id: &str,
    mut history: Vec<RelayHistoryRecord>,
    seeds: &[SeedRelay],
    overrides: &[RelayAddressOverride],
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
    let current_ids: HashSet<_> = directory
        .relays
        .iter()
        .map(|relay| relay.id.clone())
        .collect();
    let mut source = signed_directory_candidates(directory, overrides);
    source.extend(
        history
            .into_iter()
            .filter(|relay| {
                relay.deployment_id == deployment_id
                    && !relay.operator_disabled
                    && !current_ids.contains(&relay.relay_id)
            })
            .flat_map(|relay| {
                relay_management_candidates(
                    relay.relay_id,
                    relay.management_addr,
                    relay.management_spki_sha256,
                    overrides,
                )
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
        .filter(|relay| seen.insert((relay.expected_id.clone(), relay.management_addr.clone())))
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
        state
            .spawn_blocking(move || store.apply_immediate(batch))
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
    state.mark_status_changed();
    Ok(())
}

async fn record_relay_success(state: &AppState, relay_id: &str) -> Result<()> {
    let now = unix_time_secs()?;
    let record = {
        let mut history = state.relay_history.write().await;
        let Some(record) = history
            .iter_mut()
            .find(|record| record.relay_id == relay_id)
        else {
            return Ok(());
        };
        let should_persist = record.consecutive_failures > 0
            || record
                .last_success_unix_secs
                .is_none_or(|last| now.saturating_sub(last) >= 300);
        record.last_success_unix_secs = Some(now);
        record.consecutive_failures = 0;
        should_persist.then(|| record.clone())
    };
    state.mark_status_changed();
    let Some(record) = record else {
        return Ok(());
    };
    let batch = WriteBatch::new().put_json(
        Table::RelayHistory,
        record.relay_id.as_bytes().to_vec(),
        &record,
    )?;
    let store = Arc::clone(&state.state_store);
    state
        .spawn_blocking(move || store.apply_immediate(batch))
        .await
        .context("Relay success history commit task failed")??;
    Ok(())
}

async fn record_relay_failure(state: &AppState, relay: &RelayCandidate) -> Result<()> {
    let now = unix_time_secs()?;
    let record = {
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
        let previous_failure = record.last_failure_unix_secs;
        record.last_failure_unix_secs = Some(now);
        record.consecutive_failures = record.consecutive_failures.saturating_add(1);
        let should_persist =
            should_persist_relay_failure(record.consecutive_failures, previous_failure, now);
        should_persist.then(|| record.clone())
    };
    state.mark_status_changed();
    let Some(record) = record else {
        return Ok(());
    };
    let batch = WriteBatch::new().put_json(
        Table::RelayHistory,
        record.relay_id.as_bytes().to_vec(),
        &record,
    )?;
    let store = Arc::clone(&state.state_store);
    state
        .spawn_blocking(move || store.apply_immediate(batch))
        .await
        .context("Relay failure history commit task failed")??;
    Ok(())
}

fn should_persist_relay_failure(
    consecutive_failures: u32,
    previous_failure_unix_secs: Option<u64>,
    now: u64,
) -> bool {
    consecutive_failures.is_power_of_two()
        || previous_failure_unix_secs.is_none_or(|last| now.saturating_sub(last) >= 60)
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

async fn prepare_mapping_listener(mapping: &Mapping) -> Result<PreparedMappingListener> {
    match mapping.protocol {
        ServiceProtocol::Tcp => Ok(PreparedMappingListener::Tcp(
            TcpListener::bind(&mapping.bind)
                .await
                .with_context(|| format!("failed to bind TCP mapping {}", mapping.bind))?,
        )),
        ServiceProtocol::Udp => Ok(PreparedMappingListener::Udp(
            UdpSocket::bind(&mapping.bind)
                .await
                .with_context(|| format!("failed to bind UDP mapping {}", mapping.bind))?,
        )),
    }
}

fn spawn_mapping_listener(
    state: AppState,
    mapping: Mapping,
    prepared: PreparedMappingListener,
) -> MappingTask {
    let (shutdown, receiver) = watch::channel(false);
    let token = Uuid::new_v4();
    let join = tokio::spawn(async move {
        supervise_mapping_listener(state, mapping, prepared, receiver, token).await;
    });
    MappingTask { shutdown, join }
}

async fn supervise_mapping_listener(
    state: AppState,
    mapping: Mapping,
    initial: PreparedMappingListener,
    mut shutdown: watch::Receiver<bool>,
    token: Uuid,
) {
    let key = mapping_key(&mapping);
    let mut prepared = Some(initial);
    let mut retry_delay = Duration::from_millis(100);
    loop {
        let listener = if let Some(listener) = prepared.take() {
            listener
        } else {
            loop {
                match prepare_mapping_listener(&mapping).await {
                    Ok(listener) => break listener,
                    Err(error) => {
                        warn!(
                            home_id = %mapping.home_id,
                            service_id = %mapping.service_id,
                            address = %mapping.bind,
                            %error,
                            retry_after_ms = retry_delay.as_millis(),
                            "local mapping listener rebind failed"
                        );
                        tokio::select! {
                            changed = shutdown.changed() => {
                                let _ = changed;
                                return;
                            }
                            () = sleep(retry_delay) => {}
                        }
                        retry_delay = retry_delay.saturating_mul(2).min(Duration::from_secs(5));
                    }
                }
            }
        };

        state.set_mapping_listener_ready(&key, token, true).await;
        let result = match listener {
            PreparedMappingListener::Tcp(listener) => {
                run_tcp_listener(state.clone(), mapping.clone(), listener, shutdown.clone()).await
            }
            PreparedMappingListener::Udp(socket) => {
                run_udp_listener(state.clone(), mapping.clone(), socket, shutdown.clone()).await
            }
        };
        state.set_mapping_listener_ready(&key, token, false).await;

        match result {
            Ok(()) => return,
            Err(error) => {
                warn!(
                    home_id = %mapping.home_id,
                    service_id = %mapping.service_id,
                    address = %mapping.bind,
                    %error,
                    "local mapping listener stopped unexpectedly; rebinding"
                );
            }
        }
        if *shutdown.borrow() {
            return;
        }
        retry_delay = Duration::from_millis(100);
    }
}

async fn stop_mapping_task(task: MappingTask) {
    let _ = task.shutdown.send(true);
    let _ = task.join.await;
}

async fn start_initial_mapping_listeners(state: &AppState) -> Result<()> {
    // Bind the whole set before publishing any task that can retain the database or accept flows.
    let mut prepared_listeners = Vec::new();
    for mapping in state.mappings.read().await.clone() {
        let prepared = prepare_mapping_listener(&mapping).await?;
        prepared_listeners.push((mapping, prepared));
    }
    let mut tasks = state.mapping_tasks.lock().await;
    for (mapping, prepared) in prepared_listeners {
        let key = mapping_key(&mapping);
        let task = spawn_mapping_listener(state.clone(), mapping, prepared);
        tasks.insert(key, task);
    }
    Ok(())
}

async fn run_tcp_listener(
    state: AppState,
    mapping: Mapping,
    listener: TcpListener,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    info!(home_id = %mapping.home_id, service_id = %mapping.service_id, address = %mapping.bind, "local TCP mapping ready");
    let mut flows = JoinSet::new();
    let (flow_stop, flow_shutdown) = watch::channel(false);
    let result = async {
        loop {
            let (local, peer) = tokio::select! {
                changed = shutdown.changed() => {
                    let _ = changed;
                    return Ok(());
                }
                _ = flows.join_next(), if !flows.is_empty() => continue,
                accepted = listener.accept() => accepted?,
            };
            let Ok(permit) = Arc::clone(&state.permits).try_acquire_owned() else {
                warn!(%peer, "travel active-flow limit reached");
                continue;
            };
            let state = state.clone();
            let mapping = mapping.clone();
            let flow_shutdown = flow_shutdown.clone();
            flows.spawn(async move {
                let _permit = permit;
                let _guard = FlowGuard::new(
                    Arc::clone(&state.active_flows),
                    state.status_generation.clone(),
                );
                if let Err(error) = tcp_flow::run(state, mapping, local, flow_shutdown).await {
                    warn!(%peer, %error, "TCP flow closed");
                }
            });
        }
    }
    .await;
    // A listener error also ends its flow lifetime. Each flow drains its own child tasks.
    drop(listener);
    let _ = flow_stop.send(true);
    while flows.join_next().await.is_some() {}
    result
}

async fn run_udp_listener(
    state: AppState,
    mapping: Mapping,
    socket: UdpSocket,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let socket = Arc::new(socket);
    info!(home_id = %mapping.home_id, service_id = %mapping.service_id, address = %mapping.bind, "local UDP mapping ready");
    let associations: Arc<Mutex<HashMap<SocketAddr, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let mut buffer = vec![0_u8; 65_507];
    let mut flows = JoinSet::new();
    let result = async {
    loop {
        let (count, peer) = tokio::select! {
            changed = shutdown.changed() => {
                let _ = changed;
                return Ok(());
            }
            received = socket.recv_from(&mut buffer) => received?,
            _ = flows.join_next(), if !flows.is_empty() => continue,
        };
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
        let association_shutdown = shutdown.clone();
        flows.spawn(async move {
            let _permit = permit;
            let _guard = FlowGuard::new(
                Arc::clone(&state.active_flows),
                state.status_generation.clone(),
            );
            if let Err(error) =
                run_udp_association(
                    &state, &mapping,
                    Arc::new(MappingDatagrams { socket, peer, outgoing: Mutex::new(rx) }),
                    association_shutdown, None, None,
                ).await
            {
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
    }.await;
    // Cancelling drops the association's sockets and flushes its small metric batches.
    flows.shutdown().await;
    result
}

struct MappingDatagrams {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    outgoing: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl flowsplice_transport::DatagramIo for MappingDatagrams {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> flowsplice_transport::IoFuture<'a, ()> {
        Box::pin(async move {
            self.socket.send_to(bytes, self.peer).await?;
            Ok(())
        })
    }

    fn recv(&self) -> flowsplice_transport::IoFuture<'_, Vec<u8>> {
        Box::pin(async move {
            self.outgoing
                .lock()
                .await
                .recv()
                .await
                .ok_or_else(|| anyhow!("UDP mapping closed"))
        })
    }
}

async fn run_udp_association(
    state: &AppState,
    mapping: &Mapping,
    endpoint: Arc<dyn flowsplice_transport::DatagramIo>,
    mut shutdown: watch::Receiver<bool>,
    mut ready: Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
    failure: Option<watch::Sender<Option<String>>>,
) -> Result<()> {
    let flow_id = Uuid::new_v4();
    state.begin_route_flow(flow_id, mapping).await;
    let inner_shutdown = shutdown.clone();
    let result = tokio::select! {
        biased;
        _ = shutdown.wait_for(|stopped| *stopped) => Ok((0, 0)),
        result = run_udp_association_inner(state, mapping, Arc::clone(&endpoint), inner_shutdown, flow_id, &mut ready) => result,
    };
    if let Some(ready) = ready {
        let reason = result.as_ref().err().map_or_else(
            || "UDP connection cancelled".to_owned(),
            ToString::to_string,
        );
        let _ = ready.send(Err(reason));
    }
    if let (Some(failure), Err(error)) = (failure, &result) {
        failure.send_replace(Some(error.to_string()));
    }
    let (uploaded_bytes, downloaded_bytes) = result.as_ref().copied().unwrap_or_default();
    state
        .finish_route_flow(
            flow_id,
            uploaded_bytes,
            downloaded_bytes,
            result.as_ref().err(),
        )
        .await;
    result?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_udp_association_inner(
    state: &AppState,
    mapping: &Mapping,
    endpoint: Arc<dyn flowsplice_transport::DatagramIo>,
    mut shutdown: watch::Receiver<bool>,
    flow_id: Uuid,
    ready: &mut Option<tokio::sync::oneshot::Sender<std::result::Result<(), String>>>,
) -> Result<(u64, u64)> {
    let config = &state.config;
    let mut network_changes = state.network_generation.subscribe();
    let carrier = open_udp_carrier_for_association(state, mapping, flow_id).await?;
    if let Some(ready) = ready.take() {
        let _ = ready.send(Ok(()));
    }
    let relay_id = carrier.relay_id.clone();
    state.select_route_relay(flow_id, &relay_id, None).await;
    let data_codec = carrier.data_codec;
    let business = carrier.stream;
    let (mut reader, mut writer) = tokio::io::split(business);
    let mut reader = DataFrameReader::new(&mut reader, DATA_FRAME_LIMIT, data_codec);
    let mut send_sequence = 0_u64;
    let mut receive_sequence = 0_u64;
    let mut uploaded_bytes = 0_u64;
    let mut downloaded_bytes = 0_u64;
    let dimensions = travel_udp_metric_dimensions(mapping, &relay_id);
    let mut upload_metrics = MetricBatch::new(
        (*state.statistics).clone(),
        "travel_flow_upload_observed_bytes".to_owned(),
        dimensions.clone(),
    );
    let mut download_metrics = MetricBatch::new(
        (*state.statistics).clone(),
        "delivered_download_datagram_bytes".to_owned(),
        dimensions,
    );
    let mut metrics_tick = interval(Duration::from_secs(5));
    metrics_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_activity = tokio::time::Instant::now();
    let result: Result<()> = async {
    loop {
        tokio::select! {
            changed = network_changes.changed() => {
                changed.context("network change notifier stopped")?;
                return Ok(());
            }
            changed = shutdown.changed() => {
                let _ = changed;
                return Ok(());
            }
            _ = metrics_tick.tick() => { upload_metrics.flush(); download_metrics.flush(); }
            () = tokio::time::sleep_until(last_activity + Duration::from_secs(config.udp_idle_secs)) => bail!("UDP association idle timeout"),
            datagram = endpoint.recv() => {
                let Ok(bytes) = datagram else { return Ok(()); };
                last_activity = tokio::time::Instant::now();
                let count = bytes.len() as u64;
                uploaded_bytes = uploaded_bytes.saturating_add(count);
                upload_metrics.record(unix_time_secs()?, count);
                state.uploaded_bytes.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                state.update_route_flow_counters(flow_id, uploaded_bytes, downloaded_bytes).await;
                write_data_frame(&mut writer, &DataFrame::Datagram { flow_id, sequence: send_sequence, bytes: bytes.into() }, DATA_FRAME_LIMIT, data_codec).await?;
                send_sequence = send_sequence.wrapping_add(1);
            }
            frame = reader.read() => {
                last_activity = tokio::time::Instant::now();
                match frame? {
                    DataFrame::Datagram { flow_id: id, sequence, bytes } if id == flow_id && sequence >= receive_sequence && bytes.len() <= 65_507 => {
                        if sequence == receive_sequence {
                            endpoint.send(&bytes).await?;
                            let count = bytes.len() as u64;
                            downloaded_bytes = downloaded_bytes.saturating_add(count);
                            download_metrics.record(unix_time_secs()?, count);
                            state.downloaded_bytes.fetch_add(count, std::sync::atomic::Ordering::Relaxed);
                            state.update_route_flow_counters(flow_id, uploaded_bytes, downloaded_bytes).await;
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
    .await;
    result?;
    Ok((uploaded_bytes, downloaded_bytes))
}

async fn open_udp_carrier_for_association(
    state: &AppState,
    mapping: &Mapping,
    flow_id: Uuid,
) -> Result<BusinessCarrier> {
    let mut opened = None;
    for relay in relay_candidates(state).await {
        let relay_label = relay.label().to_owned();
        let started = Instant::now();
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
                state
                    .record_route_attempt(
                        flow_id,
                        Some(carrier.relay_id.clone()),
                        ServiceProtocol::Udp,
                        "succeeded",
                        Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
                        None,
                    )
                    .await;
                opened = Some(carrier);
                break;
            }
            Err(error) => {
                state
                    .record_route_attempt(
                        flow_id,
                        Some(relay_label.clone()),
                        ServiceProtocol::Udp,
                        "failed",
                        Some(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)),
                        Some(&error.to_string()),
                    )
                    .await;
                warn!(relay = %relay_label, %error, "UDP carrier attempt failed");
            }
        }
    }
    opened.ok_or_else(|| anyhow::anyhow!("all UDP carrier attempts failed"))
}

fn travel_flow_metric_dimensions(mapping: &Mapping) -> BTreeMap<String, String> {
    let mut dimensions = BTreeMap::new();
    dimensions.insert("home_id".to_owned(), mapping.home_id.clone());
    dimensions.insert("service_id".to_owned(), mapping.service_id.clone());
    dimensions.insert(
        "protocol".to_owned(),
        match mapping.protocol {
            ServiceProtocol::Tcp => "tcp",
            ServiceProtocol::Udp => "udp",
        }
        .to_owned(),
    );
    // Embedded flows have no local address. Empty metric dimensions cannot be signed,
    // and a failed statistics flush would disconnect the catalog/login session.
    if !mapping.bind.is_empty() {
        dimensions.insert("mapping".to_owned(), mapping.bind.clone());
    }
    dimensions
}

fn travel_udp_metric_dimensions(mapping: &Mapping, relay_id: &str) -> BTreeMap<String, String> {
    let mut dimensions = travel_flow_metric_dimensions(mapping);
    dimensions.insert("relay_id".to_owned(), relay_id.to_owned());
    dimensions
}

#[cfg(feature = "frontend")]
async fn run_ui(state: AppState) -> Result<()> {
    let api = Router::new()
        .route("/status", get(api_status))
        .route("/mappings", post(api_upsert_mapping))
        .route("/mappings/delete", post(api_delete_mapping))
        .route("/catalog", get(api_catalog))
        .route("/relays", get(api_relays))
        .route("/diagnostics", get(api_diagnostics))
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

#[cfg(feature = "frontend")]
async fn api_status(State(state): State<AppState>) -> Json<StatusResponse> {
    Json(travel_status(&state).await)
}

async fn travel_status(state: &AppState) -> StatusResponse {
    use std::sync::atomic::Ordering;
    let generation = state.catalog.read().await.generation;
    let directory_generation = state.directory.read().await.generation;
    let mut active_relays: Vec<_> = state.flow_relays.lock().await.values().cloned().collect();
    active_relays.sort();
    active_relays.dedup();
    let mappings = state.mappings.read().await.clone();
    let ready_mapping_listeners = state.ready_mapping_listeners.read().await;
    let mapping_listeners_ready = mappings
        .iter()
        .all(|mapping| ready_mapping_listeners.contains_key(&mapping_key(mapping)));
    let relay_connected =
        !active_relays.is_empty() || !state.connected_relays.read().await.is_empty();
    let online = status_is_online(mapping_listeners_ready, relay_connected);
    StatusResponse {
        ok: true,
        online,
        travel_id: state.config.id.clone(),
        uptime_secs: state.started.elapsed().as_secs(),
        active_flows: state.active_flows.load(Ordering::Relaxed),
        catalog_generation: generation,
        relay_directory_generation: directory_generation,
        active_relays,
        session_uploaded_bytes: state.uploaded_bytes.load(Ordering::Relaxed),
        session_downloaded_bytes: state.downloaded_bytes.load(Ordering::Relaxed),
        mappings,
        private_key_password_rotation_available: travel_password_rotation_is_local(&state.config),
    }
}

const fn status_is_online(mapping_listeners_ready: bool, relay_connected: bool) -> bool {
    mapping_listeners_ready && relay_connected
}

#[cfg(feature = "frontend")]
async fn api_upsert_mapping(
    State(state): State<AppState>,
    Json(mapping): Json<Mapping>,
) -> ApiResult<Mapping> {
    upsert_mapping(&state, mapping)
        .await
        .map(Json)
        .map_err(api_error)
}

async fn upsert_mapping(state: &AppState, mapping: Mapping) -> Result<Mapping> {
    let _operation = state.mapping_operation.lock().await;
    let current = state.mappings.read().await.clone();
    let key = mapping_key(&mapping);
    if current
        .iter()
        .find(|candidate| mapping_key(candidate) == key)
        .is_some_and(|candidate| candidate == &mapping)
    {
        return Ok(mapping);
    }

    let mut next = current;
    if let Some(existing) = next
        .iter_mut()
        .find(|candidate| mapping_key(candidate) == key)
    {
        *existing = mapping.clone();
    } else {
        next.push(mapping.clone());
    }
    validate_mapping_set(&state.config, &next)?;

    // Bind before committing so an occupied or invalid new port leaves both durable state and the
    // currently running listener untouched.
    let prepared = prepare_mapping_listener(&mapping).await?;
    let store = Arc::clone(&state.state_store);
    let durable = next.clone();
    state
        .spawn_blocking(move || persist_runtime_mappings(&store, &durable))
        .await
        .context("Travel mapping persistence task failed")??;

    let task = spawn_mapping_listener(state.clone(), mapping.clone(), prepared);
    let old = state.mapping_tasks.lock().await.insert(key, task);
    *state.mappings.write().await = next;
    state.mark_status_changed();
    if let Some(old) = old {
        stop_mapping_task(old).await;
    }
    Ok(mapping)
}

#[cfg(feature = "frontend")]
async fn api_delete_mapping(
    State(state): State<AppState>,
    Json(identity): Json<MappingIdentityRequest>,
) -> ApiResult<Mapping> {
    delete_mapping(
        &state,
        identity.home_id,
        identity.service_id,
        identity.protocol,
    )
    .await
    .map(Json)
    .map_err(api_error)
}

async fn delete_mapping(
    state: &AppState,
    home_id: String,
    service_id: String,
    protocol: ServiceProtocol,
) -> Result<Mapping> {
    let _operation = state.mapping_operation.lock().await;
    let requested = Mapping {
        home_id,
        service_id,
        protocol,
        bind: String::new(),
    };
    let key = mapping_key(&requested);
    let current = state.mappings.read().await.clone();
    let removed = current
        .iter()
        .find(|candidate| mapping_key(candidate) == key)
        .cloned()
        .ok_or_else(|| anyhow!("mapping does not exist"))?;
    let next = current
        .into_iter()
        .filter(|candidate| mapping_key(candidate) != key)
        .collect::<Vec<_>>();
    let store = Arc::clone(&state.state_store);
    let durable = next.clone();
    state
        .spawn_blocking(move || persist_runtime_mappings(&store, &durable))
        .await
        .context("Travel mapping persistence task failed")??;
    let old = state.mapping_tasks.lock().await.remove(&key);
    *state.mappings.write().await = next;
    state.mark_status_changed();
    if let Some(old) = old {
        stop_mapping_task(old).await;
    }
    Ok(removed)
}

#[cfg(feature = "frontend")]
async fn api_catalog(State(state): State<AppState>) -> Json<Catalog> {
    Json(state.catalog.read().await.clone())
}

#[cfg(feature = "frontend")]
async fn api_relays(State(state): State<AppState>) -> Json<RelayDirectory> {
    Json(state.directory.read().await.clone())
}

#[cfg(feature = "frontend")]
async fn api_diagnostics(State(state): State<AppState>) -> Json<DiagnosticsSnapshot> {
    Json(diagnostics_snapshot(&state).await)
}

async fn diagnostics_snapshot(state: &AppState) -> DiagnosticsSnapshot {
    let now = unix_time_secs().unwrap_or_default();
    let diagnostics = state.diagnostics.lock().await;
    let (mut flows, active_counts, events) = collect_flow_snapshot_entries(&diagnostics);
    drop(diagnostics);
    flows.sort_by(|left, right| {
        left.started_at_unix_secs
            .cmp(&right.started_at_unix_secs)
            .then_with(|| left.flow_id.cmp(&right.flow_id))
    });
    let history = state.relay_history.read().await.clone();
    let directory = state.directory.read().await.clone();
    let catalog_generation = state.catalog.read().await.generation;
    let connected_relays = {
        let mut connected = state
            .connected_relays
            .read()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        connected.sort();
        connected
    };
    let mut relays = collect_relay_snapshots(
        &history,
        &directory,
        &connected_relays,
        &active_counts,
        now,
        &state.config.seed_relays,
    );
    relays.sort_by(|left, right| {
        relay_observation_rank(&left.observation)
            .cmp(&relay_observation_rank(&right.observation))
            .then_with(|| left.relay_id.cmp(&right.relay_id))
            .then_with(|| left.redacted_endpoint.cmp(&right.redacted_endpoint))
    });
    let last_accepted_unix_secs = history
        .iter()
        .filter(|record| record.current_member)
        .map(|record| record.last_seen_unix_secs)
        .max();
    let has_active_tcp_flow = flows
        .iter()
        .any(|flow| flow.protocol == ServiceProtocol::Tcp);
    let degraded_reason = connected_relays.is_empty().then(|| {
        if has_active_tcp_flow {
            "Control plane degraded; established TCP Flows continue.".to_owned()
        } else {
            "No active catalog subscription.".to_owned()
        }
    });
    DiagnosticsSnapshot {
        generated_at_unix_secs: now,
        flows,
        relays,
        control_plane: ControlPlaneSnapshot {
            catalog_generation,
            relay_directory_generation: directory.generation,
            last_accepted_unix_secs,
            catalog_subscription_relay: connected_relays.first().cloned(),
            connected_relays,
            directory_size: directory.relays.len(),
            degraded_reason,
        },
        events,
    }
}

fn collect_flow_snapshot_entries(
    diagnostics: &DiagnosticsState,
) -> (
    Vec<FlowRouteSnapshot>,
    HashMap<String, usize>,
    Vec<RouteEvent>,
) {
    let flows = diagnostics
        .flows
        .iter()
        .map(|(flow_id, flow)| FlowRouteSnapshot {
            flow_id: flow_id.to_string(),
            home_id: flow.mapping.home_id.clone(),
            service_id: flow.mapping.service_id.clone(),
            protocol: flow.mapping.protocol,
            local_bind: flow.mapping.bind.clone(),
            selected_relay: flow.selected_relay.clone(),
            started_at_unix_secs: flow.started_at_unix_secs,
            last_switch_unix_secs: flow.last_switch_unix_secs,
            switch_count: flow.switch_count,
            uploaded_bytes: flow.uploaded_bytes,
            downloaded_bytes: flow.downloaded_bytes,
            recovering: flow.recovering,
        })
        .collect::<Vec<_>>();
    let active_counts = flows
        .iter()
        .fold(HashMap::<String, usize>::new(), |mut counts, flow| {
            if let Some(relay_id) = &flow.selected_relay {
                *counts.entry(relay_id.clone()).or_default() += 1;
            }
            counts
        });
    let events = diagnostics.events.iter().rev().cloned().collect::<Vec<_>>();
    (flows, active_counts, events)
}

fn collect_relay_snapshots(
    history: &[RelayHistoryRecord],
    directory: &RelayDirectory,
    connected_relays: &[String],
    active_counts: &HashMap<String, usize>,
    now: u64,
    seed_relays: &[SeedRelay],
) -> Vec<RelayRouteSnapshot> {
    let mut relays = Vec::new();
    let mut known_relay_ids = HashSet::new();
    let mut known_addresses = HashSet::new();
    for record in history {
        known_relay_ids.insert(record.relay_id.clone());
        known_addresses.insert(record.management_addr.clone());
        let active_flow_count = active_counts
            .get(&record.relay_id)
            .copied()
            .unwrap_or_default();
        relays.push(RelayRouteSnapshot {
            relay_id: Some(record.relay_id.clone()),
            redacted_endpoint: redact_relay_endpoint(&record.management_addr),
            current_member: record.current_member,
            observation: relay_observation(
                record.current_member,
                active_flow_count,
                connected_relays
                    .iter()
                    .any(|value| value == &record.relay_id),
                record.last_success_unix_secs,
                record.last_failure_unix_secs,
                now,
            )
            .to_owned(),
            active_flow_count,
            last_seen_unix_secs: Some(record.last_seen_unix_secs),
            last_success_unix_secs: record.last_success_unix_secs,
            last_failure_unix_secs: record.last_failure_unix_secs,
            consecutive_failures: record.consecutive_failures,
        });
    }
    for endpoint in &directory.relays {
        if known_relay_ids.insert(endpoint.id.clone()) {
            let active_flow_count = active_counts.get(&endpoint.id).copied().unwrap_or_default();
            relays.push(RelayRouteSnapshot {
                relay_id: Some(endpoint.id.clone()),
                redacted_endpoint: redact_relay_endpoint(&endpoint.management_addr),
                current_member: true,
                observation: relay_observation(
                    true,
                    active_flow_count,
                    connected_relays.iter().any(|value| value == &endpoint.id),
                    None,
                    None,
                    now,
                )
                .to_owned(),
                active_flow_count,
                last_seen_unix_secs: None,
                last_success_unix_secs: None,
                last_failure_unix_secs: None,
                consecutive_failures: 0,
            });
        }
        known_addresses.insert(endpoint.management_addr.clone());
    }
    for seed in seed_relays {
        if known_addresses.insert(seed.management_addr.clone()) {
            relays.push(RelayRouteSnapshot {
                relay_id: None,
                redacted_endpoint: redact_relay_endpoint(&seed.management_addr),
                current_member: false,
                observation: "bootstrap_only".to_owned(),
                active_flow_count: 0,
                last_seen_unix_secs: None,
                last_success_unix_secs: None,
                last_failure_unix_secs: None,
                consecutive_failures: 0,
            });
        }
    }
    relays
}

fn relay_observation(
    current_member: bool,
    active_flow_count: usize,
    connected: bool,
    last_success: Option<u64>,
    last_failure: Option<u64>,
    now: u64,
) -> &'static str {
    if active_flow_count > 0 {
        return "in_use";
    }
    if !current_member {
        return "removed";
    }
    if last_failure.is_some_and(|failure| last_success.is_none_or(|success| failure >= success)) {
        return "recent_failure";
    }
    if connected || last_success.is_some_and(|success| now.saturating_sub(success) <= 15 * 60) {
        return "recently_reachable";
    }
    "eligible_unverified"
}

fn relay_observation_rank(value: &str) -> u8 {
    match value {
        "in_use" => 0,
        "recently_reachable" => 1,
        "eligible_unverified" => 2,
        "recent_failure" => 3,
        "bootstrap_only" => 4,
        _ => 5,
    }
}

fn redact_relay_endpoint(value: &str) -> String {
    value
        .rsplit_once(':')
        .map_or_else(|| "•••".to_owned(), |(_, port)| format!("•••:{port}"))
}

fn sanitized_route_reason(value: &str) -> String {
    let lower = value.to_ascii_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        "Route attempt timed out.".to_owned()
    } else if lower.contains("tls") || lower.contains("certificate") || lower.contains("secure") {
        "Secure route setup failed.".to_owned()
    } else if lower.contains("invalid") {
        "Relay returned an invalid route response.".to_owned()
    } else {
        "Relay route attempt failed.".to_owned()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg(feature = "frontend")]
struct StatisticsQuery {
    #[serde(default = "default_statistics_period")]
    period: String,
}

#[cfg(feature = "frontend")]
fn default_statistics_period() -> String {
    "day".to_owned()
}

#[derive(Serialize)]
#[cfg(feature = "frontend")]
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
#[cfg(feature = "frontend")]
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

#[cfg(feature = "frontend")]
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
    let points = state
        .spawn_blocking(move || statistics.query(from, now))
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

#[cfg(feature = "frontend")]
async fn api_remote_enrollments(
    State(state): State<AppState>,
) -> ApiResult<Vec<RemoteEnrollmentStatus>> {
    let store = Arc::clone(&state.state_store);
    state
        .spawn_blocking(move || {
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

#[cfg(feature = "frontend")]
async fn api_create_remote_enrollment(
    State(state): State<AppState>,
    Json(request): Json<CreateRemoteEnrollmentRequest>,
) -> ApiResult<RemoteEnrollmentStatus> {
    create_remote_enrollment(&state, request)
        .await
        .map(Json)
        .map_err(api_error)
}

#[cfg(feature = "frontend")]
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
    let status = state.spawn_blocking(move || {
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
    .context("Travel remote enrollment creation task failed")??;
    state.mark_enrollment_outbox_changed();
    Ok(status)
}

#[cfg(feature = "frontend")]
fn remote_enrollment_capacity_available(current: usize, maximum: usize) -> bool {
    current < maximum
}

#[cfg(feature = "frontend")]
async fn api_install_remote_enrollment(
    State(state): State<AppState>,
    Json(request): Json<InstallRemoteEnrollmentRequest>,
) -> ApiResult<InstallRemoteEnrollmentResponse> {
    install_remote_enrollment(&state, request)
        .await
        .map(Json)
        .map_err(api_error)
}

#[cfg(feature = "frontend")]
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
    let response = state
        .spawn_blocking(move || -> Result<InstallRemoteEnrollmentResponse> {
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
        .context("Travel remote enrollment installation task failed")??;
    state.mark_enrollment_outbox_changed();
    Ok(response)
}

#[cfg(feature = "frontend")]
async fn api_rotate_private_key_password(
    State(state): State<AppState>,
    Json(request): Json<RotatePrivateKeyPasswordRequest>,
) -> ApiResult<RotatePrivateKeyPasswordResponse> {
    rotate_travel_private_key_password(&state, request)
        .await
        .map(Json)
        .map_err(api_error)
}

#[cfg(feature = "frontend")]
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
    state
        .spawn_blocking(move || {
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
        .is_ok_and(|address| address.ip().is_loopback())
        || test_remote_ui_enabled(config)
}

#[cfg(feature = "frontend")]
fn api_error(error: impl Into<anyhow::Error>) -> (StatusCode, Json<ApiError>) {
    let error = error.into();
    (
        StatusCode::BAD_REQUEST,
        Json(ApiError {
            error: error.to_string(),
        }),
    )
}

#[cfg(feature = "frontend")]
async fn serve_spa(request: Request) -> Response {
    SPA.serve(request)
}

#[cfg(feature = "frontend")]
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

#[cfg(feature = "frontend")]
fn local_ui_request_allowed(request: &Request, listen: &str) -> bool {
    let Ok(address) = listen.parse::<SocketAddr>() else {
        return false;
    };
    if !address.ip().is_loopback() {
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

struct FlowGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
    status_generation: watch::Sender<u64>,
}

impl FlowGuard {
    fn new(
        counter: Arc<std::sync::atomic::AtomicUsize>,
        status_generation: watch::Sender<u64>,
    ) -> Self {
        use std::sync::atomic::Ordering;
        counter.fetch_add(1, Ordering::Relaxed);
        status_generation.send_modify(|generation| *generation = generation.wrapping_add(1));
        Self {
            counter,
            status_generation,
        }
    }
}

impl Drop for FlowGuard {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        self.counter.fetch_sub(1, Ordering::Relaxed);
        self.status_generation
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

#[cfg(all(test, feature = "frontend"))]
mod tests {
    #[cfg(feature = "frontend")]
    use axum::{body::Body, extract::Request, http::Method};
    #[cfg(feature = "frontend")]
    use clap::{CommandFactory, Parser};
    use flowsplice_core::{
        authorization::TrustedTravelAuthority,
        deployment::{
            ControlSnapshotPayload, DeploymentTrust, HomeEndpointTrust, VerifiedControlSnapshot,
        },
        protocol::{Catalog, RelayDirectory, RelayEndpoint, ServiceProtocol},
    };
    use flowsplice_storage::{StateStore, Table, WriteBatch};

    use super::{
        Cli, ConfiguredHome, ControlTrustState, DiagnosticsState, MAX_ROUTE_EVENTS, Mapping,
        RELAY_HISTORY_VERSION, RelayAddressOverride, RelayHistoryRecord, SeedRelay,
        TRAVEL_MAPPINGS_KEY, bootstrap_candidate_pool, configured_home_ids,
        configured_homes_are_trusted, enrollment_outbox_scan_needed, load_relay_history,
        local_ui_request_allowed, persist_runtime_mappings, redact_relay_endpoint,
        relay_observation, remote_enrollment_capacity_available, remote_enrollment_outbox_expired,
        require_authenticated_relay_in_snapshot, require_control_snapshot_subject,
        sanitized_route_reason, should_persist_relay_failure, signed_directory_candidates,
        statistics_outbox_scan_needed, status_is_online, trusted_home_business_pins,
    };

    #[test]
    fn travel_cli_exposes_remote_enrollment_only() {
        let command = Cli::command();
        let subcommands = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect::<Vec<_>>();
        assert!(subcommands.contains(&"enroll-remote"));
        assert!(!subcommands.contains(&"enroll-init"));
        assert!(!subcommands.contains(&"enroll-import"));
    }

    #[test]
    fn online_status_requires_mapping_listeners_and_a_relay() {
        assert!(status_is_online(true, true));
        assert!(!status_is_online(false, true));
        assert!(!status_is_online(true, false));
        assert!(!status_is_online(false, false));
    }

    #[test]
    fn remote_enrollment_cli_accepts_a_relay_with_a_trusted_public_key() {
        let parsed = Cli::try_parse_from([
            "flowsplice-travelagent",
            "enroll-remote",
            "--travel-id",
            "travel-1",
            "--home-id",
            "home-1",
            "--install-dir",
            "/tmp/travel-1",
            "--relay",
            "relay.example:8443",
            "--deployment-root-public-key",
            "/external/deployment-root.pub",
        ]);
        assert!(parsed.is_ok());
    }

    #[test]
    fn remote_enrollment_cli_requires_one_first_contact_source() {
        let parsed = Cli::try_parse_from([
            "flowsplice-travelagent",
            "enroll-remote",
            "--travel-id",
            "travel-1",
            "--home-id",
            "home-1",
            "--install-dir",
            "/tmp/travel-1",
        ]);
        assert!(parsed.is_err());
    }

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
    fn idle_outbox_scans_are_gated_by_pending_work() {
        assert!(!enrollment_outbox_scan_needed(false, 4, 4, false));
        assert!(enrollment_outbox_scan_needed(true, 4, 4, false));
        assert!(enrollment_outbox_scan_needed(false, 5, 4, false));
        assert!(!enrollment_outbox_scan_needed(true, 5, 4, true));

        assert!(!statistics_outbox_scan_needed(0, false, false));
        assert!(statistics_outbox_scan_needed(1, false, true));
        assert!(statistics_outbox_scan_needed(0, true, false));
        assert!(!statistics_outbox_scan_needed(0, true, true));
    }

    #[test]
    fn runtime_mappings_are_immediately_durable_in_redb() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let store = StateStore::open(directory.path().join("travel.redb"))?;
        let mappings = vec![Mapping {
            home_id: "home-1".to_owned(),
            service_id: "tcp-echo".to_owned(),
            protocol: ServiceProtocol::Tcp,
            bind: "127.0.0.1:10080".to_owned(),
        }];
        persist_runtime_mappings(&store, &mappings)?;
        let loaded = store
            .get_json::<Vec<Mapping>>(Table::TravelMappings, TRAVEL_MAPPINGS_KEY)?
            .unwrap_or_default();
        assert_eq!(loaded, mappings);
        Ok(())
    }

    #[test]
    fn travel_ui_requires_exact_loopback_host_and_origin() -> anyhow::Result<()> {
        let get = Request::builder()
            .uri("http://127.0.0.1:9080/")
            .header("host", "127.0.0.1:9080")
            .body(Body::empty())?;
        assert!(local_ui_request_allowed(&get, "127.0.0.1:9080"));

        let ipv6_get = Request::builder()
            .uri("http://[::1]:9080/")
            .header("host", "[::1]:9080")
            .body(Body::empty())?;
        assert!(local_ui_request_allowed(&ipv6_get, "[::1]:9080"));

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

        let ipv6_post = Request::builder()
            .method(Method::POST)
            .uri("http://[::1]:9080/api/keys/password")
            .header("host", "[::1]:9080")
            .header("origin", "http://[::1]:9080")
            .body(Body::empty())?;
        assert!(local_ui_request_allowed(&ipv6_post, "[::1]:9080"));

        let non_loopback = Request::builder()
            .uri("http://192.0.2.1:9080/")
            .header("host", "192.0.2.1:9080")
            .body(Body::empty())?;
        assert!(!local_ui_request_allowed(&non_loopback, "192.0.2.1:9080"));

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
            &[],
        );
        assert_eq!(pool.len(), 3);
        assert_eq!(pool[0].expected_id.as_deref(), Some("relay-current"));
        assert_eq!(pool[0].management_addr, "current.example:8443");
        assert_eq!(pool[1].expected_id.as_deref(), Some("relay-removed"));
        assert_eq!(pool[2].expected_id, None);

        let business = signed_directory_candidates(directory, &[]);
        assert_eq!(business.len(), 1);
        assert_eq!(business[0].expected_id.as_deref(), Some("relay-current"));
        assert!(
            business
                .iter()
                .all(|candidate| candidate.expected_id.as_deref() != Some("relay-removed"))
        );
        assert!(signed_directory_candidates(RelayDirectory::default(), &[]).is_empty());
    }

    #[test]
    fn local_address_overrides_preserve_signed_relay_identity() {
        let directory = RelayDirectory {
            generation: 1,
            relays: vec![RelayEndpoint {
                id: "relay-1".to_owned(),
                management_addr: "relay.example:8443".to_owned(),
                data_public_addr: "relay.example:8444".to_owned(),
                management_spki_sha256: "11".repeat(32),
            }],
        };
        let overrides = [RelayAddressOverride {
            id: "relay-1".to_owned(),
            management_addr: "10.0.2.2:18443".to_owned(),
            data_addr: "10.0.2.2:18444".to_owned(),
            bootstrap_hint: false,
        }];
        let candidates = signed_directory_candidates(directory.clone(), &overrides);
        assert_eq!(candidates[0].expected_id.as_deref(), Some("relay-1"));
        assert_eq!(candidates[0].management_addr, "10.0.2.2:18443");
        assert_eq!(candidates[0].management_spki_sha256, Some("11".repeat(32)));

        let bootstrap = bootstrap_candidate_pool(
            directory,
            "deployment-1",
            vec![],
            &[SeedRelay {
                management_addr: "10.0.2.2:18443".to_owned(),
            }],
            &overrides,
        );
        assert_eq!(bootstrap.len(), 3);
        assert_eq!(bootstrap[0].expected_id.as_deref(), Some("relay-1"));
        assert_eq!(bootstrap[0].management_addr, "10.0.2.2:18443");
        assert_eq!(bootstrap[1].management_addr, "relay.example:8443");
        assert_eq!(bootstrap[2].expected_id, None);
    }

    #[test]
    fn relay_failure_persistence_is_exponentially_throttled() {
        assert!(should_persist_relay_failure(1, None, 100));
        assert!(should_persist_relay_failure(2, Some(100), 101));
        assert!(!should_persist_relay_failure(3, Some(101), 102));
        assert!(should_persist_relay_failure(4, Some(102), 103));
        assert!(!should_persist_relay_failure(5, Some(103), 104));
        assert!(should_persist_relay_failure(5, Some(100), 160));
    }

    #[test]
    fn diagnostics_distinguish_directory_membership_from_observed_reachability() {
        assert_eq!(
            relay_observation(true, 1, false, None, Some(100), 200),
            "in_use"
        );
        assert_eq!(
            relay_observation(true, 0, true, None, None, 200),
            "recently_reachable"
        );
        assert_eq!(
            relay_observation(true, 0, false, None, None, 200),
            "eligible_unverified"
        );
        assert_eq!(
            relay_observation(true, 0, false, Some(100), Some(150), 200),
            "recent_failure"
        );
        assert_eq!(
            relay_observation(false, 0, false, Some(190), None, 200),
            "removed"
        );
    }

    #[test]
    fn diagnostics_redact_endpoints_and_failure_details() {
        assert_eq!(redact_relay_endpoint("relay.example:8443"), "•••:8443");
        assert_eq!(redact_relay_endpoint("not-an-endpoint"), "•••");
        let reason = sanitized_route_reason(
            "failed to read /Users/example/secret/config.toml after TLS timeout",
        );
        assert_eq!(reason, "Route attempt timed out.");
        assert!(!reason.contains("/Users"));
    }

    #[test]
    fn diagnostics_event_timeline_is_bounded() {
        let mut diagnostics = DiagnosticsState::default();
        for index in 0..(MAX_ROUTE_EVENTS + 4) {
            diagnostics.push_event(
                u64::try_from(index).unwrap_or_default(),
                None,
                None,
                ServiceProtocol::Tcp,
                "flow",
                "started",
                None,
                None,
            );
        }
        assert_eq!(diagnostics.events.len(), MAX_ROUTE_EVENTS);
        assert_eq!(diagnostics.events.front().map(|event| event.id), Some(5));
    }
}
