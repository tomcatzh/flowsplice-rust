use super::*;
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use flowsplice_core::{
    business::{HomeServiceGrant, SignedHomeServiceGrant, payload_digest},
    deployment::{HomeEndpointCredential, SignedHomeEndpointCredential},
    protocol::{HomeCatalog, Service, ServiceProtocol},
};
use serde_json::json;

const NOW: u64 = 1_000;
struct Fixture {
    key: EcdsaKeyPair,
    trust: DeploymentTrust,
    descriptor: ServiceClassDescriptor,
}
impl Fixture {
    fn new() -> Result<Self> {
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow::anyhow!("key generation"))?;
        // targets() receives already verified trust; this fixture tests its signed
        // endpoint/grant boundary, rather than repeating outer snapshot validation.
        let trust = serde_json::from_value(
            json!({"version":1,"deployment_id":"deployment","generation":1,"not_before_unix_secs":100,"not_after_unix_secs":10000,"management_ca_certificate_pem":"fixture","business_ca_certificate_pem":"fixture","server_control_keys":[],"home_endpoints":[],"home_enrollment_authorities":[{"id":"home-authority","epoch":1,"issuer_home_id":"issuer","public_key":hex::encode(key.public_key().as_ref())}],"travel_authorities":[]}),
        )?;
        let descriptor = ServiceClassDescriptor {
            version: 1,
            approving_home_id: "issuer".into(),
            application_protocol: "flowsplice.pty.v1".into(),
            protocol: ServiceProtocol::Tcp,
        };
        Ok(Self {
            key,
            trust,
            descriptor,
        })
    }
    fn home(&self, id: &str, service_id: &str, app: &str) -> Result<HomeCatalog> {
        let endpoint: HomeEndpointCredential = serde_json::from_value(
            json!({"version":1,"object_type":"flowsplice.home_endpoint_credential","deployment_id":"deployment","credential_id":Uuid::new_v4(),"authority_id":"home-authority","authority_epoch":1,"enrollment_request_id":Uuid::new_v4(),"home_id":id,"management_spki_sha256":"33".repeat(32),"business_spki_sha256":"44".repeat(32),"not_before_unix_secs":200,"not_after_unix_secs":8000}),
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
            not_before_unix_secs: 300,
            not_after_unix_secs: 7000,
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
#[test]
fn targets_discover_two_and_later_third_without_matching_display_aliases() -> Result<()> {
    let f = Fixture::new()?;
    let mut catalog = Catalog {
        generation: 1,
        homes: vec![
            f.home("mac", "terminal", "flowsplice.pty.v1")?,
            f.home("vps", "console", "flowsplice.pty.v1")?,
            f.home("ordinary", "terminal", "other.application")?,
        ],
    };
    let initial = targets(&catalog, &f.trust, &f.descriptor, NOW)?;
    assert_eq!(initial.len(), 2);
    assert_eq!(initial[0].id, serde_json::to_string(&("mac", "terminal"))?);
    assert_eq!(initial[1].home_id, "vps");
    catalog
        .homes
        .push(f.home("later", "pty-new", "flowsplice.pty.v1")?);
    assert_eq!(targets(&catalog, &f.trust, &f.descriptor, NOW)?.len(), 3);
    Ok(())
}
#[test]
fn targets_reject_missing_tampered_expired_or_wrong_home_material() -> Result<()> {
    let f = Fixture::new()?;
    let good = f.home("mac", "terminal", "flowsplice.pty.v1")?;
    let mut invalid = Vec::new();
    let mut home = good.clone();
    home.service_grant = None;
    invalid.push(home);
    let mut home = good.clone();
    home.endpoint_credential = None;
    invalid.push(home);
    let mut home = good.clone();
    home.home_id = "substituted".into();
    invalid.push(home);
    let mut home = good.clone();
    home.services[0].protocol = ServiceProtocol::Udp;
    invalid.push(home);
    let mut home = good.clone();
    home.service_grant
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("grant"))?
        .signature_hex = "00".into();
    invalid.push(home);
    let mut home = good.clone();
    home.endpoint_credential
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("endpoint"))?
        .signature_hex = "00".into();
    invalid.push(home);
    for home in invalid {
        assert!(
            targets(
                &Catalog {
                    generation: 1,
                    homes: vec![home]
                },
                &f.trust,
                &f.descriptor,
                NOW
            )?
            .is_empty()
        );
    }
    for expired in [7000, 8000] {
        assert!(
            targets(
                &Catalog {
                    generation: 1,
                    homes: vec![good.clone()]
                },
                &f.trust,
                &f.descriptor,
                expired
            )?
            .is_empty()
        );
    }
    assert!(targets(&Catalog::default(), &f.trust, &f.descriptor, 10000).is_err());
    Ok(())
}
