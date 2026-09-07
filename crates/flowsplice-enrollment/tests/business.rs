use anyhow::{Result, anyhow};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use flowsplice_core::{
    authorization::TravelCredentialScope,
    business::{BusinessDescriptor, BusinessService, HomeServiceGrant, SignedHomeServiceGrant},
    deployment::{DeploymentTrust, SignedDeploymentTrust},
    protocol::ServiceProtocol,
};
use flowsplice_enrollment::{
    MAX_REQUEST_AGE_SECS,
    business::*,
    create_enrollment_request,
    home::{
        HomeEnrollmentProfile, HomeIssuerMaterial, create_home_enrollment_request,
        prepare_home_enrollment_approval,
    },
    issuer::{IssuerMaterial, ProtectedKey},
    key::generate_encrypted_private_key,
    prepare_enrollment_approval,
};
use serde_json::json;
use std::{fs, path::PathBuf};

const NOW: u64 = 1_800_000_000;
const PASSWORD: &[u8] = b"business-fixture-password";
struct Fixture {
    dir: tempfile::TempDir,
    ca: PathBuf,
    ca_key: PathBuf,
    authority: PathBuf,
    key: EcdsaKeyPair,
    root: String,
    trust: SignedDeploymentTrust,
}
impl Fixture {
    fn new() -> Result<Self> {
        flowsplice_core::init_crypto();
        let dir = tempfile::tempdir()?;
        let ca_key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        let mut params = rcgen::CertificateParams::default();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
        ];
        let certificate = params.self_signed(&ca_key_pair)?;
        let ca = dir.path().join("ca.crt");
        let ca_key = dir.path().join("ca.key");
        fs::write(&ca, certificate.pem())?;
        fs::write(&ca_key, ca_key_pair.serialize_pem())?;
        let generated = generate_encrypted_private_key(PASSWORD)?;
        let authority = dir.path().join("authority.key");
        fs::write(&authority, generated.encrypted_pem.as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&ca_key, &authority] {
                fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            }
        }
        let key = EcdsaKeyPair::from_pkcs8(
            &ECDSA_P256_SHA256_ASN1_SIGNING,
            &generated.key_pair.serialize_der(),
        )
        .map_err(|_| anyhow!("test key decode failed"))?;
        let root = hex::encode(key.public_key().as_ref());
        let trust: DeploymentTrust = serde_json::from_value(json!({
            "version":1,"deployment_id":"business-fixture","generation":1,"not_before_unix_secs":NOW-600,"not_after_unix_secs":NOW+100_000,
            "management_ca_certificate_pem":certificate.pem(),"business_ca_certificate_pem":certificate.pem(),
            "server_control_keys":[{"server_id":"server","epoch":1,"public_key":root}],
            "home_endpoints":[{"home_id":"super-home","management_spki_pins":["11".repeat(32)],"business_spki_pins":["22".repeat(32)]},{"home_id":"wrong-home","management_spki_pins":["33".repeat(32)],"business_spki_pins":["44".repeat(32)]}],
            "home_enrollment_authorities":[{"id":"home-authority","epoch":1,"issuer_home_id":"super-home","public_key":root}],
            "travel_authorities":[{"kind":"global","id":"travel-authority","epoch":1,"home_id":"super-home","public_key":root},{"kind":"global","id":"wrong-authority","epoch":1,"home_id":"wrong-home","public_key":root}]
        }))?;
        let trust = SignedDeploymentTrust::sign(&trust, &key)?;
        Ok(Self {
            dir,
            ca,
            ca_key,
            authority,
            key,
            root,
            trust,
        })
    }
    fn home_material(&self, password: &'static [u8]) -> HomeIssuerMaterial<'_> {
        HomeIssuerMaterial {
            deployment_trust: &self.trust,
            deployment_root_public_key: &self.root,
            management_ca_certificate: &self.ca,
            management_ca_key: ProtectedKey {
                path: &self.ca_key,
                password: None,
                allow_unencrypted: true,
            },
            business_ca_certificate: &self.ca,
            business_ca_key: ProtectedKey {
                path: &self.ca_key,
                password: None,
                allow_unencrypted: true,
            },
            home_enrollment_authority_key: ProtectedKey {
                path: &self.authority,
                password: Some(password),
                allow_unencrypted: false,
            },
        }
    }
    fn travel_material(&self) -> IssuerMaterial<'_> {
        IssuerMaterial {
            deployment_trust: &self.trust,
            deployment_root_public_key: &self.root,
            home_endpoint_credential: None,
            management_ca_certificate: &self.ca,
            management_ca_key: ProtectedKey {
                path: &self.ca_key,
                password: None,
                allow_unencrypted: true,
            },
            business_ca_certificate: &self.ca,
            business_ca_key: ProtectedKey {
                path: &self.ca_key,
                password: None,
                allow_unencrypted: true,
            },
            travel_authority_key: ProtectedKey {
                path: &self.authority,
                password: Some(PASSWORD),
                allow_unencrypted: false,
            },
        }
    }
    fn request(&self) -> Result<BusinessHomeRequest> {
        Ok(BusinessHomeRequest {
            version: 1,
            object_type: BUSINESS_HOME_REQUEST_TYPE.into(),
            request: create_home_enrollment_request(
                "business-home",
                &self.dir.path().join("home"),
                NOW,
            )?,
            services: vec![service("pty"), service("second")],
        })
    }
    fn issue(
        &self,
        request: BusinessHomeRequest,
        services: Vec<BusinessService>,
    ) -> Result<BusinessHomeResponse> {
        let approval = prepare_home_enrollment_approval(
            request.request.clone(),
            3600,
            "home-authority".into(),
            HomeEnrollmentProfile::ServingOnly,
            NOW,
        )?;
        issue_business_home(
            request,
            approval,
            services,
            &self.home_material(PASSWORD),
            NOW,
        )
    }
}
fn service(id: &str) -> BusinessService {
    BusinessService {
        service_id: id.into(),
        protocol: ServiceProtocol::Tcp,
        application_protocol: "pty/1".into(),
        capabilities: vec!["read".into(), "write".into()],
    }
}

#[test]
fn real_home_issuance_binds_csr_serving_only_and_exact_service_subsets() -> Result<()> {
    let f = Fixture::new()?;
    let request = f.request()?;
    let single = f.issue(request.clone(), vec![service("pty")])?;
    let trust = single.validate(&f.root, NOW)?;
    let endpoint = single
        .response
        .signed_endpoint_credential
        .verify(&trust, NOW)?;
    assert!(single.response.issuer_bundle.is_none());
    assert!(endpoint.delegated_travel_authorities.is_empty());
    assert!(endpoint.issuer_bundle_sha256.is_none());
    assert_eq!(
        single
            .grant
            .verify(&trust, &single.response.signed_endpoint_credential, NOW)?
            .services,
        vec![service("pty")]
    );
    let multiple = f.issue(request.clone(), request.services.clone())?;
    assert_eq!(
        multiple
            .grant
            .verify(&trust, &multiple.response.signed_endpoint_credential, NOW)?
            .services,
        request.services
    );
    assert!(
        f.issue(request.clone(), vec![service("unrequested")])
            .is_err()
    );
    let mut altered = service("pty");
    altered.capabilities = vec!["read".into()];
    assert!(f.issue(request.clone(), vec![altered]).is_err());
    let mut changed = single.clone();
    changed.request.request.management_csr_pem = "invalid CSR".into();
    assert!(changed.validate(&f.root, NOW).is_err());
    let mut changed = single.clone();
    changed.request.request.nonce = "ff".repeat(32);
    assert!(changed.validate(&f.root, NOW).is_err());
    let mut grant: HomeServiceGrant =
        serde_json::from_slice(&hex::decode(&single.grant.payload_hex)?)?;
    grant.services.push(service("extra"));
    let mut changed = single.clone();
    changed.grant = SignedHomeServiceGrant::sign(&grant, &f.key)?;
    assert!(changed.validate(&f.root, NOW).is_err());
    let approval = prepare_home_enrollment_approval(
        request.request.clone(),
        3600,
        "home-authority".into(),
        HomeEnrollmentProfile::ServingOnly,
        NOW,
    )?;
    assert!(
        issue_business_home(
            request.clone(),
            approval,
            request.services.clone(),
            &f.home_material(b"wrong-password"),
            NOW
        )
        .is_err()
    );
    assert_eq!(
        serde_json::to_value(HomeResponseEnvelope::from(single.response.clone()))?,
        serde_json::to_value(&single.response)?
    );
    strict_home_wrappers(&single)?;
    Ok(())
}

fn strict_home_wrappers(response: &BusinessHomeResponse) -> Result<()> {
    let mut extra = serde_json::to_value(response)?;
    extra["extra_grant"] = json!({});
    assert!(serde_json::from_value::<HomeResponseEnvelope>(extra).is_err());
    for (field, value) in [("version", json!(2)), ("object_type", json!("obsolete"))] {
        let mut changed = serde_json::to_value(response)?;
        changed[field] = value.clone();
        let changed: BusinessHomeResponse = serde_json::from_value(changed)?;
        assert!(changed.validate("unused-root", NOW).is_err());
        let mut request = serde_json::to_value(&response.request)?;
        request[field] = value;
        assert!(
            serde_json::from_value::<BusinessHomeRequest>(request)?
                .validate(NOW)
                .is_err()
        );
    }
    assert!(
        response
            .request
            .validate(NOW + MAX_REQUEST_AGE_SECS + 1)
            .is_err()
    );
    Ok(())
}

#[test]
fn real_travel_issuance_proves_cross_home_intent_and_caps_expiry() -> Result<()> {
    let f = Fixture::new()?;
    let home = f.issue(f.request()?, vec![service("pty")])?;
    let descriptor = BusinessDescriptor {
        version: 1,
        approving_home_id: "super-home".into(),
        endpoint: home.response.signed_endpoint_credential.clone(),
        grant: home.grant.clone(),
        service_id: "pty".into(),
    };
    let request = BusinessTravelRequest {
        version: 1,
        object_type: BUSINESS_TRAVEL_REQUEST_TYPE.into(),
        request: create_enrollment_request(
            "business-travel",
            PASSWORD,
            &f.dir.path().join("travel"),
            NOW,
        )?,
        descriptor,
    };
    let scope = TravelCredentialScope::Service {
        home_id: "business-home".into(),
        service_id: "pty".into(),
        protocol: ServiceProtocol::Tcp,
    };
    let approval = prepare_enrollment_approval(
        request.request.clone(),
        86400,
        "travel-authority".into(),
        scope.clone(),
        NOW,
    )?;
    let response =
        issue_business_travel(request.clone(), approval.clone(), &f.travel_material(), NOW)?;
    let (credential, trust) = response.validate(&f.root, NOW)?;
    assert_eq!(credential.scope, scope);
    assert_eq!(credential.not_after_unix_secs, NOW + 3600);
    response.approval.verify(
        &trust,
        None,
        &request.descriptor,
        &flowsplice_core::business::json_digest(&request)?,
        &response.response.signed_credential,
        NOW,
    )?;
    assert!(request.validate(&trust, "wrong-home", NOW).is_err());
    let mut changed = response.clone();
    changed.request.descriptor.service_id = "absent".into();
    assert!(changed.validate(&f.root, NOW).is_err());
    let mut changed = response.clone();
    changed.request.request.request_id = uuid::Uuid::new_v4();
    assert!(changed.validate(&f.root, NOW).is_err());
    let mut changed = response.clone();
    changed.approval.signature_hex = "00".repeat(70);
    assert!(changed.validate(&f.root, NOW).is_err());
    let mut widened = approval.clone();
    widened.scope = TravelCredentialScope::Global;
    assert!(issue_business_travel(request.clone(), widened, &f.travel_material(), NOW).is_err());
    let mut wrong_issuer = approval;
    wrong_issuer.authority_id = "wrong-authority".into();
    assert!(
        issue_business_travel(request.clone(), wrong_issuer, &f.travel_material(), NOW).is_err()
    );
    assert_eq!(
        serde_json::to_value(TravelResponseEnvelope::from(response.response.clone()))?,
        serde_json::to_value(&response.response)?
    );
    strict_travel_wrappers(&response, &trust)?;
    Ok(())
}
fn strict_travel_wrappers(
    response: &BusinessTravelResponse,
    trust: &DeploymentTrust,
) -> Result<()> {
    let mut extra = serde_json::to_value(response)?;
    extra["extra_proof"] = json!({});
    assert!(serde_json::from_value::<TravelResponseEnvelope>(extra).is_err());
    for (field, value) in [("version", json!(2)), ("object_type", json!("obsolete"))] {
        let mut changed = serde_json::to_value(response)?;
        changed[field] = value.clone();
        assert!(
            serde_json::from_value::<BusinessTravelResponse>(changed)?
                .validate("unused-root", NOW)
                .is_err()
        );
        let mut request = serde_json::to_value(&response.request)?;
        request[field] = value;
        assert!(
            serde_json::from_value::<BusinessTravelRequest>(request)?
                .validate(trust, "super-home", NOW)
                .is_err()
        );
    }
    assert!(
        response
            .request
            .validate(trust, "super-home", NOW + MAX_REQUEST_AGE_SECS + 1)
            .is_err()
    );
    Ok(())
}

#[test]
fn synthetic_request_wrappers_reject_unknown_fields_and_bad_kind_before_csr_validation()
-> Result<()> {
    // Deliberately unsigned placeholders: this test asserts schema/version handling only.
    let request = json!({
        "version": 1, "object_type": BUSINESS_HOME_REQUEST_TYPE,
        "request": {"version": 1,"request_id": uuid::Uuid::new_v4(),"nonce":"11".repeat(32),
            "home_id":"home","created_at_unix_secs":NOW,"management_csr_pem":"placeholder","business_csr_pem":"placeholder"},
        "services": [service("pty")]
    });
    let mut extra = request.clone();
    extra["unexpected"] = json!(true);
    assert!(serde_json::from_value::<HomeRequestEnvelope>(extra).is_err());
    for (field, value) in [
        ("version", json!(0)),
        ("object_type", json!("legacy-business-kind")),
    ] {
        let mut changed = request.clone();
        changed[field] = value;
        let changed: BusinessHomeRequest = serde_json::from_value(changed)?;
        let error = changed
            .validate(NOW)
            .err()
            .ok_or_else(|| anyhow!("invalid wrapper accepted"))?;
        assert!(!error.to_string().contains("CSR"));
    }
    for malformed in [
        json!(null),
        json!({"version":1,"object_type":BUSINESS_TRAVEL_RESPONSE_TYPE,"unexpected":true}),
    ] {
        assert!(serde_json::from_value::<TravelResponseEnvelope>(malformed.clone()).is_err());
        assert!(serde_json::from_value::<HomeResponseEnvelope>(malformed).is_err());
    }
    Ok(())
}
