use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    digest,
    rand::{SecureRandom, SystemRandom},
    signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair as _},
};
use flowsplice_core::{
    authorization::TrustedTravelAuthority,
    deployment::{
        DeploymentTrust, HOME_ENDPOINT_CREDENTIAL_OBJECT_TYPE, HOME_ENDPOINT_CREDENTIAL_VERSION,
        HomeEndpointCredential, SignedDeploymentTrust, SignedHomeEndpointCredential,
    },
    protocol::Role,
    tls::{peer_identity, require_peer},
};
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, PublicKeyData, SanType,
    string::Ia5String,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    BUSINESS_CA_FILE, DEPLOYMENT_TRUST_FILE, MANAGEMENT_CA_FILE, MAX_REQUEST_AGE_SECS,
    MAX_VALID_DAYS, certificate_from_pem,
    issuer::{ProtectedKey, load_ca_issuer},
    key, verify_client_chain, write_json_private, write_or_verify, write_private_file,
};

pub const HOME_ENROLLMENT_VERSION: u32 = 1;
pub const HOME_MANAGEMENT_KEY_FILE: &str = "home-management.key";
pub const HOME_BUSINESS_KEY_FILE: &str = "home-business.key";
pub const HOME_MANAGEMENT_CERT_FILE: &str = "home-management.crt";
pub const HOME_BUSINESS_CERT_FILE: &str = "home-business.crt";
pub const HOME_REQUEST_FILE: &str = "home-enrollment-request.json";
pub const HOME_STATE_FILE: &str = "home-enrollment-state.json";
pub const HOME_RESPONSE_FILE: &str = "home-enrollment-response.json";
pub const HOME_ENDPOINT_CREDENTIAL_FILE: &str = "home-endpoint-credential.json";
pub const HOME_ISSUER_MANAGEMENT_CA_KEY_FILE: &str = "management-ca.key";
pub const HOME_ISSUER_BUSINESS_CA_KEY_FILE: &str = "business-ca.key";
pub const HOME_ISSUER_HOME_AUTHORITY_KEY_FILE: &str = "home-authority.key";
pub const HOME_ISSUER_GLOBAL_AUTHORITY_KEY_FILE: &str = "global-authority.key";
pub const HOME_ISSUER_ENROLLMENT_AUTHORITY_KEY_FILE: &str = "home-enrollment-authority.key";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HomeEnrollmentProfile {
    ServingOnly,
    HomeIssuer,
    GlobalIssuer,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeEnrollmentRequest {
    pub version: u32,
    pub request_id: Uuid,
    pub nonce: String,
    pub home_id: String,
    pub created_at_unix_secs: u64,
    pub management_csr_pem: String,
    pub business_csr_pem: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeEnrollmentApproval {
    pub version: u32,
    pub credential_id: Uuid,
    pub authority_id: String,
    pub profile: HomeEnrollmentProfile,
    pub request: HomeEnrollmentRequest,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeEnrollmentResponse {
    pub version: u32,
    pub approval: HomeEnrollmentApproval,
    pub deployment_trust: SignedDeploymentTrust,
    pub management_certificate_pem: String,
    pub business_certificate_pem: String,
    pub signed_endpoint_credential: SignedHomeEndpointCredential,
    pub issuer_bundle: Option<HomeIssuerBundle>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeIssuerBundle {
    pub management_ca_key_pem: String,
    pub business_ca_key_pem: String,
    pub home_authority_id: String,
    pub home_authority_key_pem: String,
    pub global_authority_id: Option<String>,
    pub global_authority_key_pem: Option<String>,
    pub home_enrollment_authority_key_pem: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeEnrollmentState {
    pub version: u32,
    pub request_id: Uuid,
    pub nonce: String,
    pub home_id: String,
    pub management_spki_sha256: String,
    pub business_spki_sha256: String,
}

pub struct ParsedHomeEnrollmentRequest {
    management: CertificateSigningRequestParams,
    business: CertificateSigningRequestParams,
}

pub struct HomeIssuerMaterial<'a> {
    pub deployment_trust: &'a SignedDeploymentTrust,
    pub deployment_root_public_key: &'a str,
    pub management_ca_certificate: &'a Path,
    pub management_ca_key: ProtectedKey<'a>,
    pub business_ca_certificate: &'a Path,
    pub business_ca_key: ProtectedKey<'a>,
    pub home_enrollment_authority_key: ProtectedKey<'a>,
}

/// Creates a fresh Home endpoint request. Runtime leaf keys intentionally remain unencrypted and
/// owner-only, matching the daemon's existing startup model; issuer keys remain encrypted.
///
/// # Errors
///
/// Returns an error for an invalid Home id, an existing enrollment directory, random generation
/// failure, or a durable-write failure.
pub fn create_home_enrollment_request(
    home_id: &str,
    enrollment_directory: &Path,
    now: u64,
) -> Result<HomeEnrollmentRequest> {
    validate_home_id(home_id)?;
    if enrollment_directory.exists() {
        bail!(
            "Home enrollment directory already exists: {}",
            enrollment_directory.display()
        );
    }
    fs::create_dir(enrollment_directory)?;
    #[cfg(unix)]
    fs::set_permissions(
        enrollment_directory,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let management = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let business = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
    let mut nonce = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| anyhow!("failed to generate Home enrollment nonce"))?;
    let request = HomeEnrollmentRequest {
        version: HOME_ENROLLMENT_VERSION,
        request_id: Uuid::new_v4(),
        nonce: hex::encode(nonce),
        home_id: home_id.to_owned(),
        created_at_unix_secs: now,
        management_csr_pem: create_home_csr(
            home_id,
            &management,
            ExtendedKeyUsagePurpose::ClientAuth,
        )?,
        business_csr_pem: create_home_csr(home_id, &business, ExtendedKeyUsagePurpose::ServerAuth)?,
    };
    let state = HomeEnrollmentState {
        version: HOME_ENROLLMENT_VERSION,
        request_id: request.request_id,
        nonce: request.nonce.clone(),
        home_id: home_id.to_owned(),
        management_spki_sha256: spki_pin(&management),
        business_spki_sha256: spki_pin(&business),
    };
    write_private_file(
        &enrollment_directory.join(HOME_MANAGEMENT_KEY_FILE),
        management.serialize_pem().as_bytes(),
    )?;
    write_private_file(
        &enrollment_directory.join(HOME_BUSINESS_KEY_FILE),
        business.serialize_pem().as_bytes(),
    )?;
    write_json_private(&enrollment_directory.join(HOME_REQUEST_FILE), &request)?;
    write_json_private(&enrollment_directory.join(HOME_STATE_FILE), &state)?;
    Ok(request)
}

/// Validates a Home enrollment request and both CSR proofs of possession.
///
/// # Errors
///
/// Returns an error for malformed, stale, mismatched, or unsupported enrollment material.
pub fn parse_home_enrollment_request(
    request: &HomeEnrollmentRequest,
    now: u64,
) -> Result<ParsedHomeEnrollmentRequest> {
    let nonce = hex::decode(&request.nonce).context("Home enrollment nonce must be hexadecimal")?;
    if request.version != HOME_ENROLLMENT_VERSION
        || request.request_id.is_nil()
        || nonce.len() != 32
    {
        bail!("unsupported or invalid Home enrollment request");
    }
    validate_home_id(&request.home_id)?;
    if request.created_at_unix_secs > now.saturating_add(300)
        || now.saturating_sub(request.created_at_unix_secs) > MAX_REQUEST_AGE_SECS
    {
        bail!("Home enrollment request is stale or from the future");
    }
    let management = CertificateSigningRequestParams::from_pem(&request.management_csr_pem)
        .context("invalid Home management CSR or proof of possession")?;
    let business = CertificateSigningRequestParams::from_pem(&request.business_csr_pem)
        .context("invalid Home business CSR or proof of possession")?;
    validate_home_csr(
        &management,
        &request.home_id,
        ExtendedKeyUsagePurpose::ClientAuth,
        "management",
    )?;
    validate_home_csr(
        &business,
        &request.home_id,
        ExtendedKeyUsagePurpose::ServerAuth,
        "business",
    )?;
    if management.public_key.subject_public_key_info()
        == business.public_key.subject_public_key_info()
    {
        bail!("Home management and business keys must be distinct");
    }
    Ok(ParsedHomeEnrollmentRequest {
        management,
        business,
    })
}

/// Binds a validated request to one permission profile and validity interval.
///
/// # Errors
///
/// Returns an error when the request, authority id, or validity interval is invalid.
pub fn prepare_home_enrollment_approval(
    request: HomeEnrollmentRequest,
    valid_for_secs: u64,
    authority_id: String,
    profile: HomeEnrollmentProfile,
    now: u64,
) -> Result<HomeEnrollmentApproval> {
    let _ = parse_home_enrollment_request(&request, now)?;
    if authority_id.is_empty()
        || valid_for_secs == 0
        || valid_for_secs > u64::from(MAX_VALID_DAYS) * 86_400
    {
        bail!("invalid Home enrollment authority or validity");
    }
    Ok(HomeEnrollmentApproval {
        version: HOME_ENROLLMENT_VERSION,
        credential_id: Uuid::new_v4(),
        authority_id,
        profile,
        request,
        not_before_unix_secs: now.saturating_sub(300),
        not_after_unix_secs: now
            .checked_add(valid_for_secs)
            .ok_or_else(|| anyhow!("Home validity overflow"))?,
    })
}

/// Issues Home TLS identities, a signed endpoint capability, and optional issuer material.
///
/// # Errors
///
/// Returns an error for an invalid approval, untrusted signer, wrong password, CA/key mismatch,
/// out-of-range validity, or signing failure.
#[allow(clippy::too_many_lines)]
pub fn issue_home_enrollment(
    approval: HomeEnrollmentApproval,
    material: &HomeIssuerMaterial<'_>,
    now: u64,
) -> Result<HomeEnrollmentResponse> {
    validate_home_approval(&approval, now)?;
    let trust = material
        .deployment_trust
        .verify(material.deployment_root_public_key, now)?;
    if approval.not_before_unix_secs < trust.not_before_unix_secs
        || approval.not_after_unix_secs > trust.not_after_unix_secs
    {
        bail!("Home enrollment validity is outside deployment trust");
    }
    let authority = trust
        .home_enrollment_authorities
        .iter()
        .find(|authority| authority.id == approval.authority_id)
        .ok_or_else(|| anyhow!("Home enrollment authority is not deployment-trusted"))?;
    let parsed = parse_home_enrollment_request(&approval.request, now)?;
    let password = material
        .home_enrollment_authority_key
        .password
        .ok_or_else(|| anyhow!("Home enrollment approval requires a signing password"))?;
    let (delegated_travel_authorities, issuer_bundle) = match approval.profile {
        HomeEnrollmentProfile::ServingOnly => (Vec::new(), None),
        HomeEnrollmentProfile::HomeIssuer | HomeEnrollmentProfile::GlobalIssuer => {
            let home_key = key::generate_encrypted_private_key(password)?;
            let home_authority_id = format!(
                "{}-authority-{}",
                approval.request.home_id, approval.credential_id
            );
            let mut delegated = vec![TrustedTravelAuthority::Home {
                id: home_authority_id.clone(),
                epoch: 1,
                home_id: approval.request.home_id.clone(),
                public_key: hex::encode(home_key.key_pair.public_key_raw()),
            }];
            let (global_authority_id, global_authority_key_pem) =
                if approval.profile == HomeEnrollmentProfile::GlobalIssuer {
                    let global_key = key::generate_encrypted_private_key(password)?;
                    let global_id = format!(
                        "{}-global-authority-{}",
                        approval.request.home_id, approval.credential_id
                    );
                    delegated.push(TrustedTravelAuthority::Global {
                        id: global_id.clone(),
                        epoch: 1,
                        home_id: approval.request.home_id.clone(),
                        public_key: hex::encode(global_key.key_pair.public_key_raw()),
                    });
                    (Some(global_id), Some(global_key.encrypted_pem.to_string()))
                } else {
                    (None, None)
                };
            let management_ca_key_pem = fs::read_to_string(material.management_ca_key.path)?;
            let business_ca_key_pem = fs::read_to_string(material.business_ca_key.path)?;
            require_encrypted_key_pem(&management_ca_key_pem, "management CA")?;
            require_encrypted_key_pem(&business_ca_key_pem, "business CA")?;
            let home_enrollment_authority_key_pem =
                if approval.profile == HomeEnrollmentProfile::GlobalIssuer {
                    let value = fs::read_to_string(material.home_enrollment_authority_key.path)?;
                    require_encrypted_key_pem(&value, "Home enrollment authority")?;
                    Some(value)
                } else {
                    None
                };
            (
                delegated,
                Some(HomeIssuerBundle {
                    management_ca_key_pem,
                    business_ca_key_pem,
                    home_authority_id,
                    home_authority_key_pem: home_key.encrypted_pem.to_string(),
                    global_authority_id,
                    global_authority_key_pem,
                    home_enrollment_authority_key_pem,
                }),
            )
        }
    };
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
    let management_ca = fs::read_to_string(material.management_ca_certificate)?;
    let business_ca = fs::read_to_string(material.business_ca_certificate)?;
    if management_ca != trust.management_ca_certificate_pem
        || business_ca != trust.business_ca_certificate_pem
    {
        bail!("Home issuer CA certificates do not match deployment trust");
    }
    let management_certificate_pem = exact_home_leaf_params(
        &approval.request.home_id,
        approval.not_before_unix_secs,
        approval.not_after_unix_secs,
        ExtendedKeyUsagePurpose::ClientAuth,
    )?
    .signed_by(&parsed.management.public_key, &management_issuer)?
    .pem();
    let business_certificate_pem = exact_home_leaf_params(
        &approval.request.home_id,
        approval.not_before_unix_secs,
        approval.not_after_unix_secs,
        ExtendedKeyUsagePurpose::ServerAuth,
    )?
    .signed_by(&parsed.business.public_key, &business_issuer)?
    .pem();
    let endpoint = HomeEndpointCredential {
        version: HOME_ENDPOINT_CREDENTIAL_VERSION,
        object_type: HOME_ENDPOINT_CREDENTIAL_OBJECT_TYPE.to_owned(),
        deployment_id: trust.deployment_id.clone(),
        credential_id: approval.credential_id,
        authority_id: authority.id.clone(),
        authority_epoch: authority.epoch,
        enrollment_request_id: approval.request.request_id,
        home_id: approval.request.home_id.clone(),
        management_spki_sha256: spki_pin(&parsed.management.public_key),
        business_spki_sha256: spki_pin(&parsed.business.public_key),
        delegated_travel_authorities,
        issuer_bundle_sha256: issuer_bundle
            .as_ref()
            .map(issuer_bundle_digest)
            .transpose()?,
        not_before_unix_secs: approval.not_before_unix_secs,
        not_after_unix_secs: approval.not_after_unix_secs,
    };
    let authority_private_key = Zeroizing::new(key::load_private_key(
        material.home_enrollment_authority_key.path,
        material.home_enrollment_authority_key.password,
        material.home_enrollment_authority_key.allow_unencrypted,
    )?);
    let authority_key = EcdsaKeyPair::from_pkcs8(
        &ECDSA_P256_SHA256_ASN1_SIGNING,
        authority_private_key.secret_der(),
    )
    .map_err(|_| anyhow!("Home enrollment authority key is not P-256 PKCS#8"))?;
    if !hex::encode(authority_key.public_key().as_ref()).eq_ignore_ascii_case(&authority.public_key)
    {
        bail!("Home enrollment authority private key does not match deployment trust");
    }
    let signed_endpoint_credential = SignedHomeEndpointCredential::sign(&endpoint, &authority_key)?;
    Ok(HomeEnrollmentResponse {
        version: HOME_ENROLLMENT_VERSION,
        approval,
        deployment_trust: material.deployment_trust.clone(),
        management_certificate_pem,
        business_certificate_pem,
        signed_endpoint_credential,
        issuer_bundle,
    })
}

/// Verifies the full root-bound Home enrollment response without installing it.
///
/// # Errors
///
/// Returns an error for a bad root or authority signature, mismatched request, certificate, key,
/// profile capability, chain, or validity interval.
pub fn validate_home_enrollment_response(
    response: &HomeEnrollmentResponse,
    deployment_root_public_key: &str,
    now: u64,
) -> Result<(HomeEndpointCredential, DeploymentTrust)> {
    if response.version != HOME_ENROLLMENT_VERSION
        || response.approval.version != HOME_ENROLLMENT_VERSION
    {
        bail!("unsupported Home enrollment response");
    }
    let parsed = parse_home_enrollment_request(
        &response.approval.request,
        response.approval.request.created_at_unix_secs,
    )?;
    validate_home_approval(
        &response.approval,
        response.approval.request.created_at_unix_secs,
    )?;
    let trust = response
        .deployment_trust
        .verify(deployment_root_public_key, now)?;
    let endpoint = response.signed_endpoint_credential.verify(&trust, now)?;
    if endpoint.credential_id != response.approval.credential_id
        || endpoint.authority_id != response.approval.authority_id
        || endpoint.enrollment_request_id != response.approval.request.request_id
        || endpoint.home_id != response.approval.request.home_id
        || endpoint.management_spki_sha256 != spki_pin(&parsed.management.public_key)
        || endpoint.business_spki_sha256 != spki_pin(&parsed.business.public_key)
        || endpoint.not_before_unix_secs != response.approval.not_before_unix_secs
        || endpoint.not_after_unix_secs != response.approval.not_after_unix_secs
    {
        bail!("signed Home endpoint credential does not match the approved request");
    }
    validate_issuer_bundle_binding(
        &endpoint,
        response.approval.profile,
        response.issuer_bundle.as_ref(),
    )?;
    validate_home_certificate(
        &response.management_certificate_pem,
        &endpoint,
        &endpoint.management_spki_sha256,
        "management",
    )?;
    validate_home_certificate(
        &response.business_certificate_pem,
        &endpoint,
        &endpoint.business_spki_sha256,
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
    Ok((endpoint, trust))
}

/// Verifies and idempotently installs a Home enrollment response beside the original request.
///
/// # Errors
///
/// Returns an error for invalid response material, mismatched local keys or request state,
/// conflicting existing files, or a durable-write failure.
pub fn install_home_enrollment_response(
    enrollment_directory: &Path,
    response: &HomeEnrollmentResponse,
    deployment_root_public_key: &str,
    now: u64,
) -> Result<HomeEndpointCredential> {
    let request: HomeEnrollmentRequest =
        crate::load_json(&enrollment_directory.join(HOME_REQUEST_FILE))?;
    let state: HomeEnrollmentState = crate::load_json(&enrollment_directory.join(HOME_STATE_FILE))?;
    if state.version != HOME_ENROLLMENT_VERSION
        || state.request_id != request.request_id
        || state.nonce != request.nonce
        || state.home_id != request.home_id
        || response.approval.request != request
    {
        bail!("Home enrollment response does not match the local request");
    }
    let (endpoint, trust) =
        validate_home_enrollment_response(response, deployment_root_public_key, now)?;
    validate_local_key(
        &enrollment_directory.join(HOME_MANAGEMENT_KEY_FILE),
        &state.management_spki_sha256,
        "management",
    )?;
    validate_local_key(
        &enrollment_directory.join(HOME_BUSINESS_KEY_FILE),
        &state.business_spki_sha256,
        "business",
    )?;
    if endpoint.management_spki_sha256 != state.management_spki_sha256
        || endpoint.business_spki_sha256 != state.business_spki_sha256
    {
        bail!("issued Home endpoint does not match local private keys");
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
        &enrollment_directory.join(HOME_MANAGEMENT_CERT_FILE),
        response.management_certificate_pem.as_bytes(),
    )?;
    write_or_verify(
        &enrollment_directory.join(HOME_BUSINESS_CERT_FILE),
        response.business_certificate_pem.as_bytes(),
    )?;
    write_or_verify(
        &enrollment_directory.join(HOME_ENDPOINT_CREDENTIAL_FILE),
        &serde_json::to_vec_pretty(&response.signed_endpoint_credential)?,
    )?;
    write_or_verify(
        &enrollment_directory.join(HOME_RESPONSE_FILE),
        &serde_json::to_vec_pretty(response)?,
    )?;
    if let Some(bundle) = &response.issuer_bundle {
        let issuer_directory = enrollment_directory
            .parent()
            .ok_or_else(|| anyhow!("Home enrollment directory has no install parent"))?
            .join("issuer");
        fs::create_dir_all(&issuer_directory)?;
        #[cfg(unix)]
        fs::set_permissions(
            &issuer_directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )?;
        write_or_verify(
            &issuer_directory.join(HOME_ISSUER_MANAGEMENT_CA_KEY_FILE),
            bundle.management_ca_key_pem.as_bytes(),
        )?;
        write_or_verify(
            &issuer_directory.join(HOME_ISSUER_BUSINESS_CA_KEY_FILE),
            bundle.business_ca_key_pem.as_bytes(),
        )?;
        write_or_verify(
            &issuer_directory.join(HOME_ISSUER_HOME_AUTHORITY_KEY_FILE),
            bundle.home_authority_key_pem.as_bytes(),
        )?;
        if let Some(value) = &bundle.global_authority_key_pem {
            write_or_verify(
                &issuer_directory.join(HOME_ISSUER_GLOBAL_AUTHORITY_KEY_FILE),
                value.as_bytes(),
            )?;
        }
        if let Some(value) = &bundle.home_enrollment_authority_key_pem {
            write_or_verify(
                &issuer_directory.join(HOME_ISSUER_ENROLLMENT_AUTHORITY_KEY_FILE),
                value.as_bytes(),
            )?;
        }
    }
    Ok(endpoint)
}

fn create_home_csr(home_id: &str, key: &KeyPair, eku: ExtendedKeyUsagePurpose) -> Result<String> {
    let mut params = exact_home_leaf_params(home_id, 0, 4_102_444_800, eku)?;
    params.not_before = time::OffsetDateTime::UNIX_EPOCH;
    params.not_after = time::OffsetDateTime::UNIX_EPOCH;
    params
        .serialize_request(key)?
        .pem()
        .context("failed to encode Home CSR")
}

fn exact_home_leaf_params(
    home_id: &str,
    not_before: u64,
    not_after: u64,
    eku: ExtendedKeyUsagePurpose,
) -> Result<CertificateParams> {
    let mut params = CertificateParams::default();
    let mut name = DistinguishedName::new();
    name.push(DnType::CommonName, format!("FlowSplice Home {home_id}"));
    params.distinguished_name = name;
    params.subject_alt_names = vec![SanType::URI(Ia5String::try_from(format!(
        "flowsplice://identity/home/{home_id}"
    ))?)];
    params.is_ca = IsCa::NoCa;
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = home_extended_key_usages(eku);
    params.not_before = time::OffsetDateTime::from_unix_timestamp(i64::try_from(not_before)?)?;
    params.not_after = time::OffsetDateTime::from_unix_timestamp(i64::try_from(not_after)?)?;
    Ok(params)
}

fn validate_home_csr(
    csr: &CertificateSigningRequestParams,
    home_id: &str,
    eku: ExtendedKeyUsagePurpose,
    label: &str,
) -> Result<()> {
    if csr.public_key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
        bail!("Home {label} CSR must use ECDSA P-256/SHA-256");
    }
    let expected_uri = format!("flowsplice://identity/home/{home_id}");
    if csr.params.subject_alt_names.len() != 1
        || !csr
            .params
            .subject_alt_names
            .iter()
            .any(|name| matches!(name, SanType::URI(uri) if uri.as_str() == expected_uri))
        || csr.params.extended_key_usages != home_extended_key_usages(eku)
    {
        bail!("Home {label} CSR has an invalid identity or purpose");
    }
    Ok(())
}

fn home_extended_key_usages(eku: ExtendedKeyUsagePurpose) -> Vec<ExtendedKeyUsagePurpose> {
    if eku == ExtendedKeyUsagePurpose::ServerAuth {
        vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ]
    } else {
        vec![eku]
    }
}

fn validate_home_approval(approval: &HomeEnrollmentApproval, now: u64) -> Result<()> {
    if approval.version != HOME_ENROLLMENT_VERSION
        || approval.credential_id.is_nil()
        || approval.authority_id.is_empty()
    {
        bail!("unsupported or invalid Home enrollment approval");
    }
    let _ = parse_home_enrollment_request(&approval.request, now)?;
    if approval.not_before_unix_secs >= approval.not_after_unix_secs
        || approval
            .not_after_unix_secs
            .saturating_sub(approval.not_before_unix_secs)
            > u64::from(MAX_VALID_DAYS) * 86_400 + 300
    {
        bail!("Home enrollment approval has an invalid validity interval");
    }
    Ok(())
}

fn validate_home_certificate(
    pem: &str,
    endpoint: &HomeEndpointCredential,
    expected_spki: &str,
    label: &str,
) -> Result<()> {
    let certificate = certificate_from_pem(pem)?;
    let identity = peer_identity(Some(std::slice::from_ref(&certificate)))?;
    require_peer(&identity, Role::Home, Some(&endpoint.home_id), &[])?;
    if !identity.spki_sha256.eq_ignore_ascii_case(expected_spki)
        || identity.not_before_unix_secs != endpoint.not_before_unix_secs
        || identity.not_after_unix_secs != endpoint.not_after_unix_secs
    {
        bail!("issued Home {label} certificate does not match endpoint credential");
    }
    Ok(())
}

fn validate_local_key(path: &Path, expected_spki: &str, label: &str) -> Result<()> {
    let private_key = Zeroizing::new(key::load_private_key(path, None, true)?);
    let key = KeyPair::try_from(&*private_key)
        .with_context(|| format!("failed to parse local Home {label} key"))?;
    if !spki_pin(&key).eq_ignore_ascii_case(expected_spki) {
        bail!("local Home {label} key does not match enrollment state");
    }
    Ok(())
}

fn spki_pin(key: &impl PublicKeyData) -> String {
    hex::encode(digest::digest(&digest::SHA256, &key.subject_public_key_info()).as_ref())
}

fn validate_home_id(home_id: &str) -> Result<()> {
    if home_id.is_empty()
        || home_id.len() > 128
        || !home_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("Home id must contain only ASCII letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

fn require_encrypted_key_pem(value: &str, label: &str) -> Result<()> {
    if !value.contains("-----BEGIN ENCRYPTED PRIVATE KEY-----") || value.len() > 1024 * 1024 {
        bail!("{label} key must be a bounded encrypted PKCS#8 PEM object");
    }
    Ok(())
}

fn issuer_bundle_digest(bundle: &HomeIssuerBundle) -> Result<String> {
    Ok(hex::encode(
        digest::digest(&digest::SHA256, &serde_json::to_vec(bundle)?).as_ref(),
    ))
}

fn validate_issuer_bundle_binding(
    endpoint: &HomeEndpointCredential,
    profile: HomeEnrollmentProfile,
    bundle: Option<&HomeIssuerBundle>,
) -> Result<()> {
    let actual_digest = bundle.map(issuer_bundle_digest).transpose()?;
    if endpoint.issuer_bundle_sha256 != actual_digest {
        bail!("Home issuer bundle does not match its signed endpoint credential");
    }

    let home_authorities = endpoint
        .delegated_travel_authorities
        .iter()
        .filter(|authority| matches!(authority, TrustedTravelAuthority::Home { .. }))
        .collect::<Vec<_>>();
    let global_authorities = endpoint
        .delegated_travel_authorities
        .iter()
        .filter(|authority| matches!(authority, TrustedTravelAuthority::Global { .. }))
        .collect::<Vec<_>>();
    match profile {
        HomeEnrollmentProfile::ServingOnly => {
            if bundle.is_some() || !endpoint.delegated_travel_authorities.is_empty() {
                bail!("serving-only Home response contains issuer capability");
            }
        }
        HomeEnrollmentProfile::HomeIssuer | HomeEnrollmentProfile::GlobalIssuer => {
            let bundle = bundle
                .ok_or_else(|| anyhow!("issuer-capable Home response has no issuer bundle"))?;
            require_encrypted_key_pem(&bundle.management_ca_key_pem, "management CA")?;
            require_encrypted_key_pem(&bundle.business_ca_key_pem, "business CA")?;
            require_encrypted_key_pem(&bundle.home_authority_key_pem, "Home Travel authority")?;
            if home_authorities.len() != 1 || home_authorities[0].id() != bundle.home_authority_id {
                bail!("Home issuer bundle does not match its delegated Home authority");
            }

            if profile == HomeEnrollmentProfile::HomeIssuer {
                if !global_authorities.is_empty()
                    || bundle.global_authority_id.is_some()
                    || bundle.global_authority_key_pem.is_some()
                    || bundle.home_enrollment_authority_key_pem.is_some()
                {
                    bail!("Home-scoped issuer response contains Global issuer capability");
                }
            } else {
                let global_id = bundle
                    .global_authority_id
                    .as_deref()
                    .ok_or_else(|| anyhow!("Global issuer response has no Global authority id"))?;
                let global_key = bundle
                    .global_authority_key_pem
                    .as_deref()
                    .ok_or_else(|| anyhow!("Global issuer response has no Global authority key"))?;
                let enrollment_key = bundle
                    .home_enrollment_authority_key_pem
                    .as_deref()
                    .ok_or_else(|| anyhow!("Global issuer response has no Home enrollment key"))?;
                require_encrypted_key_pem(global_key, "Global Travel authority")?;
                require_encrypted_key_pem(enrollment_key, "Home enrollment authority")?;
                if global_authorities.len() != 1 || global_authorities[0].id() != global_id {
                    bail!("Global issuer bundle does not match its delegated Global authority");
                }
            }
        }
    }
    Ok(())
}

#[must_use]
pub fn home_enrollment_paths(directory: &Path) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    (
        directory.join(HOME_MANAGEMENT_KEY_FILE),
        directory.join(HOME_BUSINESS_KEY_FILE),
        directory.join(HOME_MANAGEMENT_CERT_FILE),
        directory.join(HOME_BUSINESS_CERT_FILE),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ENCRYPTED_KEY: &str =
        "-----BEGIN ENCRYPTED PRIVATE KEY-----\nAA==\n-----END ENCRYPTED PRIVATE KEY-----\n";

    #[test]
    fn issuer_bundle_is_bound_to_profile_and_endpoint_authorities() -> Result<()> {
        let bundle = HomeIssuerBundle {
            management_ca_key_pem: ENCRYPTED_KEY.to_owned(),
            business_ca_key_pem: ENCRYPTED_KEY.to_owned(),
            home_authority_id: "home-2-authority".to_owned(),
            home_authority_key_pem: ENCRYPTED_KEY.to_owned(),
            global_authority_id: Some("home-2-global".to_owned()),
            global_authority_key_pem: Some(ENCRYPTED_KEY.to_owned()),
            home_enrollment_authority_key_pem: Some(ENCRYPTED_KEY.to_owned()),
        };
        let endpoint = HomeEndpointCredential {
            version: HOME_ENDPOINT_CREDENTIAL_VERSION,
            object_type: HOME_ENDPOINT_CREDENTIAL_OBJECT_TYPE.to_owned(),
            deployment_id: "deployment-1".to_owned(),
            credential_id: Uuid::from_u128(1),
            authority_id: "home-enrollment".to_owned(),
            authority_epoch: 1,
            enrollment_request_id: Uuid::from_u128(2),
            home_id: "home-2".to_owned(),
            management_spki_sha256: "11".repeat(32),
            business_spki_sha256: "22".repeat(32),
            delegated_travel_authorities: vec![
                TrustedTravelAuthority::Home {
                    id: bundle.home_authority_id.clone(),
                    epoch: 1,
                    home_id: "home-2".to_owned(),
                    public_key: "04".to_owned() + &"33".repeat(64),
                },
                TrustedTravelAuthority::Global {
                    id: "home-2-global".to_owned(),
                    epoch: 1,
                    home_id: "home-2".to_owned(),
                    public_key: "04".to_owned() + &"44".repeat(64),
                },
            ],
            issuer_bundle_sha256: Some(issuer_bundle_digest(&bundle)?),
            not_before_unix_secs: 100,
            not_after_unix_secs: 200,
        };

        validate_issuer_bundle_binding(
            &endpoint,
            HomeEnrollmentProfile::GlobalIssuer,
            Some(&bundle),
        )?;
        assert!(
            validate_issuer_bundle_binding(
                &endpoint,
                HomeEnrollmentProfile::ServingOnly,
                Some(&bundle),
            )
            .is_err()
        );
        let mut tampered = bundle;
        if let Some(key) = &mut tampered.global_authority_key_pem {
            key.push('x');
        }
        assert!(
            validate_issuer_bundle_binding(
                &endpoint,
                HomeEnrollmentProfile::GlobalIssuer,
                Some(&tampered),
            )
            .is_err()
        );
        Ok(())
    }
}
