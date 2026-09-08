//! Serving Home initialization without issuer, enrollment, or UI resources.
use crate::{
    HomeControlConfig, HomeControlHooks, HomeStatistics, HomeTls, IssuerControlRequest,
    TcpFlowRegistry, TravelAuthorizationState, run_control_loop,
};
use anyhow::{Context, Result, bail};
use flowsplice_core::{
    MAX_DATA_PAYLOAD,
    authorization::{load_initialized_authorization_cache, load_json, unix_time_secs},
    business::{HomeServiceGrant, SignedHomeServiceGrant},
    deployment::{DeploymentTrust, SignedDeploymentTrust, SignedHomeEndpointCredential},
    protocol::{Role, Service},
    statistics::statistics_signing_key,
    tls::{load_private_key as load_management_private_key, validate_spki_pins},
};
use flowsplice_storage::{LocalStatistics, StateStore};
use flowsplice_transport::ServiceProvider;
use serde::Deserialize;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Semaphore, mpsc};

/// Transport and serving configuration using existing Home defaults and identities.
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HomeRuntimeConfig {
    pub id: String,
    pub alias: String,
    pub server_control_addr: String,
    pub server_name: String,
    pub server_id: String,
    pub management_cert: PathBuf,
    pub management_key: PathBuf,
    pub management_ca: PathBuf,
    pub business_cert: PathBuf,
    pub business_key: PathBuf,
    pub business_ca: PathBuf,
    pub deployment_root_public_key: PathBuf,
    pub deployment_trust: PathBuf,
    #[serde(default)]
    pub endpoint_credential: Option<PathBuf>,
    #[serde(default)]
    pub business_service_grant: Option<PathBuf>,
    #[serde(default)]
    pub server_spki_pins: Vec<String>,
    pub travel_authorization_cache: PathBuf,
    pub state_store: PathBuf,
    pub services: Vec<Service>,
    #[serde(default = "default_handshake_timeout")]
    pub handshake_timeout_secs: u64,
    #[serde(default = "default_udp_idle")]
    pub udp_idle_secs: u64,
    #[serde(default = "default_max_active_flows")]
    pub max_active_flows: usize,
    #[serde(default = "default_max_active_carriers")]
    pub max_active_carriers: usize,
    #[serde(default = "default_max_carriers_per_flow")]
    pub max_carriers_per_flow: usize,
    #[serde(default = "default_carrier_heartbeat")]
    pub carrier_heartbeat_secs: u64,
    #[serde(default = "default_carrier_timeout")]
    pub carrier_timeout_secs: u64,
    #[serde(default = "default_flow_detach_timeout")]
    pub flow_detach_timeout_secs: u64,
    #[serde(default = "default_max_unacked_bytes")]
    pub max_unacked_bytes: usize,
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

const fn default_flow_detach_timeout() -> u64 {
    120
}

const fn default_max_unacked_bytes() -> usize {
    1_048_576
}

/// Initialized serving Home runtime; creates no identities or issuer resources.
pub struct HomeRuntime {
    started: AtomicBool,
    config: Arc<HomeControlConfig>,
    statistics: HomeStatistics,
    tls: Arc<HomeTls>,
    authorization: Arc<TravelAuthorizationState>,
    permits: Arc<Semaphore>,
    tcp_flows: Arc<TcpFlowRegistry>,
    endpoint_credential: Option<SignedHomeEndpointCredential>,
    business_service_grant: Option<HomeServiceGrant>,
    trust_expires_at: u64,
}
impl HomeRuntime {
    /// Load and verify existing transport identities, trust and persistent authorization state.
    ///
    /// # Errors
    /// Returns invalid configuration, trust, identity, state-store or certificate errors.
    pub fn load(config: HomeRuntimeConfig, provider: Arc<dyn ServiceProvider>) -> Result<Self> {
        flowsplice_core::init_crypto();
        validate_service_metadata(&config.services)?;
        validate_spki_pins(&config.server_spki_pins, "server")?;
        let root_public_key = std::fs::read_to_string(&config.deployment_root_public_key)
            .context("failed to read deployment root public key")?;
        let signed: SignedDeploymentTrust = load_json(&config.deployment_trust)?;
        let deployment_trust = signed.verify(root_public_key.trim(), unix_time_secs()?)?;
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
        let business_service_grant = load_business_service_grant(
            config.business_service_grant.as_deref(),
            &deployment_trust,
            endpoint_credential.as_ref().map(|(signed, _)| signed),
            &config.services,
        )?;
        validate_runtime_limits(&config)?;
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
        let tls = Arc::new(HomeTls::load(
            &config.management_cert,
            &config.management_key,
            &config.management_ca,
            &config.business_cert,
            &config.business_key,
            &config.business_ca,
        )?);
        let authorization_cache =
            load_initialized_authorization_cache(&config.travel_authorization_cache)?;
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
            provider,
        );
        let control_config = Arc::new(HomeControlConfig {
            id: config.id,
            alias: config.alias,
            server_control_addr: config.server_control_addr,
            server_name: config.server_name,
            server_id: config.server_id,
            server_spki_pins: config.server_spki_pins,
            endpoint_credential: config.endpoint_credential,
            services: config.services,
            business_services: business_service_grant
                .as_ref()
                .map_or_else(Vec::new, |grant| grant.services.clone()),
            handshake_timeout_secs: config.handshake_timeout_secs,
            udp_idle_secs: config.udp_idle_secs,
            travel_authorization_cache: config.travel_authorization_cache,
        });
        let trust_expires_at = endpoint_credential
            .as_ref()
            .map_or(deployment_trust.not_after_unix_secs, |(_, expires)| {
                deployment_trust.not_after_unix_secs.min(*expires)
            });
        Ok(Self {
            started: AtomicBool::new(false),
            config: control_config,
            statistics,
            tls,
            authorization,
            permits,
            tcp_flows,
            endpoint_credential: endpoint_credential.map(|(signed, _)| signed),
            trust_expires_at: business_service_grant
                .as_ref()
                .map_or(trust_expires_at, |grant| {
                    trust_expires_at.min(grant.not_after_unix_secs)
                }),
            business_service_grant,
        })
    }
    /// Return the shared statistics recorder and persistent store.
    #[must_use]
    pub fn statistics(&self) -> HomeStatistics {
        self.statistics.clone()
    }
    /// Return the synchronized authorization state.
    #[must_use]
    pub fn authorization(&self) -> Arc<TravelAuthorizationState> {
        Arc::clone(&self.authorization)
    }
    /// Return the verified serving endpoint credential, when configured.
    #[must_use]
    pub fn endpoint_credential(&self) -> Option<&SignedHomeEndpointCredential> {
        self.endpoint_credential.as_ref()
    }

    /// Return the verified service grant, when configured.
    #[must_use]
    pub fn business_service_grant(&self) -> Option<&HomeServiceGrant> {
        self.business_service_grant.as_ref()
    }

    /// Run a provisioned business Home with no enrollment or issuance adapter.
    ///
    /// # Errors
    /// Returns trust-expiration and unrecoverable runtime errors.
    pub async fn run_serving(&self) -> Result<()> {
        self.run(None, Arc::new(crate::control::ServingOnly)).await
    }

    /// Permanently stop accepting work and cancel all active business I/O.
    /// Await the running `run`/`run_serving` future before dropping the runtime to
    /// release its control connection and persistent store. Create a new runtime to restart.
    pub async fn shutdown(&self) {
        self.tcp_flows.shutdown().await;
    }
    /// Maintain Home control and business transport until trust expiry or failure.
    ///
    /// # Errors
    /// Returns trust-expiration and unrecoverable runtime errors.
    pub async fn run(
        &self,
        issuer_control_rx: Option<mpsc::Receiver<IssuerControlRequest>>,
        hooks: Arc<dyn HomeControlHooks>,
    ) -> Result<()> {
        if self.started.swap(true, Ordering::AcqRel) {
            bail!("Home runtime has already been run; load a new runtime to restart");
        }
        // Cancellation of the caller's run future must also cancel detached work.
        let _stop = StopOnDrop(&self.tcp_flows);
        let result = {
            let control = run_control_loop(
                Arc::clone(&self.config),
                Arc::clone(&self.permits),
                Arc::clone(&self.tls),
                Arc::clone(&self.tcp_flows),
                Arc::clone(&self.authorization),
                issuer_control_rx,
                self.statistics.clone(),
                hooks,
            );
            tokio::select! {
                biased;
                () = self.tcp_flows.tasks.stopped() => Ok(()),
                result = monitor_trust_expiry(self.trust_expires_at) => result,
                result = control => result,
            }
        };
        self.tcp_flows.shutdown().await;
        let statistics = self.statistics.clone();
        self.tcp_flows
            .tasks
            .blocking(move || {
                statistics
                    .local
                    .flush_and_stage(
                        &statistics.deployment_id,
                        Role::Home,
                        &statistics.reporter_id,
                        &statistics.certificate_pem,
                        &statistics.signer,
                    )
                    .map(|_| ())
            })
            .await
            .context("Home final statistics flush failed")?;
        result
    }
}

impl Drop for HomeRuntime {
    fn drop(&mut self) {
        self.tcp_flows.tasks.stop();
    }
}

struct StopOnDrop<'a>(&'a TcpFlowRegistry);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.tasks.stop();
    }
}

async fn monitor_trust_expiry(not_after_unix_secs: u64) -> Result<()> {
    loop {
        let now = unix_time_secs()?;
        if now >= not_after_unix_secs {
            bail!("deployment trust expired; refusing to continue");
        }
        tokio::time::sleep(Duration::from_secs((not_after_unix_secs - now).min(30))).await;
    }
}

fn validate_service_metadata(services: &[Service]) -> Result<()> {
    let mut ids = HashSet::new();
    for service in services {
        if service.id.is_empty() || !ids.insert(&service.id) {
            bail!("service ids must be non-empty and unique");
        }
        if service.target.is_empty() {
            bail!("service target metadata must be non-empty");
        }
    }
    Ok(())
}

fn load_business_service_grant(
    path: Option<&Path>,
    trust: &DeploymentTrust,
    endpoint: Option<&SignedHomeEndpointCredential>,
    services: &[Service],
) -> Result<Option<HomeServiceGrant>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let endpoint =
        endpoint.context("business service grant requires a Home endpoint credential")?;
    let signed: SignedHomeServiceGrant = load_json(path)?;
    let grant = signed.verify(trust, endpoint, unix_time_secs()?)?;
    grant.validate_catalog_services(services)?;
    Ok(Some(grant))
}

fn validate_runtime_limits(config: &HomeRuntimeConfig) -> Result<()> {
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
    Ok(())
}
