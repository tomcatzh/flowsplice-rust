use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::tls::{PeerIdentity, validate_spki_pin};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelCredential {
    pub credential_id: Uuid,
    pub travel_id: String,
    pub management_spki_sha256: String,
    pub business_spki_sha256: String,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedTravelCredential {
    pub payload_hex: String,
    pub signature_hex: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelCredentialBundle {
    pub credentials: Vec<SignedTravelCredential>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelRevocation {
    pub credential_id: Uuid,
    pub revoked_at_unix_secs: u64,
    pub reason: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelAuthorizationSnapshot {
    pub generation: u64,
    pub credentials: Vec<SignedTravelCredential>,
    pub revocations: Vec<TravelRevocation>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AuthorizationCache {
    pub generation: u64,
    pub revoked_credentials: HashSet<Uuid>,
}

#[derive(Clone, Debug)]
pub struct VerifiedAuthorization {
    generation: u64,
    credentials: HashMap<Uuid, TravelCredential>,
    management_index: HashMap<(String, String), Uuid>,
    business_index: HashMap<(String, String), Uuid>,
    revoked_credentials: HashSet<Uuid>,
}

impl SignedTravelCredential {
    /// Verifies the detached offline signature and decodes the exact signed payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the public key, payload, signature, or credential is invalid.
    pub fn verify(&self, authority_public_key_hex: &str) -> Result<TravelCredential> {
        let public_key = decode_authority_public_key(authority_public_key_hex)?;
        let payload = hex::decode(&self.payload_hex)
            .context("signed Travel credential payload must be hexadecimal")?;
        let signature = hex::decode(&self.signature_hex)
            .context("signed Travel credential signature must be hexadecimal")?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, public_key)
            .verify(&payload, &signature)
            .map_err(|_| anyhow!("signed Travel credential has an invalid signature"))?;
        let credential: TravelCredential = serde_json::from_slice(&payload)
            .context("signed Travel credential payload is invalid")?;
        credential.validate()?;
        Ok(credential)
    }
}

/// Validates a raw uncompressed P-256 Travel-authorization public key.
///
/// # Errors
///
/// Returns an error when the value is not the expected hexadecimal SEC1 encoding.
pub fn validate_authority_public_key(authority_public_key_hex: &str) -> Result<()> {
    decode_authority_public_key(authority_public_key_hex).map(|_| ())
}

fn decode_authority_public_key(authority_public_key_hex: &str) -> Result<Vec<u8>> {
    let public_key = hex::decode(authority_public_key_hex)
        .context("Travel authorization public key must be hexadecimal")?;
    if public_key.len() != 65 || public_key.first() != Some(&4) {
        bail!("Travel authorization public key must be an uncompressed P-256 point");
    }
    Ok(public_key)
}

impl TravelCredential {
    fn validate(&self) -> Result<()> {
        if self.credential_id.is_nil() || self.travel_id.is_empty() {
            bail!("Travel credential id and Travel id must be non-empty");
        }
        validate_spki_pin(&self.management_spki_sha256, "Travel management")?;
        validate_spki_pin(&self.business_spki_sha256, "Travel business")?;
        if self.not_before_unix_secs >= self.not_after_unix_secs {
            bail!("Travel credential validity interval is empty");
        }
        Ok(())
    }

    #[must_use]
    pub const fn active_at(&self, unix_secs: u64) -> bool {
        unix_secs >= self.not_before_unix_secs && unix_secs < self.not_after_unix_secs
    }
}

impl VerifiedAuthorization {
    /// Verifies every signed credential and constructs immutable lookup indexes.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid signatures, duplicate identities, or invalid revocations.
    pub fn verify(
        snapshot: &TravelAuthorizationSnapshot,
        authority_public_key_hex: &str,
    ) -> Result<Self> {
        if snapshot.generation == 0 {
            bail!("Travel authorization generation must be positive");
        }
        let mut credentials = HashMap::new();
        let mut management_index = HashMap::new();
        let mut business_index = HashMap::new();
        for signed in &snapshot.credentials {
            let credential = signed.verify(authority_public_key_hex)?;
            let credential_id = credential.credential_id;
            if credentials
                .insert(credential_id, credential.clone())
                .is_some()
            {
                bail!("duplicate Travel credential id {credential_id}");
            }
            let management_key = (
                credential.travel_id.clone(),
                credential.management_spki_sha256.to_ascii_lowercase(),
            );
            if management_index
                .insert(management_key, credential_id)
                .is_some()
            {
                bail!("a Travel management identity is assigned to multiple credentials");
            }
            let business_key = (
                credential.travel_id.clone(),
                credential.business_spki_sha256.to_ascii_lowercase(),
            );
            if business_index.insert(business_key, credential_id).is_some() {
                bail!("a Travel business identity is assigned to multiple credentials");
            }
        }
        let mut revoked_credentials = HashSet::new();
        for revocation in &snapshot.revocations {
            if !credentials.contains_key(&revocation.credential_id) {
                bail!(
                    "revocation references unknown Travel credential {}",
                    revocation.credential_id
                );
            }
            if !revoked_credentials.insert(revocation.credential_id) {
                bail!(
                    "duplicate revocation for Travel credential {}",
                    revocation.credential_id
                );
            }
        }
        Ok(Self {
            generation: snapshot.generation,
            credentials,
            management_index,
            business_index,
            revoked_credentials,
        })
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub fn revoked_credentials(&self) -> &HashSet<Uuid> {
        &self.revoked_credentials
    }

    #[must_use]
    pub fn credential(&self, credential_id: Uuid) -> Option<&TravelCredential> {
        self.credentials.get(&credential_id)
    }

    #[must_use]
    pub fn is_active(&self, credential_id: Uuid, unix_secs: u64) -> bool {
        self.credentials
            .get(&credential_id)
            .is_some_and(|credential| {
                !self.revoked_credentials.contains(&credential_id)
                    && credential.active_at(unix_secs)
            })
    }

    /// Resolves a management TLS identity to one active signed credential.
    ///
    /// # Errors
    ///
    /// Returns an error when no active credential binds this Travel identity and SPKI.
    pub fn authorize_management(
        &self,
        identity: &PeerIdentity,
        unix_secs: u64,
    ) -> Result<&TravelCredential> {
        self.authorize(identity, unix_secs, &self.management_index)
    }

    /// Resolves a business TLS identity to one active signed credential.
    ///
    /// # Errors
    ///
    /// Returns an error when no active credential binds this Travel identity and SPKI.
    pub fn authorize_business(
        &self,
        identity: &PeerIdentity,
        unix_secs: u64,
    ) -> Result<&TravelCredential> {
        self.authorize(identity, unix_secs, &self.business_index)
    }

    fn authorize<'a>(
        &'a self,
        identity: &PeerIdentity,
        unix_secs: u64,
        index: &HashMap<(String, String), Uuid>,
    ) -> Result<&'a TravelCredential> {
        let key = (
            identity.id.clone(),
            identity.spki_sha256.to_ascii_lowercase(),
        );
        let credential_id = index
            .get(&key)
            .copied()
            .ok_or_else(|| anyhow!("Travel certificate is not covered by a signed credential"))?;
        if !identity.active_at(unix_secs) {
            bail!("Travel certificate is expired or not yet valid");
        }
        if !self.is_active(credential_id, unix_secs) {
            bail!("Travel credential is revoked, expired, or not yet valid");
        }
        self.credentials
            .get(&credential_id)
            .ok_or_else(|| anyhow!("Travel credential index is inconsistent"))
    }
}

impl AuthorizationCache {
    /// Rejects rollback or removal of any previously observed revocation.
    ///
    /// # Errors
    ///
    /// Returns an error when the proposed snapshot is older or loses a revocation.
    pub fn accept(&self, authorization: &VerifiedAuthorization) -> Result<Self> {
        if authorization.generation() < self.generation {
            bail!("Travel authorization generation rollback detected");
        }
        if !self
            .revoked_credentials
            .is_subset(authorization.revoked_credentials())
        {
            bail!("Travel authorization snapshot removed a prior revocation");
        }
        Ok(Self {
            generation: authorization.generation(),
            revoked_credentials: authorization.revoked_credentials().clone(),
        })
    }
}

/// Returns the current Unix time in whole seconds.
///
/// # Errors
///
/// Returns an error only if the system clock is before the Unix epoch.
pub fn unix_time_secs() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

/// Loads a strict JSON value from disk.
///
/// # Errors
///
/// Returns an error when the file cannot be read or decoded.
pub fn load_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("failed to parse {}", path.display()))
}

/// Atomically replaces a small JSON state file and fsyncs both file and directory.
///
/// # Errors
///
/// Returns an error when serialization or durable replacement fails.
pub fn store_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("state path has no file name"))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        let bytes = serde_json::to_vec_pretty(value).context("failed to encode JSON state")?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use aws_lc_rs::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
    };

    use crate::protocol::Role;

    use super::*;

    fn signed(key: &EcdsaKeyPair, credential: &TravelCredential) -> Result<SignedTravelCredential> {
        let payload = serde_json::to_vec(credential)?;
        let signature = key
            .sign(&SystemRandom::new(), &payload)
            .map_err(|_| anyhow!("failed to sign test credential"))?;
        Ok(SignedTravelCredential {
            payload_hex: hex::encode(payload),
            signature_hex: hex::encode(signature.as_ref()),
        })
    }

    fn fixture() -> Result<(String, TravelCredential, SignedTravelCredential)> {
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate test key"))?;
        let credential = TravelCredential {
            credential_id: Uuid::new_v4(),
            travel_id: "travel-1".to_owned(),
            management_spki_sha256: "11".repeat(32),
            business_spki_sha256: "22".repeat(32),
            not_before_unix_secs: 100,
            not_after_unix_secs: 200,
        };
        let signed = signed(&key, &credential)?;
        Ok((hex::encode(key.public_key().as_ref()), credential, signed))
    }

    #[test]
    fn signed_credentials_bind_both_tls_identities_and_validity() -> Result<()> {
        let (public_key, credential, signed) = fixture()?;
        let snapshot = TravelAuthorizationSnapshot {
            generation: 1,
            credentials: vec![signed],
            revocations: vec![],
        };
        let authorization = VerifiedAuthorization::verify(&snapshot, &public_key)?;
        let management = PeerIdentity {
            role: Role::Travel,
            id: credential.travel_id.clone(),
            spki_sha256: credential.management_spki_sha256.clone(),
            not_before_unix_secs: 100,
            not_after_unix_secs: 200,
        };
        let business = PeerIdentity {
            role: Role::Travel,
            id: credential.travel_id.clone(),
            spki_sha256: credential.business_spki_sha256.clone(),
            not_before_unix_secs: 100,
            not_after_unix_secs: 200,
        };
        assert_eq!(
            authorization
                .authorize_management(&management, 150)?
                .credential_id,
            credential.credential_id
        );
        assert_eq!(
            authorization
                .authorize_business(&business, 150)?
                .credential_id,
            credential.credential_id
        );
        assert!(authorization.authorize_management(&management, 99).is_err());
        assert!(authorization.authorize_business(&business, 200).is_err());
        Ok(())
    }

    #[test]
    fn tampering_revocation_and_rollback_fail_closed() -> Result<()> {
        let (public_key, credential, mut signed) = fixture()?;
        signed.payload_hex.replace_range(0..2, "00");
        assert!(signed.verify(&public_key).is_err());

        let (_, _, valid) = fixture()?;
        let wrong_revocation = TravelRevocation {
            credential_id: credential.credential_id,
            revoked_at_unix_secs: 150,
            reason: "stolen".to_owned(),
        };
        assert!(
            VerifiedAuthorization::verify(
                &TravelAuthorizationSnapshot {
                    generation: 1,
                    credentials: vec![valid],
                    revocations: vec![wrong_revocation],
                },
                &public_key,
            )
            .is_err()
        );

        let (public_key, credential, valid) = fixture()?;
        let revoked = VerifiedAuthorization::verify(
            &TravelAuthorizationSnapshot {
                generation: 3,
                credentials: vec![valid.clone()],
                revocations: vec![TravelRevocation {
                    credential_id: credential.credential_id,
                    revoked_at_unix_secs: 150,
                    reason: "stolen".to_owned(),
                }],
            },
            &public_key,
        )?;
        let cache = AuthorizationCache::default().accept(&revoked)?;
        let rollback = VerifiedAuthorization::verify(
            &TravelAuthorizationSnapshot {
                generation: 2,
                credentials: vec![valid],
                revocations: vec![],
            },
            &public_key,
        )?;
        assert!(cache.accept(&rollback).is_err());
        Ok(())
    }
}
