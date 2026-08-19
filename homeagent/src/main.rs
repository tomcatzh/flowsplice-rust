#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    io::{self, IsTerminal},
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Request, State},
    http::{Method, StatusCode, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use embedded_spa::{EmbeddedSpa, EmbeddedSpaConfig};
use flowsplice_core::{
    CONTROL_FRAME_LIMIT, DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    authorization::{
        AuthorizationCache, SignedTravelCredential, TravelAuthorizationSnapshot,
        TravelCredentialScope, TrustedTravelAuthority, VerifiedAuthorization, load_json,
        store_json_atomic, unix_time_secs,
    },
    config::load_toml,
    deployment::{DeploymentTrust, SignedDeploymentTrust},
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{ControlMessage, DataFrame, HomeCatalog, Role, Service, ServiceProtocol},
    route::{RouteSide, write_preface},
    tls::{
        client_connector, peer_identity, require_peer, server_acceptor, server_name,
        validate_spki_pins,
    },
};
use flowsplice_enrollment::{
    DEFAULT_VALID_DAYS, MAX_VALID_DAYS, TravelEnrollmentRequest, TravelEnrollmentResponse,
    issuer::{IssuerMaterial, ProtectedKey, issue_enrollment},
    key::{
        MIN_PRIVATE_KEY_PASSWORD_CHARACTERS, PrivateKeyRotationTarget,
        recover_private_key_password_rotation, rotate_private_key_passwords,
    },
    prepare_enrollment_approval,
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
struct Args {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "homeagent.toml")]
    config: PathBuf,
}

#[derive(Clone, Deserialize)]
struct Config {
    id: String,
    alias: String,
    server_control_addr: String,
    server_data_addr: String,
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
    server_spki_pins: Vec<String>,
    travel_authorization_cache: PathBuf,
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
    listen: String,
    management_ca_key: PathBuf,
    business_ca_key: PathBuf,
    home_authority: SigningAuthorityConfig,
    #[serde(default)]
    global_authority: Option<SigningAuthorityConfig>,
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
    authorities: Arc<Vec<TrustedTravelAuthority>>,
    deployment_id: Arc<String>,
}

impl TravelAuthorizationState {
    fn new(
        cache: AuthorizationCache,
        deployment_id: String,
        authorities: Vec<TrustedTravelAuthority>,
    ) -> Arc<Self> {
        let (tx, _) = watch::channel(None);
        Arc::new(Self {
            tx,
            cache: Mutex::new(cache),
            authorities: Arc::new(authorities),
            deployment_id: Arc::new(deployment_id),
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
    authorization: Arc<TravelAuthorizationState>,
    control_tx: mpsc::Sender<IssuerControlRequest>,
    key_operation: Arc<Mutex<()>>,
    sensitive_operation: Arc<Semaphore>,
    issuance_ledger: Arc<Mutex<IssuanceLedger>>,
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
    let config: Config = load_toml(&args.config)?;
    validate_services(&config.services)?;
    validate_spki_pins(&config.server_spki_pins, "server")?;
    let (_, deployment_trust) = load_home_trust(&config)?;
    if let Some(issuer) = &config.issuer {
        validate_issuer_config(&config, issuer)?;
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
    let authorization = TravelAuthorizationState::new(
        authorization_cache,
        deployment_trust.deployment_id,
        deployment_trust.travel_authorities,
    );
    let permits = Arc::new(Semaphore::new(config.max_active_flows));
    let tcp_flows = TcpFlowRegistry::new(
        Arc::clone(&permits),
        Duration::from_secs(config.carrier_heartbeat_secs),
        Duration::from_secs(config.carrier_timeout_secs),
        Duration::from_secs(config.flow_detach_timeout_secs),
        config.max_unacked_bytes,
        config.max_active_carriers,
        config.max_carriers_per_flow,
    );
    let (issuer_state, issuer_control_rx) = if let Some(issuer) = &config.issuer {
        let (issuer_control_tx, issuer_control_rx) = mpsc::channel(32);
        let issuance_ledger = IssuanceLedger::load(ledger_path(&issuer.management_ca_key)?)?;
        (
            Some(IssuerAppState {
                config: Arc::clone(&config),
                issuer: Arc::new(issuer.clone()),
                authorization: Arc::clone(&authorization),
                control_tx: issuer_control_tx,
                key_operation: Arc::new(Mutex::new(())),
                sensitive_operation: Arc::new(Semaphore::new(1)),
                issuance_ledger: Arc::new(Mutex::new(issuance_ledger)),
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
    );
    let trust_expiry = monitor_trust_expiry(deployment_trust.not_after_unix_secs);
    if let Some(issuer_state) = issuer_state {
        let issuer = run_issuer_ui(issuer_state);
        tokio::try_join!(control, issuer, trust_expiry)?;
    } else {
        tokio::try_join!(control, trust_expiry)?;
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
) -> Result<()> {
    loop {
        if let Err(error) = run_control(
            Arc::clone(&config),
            Arc::clone(&permits),
            Arc::clone(&tls),
            Arc::clone(&tcp_flows),
            Arc::clone(&authorization),
            &mut issuer_control_rx,
        )
        .await
        {
            warn!(%error, "server control disconnected; reconnecting");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn validate_issuer_config(config: &Config, issuer: &IssuerConfig) -> Result<()> {
    let listen = issuer
        .listen
        .parse::<SocketAddr>()
        .context("invalid Home issuer listen address")?;
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
    validate_signing_authority(
        config,
        &trust.travel_authorities,
        &issuer.home_authority,
        false,
    )?;
    if let Some(authority) = &issuer.global_authority {
        validate_signing_authority(config, &trust.travel_authorities, authority, true)?;
    }
    if issuer.allow_unencrypted_test_keys
        && std::env::var("FLOWSPLICE_ALLOW_UNENCRYPTED_TEST_KEYS").as_deref() != Ok("1")
    {
        bail!("unencrypted issuer keys are disabled outside the explicit test environment");
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
    )
    .await
}

#[allow(clippy::too_many_lines)]
async fn run_control_session(
    stream: ClientTlsStream<TcpStream>,
    config: Arc<Config>,
    permits: Arc<Semaphore>,
    tls: Arc<TlsMaterial>,
    tcp_flows: Arc<TcpFlowRegistry>,
    authorization: Arc<TravelAuthorizationState>,
    issuer_control_rx: &mut Option<mpsc::Receiver<IssuerControlRequest>>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    write_json(
        &mut writer,
        &ControlMessage::Hello {
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
        ControlMessage::Hello { role, id } if role == Role::Server && id == config.server_id => {}
        _ => bail!("server sent an invalid HELLO"),
    }
    write_json(
        &mut writer,
        &ControlMessage::HomeRegister {
            home: HomeCatalog {
                home_id: config.id.clone(),
                home_alias: config.alias.clone(),
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
                    ControlMessage::OpenWork { work_id, work_secret, credential_id } => {
                        ensure_credential_active(&authorization.tx.subscribe(), credential_id)?;
                        let config = Arc::clone(&config);
                        let tls = Arc::clone(&tls);
                        let permits = Arc::clone(&permits);
                        let tcp_flows = Arc::clone(&tcp_flows);
                        let authorization_rx = authorization.tx.subscribe();
                        tokio::spawn(async move {
                            if let Err(error) = run_work(config, tls, permits, tcp_flows, authorization_rx, credential_id, work_id, work_secret).await {
                                warn!(%work_id, %error, "home work failed");
                            }
                        });
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
        }
    }
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
) -> Result<()> {
    let socket = timeout(Duration::from_secs(config.handshake_timeout_secs), async {
        let mut socket = TcpStream::connect(&config.server_data_addr).await?;
        write_preface(&mut socket, RouteSide::Home, work_id, &work_secret).await?;
        Ok::<_, anyhow::Error>(socket)
    })
    .await
    .context("Server data work setup timed out")??;
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
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(&service.target).await?;
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
                write_json(&mut tls_writer, &DataFrame::Datagram { flow_id, sequence: send_sequence, bytes: buffer[..count].to_vec() }, DATA_FRAME_LIMIT).await?;
                send_sequence = send_sequence.wrapping_add(1);
            }
            frame = timeout(Duration::from_secs(idle_secs), tls_reader.read::<DataFrame>()) => {
                match frame.context("UDP association idle timeout")?? {
                    DataFrame::Datagram { flow_id: id, sequence, bytes } if id == flow_id && sequence >= receive_sequence && bytes.len() <= 65_507 => {
                        if sequence == receive_sequence {
                            socket.send(&bytes).await?;
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
    let authorization =
        VerifiedAuthorization::verify(&snapshot, &state.authorities, &state.deployment_id)?;
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
        .route("/issue", post(api_issue))
        .route("/revoke", post(api_revoke))
        .route(
            "/private-key-password",
            post(api_rotate_private_key_password),
        )
        .fallback(|| async { StatusCode::NOT_FOUND });
    let app = Router::new()
        .nest("/api", api)
        .fallback(serve_spa)
        .with_state(state.clone())
        .layer(from_fn_with_state(state.clone(), authorize_issuer_ui));
    let listener = tokio::net::TcpListener::bind(&state.issuer.listen).await?;
    info!(address = %state.issuer.listen, "Home Travel issuer UI ready");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn api_issuer_status(State(state): State<IssuerAppState>) -> Json<IssuerStatus> {
    Json(IssuerStatus {
        home_id: state.config.id.clone(),
        home_alias: state.config.alias.clone(),
        default_valid_days: state.issuer.default_valid_days,
        global_authority_available: state.issuer.global_authority.is_some(),
        private_key_password_rotation_available: issuer_password_rotation_is_local(&state.issuer),
        services: state.config.services.clone(),
    })
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
    issue_from_home(&state, request)
        .await
        .map(Json)
        .map_err(api_error)
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
    if !issuer_password_rotation_is_local(&state.issuer) {
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

fn issuer_password_rotation_is_local(issuer: &IssuerConfig) -> bool {
    issuer
        .listen
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
    let reason = request.reason.trim();
    if reason.is_empty() || reason.len() > 256 {
        return Err(api_error(anyhow::anyhow!(
            "revocation reason must contain 1 to 256 bytes"
        )));
    }
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
    if local_ui_request_allowed(&request, &state.issuer.listen) {
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

    use super::{Config, local_ui_request_allowed, requested_validity_secs};
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
server_data_addr = "127.0.0.1:7444"
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
