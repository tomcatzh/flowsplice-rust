#![forbid(unsafe_code)]

use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::digest;
use flowsplice_core::authorization::{
    SignedTravelCredential, TravelCredential, validate_authority_public_key,
};
use flowsplice_core::{
    protocol::Role,
    tls::{peer_identity, require_peer},
};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyUsagePurpose, PublicKeyData, SanType, string::Ia5String,
};
use rustls::{RootCertStore, server::WebPkiClientVerifier};
use rustls_pki_types::{CertificateDer, UnixTime, pem::PemObject};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;
use zeroize::Zeroizing;

pub mod issuer;
pub mod key;

pub const ENROLLMENT_VERSION: u32 = 1;
pub const DEFAULT_VALID_DAYS: u32 = 365;
pub const MAX_VALID_DAYS: u32 = 3650;
pub const MAX_REQUEST_AGE_SECS: u64 = 7 * 24 * 60 * 60;
pub const MANAGEMENT_KEY_FILE: &str = "travel-management.key";
pub const BUSINESS_KEY_FILE: &str = "travel-business.key";
pub const MANAGEMENT_CERT_FILE: &str = "travel-management.crt";
pub const BUSINESS_CERT_FILE: &str = "travel-business.crt";
pub const REQUEST_FILE: &str = "enrollment-request.json";
pub const STATE_FILE: &str = "enrollment-state.json";
pub const RESPONSE_FILE: &str = "enrollment-response.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelEnrollmentRequest {
    pub version: u32,
    pub request_id: Uuid,
    pub travel_id: String,
    pub created_at_unix_secs: u64,
    pub management_csr_pem: String,
    pub business_csr_pem: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelEnrollmentApproval {
    pub version: u32,
    pub credential_id: Uuid,
    pub request: TravelEnrollmentRequest,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelEnrollmentResponse {
    pub version: u32,
    pub approval: TravelEnrollmentApproval,
    pub management_certificate_pem: String,
    pub business_certificate_pem: String,
    pub signed_credential: SignedTravelCredential,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentState {
    pub version: u32,
    pub request_id: Uuid,
    pub travel_id: String,
    pub authority_public_key: String,
    pub management_spki_sha256: String,
    pub business_spki_sha256: String,
}

pub struct ParsedEnrollmentRequest {
    pub management: CertificateSigningRequestParams,
    pub business: CertificateSigningRequestParams,
}

/// Creates an enrollment directory with two encrypted private keys and a public request.
///
/// # Errors
///
/// Returns an error for invalid identity/authority data, an existing output path, or failed key
/// generation and durable file creation.
pub fn create_enrollment_request(
    travel_id: &str,
    authority_public_key: &str,
    password: &[u8],
    output_dir: &Path,
    now: u64,
) -> Result<TravelEnrollmentRequest> {
    validate_travel_id(travel_id)?;
    validate_authority_public_key(authority_public_key)?;
    if output_dir.exists() {
        bail!(
            "enrollment output path already exists: {}",
            output_dir.display()
        );
    }
    fs::create_dir(output_dir).with_context(|| {
        format!(
            "failed to create enrollment directory {}",
            output_dir.display()
        )
    })?;
    fs::set_permissions(output_dir, fs::Permissions::from_mode(0o700))?;

    let management = key::generate_encrypted_private_key(password)?;
    let business = key::generate_encrypted_private_key(password)?;
    let request = TravelEnrollmentRequest {
        version: ENROLLMENT_VERSION,
        request_id: Uuid::new_v4(),
        travel_id: travel_id.to_owned(),
        created_at_unix_secs: now,
        management_csr_pem: create_csr(travel_id, &management.key_pair)?,
        business_csr_pem: create_csr(travel_id, &business.key_pair)?,
    };
    let state = EnrollmentState {
        version: ENROLLMENT_VERSION,
        request_id: request.request_id,
        travel_id: travel_id.to_owned(),
        authority_public_key: authority_public_key.to_ascii_lowercase(),
        management_spki_sha256: spki_pin(&management.key_pair),
        business_spki_sha256: spki_pin(&business.key_pair),
    };
    write_private_file(
        &output_dir.join(MANAGEMENT_KEY_FILE),
        management.encrypted_pem.as_bytes(),
    )?;
    write_private_file(
        &output_dir.join(BUSINESS_KEY_FILE),
        business.encrypted_pem.as_bytes(),
    )?;
    write_json_private(&output_dir.join(REQUEST_FILE), &request)?;
    write_json_private(&output_dir.join(STATE_FILE), &state)?;
    Ok(request)
}

/// Verifies a request and returns its proof-of-possession public keys.
///
/// # Errors
///
/// Returns an error for unsupported versions, malformed identities, invalid CSRs, mismatched
/// identities, unsupported key algorithms, or stale/future requests.
pub fn parse_enrollment_request(
    request: &TravelEnrollmentRequest,
    now: u64,
) -> Result<ParsedEnrollmentRequest> {
    if request.version != ENROLLMENT_VERSION || request.request_id.is_nil() {
        bail!("unsupported or invalid enrollment request");
    }
    validate_travel_id(&request.travel_id)?;
    if request.created_at_unix_secs > now.saturating_add(300)
        || now.saturating_sub(request.created_at_unix_secs) > MAX_REQUEST_AGE_SECS
    {
        bail!("enrollment request is stale or from the future");
    }
    let management = CertificateSigningRequestParams::from_pem(&request.management_csr_pem)
        .context("invalid management CSR or proof of possession")?;
    let business = CertificateSigningRequestParams::from_pem(&request.business_csr_pem)
        .context("invalid business CSR or proof of possession")?;
    validate_csr(&management, &request.travel_id, "management")?;
    validate_csr(&business, &request.travel_id, "business")?;
    if management.public_key.subject_public_key_info()
        == business.public_key.subject_public_key_info()
    {
        bail!("management and business enrollment keys must be distinct");
    }
    Ok(ParsedEnrollmentRequest {
        management,
        business,
    })
}

/// Creates a bounded approval, defaulting to one year at the caller.
///
/// # Errors
///
/// Returns an error when the request or validity is invalid.
pub fn prepare_enrollment_approval(
    request: TravelEnrollmentRequest,
    valid_days: u32,
    now: u64,
) -> Result<TravelEnrollmentApproval> {
    let _ = parse_enrollment_request(&request, now)?;
    if valid_days == 0 || valid_days > MAX_VALID_DAYS {
        bail!("Travel validity must be between 1 and {MAX_VALID_DAYS} days");
    }
    Ok(TravelEnrollmentApproval {
        version: ENROLLMENT_VERSION,
        credential_id: Uuid::new_v4(),
        request,
        not_before_unix_secs: now.saturating_sub(300),
        not_after_unix_secs: now
            .checked_add(u64::from(valid_days) * 24 * 60 * 60)
            .ok_or_else(|| anyhow!("Travel validity overflow"))?,
    })
}

/// Verifies the signed response and returns the exact credential it authorizes.
///
/// # Errors
///
/// Returns an error for mismatched requests, certificates, identities, validity intervals,
/// public keys, or authorization signatures.
pub fn validate_enrollment_response(
    response: &TravelEnrollmentResponse,
    authority_public_key: &str,
    now: u64,
) -> Result<TravelCredential> {
    validate_authority_public_key(authority_public_key)?;
    if response.version != ENROLLMENT_VERSION
        || response.approval.version != ENROLLMENT_VERSION
        || response.approval.credential_id.is_nil()
    {
        bail!("unsupported or invalid enrollment response");
    }
    let parsed = parse_enrollment_request(
        &response.approval.request,
        response.approval.request.created_at_unix_secs,
    )?;
    validate_validity_interval(&response.approval)?;
    if now >= response.approval.not_after_unix_secs {
        bail!("enrollment response is already expired");
    }
    let expected = expected_credential(
        &response.approval,
        spki_pin(&parsed.management.public_key),
        spki_pin(&parsed.business.public_key),
    );
    let actual = response.signed_credential.verify(authority_public_key)?;
    if actual != expected {
        bail!("signed Travel credential does not match the approved request");
    }
    validate_issued_certificate(
        &response.management_certificate_pem,
        &expected,
        &expected.management_spki_sha256,
        "management",
    )?;
    validate_issued_certificate(
        &response.business_certificate_pem,
        &expected,
        &expected.business_spki_sha256,
        "business",
    )?;
    Ok(actual)
}

/// Verifies and installs a response into the original enrollment directory.
///
/// Existing certificate/response files are accepted only when byte-for-byte identical, making
/// import idempotent without allowing replacement of local identity material.
///
/// # Errors
///
/// Returns an error for a mismatched local request/state/key, invalid response or certificate
/// chain, wrong password, or conflicting existing file.
pub fn install_enrollment_response(
    enrollment_directory: &Path,
    response: &TravelEnrollmentResponse,
    management_ca: &Path,
    business_ca: &Path,
    password: &[u8],
    now: u64,
) -> Result<TravelCredential> {
    let request: TravelEnrollmentRequest = load_json(&enrollment_directory.join(REQUEST_FILE))?;
    let state: EnrollmentState = load_json(&enrollment_directory.join(STATE_FILE))?;
    if state.version != ENROLLMENT_VERSION
        || state.request_id != request.request_id
        || state.travel_id != request.travel_id
        || response.approval.request != request
    {
        bail!("enrollment response does not match the local enrollment request");
    }
    let credential = validate_enrollment_response(response, &state.authority_public_key, now)?;
    let (management_key_path, business_key_path, management_cert_path, business_cert_path) =
        enrollment_paths(enrollment_directory);
    validate_local_key(
        &management_key_path,
        password,
        &state.management_spki_sha256,
        "management",
    )?;
    validate_local_key(
        &business_key_path,
        password,
        &state.business_spki_sha256,
        "business",
    )?;
    if credential.management_spki_sha256 != state.management_spki_sha256
        || credential.business_spki_sha256 != state.business_spki_sha256
    {
        bail!("issued credential does not match the local private keys");
    }
    let management_certificate = certificate_from_pem(&response.management_certificate_pem)?;
    let business_certificate = certificate_from_pem(&response.business_certificate_pem)?;
    verify_client_chain(&management_certificate, management_ca, now, "management")?;
    verify_client_chain(&business_certificate, business_ca, now, "business")?;

    write_or_verify(
        &management_cert_path,
        response.management_certificate_pem.as_bytes(),
    )?;
    write_or_verify(
        &business_cert_path,
        response.business_certificate_pem.as_bytes(),
    )?;
    write_or_verify(
        &enrollment_directory.join(RESPONSE_FILE),
        &serde_json::to_vec_pretty(response)?,
    )?;
    Ok(credential)
}

/// Loads strict JSON from disk.
///
/// # Errors
///
/// Returns an error when the file cannot be read or decoded.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let data = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&data).with_context(|| format!("failed to parse {}", path.display()))
}

/// Writes strict JSON to a new private file.
///
/// # Errors
///
/// Returns an error when serialization or file creation fails.
pub fn write_json_private<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut data = serde_json::to_vec_pretty(value)?;
    data.push(b'\n');
    write_private_file(path, &data)
}

fn create_csr(travel_id: &str, key_pair: &rcgen::KeyPair) -> Result<String> {
    let mut params = exact_leaf_params(travel_id, 0, 4_102_444_800)?;
    params.not_before = time::OffsetDateTime::UNIX_EPOCH;
    params.not_after = time::OffsetDateTime::UNIX_EPOCH;
    params
        .serialize_request(key_pair)
        .context("failed to create Travel CSR")?
        .pem()
        .context("failed to encode Travel CSR")
}

pub(crate) fn exact_leaf_params(
    travel_id: &str,
    not_before: u64,
    not_after: u64,
) -> Result<CertificateParams> {
    let mut params = CertificateParams::default();
    let mut distinguished_name = DistinguishedName::new();
    distinguished_name.push(DnType::CommonName, format!("FlowSplice Travel {travel_id}"));
    params.distinguished_name = distinguished_name;
    params.subject_alt_names = vec![SanType::URI(Ia5String::try_from(format!(
        "flowsplice://identity/travel/{travel_id}"
    ))?)];
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    params.not_before = time::OffsetDateTime::from_unix_timestamp(i64::try_from(not_before)?)?;
    params.not_after = time::OffsetDateTime::from_unix_timestamp(i64::try_from(not_after)?)?;
    Ok(params)
}

fn validate_csr(csr: &CertificateSigningRequestParams, travel_id: &str, label: &str) -> Result<()> {
    if csr.public_key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
        bail!("{label} CSR must use ECDSA P-256/SHA-256");
    }
    let expected_uri = format!("flowsplice://identity/travel/{travel_id}");
    let has_expected_uri = csr
        .params
        .subject_alt_names
        .iter()
        .any(|name| matches!(name, SanType::URI(uri) if uri.as_str() == expected_uri));
    if csr.params.subject_alt_names.len() != 1 || !has_expected_uri {
        bail!("{label} CSR has an invalid FlowSplice Travel identity");
    }
    Ok(())
}

pub(crate) fn spki_pin(key: &impl PublicKeyData) -> String {
    let spki = key.subject_public_key_info();
    hex::encode(digest::digest(&digest::SHA256, &spki).as_ref())
}

pub(crate) fn validate_approval(approval: &TravelEnrollmentApproval, now: u64) -> Result<()> {
    if approval.version != ENROLLMENT_VERSION || approval.credential_id.is_nil() {
        bail!("unsupported or invalid enrollment approval");
    }
    let _ = parse_enrollment_request(&approval.request, now)?;
    validate_validity_interval(approval)
}

fn validate_validity_interval(approval: &TravelEnrollmentApproval) -> Result<()> {
    if approval.not_before_unix_secs >= approval.not_after_unix_secs
        || approval
            .not_after_unix_secs
            .saturating_sub(approval.not_before_unix_secs)
            > u64::from(MAX_VALID_DAYS) * 24 * 60 * 60 + 300
    {
        bail!("enrollment approval has an invalid validity interval");
    }
    Ok(())
}

pub(crate) fn expected_credential(
    approval: &TravelEnrollmentApproval,
    management_spki_sha256: String,
    business_spki_sha256: String,
) -> TravelCredential {
    TravelCredential {
        credential_id: approval.credential_id,
        travel_id: approval.request.travel_id.clone(),
        management_spki_sha256,
        business_spki_sha256,
        not_before_unix_secs: approval.not_before_unix_secs,
        not_after_unix_secs: approval.not_after_unix_secs,
    }
}

fn validate_travel_id(travel_id: &str) -> Result<()> {
    if travel_id.is_empty()
        || travel_id.len() > 128
        || !travel_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("Travel id must contain only ASCII letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to create private file {}", path.display()))?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

fn validate_issued_certificate(
    certificate_pem: &str,
    credential: &TravelCredential,
    expected_spki: &str,
    label: &str,
) -> Result<()> {
    let certificate = certificate_from_pem(certificate_pem)
        .with_context(|| format!("invalid issued {label} certificate"))?;
    let identity = peer_identity(Some(std::slice::from_ref(&certificate)))?;
    require_peer(&identity, Role::Travel, Some(&credential.travel_id), &[])?;
    if !identity.spki_sha256.eq_ignore_ascii_case(expected_spki)
        || identity.not_before_unix_secs != credential.not_before_unix_secs
        || identity.not_after_unix_secs != credential.not_after_unix_secs
    {
        bail!("issued {label} certificate does not match its approved credential");
    }
    Ok(())
}

fn validate_local_key(
    path: &Path,
    password: &[u8],
    expected_spki: &str,
    label: &str,
) -> Result<()> {
    let private_key = Zeroizing::new(key::load_private_key(path, Some(password), false)?);
    let key_pair = rcgen::KeyPair::try_from(&*private_key)
        .with_context(|| format!("failed to parse local {label} private key"))?;
    if !spki_pin(&key_pair).eq_ignore_ascii_case(expected_spki) {
        bail!("local {label} private key does not match enrollment state");
    }
    Ok(())
}

fn certificate_from_pem(pem: &str) -> Result<CertificateDer<'static>> {
    CertificateDer::from_pem_slice(pem.as_bytes()).context("failed to parse PEM certificate")
}

fn verify_client_chain(
    certificate: &CertificateDer<'_>,
    ca_path: &Path,
    now: u64,
    label: &str,
) -> Result<()> {
    let roots = CertificateDer::pem_file_iter(ca_path)
        .with_context(|| format!("failed to open {label} CA {}", ca_path.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse {label} CA {}", ca_path.display()))?;
    let mut store = RootCertStore::empty();
    let root_count = roots.len();
    let (added, ignored) = store.add_parsable_certificates(roots);
    if root_count == 0 || added != root_count || ignored != 0 {
        bail!("{label} CA file contains an invalid trust anchor");
    }
    let verifier = WebPkiClientVerifier::builder(Arc::new(store))
        .build()
        .context("failed to construct client certificate verifier")?;
    verifier
        .verify_client_cert(
            certificate,
            &[],
            UnixTime::since_unix_epoch(Duration::from_secs(now)),
        )
        .with_context(|| format!("issued {label} certificate does not chain to its CA"))?;
    Ok(())
}

fn write_or_verify(path: &Path, data: &[u8]) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.file_type().is_file() || fs::read(path)? != data {
                bail!(
                    "refusing to replace conflicting enrollment file {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            write_private_file(path, data)
        }
        Err(error) => Err(error.into()),
    }
}

#[must_use]
pub fn enrollment_paths(directory: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    (
        directory.join(MANAGEMENT_KEY_FILE),
        directory.join(BUSINESS_KEY_FILE),
        directory.join(MANAGEMENT_CERT_FILE),
        directory.join(BUSINESS_CERT_FILE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_lc_rs::{rand::SystemRandom, signature};
    use signature::KeyPair as _;

    fn authority_public_key() -> Result<String> {
        let rng = SystemRandom::new();
        let key = signature::EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            signature::EcdsaKeyPair::generate_pkcs8(
                &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                &rng,
            )?
            .as_ref(),
        )?;
        Ok(hex::encode(key.public_key().as_ref()))
    }

    #[test]
    fn request_has_distinct_proven_p256_keys_and_default_one_year_approval() -> Result<()> {
        let temporary_directory = tempfile::tempdir()?;
        let directory = temporary_directory.path().join("enrollment");
        let request = create_enrollment_request(
            "macbook-travel",
            &authority_public_key()?,
            b"long test password",
            &directory,
            1_800_000_000,
        )?;
        let parsed = parse_enrollment_request(&request, 1_800_000_001)?;
        assert_ne!(
            parsed.management.public_key.subject_public_key_info(),
            parsed.business.public_key.subject_public_key_info()
        );
        let approval = prepare_enrollment_approval(request, DEFAULT_VALID_DAYS, 1_800_000_001)?;
        assert_eq!(
            approval.not_after_unix_secs - 1_800_000_001,
            365 * 24 * 60 * 60
        );
        assert!(prepare_enrollment_approval(approval.request, 0, 1_800_000_001).is_err());
        Ok(())
    }
}
