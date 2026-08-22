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
use aws_lc_rs::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use flowsplice_core::authorization::{
    SignedTravelCredential, TRAVEL_CREDENTIAL_OBJECT_TYPE, TRAVEL_CREDENTIAL_VERSION,
    TravelCredential, TravelCredentialScope, TrustedTravelAuthority,
};
use flowsplice_core::{
    deployment::{DeploymentTrust, SignedDeploymentTrust, SignedHomeEndpointCredential},
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

pub mod home;
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
pub const MANAGEMENT_CA_FILE: &str = "management-ca.crt";
pub const BUSINESS_CA_FILE: &str = "business-ca.crt";
pub const REQUEST_FILE: &str = "enrollment-request.json";
pub const STATE_FILE: &str = "enrollment-state.json";
pub const RESPONSE_FILE: &str = "enrollment-response.json";
pub const DEPLOYMENT_TRUST_FILE: &str = "deployment-trust.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelEnrollmentRequest {
    pub version: u32,
    pub request_id: Uuid,
    pub nonce: String,
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
    pub authority_id: String,
    pub scope: TravelCredentialScope,
    pub request: TravelEnrollmentRequest,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelEnrollmentResponse {
    pub version: u32,
    pub approval: TravelEnrollmentApproval,
    pub deployment_trust: SignedDeploymentTrust,
    #[serde(default)]
    pub home_endpoint_credential: Option<SignedHomeEndpointCredential>,
    pub management_certificate_pem: String,
    pub business_certificate_pem: String,
    pub signed_credential: SignedTravelCredential,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentState {
    pub version: u32,
    pub request_id: Uuid,
    pub nonce: String,
    pub travel_id: String,
    pub management_spki_sha256: String,
    pub business_spki_sha256: String,
}

pub struct ParsedEnrollmentRequest {
    pub management: CertificateSigningRequestParams,
    pub business: CertificateSigningRequestParams,
}

pub(crate) struct EnrollmentCredentialBindings<'a> {
    pub signed_trust: &'a SignedDeploymentTrust,
    pub trust: &'a DeploymentTrust,
    pub authority: &'a TrustedTravelAuthority,
    pub management_certificate_pem: &'a str,
    pub business_certificate_pem: &'a str,
}

/// Creates an enrollment directory with two encrypted private keys and a public request.
///
/// # Errors
///
/// Returns an error for invalid identity/authority data, an existing enrollment directory, or failed key
/// generation and durable file creation.
pub fn create_enrollment_request(
    travel_id: &str,
    password: &[u8],
    enrollment_directory: &Path,
    now: u64,
) -> Result<TravelEnrollmentRequest> {
    validate_travel_id(travel_id)?;
    if enrollment_directory.exists() {
        bail!(
            "enrollment directory already exists: {}",
            enrollment_directory.display()
        );
    }
    fs::create_dir(enrollment_directory).with_context(|| {
        format!(
            "failed to create enrollment directory {}",
            enrollment_directory.display()
        )
    })?;
    fs::set_permissions(enrollment_directory, fs::Permissions::from_mode(0o700))?;

    let management = key::generate_encrypted_private_key(password)?;
    let business = key::generate_encrypted_private_key(password)?;
    let mut nonce = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow!("failed to generate enrollment nonce"))?;
    let request = TravelEnrollmentRequest {
        version: ENROLLMENT_VERSION,
        request_id: Uuid::new_v4(),
        nonce: hex::encode(nonce),
        travel_id: travel_id.to_owned(),
        created_at_unix_secs: now,
        management_csr_pem: create_csr(travel_id, &management.key_pair)?,
        business_csr_pem: create_csr(travel_id, &business.key_pair)?,
    };
    let state = EnrollmentState {
        version: ENROLLMENT_VERSION,
        request_id: request.request_id,
        nonce: request.nonce.clone(),
        travel_id: travel_id.to_owned(),
        management_spki_sha256: spki_pin(&management.key_pair),
        business_spki_sha256: spki_pin(&business.key_pair),
    };
    write_private_file(
        &enrollment_directory.join(MANAGEMENT_KEY_FILE),
        management.encrypted_pem.as_bytes(),
    )?;
    write_private_file(
        &enrollment_directory.join(BUSINESS_KEY_FILE),
        business.encrypted_pem.as_bytes(),
    )?;
    write_json_private(&enrollment_directory.join(REQUEST_FILE), &request)?;
    write_json_private(&enrollment_directory.join(STATE_FILE), &state)?;
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
    let nonce = hex::decode(&request.nonce).context("enrollment nonce must be hexadecimal")?;
    if request.version != ENROLLMENT_VERSION || request.request_id.is_nil() || nonce.len() != 32 {
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
    valid_for_secs: u64,
    authority_id: String,
    scope: TravelCredentialScope,
    now: u64,
) -> Result<TravelEnrollmentApproval> {
    let _ = parse_enrollment_request(&request, now)?;
    if authority_id.is_empty() {
        bail!("Travel authority id must be non-empty");
    }
    let max_valid_secs = u64::from(MAX_VALID_DAYS) * 24 * 60 * 60;
    if valid_for_secs == 0 || valid_for_secs > max_valid_secs {
        bail!("Travel validity must be between 1 second and {MAX_VALID_DAYS} days");
    }
    Ok(TravelEnrollmentApproval {
        version: ENROLLMENT_VERSION,
        credential_id: Uuid::new_v4(),
        authority_id,
        scope,
        request,
        not_before_unix_secs: now.saturating_sub(300),
        not_after_unix_secs: now
            .checked_add(valid_for_secs)
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
    deployment_root_public_key: &str,
    now: u64,
) -> Result<(TravelCredential, DeploymentTrust)> {
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
    if now.saturating_add(flowsplice_core::deployment::MAX_CLOCK_SKEW_SECS)
        < response.approval.not_before_unix_secs
    {
        bail!("enrollment response is not yet valid");
    }
    if now >= response.approval.not_after_unix_secs {
        bail!("enrollment response is already expired");
    }
    let trust = response
        .deployment_trust
        .verify(deployment_root_public_key, now)?;
    if response.approval.not_before_unix_secs < trust.not_before_unix_secs
        || response.approval.not_after_unix_secs > trust.not_after_unix_secs
    {
        bail!("enrollment validity is outside the deployment-trusted authority window");
    }
    if response.signed_credential.authority_id != response.approval.authority_id {
        bail!("signed Travel credential has the wrong authority id");
    }
    let endpoint_credentials = response
        .home_endpoint_credential
        .iter()
        .cloned()
        .collect::<Vec<_>>();
    if let Some(endpoint) = &response.home_endpoint_credential {
        let endpoint = endpoint.verify(&trust, now)?;
        if response.approval.not_after_unix_secs > endpoint.not_after_unix_secs {
            bail!("enrollment validity exceeds the issuing Home endpoint validity");
        }
    }
    let authorities = trust.travel_authorities_with_home_delegations(&endpoint_credentials, now)?;
    let authority = authorities
        .iter()
        .find(|authority| authority.id() == response.approval.authority_id)
        .ok_or_else(|| {
            anyhow!(
                "Travel authority {} is not trusted",
                response.approval.authority_id
            )
        })?;
    let actual = response.signed_credential.verify(authority)?;
    let expected = expected_credential(
        &response.approval,
        &EnrollmentCredentialBindings {
            signed_trust: &response.deployment_trust,
            trust: &trust,
            authority,
            management_certificate_pem: &response.management_certificate_pem,
            business_certificate_pem: &response.business_certificate_pem,
        },
        spki_pin(&parsed.management.public_key),
        spki_pin(&parsed.business.public_key),
    )?;
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
    verify_client_chain(
        &certificate_from_pem(&response.management_certificate_pem)?,
        &trust.management_ca_certificate_pem,
        now,
        "management",
    )?;
    verify_client_chain(
        &certificate_from_pem(&response.business_certificate_pem)?,
        &trust.business_ca_certificate_pem,
        now,
        "business",
    )?;
    Ok((actual, trust))
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
    deployment_root_public_key: &str,
    password: &[u8],
    now: u64,
) -> Result<TravelCredential> {
    let request: TravelEnrollmentRequest = load_json(&enrollment_directory.join(REQUEST_FILE))?;
    let state: EnrollmentState = load_json(&enrollment_directory.join(STATE_FILE))?;
    if state.version != ENROLLMENT_VERSION
        || state.request_id != request.request_id
        || state.nonce != request.nonce
        || state.travel_id != request.travel_id
        || response.approval.request != request
    {
        bail!("enrollment response does not match the local enrollment request");
    }
    let (credential, trust) =
        validate_enrollment_response(response, deployment_root_public_key, now)?;
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
    write_or_verify(
        &enrollment_directory.join(MANAGEMENT_CA_FILE),
        trust.management_ca_certificate_pem.as_bytes(),
    )?;
    write_or_verify(
        &enrollment_directory.join(BUSINESS_CA_FILE),
        trust.business_ca_certificate_pem.as_bytes(),
    )?;
    write_or_verify(
        &enrollment_directory.join(DEPLOYMENT_TRUST_FILE),
        &serde_json::to_vec_pretty(&response.deployment_trust)?,
    )?;
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
    if approval.version != ENROLLMENT_VERSION
        || approval.credential_id.is_nil()
        || approval.authority_id.is_empty()
    {
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
    bindings: &EnrollmentCredentialBindings<'_>,
    management_spki_sha256: String,
    business_spki_sha256: String,
) -> Result<TravelCredential> {
    Ok(TravelCredential {
        version: TRAVEL_CREDENTIAL_VERSION,
        object_type: TRAVEL_CREDENTIAL_OBJECT_TYPE.to_owned(),
        deployment_id: bindings.trust.deployment_id.clone(),
        deployment_trust_sha256: bindings.signed_trust.payload_digest_sha256()?,
        credential_id: approval.credential_id,
        authority_id: approval.authority_id.clone(),
        authority_epoch: bindings.authority.epoch(),
        enrollment_request_id: approval.request.request_id,
        enrollment_nonce: approval.request.nonce.clone(),
        enrollment_request_sha256: sha256_json(&approval.request)?,
        travel_id: approval.request.travel_id.clone(),
        management_spki_sha256,
        business_spki_sha256,
        management_ca_sha256: sha256_bytes(bindings.trust.management_ca_certificate_pem.as_bytes()),
        business_ca_sha256: sha256_bytes(bindings.trust.business_ca_certificate_pem.as_bytes()),
        management_certificate_sha256: certificate_sha256(bindings.management_certificate_pem)?,
        business_certificate_sha256: certificate_sha256(bindings.business_certificate_pem)?,
        scope: approval.scope.clone(),
        not_before_unix_secs: approval.not_before_unix_secs,
        not_after_unix_secs: approval.not_after_unix_secs,
    })
}

fn sha256_json<T: Serialize>(value: &T) -> Result<String> {
    Ok(sha256_bytes(&serde_json::to_vec(value)?))
}

fn sha256_bytes(value: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, value).as_ref())
}

fn certificate_sha256(pem: &str) -> Result<String> {
    Ok(sha256_bytes(certificate_from_pem(pem)?.as_ref()))
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

pub(crate) fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
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

pub(crate) fn certificate_from_pem(pem: &str) -> Result<CertificateDer<'static>> {
    let mut certificates = CertificateDer::pem_slice_iter(pem.as_bytes());
    let certificate = certificates
        .next()
        .transpose()
        .context("failed to parse PEM certificate")?
        .ok_or_else(|| anyhow!("PEM contains no certificate"))?;
    if certificates.next().is_some() {
        bail!("PEM must contain exactly one certificate");
    }
    Ok(certificate)
}

pub(crate) fn verify_client_chain(
    certificate: &CertificateDer<'_>,
    ca_pem: &str,
    now: u64,
    label: &str,
) -> Result<()> {
    let roots = CertificateDer::pem_slice_iter(ca_pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse configured {label} CA"))?;
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

pub(crate) fn write_or_verify(path: &Path, data: &[u8]) -> Result<()> {
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

    #[test]
    fn request_has_distinct_proven_p256_keys_and_default_one_year_approval() -> Result<()> {
        let temporary_directory = tempfile::tempdir()?;
        let directory = temporary_directory.path().join("enrollment");
        let request = create_enrollment_request(
            "macbook-travel",
            b"long test password",
            &directory,
            1_800_000_000,
        )?;
        let parsed = parse_enrollment_request(&request, 1_800_000_001)?;
        assert_ne!(
            parsed.management.public_key.subject_public_key_info(),
            parsed.business.public_key.subject_public_key_info()
        );
        let approval = prepare_enrollment_approval(
            request,
            u64::from(DEFAULT_VALID_DAYS) * 24 * 60 * 60,
            "home-1-authority".to_owned(),
            TravelCredentialScope::Home {
                home_id: "home-1".to_owned(),
            },
            1_800_000_001,
        )?;
        assert_eq!(
            approval.not_after_unix_secs - 1_800_000_001,
            365 * 24 * 60 * 60
        );
        assert!(
            prepare_enrollment_approval(
                approval.request,
                0,
                "home-1-authority".to_owned(),
                TravelCredentialScope::Home {
                    home_id: "home-1".to_owned(),
                },
                1_800_000_001,
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn approval_accepts_an_exact_thirty_minute_test_window() -> Result<()> {
        let temporary_directory = tempfile::tempdir()?;
        let request = create_enrollment_request(
            "short-lived-travel",
            b"long test password",
            &temporary_directory.path().join("enrollment"),
            1_800_000_000,
        )?;
        let approval = prepare_enrollment_approval(
            request,
            30 * 60,
            "home-1-authority".to_owned(),
            TravelCredentialScope::Home {
                home_id: "home-1".to_owned(),
            },
            1_800_000_001,
        )?;
        assert_eq!(approval.not_after_unix_secs - 1_800_000_001, 30 * 60);
        Ok(())
    }
}
