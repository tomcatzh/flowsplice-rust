use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _},
};
use flowsplice_core::{authorization::SignedTravelCredential, deployment::SignedDeploymentTrust};
use rcgen::{Issuer, KeyPair, PublicKeyData};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use x509_parser::parse_x509_certificate;
use zeroize::Zeroizing;

use crate::{
    EnrollmentCredentialBindings, TravelEnrollmentApproval, TravelEnrollmentResponse,
    exact_leaf_params, expected_credential, key, parse_enrollment_request, spki_pin,
    validate_approval,
};

pub struct ProtectedKey<'a> {
    pub path: &'a Path,
    pub password: Option<&'a [u8]>,
    pub allow_unencrypted: bool,
}

pub struct IssuerMaterial<'a> {
    pub deployment_trust: &'a SignedDeploymentTrust,
    pub deployment_root_public_key: &'a str,
    pub management_ca_certificate: &'a Path,
    pub management_ca_key: ProtectedKey<'a>,
    pub business_ca_certificate: &'a Path,
    pub business_ca_key: ProtectedKey<'a>,
    pub travel_authority_key: ProtectedKey<'a>,
}

/// Signs the two requested TLS identities and the exact Travel authorization credential.
///
/// The approval is treated as the policy decision. CSR extensions are verified for proof of
/// possession and identity, but the issued certificate extensions are constructed locally.
///
/// # Errors
///
/// Returns an error for an invalid or stale approval, wrong key passwords, CA/key mismatches,
/// an unexpected authorization key, or signing failures.
pub fn issue_enrollment(
    approval: TravelEnrollmentApproval,
    material: &IssuerMaterial<'_>,
    now: u64,
) -> Result<TravelEnrollmentResponse> {
    validate_approval(&approval, now)?;
    let trust = material
        .deployment_trust
        .verify(material.deployment_root_public_key, now)?;
    if approval.not_before_unix_secs < trust.not_before_unix_secs
        || approval.not_after_unix_secs > trust.not_after_unix_secs
    {
        bail!("enrollment validity is outside the deployment-trusted authority window");
    }
    let authority = trust.travel_authority_by_id(&approval.authority_id)?;
    let parsed = parse_enrollment_request(&approval.request, now)?;

    let management_issuer = load_ca_issuer(
        material.management_ca_certificate,
        &material.management_ca_key,
        "management",
    )?;
    let business_issuer = load_ca_issuer(
        material.business_ca_certificate,
        &material.business_ca_key,
        "business",
    )?;
    let management_ca_certificate_pem = fs::read_to_string(material.management_ca_certificate)
        .context("failed to read management CA certificate for enrollment response")?;
    let business_ca_certificate_pem = fs::read_to_string(material.business_ca_certificate)
        .context("failed to read business CA certificate for enrollment response")?;
    if management_ca_certificate_pem != trust.management_ca_certificate_pem
        || business_ca_certificate_pem != trust.business_ca_certificate_pem
    {
        bail!("issuer CA certificates do not match deployment trust");
    }

    let management_params = exact_leaf_params(
        &approval.request.travel_id,
        approval.not_before_unix_secs,
        approval.not_after_unix_secs,
    )?;
    let management_certificate = management_params
        .signed_by(&parsed.management.public_key, &management_issuer)
        .context("failed to sign Travel management certificate")?;
    let business_params = exact_leaf_params(
        &approval.request.travel_id,
        approval.not_before_unix_secs,
        approval.not_after_unix_secs,
    )?;
    let business_certificate = business_params
        .signed_by(&parsed.business.public_key, &business_issuer)
        .context("failed to sign Travel business certificate")?;

    let management_certificate_pem = management_certificate.pem();
    let business_certificate_pem = business_certificate.pem();
    let credential = expected_credential(
        &approval,
        &EnrollmentCredentialBindings {
            signed_trust: material.deployment_trust,
            trust: &trust,
            authority,
            management_certificate_pem: &management_certificate_pem,
            business_certificate_pem: &business_certificate_pem,
        },
        spki_pin(&parsed.management.public_key),
        spki_pin(&parsed.business.public_key),
    )?;
    let authority_private_key = Zeroizing::new(
        key::load_private_key(
            material.travel_authority_key.path,
            material.travel_authority_key.password,
            material.travel_authority_key.allow_unencrypted,
        )
        .context("failed to load Travel authorization private key")?,
    );
    let authority_key = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        authority_private_key.secret_der(),
    )
    .map_err(|_| anyhow!("Travel authorization key is not a P-256 PKCS#8 private key"))?;
    let actual_public_key = hex::encode(authority_key.public_key().as_ref());
    if !actual_public_key.eq_ignore_ascii_case(authority.public_key()) {
        bail!("Travel authorization private key does not match the expected public key");
    }
    let payload = serde_json::to_vec(&credential)?;
    let signature = authority_key
        .sign(&SystemRandom::new(), &payload)
        .map_err(|_| anyhow!("failed to sign Travel authorization credential"))?;
    let signed_credential = SignedTravelCredential {
        authority_id: credential.authority_id.clone(),
        payload_hex: hex::encode(payload),
        signature_hex: hex::encode(signature.as_ref()),
    };

    Ok(TravelEnrollmentResponse {
        version: crate::ENROLLMENT_VERSION,
        approval,
        deployment_trust: material.deployment_trust.clone(),
        management_certificate_pem,
        business_certificate_pem,
        signed_credential,
    })
}

fn load_ca_issuer(
    certificate_path: &Path,
    protected_key: &ProtectedKey<'_>,
    label: &str,
) -> Result<Issuer<'static, KeyPair>> {
    let certificate_pem = fs::read_to_string(certificate_path)
        .with_context(|| format!("failed to read {label} CA certificate"))?;
    let certificate = first_certificate(certificate_path, label)?;
    let (_, parsed) = parse_x509_certificate(certificate.as_ref())
        .map_err(|error| anyhow!("failed to parse {label} CA certificate: {error}"))?;
    let private_key = Zeroizing::new(
        key::load_private_key(
            protected_key.path,
            protected_key.password,
            protected_key.allow_unencrypted,
        )
        .with_context(|| format!("failed to load {label} CA private key"))?,
    );
    let key_pair = KeyPair::try_from(&*private_key)
        .with_context(|| format!("failed to parse {label} CA private key"))?;
    if parsed.public_key().raw != key_pair.subject_public_key_info() {
        bail!("{label} CA certificate does not match its private key");
    }
    Issuer::from_ca_cert_pem(&certificate_pem, key_pair)
        .with_context(|| format!("failed to load {label} CA issuer"))
}

fn first_certificate(path: &Path, label: &str) -> Result<CertificateDer<'static>> {
    CertificateDer::pem_file_iter(path)
        .with_context(|| format!("failed to open {label} CA certificate"))?
        .next()
        .ok_or_else(|| anyhow!("{label} CA file contains no certificate"))?
        .with_context(|| format!("failed to decode {label} CA certificate"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, path::PathBuf};

    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyUsagePurpose,
        PKCS_ECDSA_P256_SHA256,
    };

    use crate::{
        BUSINESS_CA_FILE, BUSINESS_CERT_FILE, DEFAULT_VALID_DAYS, MANAGEMENT_CA_FILE,
        MANAGEMENT_CERT_FILE, create_enrollment_request, install_enrollment_response,
        prepare_enrollment_approval, validate_enrollment_response,
    };
    use flowsplice_core::{
        authorization::{TravelCredentialScope, TrustedTravelAuthority},
        deployment::{
            DEPLOYMENT_TRUST_VERSION, DeploymentTrust, HomeEndpointTrust, ServerControlKey,
            SignedDeploymentTrust,
        },
    };

    struct TestCa {
        certificate: PathBuf,
        key: PathBuf,
    }

    fn create_test_ca(directory: &Path, name: &str) -> Result<TestCa> {
        let key_pair = rcgen::KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let mut params = CertificateParams::default();
        let mut distinguished_name = DistinguishedName::new();
        distinguished_name.push(DnType::CommonName, format!("FlowSplice {name} Test CA"));
        params.distinguished_name = distinguished_name;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)?;
        params.not_after = time::OffsetDateTime::from_unix_timestamp(2_200_000_000)?;
        let certificate = params.self_signed(&key_pair)?;
        let certificate_path = directory.join(format!("{name}-ca.crt"));
        let key_path = directory.join(format!("{name}-ca.key"));
        fs::write(&certificate_path, certificate.pem())?;
        fs::write(&key_path, key_pair.serialize_pem())?;
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))?;
        Ok(TestCa {
            certificate: certificate_path,
            key: key_path,
        })
    }

    fn assert_installed_ca_bundle(
        enrollment_directory: &Path,
        management_ca_pem: &str,
        business_ca_pem: &str,
    ) -> Result<()> {
        assert_eq!(
            fs::read_to_string(enrollment_directory.join(MANAGEMENT_CA_FILE))?,
            management_ca_pem
        );
        assert_eq!(
            fs::read_to_string(enrollment_directory.join(BUSINESS_CA_FILE))?,
            business_ca_pem
        );
        Ok(())
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn issue_validate_and_install_are_bound_to_the_original_keys() -> Result<()> {
        flowsplice_core::init_crypto();
        let temporary = tempfile::tempdir()?;
        let management_ca = create_test_ca(temporary.path(), "management")?;
        let business_ca = create_test_ca(temporary.path(), "business")?;
        let authority = rcgen::KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let authority_key_path = temporary.path().join("authority.key");
        fs::write(&authority_key_path, authority.serialize_pem())?;
        fs::set_permissions(&authority_key_path, fs::Permissions::from_mode(0o600))?;
        let authority_key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &authority.serialize_der())
                .map_err(|_| anyhow!("failed to load authority fixture"))?;
        let authority_public_key = hex::encode(authority_key.public_key().as_ref());
        let root_key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate deployment root fixture"))?;
        let server_key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate Server control fixture"))?;
        let root_public_key = hex::encode(root_key.public_key().as_ref());
        let enrollment_directory = temporary.path().join("travel");
        let now = 1_800_000_000;
        let management_ca_pem = fs::read_to_string(&management_ca.certificate)?;
        let business_ca_pem = fs::read_to_string(&business_ca.certificate)?;
        let deployment_trust = SignedDeploymentTrust::sign(
            &DeploymentTrust {
                version: DEPLOYMENT_TRUST_VERSION,
                deployment_id: "test-deployment".to_owned(),
                generation: 1,
                not_before_unix_secs: now - 300,
                not_after_unix_secs: now + u64::from(DEFAULT_VALID_DAYS) * 24 * 60 * 60 + 600,
                management_ca_certificate_pem: management_ca_pem.clone(),
                business_ca_certificate_pem: business_ca_pem.clone(),
                server_control_keys: vec![ServerControlKey {
                    server_id: "server-1".to_owned(),
                    epoch: 1,
                    public_key: hex::encode(server_key.public_key().as_ref()),
                }],
                home_endpoints: vec![HomeEndpointTrust {
                    home_id: "home-1".to_owned(),
                    management_spki_pins: vec!["11".repeat(32)],
                    business_spki_pins: vec!["22".repeat(32)],
                }],
                travel_authorities: vec![TrustedTravelAuthority::Home {
                    id: "home-1-authority".to_owned(),
                    epoch: 1,
                    home_id: "home-1".to_owned(),
                    public_key: authority_public_key,
                }],
            },
            &root_key,
        )?;
        let request = create_enrollment_request(
            "travel-test",
            b"correct horse battery staple",
            &enrollment_directory,
            now,
        )?;
        let approval = prepare_enrollment_approval(
            request,
            u64::from(DEFAULT_VALID_DAYS) * 24 * 60 * 60,
            "home-1-authority".to_owned(),
            TravelCredentialScope::Home {
                home_id: "home-1".to_owned(),
            },
            now + 1,
        )?;
        let material = IssuerMaterial {
            deployment_trust: &deployment_trust,
            deployment_root_public_key: &root_public_key,
            management_ca_certificate: &management_ca.certificate,
            management_ca_key: ProtectedKey {
                path: &management_ca.key,
                password: None,
                allow_unencrypted: true,
            },
            business_ca_certificate: &business_ca.certificate,
            business_ca_key: ProtectedKey {
                path: &business_ca.key,
                password: None,
                allow_unencrypted: true,
            },
            travel_authority_key: ProtectedKey {
                path: &authority_key_path,
                password: None,
                allow_unencrypted: true,
            },
        };
        let response = issue_enrollment(approval, &material, now + 2)?;
        let (expected, _) = validate_enrollment_response(&response, &root_public_key, now + 2)?;
        let installed = install_enrollment_response(
            &enrollment_directory,
            &response,
            &root_public_key,
            b"correct horse battery staple",
            now + 2,
        )?;
        assert_eq!(installed, expected);
        assert_installed_ca_bundle(&enrollment_directory, &management_ca_pem, &business_ca_pem)?;
        assert!(enrollment_directory.join(MANAGEMENT_CERT_FILE).is_file());
        assert!(enrollment_directory.join(BUSINESS_CERT_FILE).is_file());
        assert_eq!(
            install_enrollment_response(
                &enrollment_directory,
                &response,
                &root_public_key,
                b"correct horse battery staple",
                now + 2,
            )?,
            expected
        );
        assert!(
            install_enrollment_response(
                &enrollment_directory,
                &response,
                &root_public_key,
                b"wrong password",
                now + 2,
            )
            .is_err()
        );
        let mut wrong_ca = response.clone();
        wrong_ca
            .deployment_trust
            .payload_hex
            .replace_range(0..2, "00");
        assert!(
            install_enrollment_response(
                &enrollment_directory,
                &wrong_ca,
                &root_public_key,
                b"correct horse battery staple",
                now + 2,
            )
            .is_err()
        );
        let mut spliced_certificate = response.clone();
        spliced_certificate.management_certificate_pem =
            spliced_certificate.business_certificate_pem.clone();
        assert!(
            validate_enrollment_response(&spliced_certificate, &root_public_key, now + 2).is_err()
        );
        let mut appended_certificate = response.clone();
        appended_certificate
            .management_certificate_pem
            .push_str(&appended_certificate.business_certificate_pem);
        assert!(
            validate_enrollment_response(&appended_certificate, &root_public_key, now + 2).is_err()
        );
        let mut future = response.clone();
        future.approval.not_before_unix_secs = now + 10_000;
        assert!(validate_enrollment_response(&future, &root_public_key, now + 2).is_err());
        let mut spliced_request = response.clone();
        let replacement = if spliced_request.approval.request.nonce.starts_with("00") {
            "01"
        } else {
            "00"
        };
        spliced_request
            .approval
            .request
            .nonce
            .replace_range(0..2, replacement);
        assert!(validate_enrollment_response(&spliced_request, &root_public_key, now + 2).is_err());
        let mut tampered = response;
        tampered
            .signed_credential
            .signature_hex
            .replace_range(0..2, "00");
        assert!(validate_enrollment_response(&tampered, &root_public_key, now + 2).is_err());
        Ok(())
    }

    #[test]
    fn private_key_errors_identify_the_issuer_role() -> Result<()> {
        flowsplice_core::init_crypto();
        let temporary = tempfile::tempdir()?;
        let management_ca = create_test_ca(temporary.path(), "management")?;
        let encrypted = key::generate_encrypted_private_key(b"correct issuer password")?;
        let encrypted_path = temporary.path().join("encrypted-management-ca.key");
        fs::write(&encrypted_path, encrypted.encrypted_pem.as_bytes())?;
        fs::set_permissions(&encrypted_path, fs::Permissions::from_mode(0o600))?;
        let protected = ProtectedKey {
            path: &encrypted_path,
            password: Some(b"wrong issuer password"),
            allow_unencrypted: false,
        };
        let Err(error) = load_ca_issuer(&management_ca.certificate, &protected, "management")
        else {
            bail!("wrong issuer password unexpectedly succeeded");
        };
        assert_eq!(
            error.to_string(),
            "failed to load management CA private key"
        );
        Ok(())
    }
}
