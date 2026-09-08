use super::catalog_for_credentials;
use anyhow::{Result, anyhow};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use flowsplice_core::{
    authorization::{TravelCredential, TravelCredentialScope},
    business::{BusinessService, HomeServiceGrant, SignedHomeServiceGrant, payload_digest},
    deployment::{DeploymentTrust, HomeEndpointCredential, SignedHomeEndpointCredential},
    protocol::{Catalog, HomeCatalog, Service, ServiceProtocol},
};
use serde_json::json;
use uuid::Uuid;
struct Fixture {
    key: EcdsaKeyPair,
    trust: DeploymentTrust,
    now: u64,
}
impl Fixture {
    fn new() -> Result<Self> {
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow::anyhow!("key generation"))?;
        // targets() receives already verified trust; this fixture tests its signed
        // endpoint/grant boundary, rather than repeating outer snapshot validation.
        let now = flowsplice_core::authorization::unix_time_secs()?;
        let trust = serde_json::from_value(
            json!({"version":1,"deployment_id":"deployment","generation":1,"not_before_unix_secs":now-1000,"not_after_unix_secs":now+10000,"management_ca_certificate_pem":"fixture","business_ca_certificate_pem":"fixture","server_control_keys":[],"home_endpoints":[],"home_enrollment_authorities":[{"id":"home-authority","epoch":1,"issuer_home_id":"issuer","public_key":hex::encode(key.public_key().as_ref())}],"travel_authorities":[]}),
        )?;
        Ok(Self { key, trust, now })
    }
    fn home(&self, id: &str, service_id: &str, app: &str) -> Result<HomeCatalog> {
        let endpoint: HomeEndpointCredential = serde_json::from_value(
            json!({"version":1,"object_type":"flowsplice.home_endpoint_credential","deployment_id":"deployment","credential_id":Uuid::new_v4(),"authority_id":"home-authority","authority_epoch":1,"enrollment_request_id":Uuid::new_v4(),"home_id":id,"management_spki_sha256":"33".repeat(32),"business_spki_sha256":"44".repeat(32),"not_before_unix_secs":self.now-900,"not_after_unix_secs":self.now+8000}),
        )?;
        let endpoint = SignedHomeEndpointCredential::sign(&endpoint, &self.key)?;
        let grant = HomeServiceGrant {
            version: 1,
            object_type: "flowsplice.home_service_grant".into(),
            deployment_id: "deployment".into(),
            authority_id: "home-authority".into(),
            authority_epoch: 1,
            home_id: id.into(),
            endpoint_payload_sha256: payload_digest(&endpoint.payload_hex)?,
            enrollment_request_sha256: "55".repeat(32),
            services: vec![BusinessService {
                service_id: service_id.into(),
                protocol: ServiceProtocol::Tcp,
                application_protocol: app.into(),
                capabilities: vec!["read".into(), "write".into()],
            }],
            not_before_unix_secs: self.now - 800,
            not_after_unix_secs: self.now + 7000,
        };
        Ok(HomeCatalog {
            home_id: id.into(),
            home_alias: "PTY".into(),
            endpoint_credential: Some(endpoint),
            service_grant: Some(SignedHomeServiceGrant::sign(&grant, &self.key)?),
            services: vec![Service {
                id: service_id.into(),
                alias: "PTY".into(),
                target: "127.0.0.1:22".into(),
                protocol: ServiceProtocol::Tcp,
            }],
        })
    }
}
fn credential(scope: &TravelCredentialScope) -> Result<TravelCredential> {
    Ok(serde_json::from_value(
        json!({"version":1,"object_type":"flowsplice.travel_credential","deployment_id":"deployment","deployment_trust_sha256":"aa".repeat(32),"credential_id":Uuid::new_v4(),"authority_id":"issuer","authority_epoch":1,"enrollment_request_id":Uuid::new_v4(),"enrollment_nonce":"66".repeat(32),"enrollment_request_sha256":"77".repeat(32),"travel_id":"travel","management_spki_sha256":"88".repeat(32),"business_spki_sha256":"99".repeat(32),"management_ca_sha256":"aa".repeat(32),"business_ca_sha256":"bb".repeat(32),"management_certificate_sha256":"cc".repeat(32),"business_certificate_sha256":"dd".repeat(32),"scope":scope,"not_before_unix_secs":1,"not_after_unix_secs":u64::MAX}),
    )?)
}
fn class() -> Result<TravelCredential> {
    credential(&TravelCredentialScope::ServiceClass {
        application_protocol: "flowsplice.pty.v1".into(),
        protocol: ServiceProtocol::Tcp,
    })
}
fn catalog(homes: Vec<HomeCatalog>) -> Catalog {
    Catalog {
        generation: 7,
        homes,
    }
}
#[test]
fn verified_class_catalog_includes_distinct_home_services_not_display_names() -> Result<()> {
    let f = Fixture::new()?;
    let raw = HomeCatalog {
        home_id: "raw".into(),
        home_alias: "PTY".into(),
        endpoint_credential: None,
        service_grant: None,
        services: vec![Service {
            id: "terminal".into(),
            alias: "PTY".into(),
            protocol: ServiceProtocol::Tcp,
            target: "ignored".into(),
        }],
    };
    let c = catalog(vec![
        f.home("mac", "terminal", "flowsplice.pty.v1")?,
        f.home("vps", "console", "flowsplice.pty.v1")?,
        f.home("other", "terminal", "another.protocol")?,
        raw,
    ]);
    let result = catalog_for_credentials(&c, &[class()?], Some(&f.trust));
    assert_eq!(result.generation, 7);
    assert_eq!(result.homes.len(), 2);
    assert_eq!(result.homes[0].home_id, "mac");
    assert_eq!(result.homes[1].services[0].id, "console");
    assert!(
        catalog_for_credentials(&c, &[class()?], None)
            .homes
            .is_empty()
    );
    Ok(())
}
#[test]
fn invalid_signed_material_never_enters_class_catalog() -> Result<()> {
    let f = Fixture::new()?;
    let good = f.home("mac", "pty", "flowsplice.pty.v1")?;
    let mut invalid = Vec::new();
    let mut h = good.clone();
    h.service_grant = None;
    invalid.push(h);
    let mut h = good.clone();
    h.endpoint_credential = None;
    invalid.push(h);
    let mut h = good.clone();
    h.service_grant
        .as_mut()
        .ok_or_else(|| anyhow!("grant"))?
        .signature_hex = "00".into();
    invalid.push(h);
    let mut h = good.clone();
    h.endpoint_credential
        .as_mut()
        .ok_or_else(|| anyhow!("endpoint"))?
        .signature_hex = "00".into();
    invalid.push(h);
    let mut h = good.clone();
    h.service_grant = f.home("swapped", "pty", "flowsplice.pty.v1")?.service_grant;
    invalid.push(h);
    let mut h = good.clone();
    h.services[0].id = "unapproved".into();
    invalid.push(h);
    let mut h = good.clone();
    h.services[0].protocol = ServiceProtocol::Udp;
    invalid.push(h);
    let mut h = good.clone();
    let signed = h.service_grant.as_mut().ok_or_else(|| anyhow!("grant"))?;
    let mut grant: HomeServiceGrant = serde_json::from_slice(&hex::decode(&signed.payload_hex)?)?;
    grant.not_after_unix_secs = f.now - 1;
    *signed = SignedHomeServiceGrant::sign(&grant, &f.key)?;
    invalid.push(h);
    let mut h = good.clone();
    let signed = h
        .endpoint_credential
        .as_mut()
        .ok_or_else(|| anyhow!("endpoint"))?;
    let mut endpoint: HomeEndpointCredential =
        serde_json::from_slice(&hex::decode(&signed.payload_hex)?)?;
    endpoint.not_after_unix_secs = f.now - 1;
    *signed = SignedHomeEndpointCredential::sign(&endpoint, &f.key)?;
    let signed_grant = h.service_grant.as_mut().ok_or_else(|| anyhow!("grant"))?;
    let mut grant: HomeServiceGrant =
        serde_json::from_slice(&hex::decode(&signed_grant.payload_hex)?)?;
    grant.not_after_unix_secs = f.now - 1;
    grant.endpoint_payload_sha256 = payload_digest(&signed.payload_hex)?;
    *signed_grant = SignedHomeServiceGrant::sign(&grant, &f.key)?;
    invalid.push(h);
    let mut udp = good.clone();
    udp.services[0].protocol = ServiceProtocol::Udp;
    let signed = udp.service_grant.as_mut().ok_or_else(|| anyhow!("grant"))?;
    let mut grant: HomeServiceGrant = serde_json::from_slice(&hex::decode(&signed.payload_hex)?)?;
    grant.services[0].protocol = ServiceProtocol::Udp;
    *signed = SignedHomeServiceGrant::sign(&grant, &f.key)?;
    invalid.push(udp);
    for h in invalid {
        assert!(
            catalog_for_credentials(&catalog(vec![h]), &[class()?], Some(&f.trust))
                .homes
                .is_empty()
        );
    }
    Ok(())
}
#[test]
fn mixed_credentials_preserve_exact_legacy_union_and_empty_home() -> Result<()> {
    let f = Fixture::new()?;
    let matching = f.home("mac", "pty", "flowsplice.pty.v1")?;
    let mut legacy = f.home("legacy", "allowed", "other.protocol")?;
    legacy.endpoint_credential = None;
    legacy.service_grant = None;
    let mut denied = legacy.services[0].clone();
    denied.id = "denied".into();
    legacy.services.push(denied);
    let empty = HomeCatalog {
        home_id: "empty".into(),
        home_alias: "Empty".into(),
        endpoint_credential: None,
        service_grant: None,
        services: Vec::new(),
    };
    let c = catalog(vec![matching, legacy, empty]);
    let narrow = credential(&TravelCredentialScope::Service {
        home_id: "legacy".into(),
        service_id: "allowed".into(),
        protocol: ServiceProtocol::Tcp,
    })?;
    let r = catalog_for_credentials(&c, &[class()?, narrow], Some(&f.trust));
    assert_eq!(
        r.homes.len(),
        2,
        "Mixed narrow approval must not reveal unrelated empty Homes"
    );
    assert_eq!(r.homes.iter().flat_map(|h| h.services.iter()).count(), 2);
    let legacy = r
        .homes
        .iter()
        .find(|h| h.home_id == "legacy")
        .ok_or_else(|| anyhow!("legacy Home"))?;
    assert_eq!(legacy.services.len(), 1);
    assert_eq!(legacy.services[0].id, "allowed");
    let empty = catalog_for_credentials(
        &c,
        &[credential(&TravelCredentialScope::Home {
            home_id: "empty".into(),
        })?],
        Some(&f.trust),
    );
    assert_eq!(empty.homes.len(), 1);
    assert!(empty.homes[0].services.is_empty());
    assert_eq!(
        catalog_for_credentials(
            &c,
            &[class()?, credential(&TravelCredentialScope::Global)?],
            Some(&f.trust)
        ),
        c
    );
    Ok(())
}
