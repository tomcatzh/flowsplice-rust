use anyhow::{Result, anyhow};
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
};
use flowsplice_core::{
    authorization::{
        SignedTravelCredential, TravelCredential, TravelCredentialScope, TrustedTravelAuthority,
    },
    business::{
        BusinessDescriptor, BusinessService, BusinessTravelApproval, HomeServiceGrant,
        SignedBusinessTravelApproval, SignedHomeServiceGrant, json_digest, payload_digest,
        validate_services,
    },
    deployment::{
        DeploymentTrust, HomeEndpointCredential, SignedDeploymentTrust,
        SignedHomeEndpointCredential,
    },
    protocol::{Service, ServiceProtocol},
};
use serde_json::json;
use uuid::Uuid;

const NOW: u64 = 1_000;
fn key() -> Result<EcdsaKeyPair> {
    EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
        .map_err(|_| anyhow!("test key generation failed"))
}
fn service() -> BusinessService {
    BusinessService {
        service_id: "pty".into(),
        protocol: ServiceProtocol::Tcp,
        application_protocol: "flowsplice-pty/1".into(),
        capabilities: vec!["read".into(), "write".into()],
    }
}
fn sign_travel(payload: &TravelCredential, key: &EcdsaKeyPair) -> Result<SignedTravelCredential> {
    let bytes = serde_json::to_vec(payload)?;
    let signature = key
        .sign(&SystemRandom::new(), &bytes)
        .map_err(|_| anyhow!("test signing failed"))?;
    Ok(SignedTravelCredential {
        authority_id: payload.authority_id.clone(),
        payload_hex: hex::encode(bytes),
        signature_hex: hex::encode(signature.as_ref()),
    })
}
struct Fixture {
    home_key: EcdsaKeyPair,
    travel_key: EcdsaKeyPair,
    trust: DeploymentTrust,
    endpoint: HomeEndpointCredential,
    descriptor: BusinessDescriptor,
    grant: HomeServiceGrant,
    travel: TravelCredential,
}
impl Fixture {
    fn new() -> Result<Self> {
        let home_key = key()?;
        let travel_key = key()?;
        let root = key()?;
        let ca = rcgen::generate_simple_self_signed(vec!["fixture.test".into()])?
            .cert
            .pem();
        let trust: DeploymentTrust = serde_json::from_value(json!({
            "version":1,"deployment_id":"deployment","generation":1,"not_before_unix_secs":100,"not_after_unix_secs":10000,
            "management_ca_certificate_pem":ca,"business_ca_certificate_pem":ca,
            "server_control_keys":[{"server_id":"server","epoch":1,"public_key":hex::encode(root.public_key().as_ref())}],
            "home_endpoints":[{"home_id":"super-home","management_spki_pins":["11".repeat(32)],"business_spki_pins":["22".repeat(32)]}],
            "home_enrollment_authorities":[{"id":"home-authority","epoch":1,"issuer_home_id":"super-home","public_key":hex::encode(home_key.public_key().as_ref())}],
            "travel_authorities":[{"kind":"global","id":"travel-authority","epoch":1,"home_id":"super-home","public_key":hex::encode(travel_key.public_key().as_ref())}]
        }))?;
        let signed_trust = SignedDeploymentTrust::sign(&trust, &root)?;
        let trust = signed_trust.verify(&hex::encode(root.public_key().as_ref()), NOW)?;
        let endpoint: HomeEndpointCredential = serde_json::from_value(json!({
            "version":1,"object_type":"flowsplice.home_endpoint_credential","deployment_id":"deployment",
            "credential_id":Uuid::new_v4(),"authority_id":"home-authority","authority_epoch":1,
            "enrollment_request_id":Uuid::new_v4(),"home_id":"business-home","management_spki_sha256":"33".repeat(32),
            "business_spki_sha256":"44".repeat(32),"not_before_unix_secs":200,"not_after_unix_secs":8000
        }))?;
        let signed_endpoint = SignedHomeEndpointCredential::sign(&endpoint, &home_key)?;
        let grant = HomeServiceGrant {
            version: 1,
            object_type: "flowsplice.home_service_grant".into(),
            deployment_id: "deployment".into(),
            authority_id: "home-authority".into(),
            authority_epoch: 1,
            home_id: "business-home".into(),
            endpoint_payload_sha256: payload_digest(&signed_endpoint.payload_hex)?,
            enrollment_request_sha256: "55".repeat(32),
            services: vec![service()],
            not_before_unix_secs: 300,
            not_after_unix_secs: 7000,
        };
        let descriptor = BusinessDescriptor {
            version: 1,
            approving_home_id: "super-home".into(),
            endpoint: signed_endpoint,
            grant: SignedHomeServiceGrant::sign(&grant, &home_key)?,
            service_id: "pty".into(),
        };
        let travel: TravelCredential = serde_json::from_value(json!({
            "version":1,"object_type":"flowsplice.travel_credential","deployment_id":"deployment",
            "deployment_trust_sha256":signed_trust.payload_digest_sha256()?,"credential_id":Uuid::new_v4(),
            "authority_id":"travel-authority","authority_epoch":1,"enrollment_request_id":Uuid::new_v4(),
            "enrollment_nonce":"66".repeat(32),"enrollment_request_sha256":"77".repeat(32),"travel_id":"travel",
            "management_spki_sha256":"88".repeat(32),"business_spki_sha256":"99".repeat(32),
            "management_ca_sha256":"aa".repeat(32),"business_ca_sha256":"bb".repeat(32),
            "management_certificate_sha256":"cc".repeat(32),"business_certificate_sha256":"dd".repeat(32),
            "scope":{"kind":"service","home_id":"business-home","service_id":"pty","protocol":"tcp"},
            "not_before_unix_secs":400,"not_after_unix_secs":6000
        }))?;
        Ok(Self {
            home_key,
            travel_key,
            trust,
            endpoint,
            descriptor,
            grant,
            travel,
        })
    }
    fn approval(
        &self,
        descriptor: &BusinessDescriptor,
        travel: &SignedTravelCredential,
    ) -> Result<BusinessTravelApproval> {
        let decoded: TravelCredential = serde_json::from_slice(&hex::decode(&travel.payload_hex)?)?;
        Ok(BusinessTravelApproval {
            version: 1,
            object_type: "flowsplice.business_travel_approval".into(),
            deployment_id: self.trust.deployment_id.clone(),
            authority_id: decoded.authority_id,
            authority_epoch: decoded.authority_epoch,
            request_id: decoded.enrollment_request_id,
            request_sha256: "ee".repeat(32),
            descriptor_sha256: json_digest(descriptor)?,
            credential_payload_sha256: payload_digest(&travel.payload_hex)?,
        })
    }
    fn verify(
        &self,
        descriptor: &BusinessDescriptor,
        travel: &TravelCredential,
        now: u64,
    ) -> Result<TravelCredential> {
        let signed = sign_travel(travel, &self.travel_key)?;
        SignedBusinessTravelApproval::sign(&self.approval(descriptor, &signed)?, &self.travel_key)?
            .verify(
                &self.trust,
                None,
                descriptor,
                &"ee".repeat(32),
                &signed,
                now,
            )
    }
}

#[test]
fn valid_descriptor_and_global_issuer_authorize_only_the_other_homes_exact_service() -> Result<()> {
    let f = Fixture::new()?;
    assert_eq!(
        f.descriptor
            .grant
            .verify(&f.trust, &f.descriptor.endpoint, NOW)?,
        f.grant
    );
    let verified = f.descriptor.verify(&f.trust, NOW)?;
    assert_eq!(verified.home_id, "business-home");
    assert_eq!(verified.approving_home_id, "super-home");
    assert_eq!(verified.service, service());
    assert_eq!(f.verify(&f.descriptor, &f.travel, NOW)?, f.travel);
    Ok(())
}

#[test]
fn home_grant_rejects_signed_binding_changes_and_unsigned_tampering() -> Result<()> {
    let f = Fixture::new()?;
    for (field, value) in [
        ("home_id", json!("other-home")),
        ("authority_id", json!("other-authority")),
        ("authority_epoch", json!(2)),
        ("deployment_id", json!("other-deployment")),
        ("endpoint_payload_sha256", json!("ff".repeat(32))),
        ("not_before_unix_secs", json!(199)),
        ("not_after_unix_secs", json!(8001)),
    ] {
        let mut value_json = serde_json::to_value(&f.grant)?;
        value_json[field] = value;
        let changed: HomeServiceGrant = serde_json::from_value(value_json)?;
        let signed = SignedHomeServiceGrant::sign(&changed, &f.home_key)?;
        assert!(
            signed
                .verify_binding(&f.trust, &f.descriptor.endpoint)
                .is_err(),
            "{field}"
        );
    }
    let mut tampered = f.descriptor.grant.clone();
    tampered.signature_hex = "00".repeat(70);
    assert!(
        tampered
            .verify_binding(&f.trust, &f.descriptor.endpoint)
            .is_err()
    );
    let mut changed = f.grant.clone();
    changed.enrollment_request_sha256 = "ff".repeat(32);
    let mut tampered = f.descriptor.grant.clone();
    tampered.payload_hex = hex::encode(serde_json::to_vec(&changed)?);
    assert!(
        tampered
            .verify_binding(&f.trust, &f.descriptor.endpoint)
            .is_err()
    );
    // Core binds this opaque digest by signature; enrollment validates it against the request.
    assert!(
        SignedHomeServiceGrant::sign(&changed, &f.home_key)?
            .verify_binding(&f.trust, &f.descriptor.endpoint)
            .is_ok()
    );
    let mut endpoint = f.endpoint.clone();
    endpoint.credential_id = Uuid::new_v4();
    assert!(
        f.descriptor
            .grant
            .verify_binding(
                &f.trust,
                &SignedHomeEndpointCredential::sign(&endpoint, &f.home_key)?
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn serving_only_grant_rejects_issuer_material_even_with_matching_endpoint_digest() -> Result<()> {
    let f = Fixture::new()?;
    for delegate in [false, true] {
        let mut endpoint = f.endpoint.clone();
        endpoint.issuer_bundle_sha256 = Some("ee".repeat(32));
        if delegate {
            endpoint.delegated_travel_authorities = vec![TrustedTravelAuthority::Home {
                id: "delegated".into(),
                epoch: 1,
                home_id: endpoint.home_id.clone(),
                public_key: hex::encode(f.travel_key.public_key().as_ref()),
            }];
        }
        let endpoint = SignedHomeEndpointCredential::sign(&endpoint, &f.home_key)?;
        endpoint.verify(&f.trust, NOW)?;
        let mut grant = f.grant.clone();
        grant.endpoint_payload_sha256 = payload_digest(&endpoint.payload_hex)?;
        assert!(
            SignedHomeServiceGrant::sign(&grant, &f.home_key)?
                .verify_binding(&f.trust, &endpoint)
                .is_err()
        );
    }
    Ok(())
}

#[test]
fn expired_grants_retain_historical_binding_but_cannot_admit_active_use() -> Result<()> {
    let f = Fixture::new()?;
    for now in [7000, 8000, 10001] {
        assert!(
            f.descriptor
                .grant
                .verify(&f.trust, &f.descriptor.endpoint, now)
                .is_err()
        );
        assert_eq!(
            f.descriptor
                .grant
                .verify_binding(&f.trust, &f.descriptor.endpoint)?,
            f.grant
        );
    }
    let mut descriptor = f.descriptor.clone();
    descriptor.service_id = "absent".into();
    assert!(descriptor.verify(&f.trust, NOW).is_err());
    descriptor = f.descriptor.clone();
    descriptor.approving_home_id = "other".into();
    assert!(descriptor.verify(&f.trust, NOW).is_err());
    Ok(())
}

#[test]
fn service_and_catalog_validation_reject_ambiguity_and_transport_widening() -> Result<()> {
    let f = Fixture::new()?;
    assert!(validate_services(&[]).is_err());
    assert!(validate_services(&vec![service(); 257]).is_err());
    assert!(validate_services(&[service(), service()]).is_err());
    for bad in ["", "has space", "nonascii-é"] {
        let mut s = service();
        s.service_id = bad.into();
        assert!(s.validate().is_err());
        s = service();
        s.application_protocol = bad.into();
        assert!(s.validate().is_err());
        s = service();
        s.capabilities = vec![bad.into()];
        assert!(s.validate().is_err());
    }
    for capabilities in [
        vec![],
        vec!["read".into(), "read".into()],
        vec!["read".into(); 17],
    ] {
        let mut s = service();
        s.capabilities = capabilities;
        assert!(s.validate().is_err());
    }
    let allowed = Service {
        id: "pty".into(),
        alias: "PTY".into(),
        protocol: ServiceProtocol::Tcp,
        target: "in-process".into(),
    };
    f.grant
        .validate_catalog_services(std::slice::from_ref(&allowed))?;
    assert!(
        f.grant
            .validate_catalog_services(&[allowed.clone(), allowed.clone()])
            .is_err()
    );
    let extra = Service {
        id: "extra".into(),
        ..allowed.clone()
    };
    assert!(
        f.grant
            .validate_catalog_services(&[allowed.clone(), extra])
            .is_err()
    );
    assert!(
        f.grant
            .validate_catalog_services(&[Service {
                protocol: ServiceProtocol::Udp,
                ..allowed
            }])
            .is_err()
    );
    Ok(())
}

#[test]
fn approvals_bind_exact_request_descriptor_credential_and_signature() -> Result<()> {
    let f = Fixture::new()?;
    let signed = sign_travel(&f.travel, &f.travel_key)?;
    let payload = f.approval(&f.descriptor, &signed)?;
    for (field, value) in [
        ("request_sha256", json!("ff".repeat(32))),
        ("descriptor_sha256", json!("ff".repeat(32))),
        ("credential_payload_sha256", json!("ff".repeat(32))),
        ("request_id", json!(Uuid::new_v4())),
        ("authority_id", json!("other")),
        ("authority_epoch", json!(2)),
        ("deployment_id", json!("other")),
    ] {
        let mut changed = serde_json::to_value(&payload)?;
        changed[field] = value;
        let changed: BusinessTravelApproval = serde_json::from_value(changed)?;
        let approval = SignedBusinessTravelApproval::sign(&changed, &f.travel_key)?;
        assert!(
            approval
                .verify(
                    &f.trust,
                    None,
                    &f.descriptor,
                    &payload.request_sha256,
                    &signed,
                    NOW
                )
                .is_err(),
            "{field}"
        );
    }
    let approval = SignedBusinessTravelApproval::sign(&payload, &f.travel_key)?;
    assert!(
        approval
            .verify(
                &f.trust,
                None,
                &f.descriptor,
                &"ff".repeat(32),
                &signed,
                NOW
            )
            .is_err()
    );
    let mut changed = f.travel.clone();
    changed.credential_id = Uuid::new_v4();
    assert!(
        approval
            .verify(
                &f.trust,
                None,
                &f.descriptor,
                &payload.request_sha256,
                &sign_travel(&changed, &f.travel_key)?,
                NOW
            )
            .is_err()
    );
    let mut wrong_signature = approval.clone();
    wrong_signature.signature_hex = "00".repeat(70);
    assert!(
        wrong_signature
            .verify(
                &f.trust,
                None,
                &f.descriptor,
                &payload.request_sha256,
                &signed,
                NOW
            )
            .is_err()
    );
    let mut wrong_outer = approval;
    wrong_outer.authority_id = "other".into();
    assert!(
        wrong_outer
            .verify(
                &f.trust,
                None,
                &f.descriptor,
                &payload.request_sha256,
                &signed,
                NOW
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn even_resigned_approvals_cannot_widen_scope_or_validity() -> Result<()> {
    let f = Fixture::new()?;
    let scopes = [
        TravelCredentialScope::Global,
        TravelCredentialScope::Home {
            home_id: "business-home".into(),
        },
        TravelCredentialScope::Service {
            home_id: "other".into(),
            service_id: "pty".into(),
            protocol: ServiceProtocol::Tcp,
        },
        TravelCredentialScope::Service {
            home_id: "business-home".into(),
            service_id: "other".into(),
            protocol: ServiceProtocol::Tcp,
        },
        TravelCredentialScope::Service {
            home_id: "business-home".into(),
            service_id: "pty".into(),
            protocol: ServiceProtocol::Udp,
        },
    ];
    for scope in scopes {
        let mut travel = f.travel.clone();
        travel.scope = scope;
        assert!(f.verify(&f.descriptor, &travel, NOW).is_err());
    }
    for (before, after) in [(400, NOW), (400, 7001), (99, 6000), (NOW + 301, 6000)] {
        let mut travel = f.travel.clone();
        travel.not_before_unix_secs = before;
        travel.not_after_unix_secs = after;
        assert!(f.verify(&f.descriptor, &travel, NOW).is_err());
    }
    let mut wrong = f.descriptor.clone();
    wrong.approving_home_id = "other".into();
    assert!(f.verify(&wrong, &f.travel, NOW).is_err());
    let mut wrong_trust = f.trust.clone();
    if let TrustedTravelAuthority::Global { home_id, .. } = &mut wrong_trust.travel_authorities[0] {
        *home_id = "other".into();
    }
    let signed = sign_travel(&f.travel, &f.travel_key)?;
    let approval =
        SignedBusinessTravelApproval::sign(&f.approval(&f.descriptor, &signed)?, &f.travel_key)?;
    assert!(
        approval
            .verify(
                &wrong_trust,
                None,
                &f.descriptor,
                &"ee".repeat(32),
                &signed,
                NOW
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn legacy_payloads_remain_strict_and_do_not_gain_business_fields() -> Result<()> {
    let f = Fixture::new()?;
    let mut endpoint = serde_json::to_value(&f.endpoint)?;
    assert!(endpoint.get("services").is_none());
    endpoint["services"] = json!([service()]);
    assert!(serde_json::from_value::<HomeEndpointCredential>(endpoint).is_err());
    let mut travel = serde_json::to_value(&f.travel)?;
    assert!(travel.get("descriptor_sha256").is_none());
    travel["descriptor_sha256"] = json!("ff".repeat(32));
    assert!(serde_json::from_value::<TravelCredential>(travel).is_err());
    Ok(())
}
