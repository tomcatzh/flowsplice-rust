use anyhow::{Result, anyhow};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
use flowsplice_core::{
    authorization::TravelCredentialScope,
    business::ServiceClassDescriptor,
    deployment::{DeploymentTrust, SignedDeploymentTrust},
    protocol::ServiceProtocol,
};
use flowsplice_enrollment::{
    business::*,
    create_enrollment_request,
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
            root,
            trust,
        })
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
}

#[test]
fn class_enrollment_proof_binds_label_descriptor_and_exact_scope() -> Result<()> {
    let f = Fixture::new()?;
    let request = ServiceClassTravelRequest {
        version: 1,
        object_type: SERVICE_CLASS_TRAVEL_REQUEST_TYPE.into(),
        request: create_enrollment_request(
            "class-travel",
            PASSWORD,
            &f.dir.path().join("travel"),
            NOW,
        )?,
        descriptor: ServiceClassDescriptor {
            version: 1,
            approving_home_id: "super-home".into(),
            application_protocol: "flowsplice.pty.v1".into(),
            protocol: ServiceProtocol::Tcp,
        },
        label: "我的 iPad".into(),
    };
    let approval = prepare_enrollment_approval(
        request.request.clone(),
        86400,
        "travel-authority".into(),
        request.descriptor.scope(),
        NOW,
    )?;
    let response =
        issue_service_class_travel(request.clone(), approval.clone(), &f.travel_material(), NOW)?;
    let (credential, _) = response.validate(&f.root, NOW)?;
    assert_eq!(credential.scope, request.descriptor.scope());
    let envelope = TravelResponseEnvelope::ServiceClass(Box::new(response.clone()));
    assert_eq!(
        serde_json::from_slice::<TravelResponseEnvelope>(&serde_json::to_vec(&envelope)?)?,
        envelope
    );
    let mut changed = response.clone();
    changed.request.label = "不同设备".into();
    assert!(changed.validate(&f.root, NOW).is_err());
    let mut changed = response.clone();
    changed.request.descriptor.application_protocol = "other.v1".into();
    assert!(changed.validate(&f.root, NOW).is_err());
    let mut changed = response.clone();
    changed.request.descriptor.approving_home_id = "wrong-home".into();
    assert!(changed.validate(&f.root, NOW).is_err());
    let mut widened = approval.clone();
    widened.scope = TravelCredentialScope::Global;
    assert!(
        issue_service_class_travel(request.clone(), widened, &f.travel_material(), NOW).is_err()
    );
    let mut wrong_signer = approval;
    wrong_signer.authority_id = "wrong-authority".into();
    assert!(issue_service_class_travel(request, wrong_signer, &f.travel_material(), NOW).is_err());
    assert!(response.validate(&f.root, NOW + 100_001).is_err());
    Ok(())
}

#[test]
fn class_labels_reject_controls_bidi_and_invalid_lengths() -> Result<()> {
    for label in ["iPad", "设备", &"é".repeat(32)] {
        validate_client_label(label)?;
    }
    for label in ["", "  ", "a\n", "a\u{202e}", "a\u{2067}", &"é".repeat(33)] {
        assert!(validate_client_label(label).is_err());
    }
    Ok(())
}
