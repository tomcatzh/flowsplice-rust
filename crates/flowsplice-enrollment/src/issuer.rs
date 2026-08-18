use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _},
};
use flowsplice_core::authorization::{SignedTravelCredential, validate_authority_public_key};
use rcgen::{Issuer, KeyPair, PublicKeyData};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use x509_parser::parse_x509_certificate;
use zeroize::Zeroizing;

use crate::{
    TravelEnrollmentApproval, TravelEnrollmentResponse, exact_leaf_params, expected_credential,
    key, parse_enrollment_request, spki_pin, validate_approval,
};

pub struct ProtectedKey<'a> {
    pub path: &'a Path,
    pub password: Option<&'a [u8]>,
    pub allow_unencrypted: bool,
}

pub struct IssuerMaterial<'a> {
    pub management_ca_certificate: &'a Path,
    pub management_ca_key: ProtectedKey<'a>,
    pub business_ca_certificate: &'a Path,
    pub business_ca_key: ProtectedKey<'a>,
    pub travel_authority_key: ProtectedKey<'a>,
    pub expected_travel_authority_public_key: &'a str,
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
    validate_authority_public_key(material.expected_travel_authority_public_key)?;
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

    let credential = expected_credential(
        &approval,
        spki_pin(&parsed.management.public_key),
        spki_pin(&parsed.business.public_key),
    );
    let authority_private_key = Zeroizing::new(key::load_private_key(
        material.travel_authority_key.path,
        material.travel_authority_key.password,
        material.travel_authority_key.allow_unencrypted,
    )?);
    let authority_key = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        authority_private_key.secret_der(),
    )
    .map_err(|_| anyhow!("Travel authorization key is not a P-256 PKCS#8 private key"))?;
    let actual_public_key = hex::encode(authority_key.public_key().as_ref());
    if !actual_public_key.eq_ignore_ascii_case(material.expected_travel_authority_public_key) {
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
        authority_public_key: actual_public_key,
        management_certificate_pem: management_certificate.pem(),
        business_certificate_pem: business_certificate.pem(),
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
    let private_key = Zeroizing::new(key::load_private_key(
        protected_key.path,
        protected_key.password,
        protected_key.allow_unencrypted,
    )?);
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
    use std::path::PathBuf;

    use rcgen::{
        BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyUsagePurpose,
        PKCS_ECDSA_P256_SHA256,
    };

    use crate::{
        BUSINESS_CERT_FILE, DEFAULT_VALID_DAYS, MANAGEMENT_CERT_FILE, create_enrollment_request,
        install_enrollment_response, prepare_enrollment_approval, validate_enrollment_response,
    };
    use flowsplice_core::authorization::TravelCredentialScope;

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
        Ok(TestCa {
            certificate: certificate_path,
            key: key_path,
        })
    }

    #[test]
    fn issue_validate_and_install_are_bound_to_the_original_keys() -> Result<()> {
        flowsplice_core::init_crypto();
        let temporary = tempfile::tempdir()?;
        let management_ca = create_test_ca(temporary.path(), "management")?;
        let business_ca = create_test_ca(temporary.path(), "business")?;
        let authority = rcgen::KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256)?;
        let authority_key_path = temporary.path().join("authority.key");
        fs::write(&authority_key_path, authority.serialize_pem())?;
        let authority_key =
            EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &authority.serialize_der())
                .map_err(|_| anyhow!("failed to load authority fixture"))?;
        let authority_public_key = hex::encode(authority_key.public_key().as_ref());
        let enrollment_directory = temporary.path().join("travel");
        let now = 1_800_000_000;
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
            expected_travel_authority_public_key: &authority_public_key,
        };
        let response = issue_enrollment(approval, &material, now + 2)?;
        let expected = validate_enrollment_response(&response, now + 2)?;
        let installed = install_enrollment_response(
            &enrollment_directory,
            &response,
            &management_ca.certificate,
            &business_ca.certificate,
            b"correct horse battery staple",
            now + 2,
        )?;
        assert_eq!(installed, expected);
        assert!(enrollment_directory.join(MANAGEMENT_CERT_FILE).is_file());
        assert!(enrollment_directory.join(BUSINESS_CERT_FILE).is_file());
        assert_eq!(
            install_enrollment_response(
                &enrollment_directory,
                &response,
                &management_ca.certificate,
                &business_ca.certificate,
                b"correct horse battery staple",
                now + 2,
            )?,
            expected
        );
        assert!(
            install_enrollment_response(
                &enrollment_directory,
                &response,
                &management_ca.certificate,
                &business_ca.certificate,
                b"wrong password",
                now + 2,
            )
            .is_err()
        );
        let mut tampered = response;
        tampered
            .signed_credential
            .signature_hex
            .replace_range(0..2, "00");
        assert!(validate_enrollment_response(&tampered, now + 2).is_err());
        Ok(())
    }
}
