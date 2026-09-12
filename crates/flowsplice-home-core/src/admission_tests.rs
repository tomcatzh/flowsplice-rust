//! Exercise Home admission after real TLS, without `TravelCore`'s preflight checks.
use super::*;
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};
use flowsplice_core::{
    authorization::{SignedTravelCredential, TravelAuthorizationSnapshot, TrustedTravelAuthority},
    route::{read_preface, verify_preface},
    tls::identity_from_certificate_pem,
};
use flowsplice_storage::StateStore;
use flowsplice_transport::{BoxStream, DatagramIo, IoFuture};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    sync::atomic::{AtomicUsize, Ordering},
};

#[derive(Default)]
struct RecordingProvider(AtomicUsize);
impl ServiceProvider for RecordingProvider {
    fn connect_tcp<'a>(&'a self, _: &'a Service, _: ServicePeer) -> IoFuture<'a, BoxStream> {
        Box::pin(async { bail!("unexpected TCP application connection") })
    }
    fn connect_udp<'a>(
        &'a self,
        _: &'a Service,
        _: ServicePeer,
    ) -> IoFuture<'a, Arc<dyn DatagramIo>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { bail!("application admission reached") })
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn raw_open_enforces_home_service_admission_after_authenticated_tls() -> Result<()> {
    flowsplice_core::init_crypto();
    let directory = tempfile::tempdir()?;
    let ca_key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut ca_params = rcgen::CertificateParams::default();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign];
    let ca = ca_params.self_signed(&ca_key)?;
    let ca_path = directory.path().join("ca.pem");
    fs::write(&ca_path, ca.pem())?;
    let issuer = rcgen::Issuer::from_ca_cert_pem(&ca.pem(), ca_key)?;
    let mut travel_pem = String::new();
    for role in ["home", "travel"] {
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()])?;
        params.subject_alt_names.push(rcgen::SanType::URI(
            format!("flowsplice://identity/{role}/{role}-1").try_into()?,
        ));
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        ];
        let cert = params.signed_by(&key, &issuer)?;
        fs::write(directory.path().join(format!("{role}.pem")), cert.pem())?;
        let key_path = directory.path().join(format!("{role}.key"));
        fs::write(&key_path, key.serialize_pem())?;
        fs::set_permissions(key_path, fs::Permissions::from_mode(0o600))?;
        if role == "travel" {
            travel_pem = cert.pem();
        }
    }
    let acceptor = server_acceptor(
        &directory.path().join("home.pem"),
        &directory.path().join("home.key"),
        &ca_path,
    )?;
    let connector = client_connector(
        &directory.path().join("travel.pem"),
        &directory.path().join("travel.key"),
        &ca_path,
    )?;
    let identity = identity_from_certificate_pem(&travel_pem)?;
    let now = unix_time_secs()?;
    let credential: TravelCredential = serde_json::from_value(serde_json::json!({
        "version":1,"object_type":"flowsplice.travel_credential","deployment_id":"admission-test",
        "deployment_trust_sha256":"aa".repeat(32),"credential_id":Uuid::new_v4(),
        "authority_id":"authority","authority_epoch":1,"enrollment_request_id":Uuid::new_v4(),
        "enrollment_nonce":"aa".repeat(32),"enrollment_request_sha256":"bb".repeat(32),
        "travel_id":"travel-1","management_spki_sha256":identity.spki_sha256,
        "business_spki_sha256":identity.spki_sha256,"management_ca_sha256":"cc".repeat(32),
        "business_ca_sha256":"cc".repeat(32),"management_certificate_sha256":identity.certificate_sha256,
        "business_certificate_sha256":identity.certificate_sha256,
        "scope":{"kind":"service","home_id":"home-1","service_id":"allowed","protocol":"udp"},
        "not_before_unix_secs":now-1,"not_after_unix_secs":now+300
    }))?;
    let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
        .map_err(|_| anyhow::anyhow!("test authority key generation failed"))?;
    let payload = serde_json::to_vec(&credential)?;
    let signature = key
        .sign(&SystemRandom::new(), &payload)
        .map_err(|_| anyhow::anyhow!("test signing failed"))?;
    let authorization = VerifiedAuthorization::verify(
        &TravelAuthorizationSnapshot {
            generation: 1,
            home_endpoint_credentials: vec![],
            revocations: vec![],
            credentials: vec![SignedTravelCredential {
                authority_id: "authority".to_owned(),
                payload_hex: hex::encode(payload),
                signature_hex: hex::encode(signature.as_ref()),
            }],
        },
        &[TrustedTravelAuthority::Home {
            id: "authority".to_owned(),
            epoch: 1,
            home_id: "home-1".to_owned(),
            public_key: hex::encode(key.public_key().as_ref()),
        }],
        "admission-test",
    )?;
    let (_publisher, authorization_rx) = watch::channel(Some(Arc::new(authorization)));
    let config = Arc::new(HomeFlowConfig {
        id: "home-1".to_owned(),
        services: ["allowed", "forbidden"]
            .into_iter()
            .map(|id| Service {
                id: id.to_owned(),
                alias: id.to_owned(),
                protocol: ServiceProtocol::Udp,
                target: "unused".to_owned(),
            })
            .collect(),
        business_services: vec![],
        handshake_timeout_secs: 5,
        udp_idle_secs: 5,
    });
    let statistics = Arc::new(LocalStatistics::new(StateStore::open(
        directory.path().join("state.redb"),
    )?));
    // The allowed control must reach the provider, not merely fail for a TLS/fixture reason.
    for (service_id, expected_calls, expected_error) in [
        ("allowed", 1, "application admission reached"),
        (
            "forbidden",
            0,
            "Travel credential is not authorized for this logical service",
        ),
    ] {
        let provider = Arc::new(RecordingProvider::default());
        let permits = Arc::new(Semaphore::new(2));
        let registry = TcpFlowRegistry::new(
            permits.clone(),
            Duration::from_secs(1),
            Duration::from_secs(5),
            Duration::from_secs(5),
            65536,
            2,
            2,
            statistics.clone(),
            provider.clone(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let work_id = Uuid::new_v4();
        let secret = vec![42; 32];
        let work = run_work(
            config.clone(),
            acceptor.clone(),
            permits,
            registry,
            authorization_rx.clone(),
            credential.credential_id,
            work_id,
            secret.clone(),
            "relay-test".to_owned(),
            listener.local_addr()?.to_string(),
            now + 300,
        );
        let client = async {
            let (mut socket, _) = listener.accept().await?;
            let (preface, mac) = read_preface(&mut socket).await?;
            assert_eq!(preface.id, work_id);
            assert_eq!(preface.side, RouteSide::Home);
            assert!(verify_preface(preface, &mac, &secret));
            let mut tls = connector.connect(server_name("localhost")?, socket).await?;
            write_json(
                &mut tls,
                &DataFrame::Open {
                    flow_id: Uuid::new_v4(),
                    carrier_id: Uuid::new_v4(),
                    service_id: service_id.to_owned(),
                    protocol: ServiceProtocol::Udp,
                    data_protocol_version: 1,
                },
                DATA_FRAME_LIMIT,
            )
            .await?;
            // Keep the peer alive until Home closes its rejected/completed work connection.
            let mut buffer = [0; 1];
            let _ = tokio::io::AsyncReadExt::read(&mut tls, &mut buffer).await;
            Ok::<_, anyhow::Error>(())
        };
        let (result, client_result) = timeout(Duration::from_secs(10), async {
            tokio::join!(Box::pin(work), Box::pin(client))
        })
        .await?;
        client_result?;
        assert_eq!(
            result
                .err()
                .context("Home unexpectedly admitted work")?
                .to_string(),
            expected_error
        );
        assert_eq!(provider.0.load(Ordering::SeqCst), expected_calls);
    }
    Ok(())
}
