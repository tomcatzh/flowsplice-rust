use super::*;
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};
use flowsplice_core::{
    authorization::TravelCredential,
    business::{BusinessService, HomeServiceGrant, payload_digest},
    deployment::HomeEndpointCredential,
    protocol::ServiceProtocol,
};
use serde_json::json;

struct Fixture {
    path: PathBuf,
    key: EcdsaKeyPair,
    trust: DeploymentTrust,
    endpoint: SignedHomeEndpointCredential,
    grant: SignedHomeServiceGrant,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
impl Fixture {
    fn new() -> Result<Self> {
        let path =
            std::env::temp_dir().join(format!("flowsplice-server-business-{}", Uuid::new_v4()));
        fs::create_dir(&path)?;
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("test key generation failed"))?;
        let now = unix_time_secs()?;
        // ServerAuthorization receives root-verified trust; CA parsing belongs to that upstream boundary.
        let trust: DeploymentTrust = serde_json::from_value(json!({
            "version":1,"deployment_id":"deployment","generation":1,"not_before_unix_secs":1,"not_after_unix_secs":now+10000,
            "management_ca_certificate_pem":"","business_ca_certificate_pem":"","server_control_keys":[],
            "home_endpoints":[{"home_id":"super-home","management_spki_pins":["11".repeat(32)],"business_spki_pins":["22".repeat(32)]}],
            "home_enrollment_authorities":[{"id":"home-authority","epoch":1,"issuer_home_id":"super-home","public_key":hex::encode(key.public_key().as_ref())}],
            "travel_authorities":[{"kind":"global","id":"travel-authority","epoch":1,"home_id":"super-home","public_key":hex::encode(key.public_key().as_ref())}]
        }))?;
        let endpoint: HomeEndpointCredential = serde_json::from_value(json!({
            "version":1,"object_type":"flowsplice.home_endpoint_credential","deployment_id":"deployment",
            "credential_id":Uuid::new_v4(),"authority_id":"home-authority","authority_epoch":1,
            "enrollment_request_id":Uuid::new_v4(),"home_id":"business-home","management_spki_sha256":"33".repeat(32),
            "business_spki_sha256":"44".repeat(32),"not_before_unix_secs":1,"not_after_unix_secs":now+8000
        }))?;
        let endpoint = SignedHomeEndpointCredential::sign(&endpoint, &key)?;
        let grant = SignedHomeServiceGrant::sign(
            &HomeServiceGrant {
                version: 1,
                object_type: "flowsplice.home_service_grant".into(),
                deployment_id: "deployment".into(),
                authority_id: "home-authority".into(),
                authority_epoch: 1,
                home_id: "business-home".into(),
                endpoint_payload_sha256: payload_digest(&endpoint.payload_hex)?,
                enrollment_request_sha256: "55".repeat(32),
                services: vec![BusinessService {
                    service_id: "pty".into(),
                    protocol: ServiceProtocol::Tcp,
                    application_protocol: "pty/1".into(),
                    capabilities: vec!["read".into()],
                }],
                not_before_unix_secs: 1,
                not_after_unix_secs: now + 7000,
            },
            &key,
        )?;
        store_json_atomic(
            &path.join("state.json"),
            &PersistentAuthorizationState::default(),
        )?;
        Ok(Self {
            path,
            key,
            trust,
            endpoint,
            grant,
        })
    }
    fn load(&self) -> Result<ServerAuthorization> {
        ServerAuthorization::load_with_trust(self.trust.clone(), self.path.join("state.json"))
    }
    fn import(&self, state: &mut ServerAuthorization) -> Result<bool> {
        state.import_business_home(&self.trust, self.endpoint.clone(), self.grant.clone())
    }
}
fn service() -> Service {
    Service {
        id: "pty".into(),
        alias: "PTY".into(),
        protocol: ServiceProtocol::Tcp,
        target: "in-process".into(),
    }
}

#[test]
fn business_import_is_atomic_idempotent_and_survives_reload_and_legacy_retry() -> Result<()> {
    let f = Fixture::new()?;
    let mut state = f.load()?;
    let before = state.snapshot.generation;
    assert!(f.import(&mut state)?);
    assert_eq!(state.snapshot.generation, before + 1);
    assert_eq!(
        state.snapshot.home_endpoint_credentials,
        vec![f.endpoint.clone()]
    );
    assert_eq!(
        state.business_home_grants.get("business-home"),
        Some(&f.grant)
    );
    assert!(!f.import(&mut state)?);
    assert!(!state.import_home_endpoint(&f.trust, f.endpoint.clone())?);
    assert_eq!(state.snapshot.generation, before + 1);
    let reloaded = f.load()?;
    assert_eq!(reloaded.business_home_grants, state.business_home_grants);
    reloaded.validate_home_services(&f.trust, "business-home", Some(&f.endpoint), &[service()])?;
    reloaded.validate_home_services(&f.trust, "business-home", Some(&f.endpoint), &[])?;
    assert!(
        reloaded
            .validate_home_services(&f.trust, "business-home", None, &[service()])
            .is_err()
    );
    for invalid in [
        Service {
            id: "extra".into(),
            ..service()
        },
        Service {
            protocol: ServiceProtocol::Udp,
            ..service()
        },
    ] {
        assert!(
            reloaded
                .validate_home_services(&f.trust, "business-home", Some(&f.endpoint), &[invalid])
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn changed_grants_and_endpoints_cannot_replace_existing_business_home() -> Result<()> {
    let f = Fixture::new()?;
    let mut state = f.load()?;
    f.import(&mut state)?;
    let before = serde_json::to_value(state.snapshot())?;
    let mut grant: HomeServiceGrant = serde_json::from_slice(&hex::decode(&f.grant.payload_hex)?)?;
    grant.services[0].service_id = "other".into();
    let changed = SignedHomeServiceGrant::sign(&grant, &f.key)?;
    assert!(
        state
            .import_business_home(&f.trust, f.endpoint.clone(), changed)
            .is_err()
    );
    let mut endpoint: HomeEndpointCredential =
        serde_json::from_slice(&hex::decode(&f.endpoint.payload_hex)?)?;
    endpoint.credential_id = Uuid::new_v4();
    let endpoint = SignedHomeEndpointCredential::sign(&endpoint, &f.key)?;
    assert!(
        state
            .import_home_endpoint(&f.trust, endpoint.clone())
            .is_err()
    );
    grant.endpoint_payload_sha256 = payload_digest(&endpoint.payload_hex)?;
    assert!(
        state
            .import_business_home(
                &f.trust,
                endpoint,
                SignedHomeServiceGrant::sign(&grant, &f.key)?
            )
            .is_err()
    );
    assert_eq!(serde_json::to_value(state.snapshot())?, before);
    assert_eq!(
        state.business_home_grants.get("business-home"),
        Some(&f.grant)
    );
    Ok(())
}

#[test]
fn failed_durable_write_publishes_neither_endpoint_grant_nor_generation() -> Result<()> {
    let f = Fixture::new()?;
    let mut state = f.load()?;
    let before = serde_json::to_value(state.snapshot())?;
    let disk_before = fs::read(f.path.join("state.json"))?;
    let blocker = f.path.join("not-a-directory");
    fs::write(&blocker, b"block")?;
    state.state_path = blocker.join("state.json");
    assert!(f.import(&mut state).is_err());
    assert_eq!(serde_json::to_value(state.snapshot())?, before);
    assert!(state.business_home_grants.is_empty());
    assert_eq!(fs::read(f.path.join("state.json"))?, disk_before);
    Ok(())
}

#[test]
fn historical_expired_grant_loads_but_service_registration_fails() -> Result<()> {
    let f = Fixture::new()?;
    let mut payload: HomeServiceGrant =
        serde_json::from_slice(&hex::decode(&f.grant.payload_hex)?)?;
    payload.not_after_unix_secs = unix_time_secs()?.saturating_sub(1);
    let expired = SignedHomeServiceGrant::sign(&payload, &f.key)?;
    let mut persisted = PersistentAuthorizationState::default();
    persisted
        .snapshot
        .home_endpoint_credentials
        .push(f.endpoint.clone());
    persisted
        .business_home_grants
        .insert("business-home".into(), expired);
    store_json_atomic(&f.path.join("state.json"), &persisted)?;
    let state = f.load()?;
    assert!(
        state
            .validate_home_services(&f.trust, "business-home", Some(&f.endpoint), &[service()])
            .is_err()
    );
    Ok(())
}

#[test]
fn acknowledgement_targets_issuer_and_exact_request_not_business_destination() -> Result<()> {
    let f = Fixture::new()?;
    let mut state = f.load()?;
    let travel: TravelCredential = serde_json::from_value(json!({
        "version":1,"object_type":"flowsplice.travel_credential","deployment_id":"deployment","deployment_trust_sha256":"aa".repeat(32),
        "credential_id":Uuid::new_v4(),"authority_id":"travel-authority","authority_epoch":1,"enrollment_request_id":Uuid::new_v4(),
        "enrollment_nonce":"66".repeat(32),"enrollment_request_sha256":"77".repeat(32),"travel_id":"travel",
        "management_spki_sha256":"88".repeat(32),"business_spki_sha256":"99".repeat(32),"management_ca_sha256":"aa".repeat(32),"business_ca_sha256":"bb".repeat(32),
        "management_certificate_sha256":"cc".repeat(32),"business_certificate_sha256":"dd".repeat(32),
        "scope":{"kind":"service","home_id":"business-home","service_id":"pty","protocol":"tcp"},"not_before_unix_secs":1,"not_after_unix_secs":unix_time_secs()?+1000
    }))?;
    let bytes = serde_json::to_vec(&travel)?;
    let signature = f
        .key
        .sign(&SystemRandom::new(), &bytes)
        .map_err(|_| anyhow!("test signing failed"))?;
    state.import_credential(
        SignedTravelCredential {
            authority_id: travel.authority_id.clone(),
            payload_hex: hex::encode(bytes),
            signature_hex: hex::encode(signature.as_ref()),
        },
        "super-home",
    )?;
    state.validate_install_acknowledgement(
        travel.credential_id,
        travel.enrollment_request_id,
        "super-home",
    )?;
    for request in [Uuid::new_v4(), Uuid::nil()] {
        assert!(
            state
                .validate_install_acknowledgement(travel.credential_id, request, "super-home")
                .is_err()
        );
    }
    assert!(
        state
            .validate_install_acknowledgement(
                Uuid::new_v4(),
                travel.enrollment_request_id,
                "super-home"
            )
            .is_err()
    );
    for home in ["business-home", "unrelated-home"] {
        assert!(
            state
                .validate_install_acknowledgement(
                    travel.credential_id,
                    travel.enrollment_request_id,
                    home
                )
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn old_empty_state_serialization_omits_business_grants() -> Result<()> {
    let value = serde_json::to_value(PersistentAuthorizationState::default())?;
    assert!(value.get("business_home_grants").is_none());
    let decoded: PersistentAuthorizationState = serde_json::from_value(value)?;
    assert!(decoded.business_home_grants.is_empty());
    Ok(())
}
