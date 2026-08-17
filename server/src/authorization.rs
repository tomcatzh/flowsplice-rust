use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use flowsplice_core::authorization::{
    TravelAuthorizationSnapshot, TravelCredentialBundle, TravelRevocation, VerifiedAuthorization,
    load_json, store_json_atomic, unix_time_secs, validate_authority_public_key,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RevocationState {
    generation: u64,
    revocations: Vec<TravelRevocation>,
}

impl Default for RevocationState {
    fn default() -> Self {
        Self {
            generation: 1,
            revocations: Vec::new(),
        }
    }
}

pub struct ServerAuthorization {
    authority_public_key: String,
    credentials_path: PathBuf,
    revocations_path: PathBuf,
    snapshot: TravelAuthorizationSnapshot,
    verified: VerifiedAuthorization,
}

impl ServerAuthorization {
    pub fn validate(
        authority_public_key: String,
        credentials_path: PathBuf,
        revocations_path: PathBuf,
    ) -> Result<()> {
        Self::load_inner(
            authority_public_key,
            credentials_path,
            revocations_path,
            false,
        )
        .map(|_| ())
    }

    pub fn load(
        authority_public_key: String,
        credentials_path: PathBuf,
        revocations_path: PathBuf,
    ) -> Result<Self> {
        Self::load_inner(
            authority_public_key,
            credentials_path,
            revocations_path,
            true,
        )
    }

    fn load_inner(
        authority_public_key: String,
        credentials_path: PathBuf,
        revocations_path: PathBuf,
        persist_initial: bool,
    ) -> Result<Self> {
        validate_authority_public_key(&authority_public_key)?;
        let bundle: TravelCredentialBundle = load_json(&credentials_path)?;
        let revocations = if revocations_path.exists() {
            load_json(&revocations_path)?
        } else {
            let initial = RevocationState::default();
            if persist_initial {
                store_json_atomic(&revocations_path, &initial)?;
            }
            initial
        };
        let snapshot = TravelAuthorizationSnapshot {
            generation: revocations.generation,
            credentials: bundle.credentials,
            revocations: revocations.revocations,
        };
        let verified = VerifiedAuthorization::verify(&snapshot, &authority_public_key)?;
        Ok(Self {
            authority_public_key,
            credentials_path,
            revocations_path,
            snapshot,
            verified,
        })
    }

    pub fn snapshot(&self) -> TravelAuthorizationSnapshot {
        self.snapshot.clone()
    }

    pub const fn verified(&self) -> &VerifiedAuthorization {
        &self.verified
    }

    pub fn revoke(&mut self, credential_id: Uuid, reason: String) -> Result<bool> {
        if self.verified.credential(credential_id).is_none() {
            bail!("unknown Travel credential {credential_id}");
        }
        if self
            .snapshot
            .revocations
            .iter()
            .any(|revocation| revocation.credential_id == credential_id)
        {
            return Ok(false);
        }
        let generation = self
            .snapshot
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Travel authorization generation exhausted"))?;
        let mut proposed = self.snapshot.clone();
        proposed.generation = generation;
        proposed.revocations.push(TravelRevocation {
            credential_id,
            revoked_at_unix_secs: unix_time_secs()?,
            reason,
        });
        let verified = VerifiedAuthorization::verify(&proposed, &self.authority_public_key)?;
        self.persist_revocations(&proposed)?;
        self.snapshot = proposed;
        self.verified = verified;
        Ok(true)
    }

    pub fn reload_credentials(&mut self) -> Result<bool> {
        let bundle: TravelCredentialBundle = load_json(&self.credentials_path)?;
        if bundle.credentials == self.snapshot.credentials {
            return Ok(false);
        }
        for existing in &self.snapshot.credentials {
            if !bundle
                .credentials
                .iter()
                .any(|candidate| candidate.payload_hex == existing.payload_hex)
            {
                bail!("Travel credential reload must be add-only");
            }
        }
        let generation = self
            .snapshot
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Travel authorization generation exhausted"))?;
        let proposed = TravelAuthorizationSnapshot {
            generation,
            credentials: bundle.credentials,
            revocations: self.snapshot.revocations.clone(),
        };
        let verified = VerifiedAuthorization::verify(&proposed, &self.authority_public_key)?;
        self.persist_revocations(&proposed)?;
        self.snapshot = proposed;
        self.verified = verified;
        Ok(true)
    }

    fn persist_revocations(&self, snapshot: &TravelAuthorizationSnapshot) -> Result<()> {
        store_json_atomic(
            &self.revocations_path,
            &RevocationState {
                generation: snapshot.generation,
                revocations: snapshot.revocations.clone(),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Context;
    use aws_lc_rs::{
        rand::SystemRandom,
        signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair, KeyPair},
    };
    use flowsplice_core::authorization::{
        SignedTravelCredential, TravelCredential, TravelCredentialBundle, store_json_atomic,
    };

    use super::*;

    fn signed(key: &EcdsaKeyPair, credential: &TravelCredential) -> Result<SignedTravelCredential> {
        let payload = serde_json::to_vec(credential)?;
        let signature = key
            .sign(&SystemRandom::new(), &payload)
            .map_err(|_| anyhow!("failed to sign fixture"))?;
        Ok(SignedTravelCredential {
            payload_hex: hex::encode(payload),
            signature_hex: hex::encode(signature.as_ref()),
        })
    }

    #[test]
    fn revocation_is_durable_monotonic_and_idempotent() -> Result<()> {
        let directory = std::env::temp_dir().join(format!("flowsplice-auth-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory)?;
        let credentials_path = directory.join("credentials.json");
        let revocations_path = directory.join("revocations.json");
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate fixture key"))?;
        let credential = TravelCredential {
            credential_id: Uuid::new_v4(),
            travel_id: "travel-1".to_owned(),
            management_spki_sha256: "11".repeat(32),
            business_spki_sha256: "22".repeat(32),
            not_before_unix_secs: 1,
            not_after_unix_secs: u64::MAX,
        };
        store_json_atomic(
            &credentials_path,
            &TravelCredentialBundle {
                credentials: vec![signed(&key, &credential)?],
            },
        )?;
        let public_key = hex::encode(key.public_key().as_ref());
        let mut authorization = ServerAuthorization::load(
            public_key.clone(),
            credentials_path.clone(),
            revocations_path.clone(),
        )?;
        assert!(authorization.revoke(credential.credential_id, "stolen".to_owned())?);
        assert!(!authorization.revoke(credential.credential_id, "again".to_owned())?);
        assert_eq!(authorization.snapshot().generation, 2);
        assert_eq!(authorization.snapshot().revocations.len(), 1);

        let reloaded = ServerAuthorization::load(
            public_key,
            credentials_path.clone(),
            revocations_path.clone(),
        )?;
        assert_eq!(reloaded.snapshot(), authorization.snapshot());
        let persisted: RevocationState = load_json(&revocations_path)?;
        assert_eq!(persisted.generation, 2);

        store_json_atomic(
            &credentials_path,
            &TravelCredentialBundle {
                credentials: vec![],
            },
        )?;
        assert!(authorization.reload_credentials().is_err());
        assert_eq!(authorization.snapshot().generation, 2);

        let second = TravelCredential {
            credential_id: Uuid::new_v4(),
            travel_id: "travel-2".to_owned(),
            management_spki_sha256: "33".repeat(32),
            business_spki_sha256: "44".repeat(32),
            not_before_unix_secs: 1,
            not_after_unix_secs: u64::MAX,
        };
        store_json_atomic(
            &credentials_path,
            &TravelCredentialBundle {
                credentials: vec![signed(&key, &credential)?, signed(&key, &second)?],
            },
        )?;
        assert!(authorization.reload_credentials()?);
        assert_eq!(authorization.snapshot().generation, 3);
        assert!(
            authorization
                .verified()
                .credential(second.credential_id)
                .is_some()
        );
        assert!(
            authorization
                .revoke(Uuid::new_v4(), "unknown".to_owned())
                .is_err()
        );
        fs::remove_dir_all(&directory)
            .with_context(|| format!("failed to remove {}", directory.display()))?;
        Ok(())
    }
}
