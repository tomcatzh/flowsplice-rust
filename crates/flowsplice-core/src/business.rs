//! Additive business authorization. Legacy endpoint and Travel credential bytes stay unchanged.

use std::collections::HashSet;

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    digest,
    rand::SystemRandom,
    signature::{ECDSA_P256_SHA256_ASN1, EcdsaKeyPair, UnparsedPublicKey},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::{
    authorization::{SignedTravelCredential, TravelCredential, TravelCredentialScope},
    deployment::{DeploymentTrust, MAX_CLOCK_SKEW_SECS, SignedHomeEndpointCredential},
    protocol::{Service, ServiceProtocol},
    tls::validate_spki_pin,
};

pub const BUSINESS_VERSION: u32 = 1;
pub const HOME_SERVICE_GRANT_TYPE: &str = "flowsplice.home_service_grant";
pub const BUSINESS_APPROVAL_TYPE: &str = "flowsplice.business_travel_approval";
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessService {
    pub service_id: String,
    pub protocol: ServiceProtocol,
    pub application_protocol: String,
    pub capabilities: Vec<String>,
}

impl BusinessService {
    /// Checks the bounded, application-independent service description.
    ///
    /// # Errors
    /// Returns an error for empty, duplicate or malformed fields.
    pub fn validate(&self) -> Result<()> {
        validate_token(&self.service_id, 128)?;
        validate_token(&self.application_protocol, 128)?;
        if self.capabilities.is_empty() || self.capabilities.len() > 16 {
            bail!("business service requires one to sixteen capabilities");
        }
        let mut capabilities = HashSet::new();
        for capability in &self.capabilities {
            validate_token(capability, 64)?;
            if !capabilities.insert(capability) {
                bail!("duplicate business capability");
            }
        }
        Ok(())
    }
}

/// Validates the complete requested or approved service set.
///
/// # Errors
/// Returns an error for an empty, excessive or ambiguous service set.
pub fn validate_services(services: &[BusinessService]) -> Result<()> {
    if services.is_empty() || services.len() > 256 {
        bail!("business Home requires one to 256 services");
    }
    let mut ids = HashSet::new();
    for service in services {
        service.validate()?;
        if !ids.insert(&service.service_id) {
            bail!("business service ids must be unique");
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeServiceGrant {
    pub version: u32,
    pub object_type: String,
    pub deployment_id: String,
    pub authority_id: String,
    pub authority_epoch: u64,
    pub home_id: String,
    pub endpoint_payload_sha256: String,
    pub enrollment_request_sha256: String,
    pub services: Vec<BusinessService>,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedHomeServiceGrant {
    pub authority_id: String,
    pub payload_hex: String,
    pub signature_hex: String,
}

impl HomeServiceGrant {
    fn validate(&self) -> Result<()> {
        if self.version != BUSINESS_VERSION || self.object_type != HOME_SERVICE_GRANT_TYPE {
            bail!("unsupported Home service grant");
        }
        validate_token(&self.home_id, 128)?;
        validate_token(&self.authority_id, 128)?;
        if self.deployment_id.is_empty() || self.authority_epoch == 0 {
            bail!("Home service grant has no deployment or authority epoch");
        }
        validate_spki_pin(
            &self.endpoint_payload_sha256,
            "business Home endpoint digest",
        )?;
        validate_spki_pin(
            &self.enrollment_request_sha256,
            "business Home request digest",
        )?;
        if self.not_before_unix_secs >= self.not_after_unix_secs {
            bail!("Home service grant validity is empty");
        }
        validate_services(&self.services)
    }

    /// Checks every advertised service against the signed allowlist.
    ///
    /// # Errors
    /// Returns an error for an unapproved id or transport, including duplicate advertisements.
    pub fn validate_catalog_services(&self, services: &[Service]) -> Result<()> {
        let mut seen = HashSet::new();
        for service in services {
            if !seen.insert(&service.id)
                || !self.services.iter().any(|approved| {
                    approved.service_id == service.id && approved.protocol == service.protocol
                })
            {
                bail!("Home advertises a service outside its signed business grant");
            }
        }
        Ok(())
    }
}

impl SignedHomeServiceGrant {
    /// Signs a service grant using the Home endpoint enrollment authority.
    ///
    /// # Errors
    /// Returns an error for an invalid payload or signing failure.
    pub fn sign(payload: &HomeServiceGrant, key: &EcdsaKeyPair) -> Result<Self> {
        payload.validate()?;
        let (payload_hex, signature_hex) = sign(payload, key)?;
        Ok(Self {
            authority_id: payload.authority_id.clone(),
            payload_hex,
            signature_hex,
        })
    }

    /// Verifies signatures and all identity/validity bindings without admitting current use.
    /// This permits loading historical state after one business Home has expired.
    ///
    /// # Errors
    /// Returns an error for tampering, unexpected issuer material, or mismatched bindings.
    pub fn verify_binding(
        &self,
        trust: &DeploymentTrust,
        endpoint: &SignedHomeEndpointCredential,
    ) -> Result<HomeServiceGrant> {
        let endpoint_payload = endpoint.verify_trust_binding(trust)?;
        let (payload, bytes): (HomeServiceGrant, _) = decode_payload(&self.payload_hex)?;
        payload.validate()?;
        if payload.deployment_id != trust.deployment_id
            || payload.home_id != endpoint_payload.home_id
            || payload.authority_id != endpoint_payload.authority_id
            || payload.authority_epoch != endpoint_payload.authority_epoch
            || self.authority_id != payload.authority_id
            || payload.endpoint_payload_sha256 != payload_digest(&endpoint.payload_hex)?
            || !endpoint_payload.delegated_travel_authorities.is_empty()
            || endpoint_payload.issuer_bundle_sha256.is_some()
            || payload.not_before_unix_secs < endpoint_payload.not_before_unix_secs
            || payload.not_after_unix_secs > endpoint_payload.not_after_unix_secs
        {
            bail!("Home service grant does not bind this serving-only endpoint");
        }
        let authority =
            trust.home_enrollment_authority(&payload.authority_id, payload.authority_epoch)?;
        verify_signature(&bytes, &self.signature_hex, &authority.public_key)?;
        Ok(payload)
    }

    /// Verifies a grant for active service use.
    ///
    /// # Errors
    /// Returns an error for an invalid, expired or not-yet-valid grant or endpoint.
    pub fn verify(
        &self,
        trust: &DeploymentTrust,
        endpoint: &SignedHomeEndpointCredential,
        now: u64,
    ) -> Result<HomeServiceGrant> {
        endpoint.verify(trust, now)?;
        let payload = self.verify_binding(trust, endpoint)?;
        if now.saturating_add(MAX_CLOCK_SKEW_SECS) < payload.not_before_unix_secs
            || now >= payload.not_after_unix_secs
        {
            bail!("Home service grant is not currently valid");
        }
        Ok(payload)
    }
}

/// Private installation data; deployment trust is verified independently by its consumer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessDescriptor {
    pub version: u32,
    pub approving_home_id: String,
    pub endpoint: SignedHomeEndpointCredential,
    pub grant: SignedHomeServiceGrant,
    pub service_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct VerifiedBusinessDescriptor {
    pub home_id: String,
    pub approving_home_id: String,
    pub service: BusinessService,
    pub not_after_unix_secs: u64,
}

impl BusinessDescriptor {
    /// Resolves exactly one signed service under root-verified deployment trust.
    ///
    /// # Errors
    /// Returns an error for invalid signatures, expiry, a changed approver or missing service.
    pub fn verify(&self, trust: &DeploymentTrust, now: u64) -> Result<VerifiedBusinessDescriptor> {
        if self.version != BUSINESS_VERSION {
            bail!("unsupported business descriptor");
        }
        let grant = self.grant.verify(trust, &self.endpoint, now)?;
        let authority =
            trust.home_enrollment_authority(&grant.authority_id, grant.authority_epoch)?;
        if self.approving_home_id != authority.issuer_home_id {
            bail!("business descriptor approval Home does not match its grant issuer");
        }
        let service = grant
            .services
            .into_iter()
            .find(|s| s.service_id == self.service_id)
            .ok_or_else(|| anyhow!("business descriptor requests an unapproved service"))?;
        Ok(VerifiedBusinessDescriptor {
            home_id: grant.home_id,
            approving_home_id: self.approving_home_id.clone(),
            service,
            not_after_unix_secs: grant.not_after_unix_secs,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessTravelApproval {
    pub version: u32,
    pub object_type: String,
    pub deployment_id: String,
    pub authority_id: String,
    pub authority_epoch: u64,
    pub request_id: Uuid,
    pub request_sha256: String,
    pub descriptor_sha256: String,
    pub credential_payload_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBusinessTravelApproval {
    pub authority_id: String,
    pub payload_hex: String,
    pub signature_hex: String,
}

impl BusinessTravelApproval {
    fn validate(&self) -> Result<()> {
        if self.version != BUSINESS_VERSION
            || self.object_type != BUSINESS_APPROVAL_TYPE
            || self.deployment_id.is_empty()
            || self.authority_id.is_empty()
            || self.authority_epoch == 0
            || self.request_id.is_nil()
        {
            bail!("unsupported or invalid business Travel approval");
        }
        for hash in [
            &self.request_sha256,
            &self.descriptor_sha256,
            &self.credential_payload_sha256,
        ] {
            validate_spki_pin(hash, "business approval binding")?;
        }
        Ok(())
    }
}

impl SignedBusinessTravelApproval {
    /// Signs the complete directed request and unchanged Travel credential binding.
    ///
    /// # Errors
    /// Returns an error for an invalid payload or signing failure.
    pub fn sign(payload: &BusinessTravelApproval, key: &EcdsaKeyPair) -> Result<Self> {
        payload.validate()?;
        let (payload_hex, signature_hex) = sign(payload, key)?;
        Ok(Self {
            authority_id: payload.authority_id.clone(),
            payload_hex,
            signature_hex,
        })
    }

    /// Validates exact business intent, issuer and credential without widening legacy scope.
    ///
    /// # Errors
    /// Returns an error for any changed request, descriptor, credential or authorization scope.
    pub fn verify(
        &self,
        trust: &DeploymentTrust,
        issuer_endpoint: Option<&SignedHomeEndpointCredential>,
        descriptor: &BusinessDescriptor,
        request_sha256: &str,
        credential: &SignedTravelCredential,
        now: u64,
    ) -> Result<TravelCredential> {
        let target = descriptor.verify(trust, now)?;
        let (payload, bytes): (BusinessTravelApproval, _) = decode_payload(&self.payload_hex)?;
        payload.validate()?;
        let authorities = trust.travel_authorities_with_home_delegations(
            &issuer_endpoint.cloned().into_iter().collect::<Vec<_>>(),
            now,
        )?;
        let authority = authorities
            .iter()
            .find(|a| a.id() == payload.authority_id)
            .ok_or_else(|| anyhow!("business approval has an untrusted Travel authority"))?;
        let travel = credential.verify(authority)?;
        if let Some(issuer) = issuer_endpoint {
            let issuer = issuer.verify(trust, now)?;
            if issuer.home_id != target.approving_home_id
                || travel.not_after_unix_secs > issuer.not_after_unix_secs
            {
                bail!("business approval exceeds its issuing Home authorization");
            }
        }
        if self.authority_id != payload.authority_id
            || payload.authority_id != travel.authority_id
            || payload.authority_epoch != authority.epoch()
            || payload.deployment_id != trust.deployment_id
            || travel.deployment_id != trust.deployment_id
            || authority.home_id() != Some(target.approving_home_id.as_str())
            || payload.request_id != travel.enrollment_request_id
            || payload.request_sha256 != request_sha256
            || payload.descriptor_sha256 != json_digest(descriptor)?
            || payload.credential_payload_sha256 != payload_digest(&credential.payload_hex)?
            || !travel.active_at(now)
            || travel.not_before_unix_secs < trust.not_before_unix_secs
            || travel.not_after_unix_secs > target.not_after_unix_secs
            || travel.scope
                != (TravelCredentialScope::Service {
                    home_id: target.home_id,
                    service_id: target.service.service_id,
                    protocol: target.service.protocol,
                })
        {
            bail!("business Travel approval does not match the exact requested service");
        }
        verify_signature(&bytes, &self.signature_hex, authority.public_key())?;
        Ok(travel)
    }
}

/// Digests the deterministic typed JSON representation used by business wrappers.
///
/// # Errors
/// Returns an error when serialization fails or exceeds the business payload bound.
pub fn json_digest<T: Serialize>(value: &T) -> Result<String> {
    let bytes = encode_payload(value)?;
    Ok(sha256(&bytes))
}

/// Digests exact signed payload bytes, independently of signature randomness.
///
/// # Errors
/// Returns an error for invalid or excessive hexadecimal input.
pub fn payload_digest(payload_hex: &str) -> Result<String> {
    Ok(sha256(&decode_bytes(payload_hex)?))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(digest::digest(&digest::SHA256, bytes).as_ref())
}

fn validate_token(value: &str, limit: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > limit
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-:/".contains(&b))
    {
        bail!("business identifiers must be bounded ASCII tokens");
    }
    Ok(())
}

fn encode_payload<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value)?;
    if bytes.len() > MAX_PAYLOAD_BYTES {
        bail!("business payload exceeds size limit");
    }
    Ok(bytes)
}

fn decode_bytes(value: &str) -> Result<Vec<u8>> {
    if value.len() > MAX_PAYLOAD_BYTES * 2 {
        bail!("business payload exceeds size limit");
    }
    hex::decode(value).context("business payload must be hexadecimal")
}

fn decode_payload<T: DeserializeOwned>(value: &str) -> Result<(T, Vec<u8>)> {
    let bytes = decode_bytes(value)?;
    let payload = serde_json::from_slice(&bytes).context("invalid business payload")?;
    Ok((payload, bytes))
}

fn sign<T: Serialize>(value: &T, key: &EcdsaKeyPair) -> Result<(String, String)> {
    let bytes = encode_payload(value)?;
    let signature = key
        .sign(&SystemRandom::new(), &bytes)
        .map_err(|_| anyhow!("business authorization signing failed"))?;
    Ok((hex::encode(bytes), hex::encode(signature.as_ref())))
}

fn verify_signature(bytes: &[u8], signature_hex: &str, public_key: &str) -> Result<()> {
    if signature_hex.len() > 160 || public_key.len() != 130 {
        bail!("invalid business signature or P-256 public key size");
    }
    let key = hex::decode(public_key).context("invalid business authority key")?;
    let signature = hex::decode(signature_hex).context("invalid business signature")?;
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, key)
        .verify(bytes, &signature)
        .map_err(|_| anyhow!("invalid business authorization signature"))
}
