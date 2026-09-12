use flowsplice_core::{
    authorization::TravelCredentialScope, business::BusinessService, protocol::ServiceProtocol,
};

fn pty(id: &str) -> BusinessService {
    BusinessService {
        service_id: id.into(),
        protocol: ServiceProtocol::Tcp,
        application_protocol: "flowsplice.pty.v1".into(),
        capabilities: vec!["read".into(), "write".into()],
    }
}
#[test]
fn exact_class_matches_distinct_home_and_service_ids_only_with_business_metadata() {
    let scope = TravelCredentialScope::ServiceClass {
        application_protocol: "flowsplice.pty.v1".into(),
        protocol: ServiceProtocol::Tcp,
    };
    for (home, id) in [("mac", "terminal"), ("vps", "console")] {
        assert!(scope.allows_business_service(home, id, ServiceProtocol::Tcp, Some(&pty(id))));
        assert!(!scope.allows_service(home, id, ServiceProtocol::Tcp));
        assert!(!scope.allows_business_service(home, id, ServiceProtocol::Tcp, None));
    }
    let mut service = pty("terminal");
    service.application_protocol = "other.protocol".into();
    assert!(!scope.allows_business_service(
        "mac",
        "terminal",
        ServiceProtocol::Tcp,
        Some(&service)
    ));
    service = pty("terminal");
    service.protocol = ServiceProtocol::Udp;
    assert!(!scope.allows_business_service(
        "mac",
        "terminal",
        ServiceProtocol::Udp,
        Some(&service)
    ));
    assert!(!scope.allows_business_service(
        "mac",
        "other",
        ServiceProtocol::Tcp,
        Some(&pty("terminal"))
    ));
}
#[test]
fn legacy_global_home_and_service_scopes_keep_original_admission() {
    for (scope, home, id, expected) in [
        (TravelCredentialScope::Global, "any", "any", true),
        (
            TravelCredentialScope::Home {
                home_id: "mac".into(),
            },
            "mac",
            "any",
            true,
        ),
        (
            TravelCredentialScope::Home {
                home_id: "mac".into(),
            },
            "vps",
            "any",
            false,
        ),
        (
            TravelCredentialScope::Service {
                home_id: "mac".into(),
                service_id: "pty".into(),
                protocol: ServiceProtocol::Tcp,
            },
            "mac",
            "pty",
            true,
        ),
        (
            TravelCredentialScope::Service {
                home_id: "mac".into(),
                service_id: "pty".into(),
                protocol: ServiceProtocol::Tcp,
            },
            "mac",
            "other",
            false,
        ),
    ] {
        assert_eq!(
            scope.allows_business_service(home, id, ServiceProtocol::Tcp, None),
            expected
        );
        assert_eq!(
            scope.allows_service(home, id, ServiceProtocol::Tcp),
            expected
        );
    }
}

#[test]
fn class_descriptor_requires_global_issuer_not_home_authority() -> anyhow::Result<()> {
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair};
    use flowsplice_core::{business::ServiceClassDescriptor, deployment::DeploymentTrust};
    let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
        .map_err(|_| anyhow::anyhow!("key generation"))?;
    let base = serde_json::json!({"version":1,"deployment_id":"deployment","generation":1,"not_before_unix_secs":100,"not_after_unix_secs":10000,"management_ca_certificate_pem":"fixture","business_ca_certificate_pem":"fixture","server_control_keys":[],"home_endpoints":[],"travel_authorities":[{"kind":"global","id":"issuer-key","epoch":1,"home_id":"issuer","public_key":hex::encode(key.public_key().as_ref())}]});
    let descriptor = ServiceClassDescriptor {
        version: 1,
        approving_home_id: "issuer".into(),
        application_protocol: "flowsplice.pty.v1".into(),
        protocol: ServiceProtocol::Tcp,
    };
    let trust: DeploymentTrust = serde_json::from_value(base.clone())?;
    descriptor.verify(&trust, 1000)?;
    let mut narrow = base;
    narrow["travel_authorities"][0]["kind"] = serde_json::json!("home");
    let trust: DeploymentTrust = serde_json::from_value(narrow)?;
    assert!(descriptor.verify(&trust, 1000).is_err());
    Ok(())
}
