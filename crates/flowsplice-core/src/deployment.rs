use std::collections::HashSet;

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

use crate::{
    authorization::{TrustedTravelAuthority, validate_trusted_authorities},
    protocol::{Catalog, RelayDirectory, ServiceProtocol},
    tls::validate_spki_pin,
};

pub const DEPLOYMENT_TRUST_VERSION: u32 = 1;
pub const CONTROL_SNAPSHOT_VERSION: u32 = 1;
pub const CONTROL_SNAPSHOT_OBJECT_TYPE: &str = "flowsplice.control_snapshot";
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

    fn fixture(now: u64) -> Result<(String, EcdsaKeyPair, SignedDeploymentTrust)> {
        let (root, server, authority) = keys()?;
        let root_public = hex::encode(root.public_key().as_ref());
        let trust = DeploymentTrust {
            version: DEPLOYMENT_TRUST_VERSION,
            deployment_id: "deployment-1".to_owned(),
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
                    },
                    HomeCatalog {
                        home_id: "home-2".to_owned(),
                        home_alias: "Home Two".to_owned(),
                        services,
                    },
                ],
            },
        };
        assert!(SignedControlSnapshot::sign(trust, &verified_trust, &payload, &server).is_err());
        Ok(())
    }
}
