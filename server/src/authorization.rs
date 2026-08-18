use std::path::PathBuf;

use anyhow::{Result, anyhow, bail};
use flowsplice_core::authorization::{
    SignedTravelCredential, TravelAuthorizationSnapshot, TravelCredentialBundle, TravelRevocation,
    TrustedTravelAuthority, VerifiedAuthorization, load_json, store_json_atomic, unix_time_secs,
    validate_trusted_authorities,
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
    authorities: Vec<TrustedTravelAuthority>,
    credentials_path: PathBuf,
    revocations_path: PathBuf,
    snapshot: TravelAuthorizationSnapshot,
    verified: VerifiedAuthorization,
}

impl ServerAuthorization {
    pub fn validate(
        authorities: Vec<TrustedTravelAuthority>,
        credentials_path: PathBuf,
        revocations_path: PathBuf,
    ) -> Result<()> {
        Self::load_inner(authorities, credentials_path, revocations_path, false).map(|_| ())
    }

    pub fn load(
        authorities: Vec<TrustedTravelAuthority>,
        credentials_path: PathBuf,
        revocations_path: PathBuf,
    ) -> Result<Self> {
        Self::load_inner(authorities, credentials_path, revocations_path, true)
    }

    fn load_inner(
        authorities: Vec<TrustedTravelAuthority>,
        credentials_path: PathBuf,
        revocations_path: PathBuf,
        persist_initial: bool,
    ) -> Result<Self> {
        validate_trusted_authorities(&authorities)?;
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
        let verified = VerifiedAuthorization::verify(&snapshot, &authorities)?;
        Ok(Self {
            authorities,
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

    pub fn revoke_from_home(
        &mut self,
        credential_id: Uuid,
        reason: String,
        publisher_home_id: &str,
    ) -> Result<bool> {
        let credential = self
            .verified
            .credential(credential_id)
            .ok_or_else(|| anyhow!("unknown Travel credential {credential_id}"))?;
        let authority = self
            .authorities
            .iter()
            .find(|authority| authority.id() == credential.authority_id)
            .ok_or_else(|| anyhow!("Travel credential references an unknown authority"))?;
        if authority.home_id() != Some(publisher_home_id) {
            bail!("Travel credential was not issued by this Home");
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
        let verified = VerifiedAuthorization::verify(&proposed, &self.authorities)?;
        self.persist_revocations(&proposed)?;
        self.snapshot = proposed;
        self.verified = verified;
        Ok(true)
    }

    pub fn import_credential(
        &mut self,
        signed: SignedTravelCredential,
        publisher_home_id: &str,
    ) -> Result<bool> {
        let authority = self
            .authorities
            .iter()
            .find(|authority| authority.id() == signed.authority_id)
            .ok_or_else(|| anyhow!("unknown Travel authority {}", signed.authority_id))?;
        if authority.home_id() != Some(publisher_home_id) {
            bail!("Travel authority is not assigned to the publishing Home");
        }
        let credential = signed.verify(authority)?;
        if let Some(existing) = self.snapshot.credentials.iter().find(|existing| {
            self.authorities
                .iter()
                .find(|authority| authority.id() == existing.authority_id)
                .and_then(|authority| existing.verify(authority).ok())
                .is_some_and(|value| value.credential_id == credential.credential_id)
        }) {
            if existing.payload_hex == signed.payload_hex {
                return Ok(false);
            }
            bail!(
                "Travel credential id {} is already assigned to different material",
                credential.credential_id
            );
        }
        let generation = self
            .snapshot
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Travel authorization generation exhausted"))?;
        let mut proposed = self.snapshot.clone();
        proposed.generation = generation;
        proposed.credentials.push(signed);
        let verified = VerifiedAuthorization::verify(&proposed, &self.authorities)?;
        store_json_atomic(
            &self.credentials_path,
            &TravelCredentialBundle {
                credentials: proposed.credentials.clone(),
            },
        )?;
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
        SignedTravelCredential, TravelCredential, TravelCredentialBundle, TravelCredentialScope,
        TrustedTravelAuthority, store_json_atomic,
    };

    use super::*;

    fn signed(key: &EcdsaKeyPair, credential: &TravelCredential) -> Result<SignedTravelCredential> {
        let payload = serde_json::to_vec(credential)?;
        let signature = key
            .sign(&SystemRandom::new(), &payload)
            .map_err(|_| anyhow!("failed to sign fixture"))?;
        Ok(SignedTravelCredential {
            authority_id: credential.authority_id.clone(),
            payload_hex: hex::encode(payload),
            signature_hex: hex::encode(signature.as_ref()),
        })
    }

    fn authorities(key: &EcdsaKeyPair) -> Vec<TrustedTravelAuthority> {
        vec![TrustedTravelAuthority::Home {
            id: "home-1-authority".to_owned(),
            home_id: "home-1".to_owned(),
            public_key: hex::encode(key.public_key().as_ref()),
        }]
    }

    fn fixture_credential(travel_id: &str, management: &str, business: &str) -> TravelCredential {
        TravelCredential {
            credential_id: Uuid::new_v4(),
            authority_id: "home-1-authority".to_owned(),
            travel_id: travel_id.to_owned(),
            management_spki_sha256: management.repeat(32),
            business_spki_sha256: business.repeat(32),
            scope: TravelCredentialScope::Home {
                home_id: "home-1".to_owned(),
            },
            not_before_unix_secs: 1,
            not_after_unix_secs: u64::MAX,
        }
    }

    #[test]
    fn revocation_is_durable_monotonic_and_idempotent() -> Result<()> {
        let directory = std::env::temp_dir().join(format!("flowsplice-auth-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory)?;
        let credentials_path = directory.join("credentials.json");
        let revocations_path = directory.join("revocations.json");
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate fixture key"))?;
        let credential = fixture_credential("travel-1", "11", "22");
        store_json_atomic(
            &credentials_path,
            &TravelCredentialBundle {
                credentials: vec![signed(&key, &credential)?],
            },
        )?;
        let authorities = authorities(&key);
        let mut authorization = ServerAuthorization::load(
            authorities.clone(),
            credentials_path.clone(),
            revocations_path.clone(),
        )?;
        assert!(authorization.revoke_from_home(
            credential.credential_id,
            "stolen".to_owned(),
            "home-1"
        )?);
        assert!(!authorization.revoke_from_home(
            credential.credential_id,
            "again".to_owned(),
            "home-1"
        )?);
        assert_eq!(authorization.snapshot().generation, 2);
        assert_eq!(authorization.snapshot().revocations.len(), 1);

        let reloaded = ServerAuthorization::load(
            authorities,
            credentials_path.clone(),
            revocations_path.clone(),
        )?;
        assert_eq!(reloaded.snapshot(), authorization.snapshot());
        let persisted: RevocationState = load_json(&revocations_path)?;
        assert_eq!(persisted.generation, 2);

        let second = fixture_credential("travel-2", "33", "44");
        assert!(authorization.import_credential(signed(&key, &second)?, "home-1")?);
        assert_eq!(authorization.snapshot().generation, 3);
        assert!(
            authorization
                .verified()
                .credential(second.credential_id)
                .is_some()
        );
        assert!(
            authorization
                .revoke_from_home(Uuid::new_v4(), "unknown".to_owned(), "home-1")
                .is_err()
        );
        fs::remove_dir_all(&directory)
            .with_context(|| format!("failed to remove {}", directory.display()))?;
        Ok(())
    }

    #[test]
    fn credential_import_is_add_only_idempotent_and_cannot_resurrect_revocation() -> Result<()> {
        let directory = std::env::temp_dir().join(format!("flowsplice-import-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory)?;
        let credentials_path = directory.join("credentials.json");
        let revocations_path = directory.join("revocations.json");
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate fixture key"))?;
        store_json_atomic(
            &credentials_path,
            &TravelCredentialBundle {
                credentials: Vec::new(),
            },
        )?;
        let authorities = authorities(&key);
        let mut authorization = ServerAuthorization::load(
            authorities.clone(),
            credentials_path.clone(),
            revocations_path.clone(),
        )?;
        let credential = fixture_credential("travel-new", "55", "66");
        let first_signature = signed(&key, &credential)?;
        assert!(authorization.import_credential(first_signature.clone(), "home-1")?);
        assert_eq!(authorization.snapshot().generation, 2);
        assert!(!authorization.import_credential(first_signature, "home-1")?);
        assert!(!authorization.import_credential(signed(&key, &credential)?, "home-1")?);
        assert_eq!(authorization.snapshot().generation, 2);

        assert!(authorization.revoke_from_home(
            credential.credential_id,
            "lost".to_owned(),
            "home-1"
        )?);
        assert!(!authorization.import_credential(signed(&key, &credential)?, "home-1")?);
        assert!(
            authorization
                .verified()
                .revoked_credentials()
                .contains(&credential.credential_id)
        );

        let mut conflicting = credential.clone();
        conflicting.travel_id = "travel-conflict".to_owned();
        assert!(
            authorization
                .import_credential(signed(&key, &conflicting)?, "home-1")
                .is_err()
        );
        let reloaded = ServerAuthorization::load(authorities, credentials_path, revocations_path)?;
        assert_eq!(reloaded.snapshot(), authorization.snapshot());
        fs::remove_dir_all(directory).ok();
        Ok(())
    }
}
