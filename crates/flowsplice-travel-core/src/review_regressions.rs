use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use aws_lc_rs::digest;
use flowsplice_core::{
    CONTROL_FRAME_LIMIT,
    authorization::{TravelCredentialScope, unix_time_secs},
    deployment::SignedDeploymentTrust,
    frame::{JsonFrameReader, write_json},
    protocol::{CONTROL_PROTOCOL_VERSION, ControlMessage, ServiceProtocol},
    statistics::SignedStatisticsReport,
    tls::{identity_server_name, optional_client_server_acceptor},
};
use flowsplice_enrollment::{
    BUSINESS_CA_FILE, BUSINESS_CERT_FILE, BUSINESS_KEY_FILE, MANAGEMENT_CA_FILE,
    MANAGEMENT_CERT_FILE, MANAGEMENT_KEY_FILE, create_enrollment_request,
    install_enrollment_response,
    issuer::{IssuerMaterial, ProtectedKey, issue_enrollment},
    prepare_enrollment_approval,
};
use flowsplice_storage::StateStore;
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, timeout},
};
use tokio_rustls::server::TlsStream as ServerTlsStream;

use super::{
    AppState, Mapping, RelayAddressOverride, RemoteEnrollmentOptions, TravelCore,
    discover_bootstrap_relay, enroll_remote, load_app_state, relay_data_addresses,
    relay_management_candidates, run_catalog_session,
};

const PRIVATE_KEY_PASSWORD: &str = "flowsplice-e2e-private-key-password";

struct Fixture {
    generated: PathBuf,
}

impl Fixture {
    fn load(name: &str) -> Result<Self> {
        let root = std::env::var("FLOWSPLICE_REVIEW_FIXTURES").context(
            "set FLOWSPLICE_REVIEW_FIXTURES with tests/check-travel-review-regressions.sh",
        )?;
        let generated = PathBuf::from(root).join(name).join("generated");
        for relative in [
            "certs/deployment-root.pub",
            "certs/deployment-trust.json",
            "certs/management-ca.crt",
            "certs/business-ca.crt",
            "certs/relay1.crt",
            "certs/relay1.key",
            "offline/management-ca.key",
            "offline/business-ca.key",
            "offline/home1-authority.key",
        ] {
            if !generated.join(relative).is_file() {
                bail!(
                    "review fixture {} is missing {relative}",
                    generated.display()
                );
            }
        }
        Ok(Self { generated })
    }

    fn cert(&self, name: &str) -> PathBuf {
        self.generated.join("certs").join(name)
    }

    fn offline(&self, name: &str) -> PathBuf {
        self.generated.join("offline").join(name)
    }

    fn deployment_root(&self) -> Result<String> {
        let root = fs::read_to_string(self.cert("deployment-root.pub"))
            .context("failed to read review deployment root")?;
        let root = root.trim().to_owned();
        if root.is_empty() {
            bail!("review deployment root is empty");
        }
        Ok(root)
    }

    fn deployment_trust(&self) -> Result<Vec<u8>> {
        fs::read(self.cert("deployment-trust.json"))
            .context("failed to read review deployment trust")
    }
}

fn install_fixture_identity(work: &Path, fixture: &Fixture, travel_id: &str) -> Result<PathBuf> {
    flowsplice_core::init_crypto();
    let identity = work.join("identity");
    let now = unix_time_secs()?;
    let request =
        create_enrollment_request(travel_id, PRIVATE_KEY_PASSWORD.as_bytes(), &identity, now)?;
    let approval = prepare_enrollment_approval(
        request,
        60 * 60,
        "home-1-authority".to_owned(),
        TravelCredentialScope::Home {
            home_id: "home-1".to_owned(),
        },
        now,
    )?;
    let signed_trust: SignedDeploymentTrust =
        flowsplice_enrollment::load_json(&fixture.cert("deployment-trust.json"))?;
    let deployment_root = fixture.deployment_root()?;
    let management_ca_certificate = fixture.cert("management-ca.crt");
    let management_ca_key = fixture.offline("management-ca.key");
    let business_ca_certificate = fixture.cert("business-ca.crt");
    let business_ca_key = fixture.offline("business-ca.key");
    let travel_authority_key = fixture.offline("home1-authority.key");
    let material = IssuerMaterial {
        deployment_trust: &signed_trust,
        deployment_root_public_key: &deployment_root,
        home_endpoint_credential: None,
        management_ca_certificate: &management_ca_certificate,
        management_ca_key: ProtectedKey {
            path: &management_ca_key,
            password: Some(PRIVATE_KEY_PASSWORD.as_bytes()),
            allow_unencrypted: false,
        },
        business_ca_certificate: &business_ca_certificate,
        business_ca_key: ProtectedKey {
            path: &business_ca_key,
            password: Some(PRIVATE_KEY_PASSWORD.as_bytes()),
            allow_unencrypted: false,
        },
        travel_authority_key: ProtectedKey {
            path: &travel_authority_key,
            password: Some(PRIVATE_KEY_PASSWORD.as_bytes()),
            allow_unencrypted: false,
        },
    };
    let response = issue_enrollment(approval, &material, now)?;
    install_enrollment_response(
        &identity,
        &response,
        &deployment_root,
        PRIVATE_KEY_PASSWORD.as_bytes(),
        now,
    )?;
    Ok(identity)
}

fn write_runtime_config(
    work: &Path,
    fixture: &Fixture,
    identity: &Path,
    mappings: &[Mapping],
) -> Result<(PathBuf, PathBuf)> {
    let config_path = work.join("travelagent.toml");
    let state_store = work.join("state").join("travel-state.redb");
    let config = serde_json::json!({
        "id": "review-travel",
        "seed_relays": [{ "management_addr": "127.0.0.1:9" }],
        "homes": [{ "id": "home-1" }],
        "deployment_root_public_key": fixture.cert("deployment-root.pub"),
        "deployment_trust": fixture.cert("deployment-trust.json"),
        "management_cert": identity.join(MANAGEMENT_CERT_FILE),
        "management_key": identity.join(MANAGEMENT_KEY_FILE),
        "management_ca": identity.join(MANAGEMENT_CA_FILE),
        "business_cert": identity.join(BUSINESS_CERT_FILE),
        "business_key": identity.join(BUSINESS_KEY_FILE),
        "business_ca": identity.join(BUSINESS_CA_FILE),
        "state_store": state_store.clone(),
        "enrollment_work_dir": work.join("enrollment-work"),
        "ui_listen": "127.0.0.1:0",
        "mappings": mappings,
        "handshake_timeout_secs": 1,
        "udp_idle_secs": 1,
        "max_active_flows": 4,
        "max_active_carriers": 4,
        "max_carriers_per_flow": 1,
        "carrier_heartbeat_secs": 1,
        "carrier_timeout_secs": 2,
        "carrier_race_timeout_secs": 1,
        "carrier_recovery_timeout_secs": 3,
        "carrier_reevaluate_secs": 1,
        "max_carrier_reevaluate_secs": 1,
        "max_unacked_bytes": 1_048_576,
    });
    fs::write(&config_path, toml::to_string_pretty(&config)?)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    Ok((config_path, state_store))
}

async fn spawn_discovery_server(
    fixture: &Fixture,
    response_root: String,
    response_trust: Vec<u8>,
) -> Result<(String, JoinHandle<Result<()>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?.to_string();
    let acceptor = optional_client_server_acceptor(
        &fixture.cert("relay1.crt"),
        &fixture.cert("relay1.key"),
        &fixture.cert("management-ca.crt"),
    )?;
    let server = tokio::spawn(async move {
        let (socket, _) = timeout(Duration::from_secs(5), listener.accept())
            .await
            .context("review discovery client did not connect")??;
        let mut stream = timeout(Duration::from_secs(5), acceptor.accept(socket))
            .await
            .context("review discovery TLS handshake timed out")??;
        let request = {
            let mut reader = JsonFrameReader::new(&mut stream, CONTROL_FRAME_LIMIT);
            reader
                .read_with_timeout::<ControlMessage>(Duration::from_secs(5))
                .await?
        };
        match request {
            ControlMessage::BootstrapDiscoveryRequest { protocol_version }
                if protocol_version == CONTROL_PROTOCOL_VERSION => {}
            _ => bail!("review discovery client sent an unexpected request"),
        }
        write_json(
            &mut stream,
            &ControlMessage::BootstrapDiscoveryResult {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                deployment_root_public_key: response_root,
                deployment_trust_json: response_trust,
                relay_data_addr: "127.0.0.1:18444".to_owned(),
            },
            CONTROL_FRAME_LIMIT,
        )
        .await?;
        Ok(())
    });
    Ok((address, server))
}

async fn join_discovery_server(server: JoinHandle<Result<()>>) -> Result<()> {
    server
        .await
        .context("review discovery server task failed")??;
    Ok(())
}

async fn wait_for_active_flow(core: &TravelCore) -> Result<()> {
    timeout(Duration::from_secs(2), async {
        loop {
            if core.status().await.active_flows == 1 {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("accepted TCP connection never became an active Travel flow")?;
    Ok(())
}

async fn shutdown_with_timeout(core: &TravelCore, label: &str) -> Result<()> {
    timeout(Duration::from_secs(3), core.shutdown())
        .await
        .with_context(|| format!("{label} shutdown did not finish within three seconds"))?;
    Ok(())
}

async fn start_catalog_session(
    state: &AppState,
    fixture: &Fixture,
) -> Result<(ServerTlsStream<TcpStream>, JoinHandle<Result<()>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let acceptor = optional_client_server_acceptor(
        &fixture.cert("relay1.crt"),
        &fixture.cert("relay1.key"),
        &fixture.cert("management-ca.crt"),
    )?;
    let connector = state.tls.management_connector.clone();
    let server = async move {
        let (socket, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .context("review catalog server did not receive a client")??;
        timeout(Duration::from_secs(2), acceptor.accept(socket))
            .await
            .context("review catalog TLS server handshake timed out")?
            .context("review catalog TLS server handshake failed")
    };
    let client = async move {
        let socket = timeout(Duration::from_secs(2), TcpStream::connect(address))
            .await
            .context("review catalog client connection timed out")??;
        socket.set_nodelay(true)?;
        timeout(
            Duration::from_secs(2),
            connector.connect(identity_server_name()?, socket),
        )
        .await
        .context("review catalog TLS client handshake timed out")?
        .context("review catalog TLS client handshake failed")
    };
    let (server, client) = tokio::try_join!(server, client)?;
    let catalog_state = state.clone();
    let catalog = tokio::spawn(async move {
        run_catalog_session(&catalog_state, "relay-1", "unused-test-spki", client).await
    });
    Ok((server, catalog))
}

async fn receive_catalog_statistics_report(
    stream: &mut ServerTlsStream<TcpStream>,
    deadline: Duration,
) -> Result<SignedStatisticsReport> {
    timeout(deadline, async {
        loop {
            let message = {
                let mut reader = JsonFrameReader::new(&mut *stream, CONTROL_FRAME_LIMIT);
                reader.read::<ControlMessage>().await?
            };
            match message {
                ControlMessage::Heartbeat { nonce } => {
                    write_json(
                        stream,
                        &ControlMessage::HeartbeatAck { nonce },
                        CONTROL_FRAME_LIMIT,
                    )
                    .await?;
                }
                ControlMessage::StatisticsReport { report } => return Ok(report),
                _ => bail!("review catalog session sent an unexpected control message"),
            }
        }
    })
    .await
    .context("review catalog session did not send a statistics report before its deadline")?
}

async fn wait_for_empty_statistics_outbox(state: &AppState) -> Result<()> {
    timeout(Duration::from_secs(2), async {
        loop {
            if state.statistics.pending_reports(1)?.is_empty() {
                return Ok(());
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("accepted statistics report remained in the Travel outbox")?
}

async fn assert_no_statistics_report_on_next_tick(
    stream: &mut ServerTlsStream<TcpStream>,
) -> Result<()> {
    match timeout(Duration::from_secs(6), async {
        loop {
            let message = {
                let mut reader = JsonFrameReader::new(&mut *stream, CONTROL_FRAME_LIMIT);
                reader.read::<ControlMessage>().await?
            };
            match message {
                ControlMessage::Heartbeat { nonce } => {
                    write_json(
                        stream,
                        &ControlMessage::HeartbeatAck { nonce },
                        CONTROL_FRAME_LIMIT,
                    )
                    .await?;
                }
                ControlMessage::StatisticsReport { .. } => {
                    bail!("acknowledged statistics report was sent again on the next tick");
                }
                _ => bail!("review catalog session sent an unexpected control message"),
            }
        }
    })
    .await
    {
        Err(_) => Ok(()),
        Ok(Ok(())) => bail!("review catalog session ended without reaching the next tick"),
        Ok(Err(error)) => Err(error),
    }
}

async fn join_catalog_after_peer_disconnect(
    catalog: JoinHandle<Result<()>>,
    label: &str,
) -> Result<()> {
    let result = timeout(Duration::from_secs(2), catalog)
        .await
        .with_context(|| format!("{label} catalog task did not stop after peer disconnect"))?
        .with_context(|| format!("{label} catalog task panicked"))?;
    assert!(
        result.is_err(),
        "{label} catalog task accepted a closed peer"
    );
    Ok(())
}

fn files_sha256(paths: &[PathBuf]) -> Result<BTreeMap<PathBuf, String>> {
    paths
        .iter()
        .map(|path| {
            let bytes = fs::read(path).with_context(|| {
                format!("failed to hash review identity file {}", path.display())
            })?;
            Ok((
                path.clone(),
                hex::encode(digest::digest(&digest::SHA256, &bytes).as_ref()),
            ))
        })
        .collect()
}

#[test]
fn r08_bootstrap_hints_keep_signed_addresses_as_recoverable_fallbacks() {
    let automatic_pin = "11".repeat(32);
    let bootstrap_hint = [RelayAddressOverride {
        id: "relay-1".to_owned(),
        management_addr: "legacy.example:8443".to_owned(),
        data_addr: "legacy.example:8444".to_owned(),
        bootstrap_hint: true,
    }];
    assert_eq!(
        relay_data_addresses("relay-1", "fresh.example:8444", &bootstrap_hint),
        vec!["fresh.example:8444", "legacy.example:8444"]
    );
    let automatic = relay_management_candidates(
        "relay-1".to_owned(),
        "fresh.example:8443".to_owned(),
        automatic_pin.clone(),
        &bootstrap_hint,
    );
    assert_eq!(automatic.len(), 2);
    assert_eq!(automatic[0].management_addr, "fresh.example:8443");
    assert_eq!(automatic[1].management_addr, "legacy.example:8443");
    assert!(automatic.iter().all(|candidate| {
        candidate.expected_id.as_deref() == Some("relay-1")
            && candidate.management_spki_sha256.as_deref() == Some(automatic_pin.as_str())
    }));

    let manual_pin = "22".repeat(32);
    let manual_alias = [RelayAddressOverride {
        id: "relay-1".to_owned(),
        management_addr: "manual-alias.example:8443".to_owned(),
        data_addr: "manual-alias.example:8444".to_owned(),
        bootstrap_hint: false,
    }];
    assert_eq!(
        relay_data_addresses("relay-1", "fresh.example:8444", &manual_alias),
        vec!["manual-alias.example:8444", "fresh.example:8444"]
    );
    let manual = relay_management_candidates(
        "relay-1".to_owned(),
        "fresh.example:8443".to_owned(),
        manual_pin.clone(),
        &manual_alias,
    );
    assert_eq!(manual.len(), 2);
    assert_eq!(manual[0].management_addr, "manual-alias.example:8443");
    assert_eq!(manual[1].management_addr, "fresh.example:8443");
    assert!(manual.iter().all(|candidate| {
        candidate.expected_id.as_deref() == Some("relay-1")
            && candidate.management_spki_sha256.as_deref() == Some(manual_pin.as_str())
    }));
}

#[tokio::test]
async fn r01_missing_trusted_root_refuses_before_creating_an_installation() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let install_dir = temporary.path().join("no-tofu-install");
    let error = match enroll_remote(
        RemoteEnrollmentOptions {
            travel_id: "review-travel".to_owned(),
            home_id: "home-1".to_owned(),
            install_dir: install_dir.clone(),
            bootstrap_config: None,
            trusted_deployment_root_public_key: None,
            selected_relay: Some("127.0.0.1:9".to_owned()),
            ui_listen: None,
            private_key_password: PRIVATE_KEY_PASSWORD.to_owned(),
            wait_timeout_secs: 1,
            #[cfg(feature = "e2e-remote-ui")]
            test_allow_remote_listen: false,
            #[cfg(feature = "e2e-remote-ui")]
            test_admin_token: None,
        },
        |_| {},
    )
    .await
    {
        Ok(()) => bail!("remote enrollment accepted a missing trusted deployment root"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("trusted deployment public key is required")
    );
    assert!(!install_dir.exists());
    Ok(())
}

#[tokio::test]
#[ignore = "run tests/check-travel-review-regressions.sh"]
async fn r01_discovery_requires_the_packaged_root_and_its_management_ca() -> Result<()> {
    let legitimate = Fixture::load("legit")?;
    let rogue = Fixture::load("rogue")?;
    let legitimate_root = legitimate.deployment_root()?;
    let legitimate_trust = legitimate.deployment_trust()?;

    let (legitimate_address, legitimate_server) = spawn_discovery_server(
        &legitimate,
        legitimate_root.clone(),
        legitimate_trust.clone(),
    )
    .await?;
    let bootstrap = discover_bootstrap_relay(&legitimate_address, &legitimate_root).await?;
    join_discovery_server(legitimate_server).await?;
    assert_eq!(bootstrap.deployment_root_public_key, legitimate_root);
    assert_eq!(bootstrap.relay_address_overrides.len(), 1);

    let (rogue_address, rogue_server) =
        spawn_discovery_server(&rogue, rogue.deployment_root()?, rogue.deployment_trust()?).await?;
    let temporary = tempfile::tempdir()?;
    let install_dir = temporary.path().join("rogue-install");
    let error = match enroll_remote(
        RemoteEnrollmentOptions {
            travel_id: "review-travel".to_owned(),
            home_id: "home-1".to_owned(),
            install_dir: install_dir.clone(),
            bootstrap_config: None,
            trusted_deployment_root_public_key: Some(legitimate_root.clone()),
            selected_relay: Some(rogue_address),
            ui_listen: None,
            private_key_password: PRIVATE_KEY_PASSWORD.to_owned(),
            wait_timeout_secs: 1,
            #[cfg(feature = "e2e-remote-ui")]
            test_allow_remote_listen: false,
            #[cfg(feature = "e2e-remote-ui")]
            test_admin_token: None,
        },
        |_| {},
    )
    .await
    {
        Ok(()) => bail!("remote enrollment accepted a rogue deployment root"),
        Err(error) => error,
    };
    join_discovery_server(rogue_server).await?;
    assert!(error.to_string().contains("deployment trust mismatch"));
    assert!(!install_dir.exists());

    let (attacker_address, attacker_server) =
        spawn_discovery_server(&rogue, legitimate_root.clone(), legitimate_trust).await?;
    let Err(error) = discover_bootstrap_relay(&attacker_address, &legitimate_root).await else {
        bail!("discovery accepted an attacker certificate with replayed trusted metadata");
    };
    join_discovery_server(attacker_server).await?;
    assert!(format!("{error:#}").contains("outside the trusted deployment"));
    Ok(())
}

#[tokio::test]
#[ignore = "run tests/check-travel-review-regressions.sh"]
async fn r01_native_start_rejects_mismatched_root_before_identity_or_config_mutation() -> Result<()>
{
    let fixture = Fixture::load("legit")?;
    let rogue = Fixture::load("rogue")?;
    let temporary = tempfile::tempdir()?;
    let identity = install_fixture_identity(temporary.path(), &fixture, "review-travel")?;
    let (config_path, state_path) =
        write_runtime_config(temporary.path(), &fixture, &identity, &[])?;
    let protected_paths = vec![
        config_path.clone(),
        fixture.cert("deployment-root.pub"),
        fixture.cert("deployment-trust.json"),
        identity.join(MANAGEMENT_KEY_FILE),
        identity.join(BUSINESS_KEY_FILE),
        identity.join(MANAGEMENT_CERT_FILE),
        identity.join(BUSINESS_CERT_FILE),
        identity.join(MANAGEMENT_CA_FILE),
        identity.join(BUSINESS_CA_FILE),
    ];
    let before = files_sha256(&protected_paths)?;
    let error = match TravelCore::start_with_trusted_root(
        &config_path,
        PRIVATE_KEY_PASSWORD,
        &rogue.deployment_root()?,
    )
    .await
    {
        Ok(core) => {
            shutdown_with_timeout(&core, "review r01 mismatched core").await?;
            bail!("native start accepted a mismatched deployment root");
        }
        Err(error) => error,
    };
    assert!(error.to_string().contains("deployment trust mismatch"));
    assert_eq!(files_sha256(&protected_paths)?, before);
    assert!(!state_path.exists());

    let core = TravelCore::start_with_trusted_root(
        &config_path,
        PRIVATE_KEY_PASSWORD,
        &fixture.deployment_root()?,
    )
    .await?;
    shutdown_with_timeout(&core, "review r01 native trusted core").await?;
    drop(core);
    drop(StateStore::open(state_path)?);
    Ok(())
}

#[tokio::test]
#[ignore = "run tests/check-travel-review-regressions.sh"]
async fn r02_stop_drains_an_accepted_tcp_flow_and_releases_its_resources() -> Result<()> {
    let fixture = Fixture::load("legit")?;
    let temporary = tempfile::tempdir()?;
    let identity = install_fixture_identity(temporary.path(), &fixture, "review-travel")?;
    let reservation = TcpListener::bind("127.0.0.1:0").await?;
    let bind = reservation.local_addr()?.to_string();
    drop(reservation);
    let mapping = Mapping {
        home_id: "home-1".to_owned(),
        service_id: "kept-open".to_owned(),
        protocol: ServiceProtocol::Tcp,
        bind: bind.clone(),
    };
    let (config_path, state_path) =
        write_runtime_config(temporary.path(), &fixture, &identity, &[mapping])?;
    let core = TravelCore::start(&config_path, PRIVATE_KEY_PASSWORD).await?;
    let mut local = TcpStream::connect(&bind).await?;
    wait_for_active_flow(&core).await?;

    shutdown_with_timeout(&core, "review r02 core").await?;
    assert_eq!(core.status().await.active_flows, 0);
    assert_eq!(
        core.state.carrier_permits.available_permits(),
        core.state.config.max_active_carriers
    );
    let mut eof = [0_u8; 1];
    assert_eq!(
        timeout(Duration::from_secs(2), local.read(&mut eof))
            .await
            .context("accepted TCP peer did not receive EOF during shutdown")??,
        0
    );
    drop(local);
    let rebound = TcpListener::bind(&bind).await?;
    drop(rebound);
    shutdown_with_timeout(&core, "review r02 idempotent core").await?;
    drop(core);

    drop(StateStore::open(&state_path)?);
    let restarted = TravelCore::start(&config_path, PRIVATE_KEY_PASSWORD).await?;
    shutdown_with_timeout(&restarted, "review r02 restarted core").await?;
    drop(restarted);
    drop(StateStore::open(state_path)?);
    Ok(())
}

#[tokio::test]
#[ignore = "run tests/check-travel-review-regressions.sh"]
async fn r04_initial_listener_failure_releases_prior_bind_and_state_store_for_retry() -> Result<()>
{
    let fixture = Fixture::load("legit")?;
    let temporary = tempfile::tempdir()?;
    let identity = install_fixture_identity(temporary.path(), &fixture, "review-travel")?;
    let first_reservation = TcpListener::bind("127.0.0.1:0").await?;
    let first_bind = first_reservation.local_addr()?.to_string();
    drop(first_reservation);
    let occupied = TcpListener::bind("127.0.0.1:0").await?;
    let occupied_bind = occupied.local_addr()?.to_string();
    let mappings = vec![
        Mapping {
            home_id: "home-1".to_owned(),
            service_id: "first".to_owned(),
            protocol: ServiceProtocol::Tcp,
            bind: first_bind.clone(),
        },
        Mapping {
            home_id: "home-1".to_owned(),
            service_id: "conflict".to_owned(),
            protocol: ServiceProtocol::Tcp,
            bind: occupied_bind,
        },
    ];
    let (config_path, state_path) =
        write_runtime_config(temporary.path(), &fixture, &identity, &mappings)?;
    let error = match TravelCore::start(&config_path, PRIVATE_KEY_PASSWORD).await {
        Ok(core) => {
            shutdown_with_timeout(&core, "review r04 unexpectedly started core").await?;
            bail!("initial mappings started despite an occupied second bind");
        }
        Err(error) => error,
    };
    assert!(!error.to_string().is_empty());
    let released_first = TcpListener::bind(&first_bind).await?;
    drop(released_first);
    drop(StateStore::open(&state_path)?);

    drop(occupied);
    let core = TravelCore::start(&config_path, PRIVATE_KEY_PASSWORD).await?;
    shutdown_with_timeout(&core, "review r04 retry core").await?;
    drop(core);
    drop(StateStore::open(state_path)?);
    Ok(())
}

#[tokio::test]
#[ignore = "run tests/check-travel-review-regressions.sh"]
async fn r06_unacknowledged_statistics_are_sent_again_once_after_a_new_session() -> Result<()> {
    let fixture = Fixture::load("legit")?;
    let temporary = tempfile::tempdir()?;
    let identity = install_fixture_identity(temporary.path(), &fixture, "review-travel")?;
    let (config_path, _) = write_runtime_config(temporary.path(), &fixture, &identity, &[])?;
    let state = load_app_state(&config_path, Some(PRIVATE_KEY_PASSWORD))?;
    state.statistics.record(
        unix_time_secs()?,
        "review_statistics_metric",
        BTreeMap::new(),
        1,
        None,
    );

    let (mut first_server, first_catalog) = start_catalog_session(&state, &fixture).await?;
    let first_report =
        receive_catalog_statistics_report(&mut first_server, Duration::from_secs(2)).await?;
    let digest = first_report.digest_sha256()?;
    assert_eq!(state.statistics.pending_reports(2)?.len(), 1);
    drop(first_server);
    join_catalog_after_peer_disconnect(first_catalog, "first").await?;

    let (mut restarted_server, restarted_catalog) = start_catalog_session(&state, &fixture).await?;
    let restarted_report =
        receive_catalog_statistics_report(&mut restarted_server, Duration::from_secs(2)).await?;
    assert_eq!(restarted_report.digest_sha256()?, digest);
    write_json(
        &mut restarted_server,
        &ControlMessage::StatisticsReportAck {
            digest_sha256: digest,
            accepted: true,
            error: None,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    wait_for_empty_statistics_outbox(&state).await?;
    assert_no_statistics_report_on_next_tick(&mut restarted_server).await?;
    drop(restarted_server);
    join_catalog_after_peer_disconnect(restarted_catalog, "restarted").await?;
    Ok(())
}
