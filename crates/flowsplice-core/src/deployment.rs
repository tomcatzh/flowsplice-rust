use std::{collections::HashSet, fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    digest,
    rand::SystemRandom,
    signature::{
        ECDSA_P256_SHA256_ASN1, ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair,
        UnparsedPublicKey,
    },
};
use rustls_pki_types::{CertificateDer, pem::PemObject};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    authorization::{TrustedTravelAuthority, validate_trusted_authorities},
    protocol::{Catalog, RelayDirectory, ServiceProtocol},
    tls::validate_spki_pin,
};

pub const DEPLOYMENT_TRUST_VERSION: u32 = 1;
pub const CONTROL_SNAPSHOT_VERSION: u32 = 1;
pub const CONTROL_SNAPSHOT_OBJECT_TYPE: &str = "flowsplice.control_snapshot";
pub const HOME_ENDPOINT_CREDENTIAL_VERSION: u32 = 1;
pub const HOME_ENDPOINT_CREDENTIAL_OBJECT_TYPE: &str = "flowsplice.home_endpoint_credential";
pub const MAX_CLOCK_SKEW_SECS: u64 = 300;
pub const MAX_CONTROL_SNAPSHOT_TTL_SECS: u64 = 300;
const MAX_RELAY_ENDPOINTS: usize = 64;
const MAX_CATALOG_HOMES: usize = 64;
const MAX_SERVICES_PER_HOME: usize = 256;
const MAX_ID_BYTES: usize = 128;
const MAX_DISPLAY_OR_TARGET_BYTES: usize = 512;
const MAX_CONTROL_SNAPSHOT_PAYLOAD_BYTES: usize = 400 * 1_024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServerControlKey {
    pub server_id: String,
    pub epoch: u64,
    pub public_key: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeEndpointTrust {
    pub home_id: String,
    pub management_spki_pins: Vec<String>,
    pub business_spki_pins: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TrustedHomeEnrollmentAuthority {
    pub id: String,
    pub epoch: u64,
    pub issuer_home_id: String,
    pub public_key: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HomeEndpointCredential {
    pub version: u32,
    pub object_type: String,
    pub deployment_id: String,
    pub credential_id: Uuid,
    pub authority_id: String,
    pub authority_epoch: u64,
    pub enrollment_request_id: Uuid,
    pub home_id: String,
    pub management_spki_sha256: String,
    pub business_spki_sha256: String,
    #[serde(default)]
    pub delegated_travel_authorities: Vec<TrustedTravelAuthority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer_bundle_sha256: Option<String>,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedHomeEndpointCredential {
    pub authority_id: String,
    pub payload_hex: String,
    pub signature_hex: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentTrust {
    pub version: u32,
    pub deployment_id: String,
    pub generation: u64,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
    pub management_ca_certificate_pem: String,
    pub business_ca_certificate_pem: String,
    pub server_control_keys: Vec<ServerControlKey>,
    pub home_endpoints: Vec<HomeEndpointTrust>,
    #[serde(default)]
    pub home_enrollment_authorities: Vec<TrustedHomeEnrollmentAuthority>,
    pub travel_authorities: Vec<TrustedTravelAuthority>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedDeploymentTrust {
    pub payload_hex: String,
    pub signature_hex: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlSnapshotPayload {
    pub version: u32,
    pub object_type: String,
    pub deployment_id: String,
    pub server_id: String,
    pub signer_epoch: u64,
    pub travel_id: String,
    pub travel_management_spki_sha256: String,
    pub generation: u64,
    pub issued_at_unix_secs: u64,
    pub expires_at_unix_secs: u64,
    pub relay_directory: RelayDirectory,
    pub catalog: Catalog,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedControlSnapshot {
    pub trust: SignedDeploymentTrust,
    pub payload_hex: String,
    pub signature_hex: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedControlSnapshot {
    pub trust: DeploymentTrust,
    pub trust_digest_sha256: String,
    pub payload: ControlSnapshotPayload,
    pub digest_sha256: String,
}

impl SignedDeploymentTrust {
    /// Returns the SHA-256 digest of the exact signed trust payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the encoded payload is not hexadecimal.
    pub fn payload_digest_sha256(&self) -> Result<String> {
        let payload = hex::decode(&self.payload_hex)
            .context("deployment trust payload must be hexadecimal")?;
        Ok(hex::encode(
            digest::digest(&digest::SHA256, &payload).as_ref(),
        ))
    }

    /// Signs one exact deployment-trust payload with the offline deployment root.
    ///
    /// # Errors
    ///
    /// Returns an error when validation, serialization, or signing fails.
    pub fn sign(payload: &DeploymentTrust, root_key: &EcdsaKeyPair) -> Result<Self> {
        payload.validate_shape()?;
        let bytes = serde_json::to_vec(payload).context("failed to encode deployment trust")?;
        let signature = root_key
            .sign(&SystemRandom::new(), &bytes)
            .map_err(|_| anyhow!("failed to sign deployment trust"))?;
        Ok(Self {
            payload_hex: hex::encode(bytes),
            signature_hex: hex::encode(signature.as_ref()),
        })
    }

    /// Verifies the deployment trust against the one preinstalled deployment root.
    ///
    /// # Errors
    ///
    /// Returns an error for a malformed, untrusted, not-yet-valid, or expired document.
    pub fn verify(&self, root_public_key: &str, now: u64) -> Result<DeploymentTrust> {
        let root = decode_p256_public_key(root_public_key, "deployment root")?;
        let payload = hex::decode(&self.payload_hex)
            .context("deployment trust payload must be hexadecimal")?;
        let signature = hex::decode(&self.signature_hex)
            .context("deployment trust signature must be hexadecimal")?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, root)
            .verify(&payload, &signature)
            .map_err(|_| anyhow!("deployment trust has an invalid root signature"))?;
        let trust: DeploymentTrust =
            serde_json::from_slice(&payload).context("deployment trust payload is invalid")?;
        trust.validate_at(now)?;
        Ok(trust)
    }
}

/// Loads a deployment root and signed deployment-trust document from separate configuration
/// files, then verifies the trust document before returning it.
///
/// # Errors
///
/// Returns an error when either file cannot be read, the signed document is malformed, or its
/// signature, shape, or validity interval is unacceptable.
pub fn load_verified_deployment_trust(
    deployment_root_public_key_path: &Path,
    deployment_trust_path: &Path,
    now: u64,
) -> Result<(String, SignedDeploymentTrust, DeploymentTrust)> {
    let deployment_root_public_key = fs::read_to_string(deployment_root_public_key_path)
        .with_context(|| {
            format!(
                "failed to read deployment root public key {}",
                deployment_root_public_key_path.display()
            )
        })?;
    let deployment_root_public_key = deployment_root_public_key.trim().to_owned();
    let bytes = fs::read(deployment_trust_path).with_context(|| {
        format!(
            "failed to read deployment trust {}",
            deployment_trust_path.display()
        )
    })?;
    let signed: SignedDeploymentTrust = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to parse deployment trust {}",
            deployment_trust_path.display()
        )
    })?;
    let trust = signed.verify(&deployment_root_public_key, now)?;
    Ok((deployment_root_public_key, signed, trust))
}

impl SignedControlSnapshot {
    /// Signs an atomic Relay-directory and Catalog snapshot with a certified Server key.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload, trust/key binding, serialization, or signing fails.
    pub fn sign(
        trust: SignedDeploymentTrust,
        verified_trust: &DeploymentTrust,
        payload: &ControlSnapshotPayload,
        server_key: &EcdsaKeyPair,
    ) -> Result<Self> {
        payload.validate_at(payload.issued_at_unix_secs)?;
        if payload.deployment_id != verified_trust.deployment_id {
            bail!("control snapshot deployment does not match deployment trust");
        }
        let expected_key =
            verified_trust.server_control_key(&payload.server_id, payload.signer_epoch)?;
        let actual_key = hex::encode(server_key.public_key().as_ref());
        if !actual_key.eq_ignore_ascii_case(expected_key) {
            bail!("Server control private key is not certified by deployment trust");
        }
        let bytes = serde_json::to_vec(payload).context("failed to encode control snapshot")?;
        if bytes.len() > MAX_CONTROL_SNAPSHOT_PAYLOAD_BYTES {
            bail!("control snapshot payload exceeds the signed-frame budget");
        }
        let signature = server_key
            .sign(&SystemRandom::new(), &bytes)
            .map_err(|_| anyhow!("failed to sign control snapshot"))?;
        Ok(Self {
            trust,
            payload_hex: hex::encode(bytes),
            signature_hex: hex::encode(signature.as_ref()),
        })
    }

    /// Verifies both the root-signed trust and Server-signed atomic control snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error for an untrusted signer, tampering, invalid contents, or stale validity.
    pub fn verify(&self, root_public_key: &str, now: u64) -> Result<VerifiedControlSnapshot> {
        let trust = self.trust.verify(root_public_key, now)?;
        let payload_bytes = hex::decode(&self.payload_hex)
            .context("control snapshot payload must be hexadecimal")?;
        if payload_bytes.len() > MAX_CONTROL_SNAPSHOT_PAYLOAD_BYTES {
            bail!("control snapshot payload exceeds the signed-frame budget");
        }
        let payload: ControlSnapshotPayload = serde_json::from_slice(&payload_bytes)
            .context("control snapshot payload is invalid")?;
        payload.validate_at(now)?;
        if payload.deployment_id != trust.deployment_id {
            bail!("control snapshot deployment does not match deployment trust");
        }
        let public_key = decode_p256_public_key(
            trust.server_control_key(&payload.server_id, payload.signer_epoch)?,
            "Server control",
        )?;
        let signature = hex::decode(&self.signature_hex)
            .context("control snapshot signature must be hexadecimal")?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, public_key)
            .verify(&payload_bytes, &signature)
            .map_err(|_| anyhow!("control snapshot has an invalid Server signature"))?;
        Ok(VerifiedControlSnapshot {
            trust,
            trust_digest_sha256: self.trust.payload_digest_sha256()?,
            payload,
            digest_sha256: hex::encode(digest::digest(&digest::SHA256, &payload_bytes).as_ref()),
        })
    }

    /// Verifies that this snapshot was cryptographically valid when it was issued.
    ///
    /// This is intentionally restricted to one-time persistence migration. A value returned by
    /// this method must never be used as current control authorization because its validity window
    /// may already have expired.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed contents or when the snapshot was not valid at its own
    /// issuance time.
    pub fn verify_at_issuance_for_migration(
        &self,
        root_public_key: &str,
    ) -> Result<VerifiedControlSnapshot> {
        let payload_bytes = hex::decode(&self.payload_hex)
            .context("control snapshot payload must be hexadecimal")?;
        if payload_bytes.len() > MAX_CONTROL_SNAPSHOT_PAYLOAD_BYTES {
            bail!("control snapshot payload exceeds the signed-frame budget");
        }
        let payload: ControlSnapshotPayload = serde_json::from_slice(&payload_bytes)
            .context("control snapshot payload is invalid")?;
        self.verify(root_public_key, payload.issued_at_unix_secs)
    }
}

impl DeploymentTrust {
    /// Returns the certified control key for one Server identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the Server is not certified by this deployment trust.
    pub fn server_control_key(&self, server_id: &str, epoch: u64) -> Result<&str> {
        self.server_control_keys
            .iter()
            .find(|key| key.server_id == server_id && key.epoch == epoch)
            .map(|key| key.public_key.as_str())
            .ok_or_else(|| {
                anyhow!("Server {server_id} has no certified control signing key at epoch {epoch}")
            })
    }

    /// Returns one certified Travel authority by id.
    ///
    /// # Errors
    ///
    /// Returns an error when the authority is not certified by this deployment trust.
    pub fn travel_authority(
        &self,
        authority_id: &str,
        authority_epoch: u64,
    ) -> Result<&TrustedTravelAuthority> {
        self.travel_authorities
            .iter()
            .find(|authority| {
                authority.id() == authority_id && authority.epoch() == authority_epoch
            })
            .ok_or_else(|| anyhow!("Travel authority {authority_id} is not deployment-trusted"))
    }

    /// Returns the uniquely named Travel authority certified by this deployment trust.
    ///
    /// # Errors
    ///
    /// Returns an error when the authority id is not certified.
    pub fn travel_authority_by_id(&self, authority_id: &str) -> Result<&TrustedTravelAuthority> {
        self.travel_authorities
            .iter()
            .find(|authority| authority.id() == authority_id)
            .ok_or_else(|| anyhow!("Travel authority {authority_id} is not deployment-trusted"))
    }

    /// Returns one root-bound Home endpoint identity.
    ///
    /// # Errors
    ///
    /// Returns an error when the Home is absent from deployment trust.
    pub fn home_endpoint(&self, home_id: &str) -> Result<&HomeEndpointTrust> {
        self.home_endpoints
            .iter()
            .find(|home| home.home_id == home_id)
            .ok_or_else(|| anyhow!("Home {home_id} endpoint is not deployment-trusted"))
    }

    /// Returns the root-certified authority allowed to approve new Home endpoint identities.
    ///
    /// # Errors
    ///
    /// Returns an error when the authority id/epoch pair is not deployment-trusted.
    pub fn home_enrollment_authority(
        &self,
        authority_id: &str,
        authority_epoch: u64,
    ) -> Result<&TrustedHomeEnrollmentAuthority> {
        self.home_enrollment_authorities
            .iter()
            .find(|authority| {
                authority.id == authority_id && authority.epoch == authority_epoch
            })
            .ok_or_else(|| {
                anyhow!(
                    "Home enrollment authority {authority_id} epoch {authority_epoch} is not deployment-trusted"
                )
            })
    }

    /// Resolves one Home's management and business pins. A valid dynamically signed endpoint
    /// credential takes precedence over the static baseline, enabling secure first installation
    /// and rotation without giving Server signing authority.
    ///
    /// # Errors
    ///
    /// Returns an error when neither a valid dynamic credential nor a static endpoint exists.
    pub fn resolve_home_endpoint(
        &self,
        home_id: &str,
        credential: Option<&SignedHomeEndpointCredential>,
        now: u64,
    ) -> Result<HomeEndpointTrust> {
        if let Some(credential) = credential {
            let verified = credential.verify(self, now)?;
            if verified.home_id != home_id {
                bail!("Home endpoint credential belongs to a different Home");
            }
            return Ok(HomeEndpointTrust {
                home_id: verified.home_id,
                management_spki_pins: vec![verified.management_spki_sha256],
                business_spki_pins: vec![verified.business_spki_sha256],
            });
        }
        Ok(self.home_endpoint(home_id)?.clone())
    }

    /// Combines root-listed Travel authorities with authority keys delegated inside verified Home
    /// endpoint credentials. Authority ids remain globally unique across both sets.
    ///
    /// # Errors
    ///
    /// Returns an error for a bad endpoint signature, an invalid delegation, or a duplicate id.
    pub fn travel_authorities_with_home_delegations(
        &self,
        credentials: &[SignedHomeEndpointCredential],
        _now: u64,
    ) -> Result<Vec<TrustedTravelAuthority>> {
        let mut authorities = self.travel_authorities.clone();
        let mut ids = authorities
            .iter()
            .map(|authority| authority.id().to_owned())
            .collect::<HashSet<_>>();
        for signed in credentials {
            // Delegated authority keys remain necessary to verify historical Travel credentials
            // after the parent Home endpoint expires. Current Home access still uses `verify`,
            // and dynamically issued Travel credentials are bounded by the endpoint expiry.
            let endpoint = signed.verify_trust_binding(self)?;
            for authority in endpoint.delegated_travel_authorities {
                if !ids.insert(authority.id().to_owned()) {
                    bail!(
                        "duplicate static or delegated Travel authority {}",
                        authority.id()
                    );
                }
                authorities.push(authority);
            }
        }
        validate_trusted_authorities(&authorities)?;
        Ok(authorities)
    }

    fn validate_shape(&self) -> Result<()> {
        if self.version != DEPLOYMENT_TRUST_VERSION
            || self.deployment_id.is_empty()
            || self.generation == 0
        {
            bail!("unsupported or invalid deployment trust");
        }
        if self.not_before_unix_secs >= self.not_after_unix_secs {
            bail!("deployment trust validity interval is empty");
        }
        validate_single_certificate(&self.management_ca_certificate_pem, "management CA")?;
        validate_single_certificate(&self.business_ca_certificate_pem, "business CA")?;
        if self.server_control_keys.is_empty() {
            bail!("deployment trust must certify at least one Server control key");
        }
        let mut server_epochs = HashSet::new();
        for key in &self.server_control_keys {
            if key.server_id.is_empty()
                || key.epoch == 0
                || !server_epochs.insert((key.server_id.as_str(), key.epoch))
            {
                bail!("Server control key id/epoch pairs must be non-empty and unique");
            }
            decode_p256_public_key(&key.public_key, "Server control")?;
        }
        if self.home_endpoints.is_empty() {
            bail!("deployment trust must bind at least one Home endpoint");
        }
        let mut home_ids = HashSet::new();
        for home in &self.home_endpoints {
            if home.home_id.is_empty() || !home_ids.insert(home.home_id.as_str()) {
                bail!("deployment-trusted Home endpoint ids must be non-empty and unique");
            }
            if home.management_spki_pins.is_empty() || home.business_spki_pins.is_empty() {
                bail!("deployment-trusted Home endpoint pins must be non-empty");
            }
            for pin in &home.management_spki_pins {
                validate_spki_pin(pin, "Home management")?;
            }
            for pin in &home.business_spki_pins {
                validate_spki_pin(pin, "Home business")?;
            }
        }
        let mut home_authority_epochs = HashSet::new();
        for authority in &self.home_enrollment_authorities {
            if authority.id.is_empty()
                || authority.epoch == 0
                || authority.issuer_home_id.is_empty()
                || !home_ids.contains(authority.issuer_home_id.as_str())
                || !home_authority_epochs.insert((authority.id.as_str(), authority.epoch))
            {
                bail!(
                    "Home enrollment authority id/epoch must be unique and its issuer Home must be statically trusted"
                );
            }
            decode_p256_public_key(&authority.public_key, "Home enrollment authority")?;
        }
        validate_trusted_authorities(&self.travel_authorities)?;
        for authority in &self.travel_authorities {
            if authority
                .home_id()
                .is_none_or(|home_id| !home_ids.contains(home_id))
            {
                bail!("Travel authority owner is absent from deployment-trusted Home endpoints");
            }
        }
        Ok(())
    }

    fn validate_at(&self, now: u64) -> Result<()> {
        self.validate_shape()?;
        if now.saturating_add(MAX_CLOCK_SKEW_SECS) < self.not_before_unix_secs {
            bail!("deployment trust is not yet valid");
        }
        if now >= self.not_after_unix_secs {
            bail!("deployment trust is expired");
        }
        Ok(())
    }
}

impl SignedHomeEndpointCredential {
    /// Signs a bounded Home endpoint credential with a root-certified enrollment authority.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed payloads or a signing failure.
    pub fn sign(payload: &HomeEndpointCredential, key: &EcdsaKeyPair) -> Result<Self> {
        payload.validate_shape()?;
        let bytes =
            serde_json::to_vec(payload).context("failed to encode Home endpoint credential")?;
        let signature = key
            .sign(&SystemRandom::new(), &bytes)
            .map_err(|_| anyhow!("failed to sign Home endpoint credential"))?;
        Ok(Self {
            authority_id: payload.authority_id.clone(),
            payload_hex: hex::encode(bytes),
            signature_hex: hex::encode(signature.as_ref()),
        })
    }

    /// Verifies a Home endpoint credential against deployment-root-bound authority material.
    ///
    /// # Errors
    ///
    /// Returns an error for tampering, an untrusted authority, identity mismatch, or expiry.
    pub fn verify(&self, trust: &DeploymentTrust, now: u64) -> Result<HomeEndpointCredential> {
        let payload = self.verify_trust_binding(trust)?;
        payload.validate_time(now)?;
        Ok(payload)
    }

    fn verify_trust_binding(&self, trust: &DeploymentTrust) -> Result<HomeEndpointCredential> {
        let payload_bytes = hex::decode(&self.payload_hex)
            .context("Home endpoint credential payload must be hexadecimal")?;
        if payload_bytes.len() > 16 * 1_024 {
            bail!("Home endpoint credential exceeds the signed payload limit");
        }
        let payload: HomeEndpointCredential = serde_json::from_slice(&payload_bytes)
            .context("Home endpoint credential payload is invalid")?;
        payload.validate_trust_binding(trust)?;
        if self.authority_id != payload.authority_id {
            bail!("Home endpoint credential authority id is inconsistent");
        }
        let authority =
            trust.home_enrollment_authority(&payload.authority_id, payload.authority_epoch)?;
        let public_key =
            decode_p256_public_key(&authority.public_key, "Home enrollment authority")?;
        let signature = hex::decode(&self.signature_hex)
            .context("Home endpoint credential signature must be hexadecimal")?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, public_key)
            .verify(&payload_bytes, &signature)
            .map_err(|_| anyhow!("Home endpoint credential has an invalid signature"))?;
        Ok(payload)
    }
}

impl HomeEndpointCredential {
    fn validate_shape(&self) -> Result<()> {
        if self.version != HOME_ENDPOINT_CREDENTIAL_VERSION
            || self.object_type != HOME_ENDPOINT_CREDENTIAL_OBJECT_TYPE
            || self.deployment_id.is_empty()
            || self.credential_id.is_nil()
            || self.authority_id.is_empty()
            || self.authority_epoch == 0
            || self.enrollment_request_id.is_nil()
            || self.home_id.is_empty()
            || self.home_id.len() > MAX_ID_BYTES
            || self.not_before_unix_secs >= self.not_after_unix_secs
        {
            bail!("unsupported or invalid Home endpoint credential");
        }
        validate_spki_pin(&self.management_spki_sha256, "Home management")?;
        validate_spki_pin(&self.business_spki_sha256, "Home business")?;
        if self.delegated_travel_authorities.len() > 2 {
            bail!("Home endpoint credential delegates too many Travel authorities");
        }
        if !self.delegated_travel_authorities.is_empty() {
            validate_trusted_authorities(&self.delegated_travel_authorities)?;
            for authority in &self.delegated_travel_authorities {
                if authority.home_id() != Some(self.home_id.as_str()) {
                    bail!("delegated Travel authority belongs to a different Home");
                }
            }
        }
        match &self.issuer_bundle_sha256 {
            Some(digest) => validate_spki_pin(digest, "Home issuer bundle")?,
            None if !self.delegated_travel_authorities.is_empty() => {
                bail!("delegated Travel authorities require a signed issuer-bundle digest");
            }
            None => {}
        }
        Ok(())
    }

    fn validate_trust_binding(&self, trust: &DeploymentTrust) -> Result<()> {
        self.validate_shape()?;
        if self.deployment_id != trust.deployment_id {
            bail!("Home endpoint credential belongs to a different deployment");
        }
        if self.not_before_unix_secs < trust.not_before_unix_secs
            || self.not_after_unix_secs > trust.not_after_unix_secs
        {
            bail!("Home endpoint credential is outside deployment trust validity");
        }
        Ok(())
    }

    fn validate_time(&self, now: u64) -> Result<()> {
        if now.saturating_add(MAX_CLOCK_SKEW_SECS) < self.not_before_unix_secs {
            bail!("Home endpoint credential is not yet valid");
        }
        if now >= self.not_after_unix_secs {
            bail!("Home endpoint credential is expired");
        }
        Ok(())
    }
}

impl ControlSnapshotPayload {
    fn validate_at(&self, now: u64) -> Result<()> {
        if self.version != CONTROL_SNAPSHOT_VERSION
            || self.object_type != CONTROL_SNAPSHOT_OBJECT_TYPE
            || self.deployment_id.is_empty()
            || self.server_id.is_empty()
            || self.signer_epoch == 0
            || self.travel_id.is_empty()
            || self.generation == 0
        {
            bail!("unsupported or invalid control snapshot");
        }
        validate_spki_pin(&self.travel_management_spki_sha256, "Travel management")?;
        if self.issued_at_unix_secs >= self.expires_at_unix_secs {
            bail!("control snapshot validity interval is empty");
        }
        if self
            .expires_at_unix_secs
            .saturating_sub(self.issued_at_unix_secs)
            > MAX_CONTROL_SNAPSHOT_TTL_SECS
        {
            bail!("control snapshot validity exceeds the maximum TTL");
        }
        if self.issued_at_unix_secs > now.saturating_add(MAX_CLOCK_SKEW_SECS) {
            bail!("control snapshot is from the future");
        }
        if now >= self.expires_at_unix_secs {
            bail!("control snapshot is expired");
        }
        validate_relay_directory(&self.relay_directory)?;
        validate_catalog(&self.catalog)?;
        Ok(())
    }
}

fn decode_p256_public_key(value: &str, label: &str) -> Result<Vec<u8>> {
    let public_key =
        hex::decode(value).with_context(|| format!("{label} key must be hexadecimal"))?;
    if public_key.len() != 65 || public_key.first() != Some(&4) {
        bail!("{label} key must be an uncompressed P-256 point");
    }
    Ok(public_key)
}

fn validate_single_certificate(pem: &str, label: &str) -> Result<()> {
    let certificates = CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .with_context(|| format!("failed to parse deployment-trusted {label}"))?;
    if certificates.len() != 1 {
        bail!("deployment-trusted {label} must contain exactly one certificate");
    }
    Ok(())
}

/// Validates the bounded Relay-directory shape accepted into signed control state.
///
/// # Errors
///
/// Returns an error for empty, duplicate, malformed, or excessive entries.
pub fn validate_relay_directory(directory: &RelayDirectory) -> Result<()> {
    if directory.generation == 0
        || directory.relays.is_empty()
        || directory.relays.len() > MAX_RELAY_ENDPOINTS
    {
        bail!("signed Relay directory must be non-empty with a positive generation");
    }
    let mut ids = HashSet::new();
    let mut addresses = HashSet::new();
    for relay in &directory.relays {
        if relay.id.is_empty()
            || relay.id.len() > MAX_ID_BYTES
            || relay.management_addr.is_empty()
            || relay.management_addr.len() > MAX_DISPLAY_OR_TARGET_BYTES
            || relay.data_public_addr.is_empty()
            || relay.data_public_addr.len() > MAX_DISPLAY_OR_TARGET_BYTES
            || !ids.insert(&relay.id)
            || !addresses.insert(&relay.management_addr)
        {
            bail!("signed Relay directory contains empty or duplicate endpoints");
        }
        validate_spki_pin(&relay.management_spki_sha256, "Relay management")?;
    }
    Ok(())
}

/// Validates the bounded Catalog shape accepted into signed control state.
///
/// # Errors
///
/// Returns an error for malformed, duplicate, or excessive entries.
pub fn validate_catalog(catalog: &Catalog) -> Result<()> {
    if catalog.homes.len() > MAX_CATALOG_HOMES {
        bail!("signed Catalog contains too many Homes");
    }
    let mut home_ids = HashSet::new();
    for home in &catalog.homes {
        if home.home_id.is_empty()
            || home.home_id.len() > MAX_ID_BYTES
            || home.home_alias.is_empty()
            || home.home_alias.len() > MAX_DISPLAY_OR_TARGET_BYTES
            || home.services.len() > MAX_SERVICES_PER_HOME
            || !home_ids.insert(&home.home_id)
        {
            bail!("signed Catalog contains empty or duplicate Home ids");
        }
        let mut services = HashSet::new();
        for service in &home.services {
            if service.id.is_empty()
                || service.id.len() > MAX_ID_BYTES
                || service.alias.is_empty()
                || service.alias.len() > MAX_DISPLAY_OR_TARGET_BYTES
                || service.target.is_empty()
                || service.target.len() > MAX_DISPLAY_OR_TARGET_BYTES
                || !services.insert((service.id.as_str(), service.protocol))
            {
                bail!("signed Catalog contains an invalid or duplicate logical service");
            }
            match service.protocol {
                ServiceProtocol::Tcp | ServiceProtocol::Udp => {}
            }
        }
    }
    Ok(())
}

/// Parses an unencrypted PKCS#8 P-256 key for daemon-side control signing.
///
/// # Errors
///
/// Returns an error when the key is not an unencrypted P-256 PKCS#8 key.
pub fn control_signing_key_from_pkcs8(der: &[u8]) -> Result<EcdsaKeyPair> {
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, der)
        .map_err(|_| anyhow!("control signing key is not an unencrypted P-256 PKCS#8 key"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{HomeCatalog, Service};
    use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair};

    fn keys() -> Result<(EcdsaKeyPair, EcdsaKeyPair, EcdsaKeyPair)> {
        Ok((
            EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
                .map_err(|_| anyhow!("root key generation failed"))?,
            EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
                .map_err(|_| anyhow!("Server key generation failed"))?,
            EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
                .map_err(|_| anyhow!("authority key generation failed"))?,
        ))
    }

    fn ca_pem(name: &str) -> Result<String> {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        Ok(params.self_signed(&key)?.pem())
    }

    fn fixture_for(
        now: u64,
        deployment_id: &str,
    ) -> Result<(String, EcdsaKeyPair, SignedDeploymentTrust)> {
        let (root, server, authority) = keys()?;
        let root_public = hex::encode(root.public_key().as_ref());
        let trust = DeploymentTrust {
            version: DEPLOYMENT_TRUST_VERSION,
            deployment_id: deployment_id.to_owned(),
            generation: 7,
            not_before_unix_secs: now - 1,
            not_after_unix_secs: now + 3_600,
            management_ca_certificate_pem: ca_pem("management")?,
            business_ca_certificate_pem: ca_pem("business")?,
            server_control_keys: vec![ServerControlKey {
                server_id: "server-1".to_owned(),
                epoch: 1,
                public_key: hex::encode(server.public_key().as_ref()),
            }],
            home_endpoints: vec![HomeEndpointTrust {
                home_id: "home-1".to_owned(),
                management_spki_pins: vec!["44".repeat(32)],
                business_spki_pins: vec!["55".repeat(32)],
            }],
            home_enrollment_authorities: vec![],
            travel_authorities: vec![TrustedTravelAuthority::Home {
                id: "home-1-authority".to_owned(),
                epoch: 1,
                home_id: "home-1".to_owned(),
                public_key: hex::encode(authority.public_key().as_ref()),
            }],
        };
        Ok((
            root_public,
            server,
            SignedDeploymentTrust::sign(&trust, &root)?,
        ))
    }

    fn fixture(now: u64) -> Result<(String, EcdsaKeyPair, SignedDeploymentTrust)> {
        fixture_for(now, "deployment-1")
    }

    #[test]
    fn selected_trust_files_support_multiple_deployments_without_code_changes() -> Result<()> {
        let now = 1_800_000_000;
        let directory = std::env::temp_dir().join(format!(
            "flowsplice-deployment-config-test-{}",
            Uuid::new_v4()
        ));
        fs::create_dir(&directory)?;

        let (root_one, _, trust_one) = fixture_for(now, "deployment-one")?;
        let (root_two, _, trust_two) = fixture_for(now, "deployment-two")?;
        let root_one_path = directory.join("root-one.pub");
        let root_two_path = directory.join("root-two.pub");
        let trust_one_path = directory.join("trust-one.json");
        let trust_two_path = directory.join("trust-two.json");
        fs::write(&root_one_path, format!("{root_one}\n"))?;
        fs::write(&root_two_path, format!("{root_two}\n"))?;
        fs::write(&trust_one_path, serde_json::to_vec_pretty(&trust_one)?)?;
        fs::write(&trust_two_path, serde_json::to_vec_pretty(&trust_two)?)?;

        let (_, _, loaded_one) =
            load_verified_deployment_trust(&root_one_path, &trust_one_path, now)?;
        let (_, _, loaded_two) =
            load_verified_deployment_trust(&root_two_path, &trust_two_path, now)?;
        assert_eq!(loaded_one.deployment_id, "deployment-one");
        assert_eq!(loaded_two.deployment_id, "deployment-two");
        assert!(load_verified_deployment_trust(&root_one_path, &trust_two_path, now).is_err());

        fs::remove_dir_all(directory)?;
        Ok(())
    }

    fn snapshot(
        now: u64,
        trust: SignedDeploymentTrust,
        root_public: &str,
        server: &EcdsaKeyPair,
    ) -> Result<SignedControlSnapshot> {
        let verified_trust = trust.verify(root_public, now)?;
        SignedControlSnapshot::sign(
            trust,
            &verified_trust,
            &ControlSnapshotPayload {
                version: CONTROL_SNAPSHOT_VERSION,
                object_type: CONTROL_SNAPSHOT_OBJECT_TYPE.to_owned(),
                deployment_id: "deployment-1".to_owned(),
                server_id: "server-1".to_owned(),
                signer_epoch: 1,
                travel_id: "travel-1".to_owned(),
                travel_management_spki_sha256: "33".repeat(32),
                generation: 9,
                issued_at_unix_secs: now,
                expires_at_unix_secs: now + 120,
                relay_directory: RelayDirectory {
                    generation: 3,
                    relays: vec![crate::protocol::RelayEndpoint {
                        id: "relay-1".to_owned(),
                        management_addr: "127.0.0.1:8443".to_owned(),
                        data_public_addr: "127.0.0.1:8444".to_owned(),
                        management_spki_sha256: "11".repeat(32),
                    }],
                },
                catalog: Catalog::default(),
            },
            server,
        )
    }

    #[test]
    fn relay_transport_cannot_rewrite_catalog_or_directory() -> Result<()> {
        let now = 1_800_000_000;
        let (root_public, server, trust) = fixture(now)?;
        let signed = snapshot(now, trust, &root_public, &server)?;
        let verified = signed.verify(&root_public, now + 1)?;
        assert_eq!(verified.payload.generation, 9);

        let mut tampered = signed.clone();
        let mut payload: ControlSnapshotPayload =
            serde_json::from_slice(&hex::decode(&tampered.payload_hex)?)?;
        payload.catalog.generation = 99;
        tampered.payload_hex = hex::encode(serde_json::to_vec(&payload)?);
        assert!(tampered.verify(&root_public, now + 1).is_err());

        let mut tampered = signed;
        let mut payload: ControlSnapshotPayload =
            serde_json::from_slice(&hex::decode(&tampered.payload_hex)?)?;
        payload.relay_directory.relays[0].management_addr = "attacker:8443".to_owned();
        tampered.payload_hex = hex::encode(serde_json::to_vec(&payload)?);
        assert!(tampered.verify(&root_public, now + 1).is_err());
        Ok(())
    }

    #[test]
    fn attacker_signed_replacement_trust_and_expired_snapshot_fail_closed() -> Result<()> {
        let now = 1_800_000_000;
        let (root_public, server, trust) = fixture(now)?;
        let signed = snapshot(now, trust.clone(), &root_public, &server)?;
        let attacker = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("attacker key generation failed"))?;
        let payload: DeploymentTrust = serde_json::from_slice(&hex::decode(&trust.payload_hex)?)?;
        let attacker_signed_trust = SignedDeploymentTrust::sign(&payload, &attacker)?;
        assert!(attacker_signed_trust.verify(&root_public, now + 1).is_err());
        assert!(signed.verify(&root_public, now + 120).is_err());
        Ok(())
    }

    #[test]
    fn expired_home_endpoint_retains_historical_travel_verification_authority() -> Result<()> {
        let (root, server, static_travel) = keys()?;
        let enrollment = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("Home enrollment key generation failed"))?;
        let delegated = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("delegated Travel key generation failed"))?;
        let trust = DeploymentTrust {
            version: DEPLOYMENT_TRUST_VERSION,
            deployment_id: "deployment-1".to_owned(),
            generation: 1,
            not_before_unix_secs: 1,
            not_after_unix_secs: 1_000,
            management_ca_certificate_pem: ca_pem("management")?,
            business_ca_certificate_pem: ca_pem("business")?,
            server_control_keys: vec![ServerControlKey {
                server_id: "server-1".to_owned(),
                epoch: 1,
                public_key: hex::encode(server.public_key().as_ref()),
            }],
            home_endpoints: vec![HomeEndpointTrust {
                home_id: "home-1".to_owned(),
                management_spki_pins: vec!["44".repeat(32)],
                business_spki_pins: vec!["55".repeat(32)],
            }],
            home_enrollment_authorities: vec![TrustedHomeEnrollmentAuthority {
                id: "home-enrollment-1".to_owned(),
                epoch: 1,
                issuer_home_id: "home-1".to_owned(),
                public_key: hex::encode(enrollment.public_key().as_ref()),
            }],
            travel_authorities: vec![TrustedTravelAuthority::Home {
                id: "home-1-travel".to_owned(),
                epoch: 1,
                home_id: "home-1".to_owned(),
                public_key: hex::encode(static_travel.public_key().as_ref()),
            }],
        };
        SignedDeploymentTrust::sign(&trust, &root)?;

        let delegated_authority = TrustedTravelAuthority::Home {
            id: "home-2-travel".to_owned(),
            epoch: 1,
            home_id: "home-2".to_owned(),
            public_key: hex::encode(delegated.public_key().as_ref()),
        };
        let signed_endpoint = SignedHomeEndpointCredential::sign(
            &HomeEndpointCredential {
                version: HOME_ENDPOINT_CREDENTIAL_VERSION,
                object_type: HOME_ENDPOINT_CREDENTIAL_OBJECT_TYPE.to_owned(),
                deployment_id: trust.deployment_id.clone(),
                credential_id: Uuid::from_u128(1),
                authority_id: "home-enrollment-1".to_owned(),
                authority_epoch: 1,
                enrollment_request_id: Uuid::from_u128(2),
                home_id: "home-2".to_owned(),
                management_spki_sha256: "66".repeat(32),
                business_spki_sha256: "77".repeat(32),
                delegated_travel_authorities: vec![delegated_authority.clone()],
                issuer_bundle_sha256: Some("88".repeat(32)),
                not_before_unix_secs: 100,
                not_after_unix_secs: 200,
            },
            &enrollment,
        )?;

        assert!(signed_endpoint.verify(&trust, 200).is_err());
        let authorities =
            trust.travel_authorities_with_home_delegations(&[signed_endpoint], 200)?;
        assert!(authorities.contains(&delegated_authority));
        Ok(())
    }

    #[test]
    fn signed_control_snapshot_rejects_aggregate_catalog_over_frame_budget() -> Result<()> {
        let now = 1_800_000_000;
        let (root_public, server, trust) = fixture(now)?;
        let verified_trust = trust.verify(&root_public, now)?;
        let services = (0..MAX_SERVICES_PER_HOME)
            .map(|index| Service {
                id: format!("service-{index}"),
                alias: "a".repeat(MAX_DISPLAY_OR_TARGET_BYTES),
                protocol: ServiceProtocol::Tcp,
                target: "t".repeat(MAX_DISPLAY_OR_TARGET_BYTES),
            })
            .collect::<Vec<_>>();
        let payload = ControlSnapshotPayload {
            version: CONTROL_SNAPSHOT_VERSION,
            object_type: CONTROL_SNAPSHOT_OBJECT_TYPE.to_owned(),
            deployment_id: "deployment-1".to_owned(),
            server_id: "server-1".to_owned(),
            signer_epoch: 1,
            travel_id: "travel-1".to_owned(),
            travel_management_spki_sha256: "33".repeat(32),
            generation: 10,
            issued_at_unix_secs: now,
            expires_at_unix_secs: now + 120,
            relay_directory: RelayDirectory {
                generation: 1,
                relays: vec![crate::protocol::RelayEndpoint {
                    id: "relay-1".to_owned(),
                    management_addr: "127.0.0.1:8443".to_owned(),
                    data_public_addr: "127.0.0.1:8444".to_owned(),
                    management_spki_sha256: "11".repeat(32),
                }],
            },
            catalog: Catalog {
                generation: 1,
                homes: vec![
                    HomeCatalog {
                        home_id: "home-1".to_owned(),
                        home_alias: "Home One".to_owned(),
                        services: services.clone(),
                        endpoint_credential: None,
                    },
                    HomeCatalog {
                        home_id: "home-2".to_owned(),
                        home_alias: "Home Two".to_owned(),
                        services,
                        endpoint_credential: None,
                    },
                ],
            },
        };
        assert!(SignedControlSnapshot::sign(trust, &verified_trust, &payload, &server).is_err());
        Ok(())
    }
}
