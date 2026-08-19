use std::{collections::HashSet, path::PathBuf};

use anyhow::{Result, anyhow, bail};
use flowsplice_core::authorization::{
    SignedTravelCredential, TravelAuthorizationSnapshot, TravelRevocation, TrustedTravelAuthority,
    VerifiedAuthorization, load_json, store_json_atomic, unix_time_secs,
    validate_trusted_authorities,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistentAuthorizationState {
    version: u32,
    snapshot: TravelAuthorizationSnapshot,
    used_enrollment_requests: HashSet<String>,
}

impl Default for PersistentAuthorizationState {
    fn default() -> Self {
        Self {
            version: AUTHORIZATION_STATE_VERSION,
            snapshot: TravelAuthorizationSnapshot {
                generation: 1,
                credentials: Vec::new(),
                revocations: Vec::new(),
            },
            used_enrollment_requests: HashSet::new(),
        }
    }
}

const AUTHORIZATION_STATE_VERSION: u32 = 1;
const MAX_CREDENTIALS: usize = 2_048;
const MAX_REVOCATIONS: usize = 2_048;
const MAX_STATE_BYTES: usize = 900 * 1_024;

pub struct ServerAuthorization {
    deployment_id: String,
    authorities: Vec<TrustedTravelAuthority>,
    state_path: PathBuf,
    snapshot: TravelAuthorizationSnapshot,
    verified: VerifiedAuthorization,
    used_enrollment_requests: HashSet<String>,
}

impl ServerAuthorization {
    pub fn validate(
        deployment_id: String,
        authorities: Vec<TrustedTravelAuthority>,
        state_path: PathBuf,
    ) -> Result<()> {
        Self::load_inner(deployment_id, authorities, state_path).map(|_| ())
    }

    pub fn load(
        deployment_id: String,
        authorities: Vec<TrustedTravelAuthority>,
        state_path: PathBuf,
    ) -> Result<Self> {
        Self::load_inner(deployment_id, authorities, state_path)
    }

    fn load_inner(
        deployment_id: String,
        authorities: Vec<TrustedTravelAuthority>,
        state_path: PathBuf,
    ) -> Result<Self> {
        validate_trusted_authorities(&authorities)?;
        let state: PersistentAuthorizationState = load_json(&state_path).map_err(|error| {
            anyhow!(
                "authorization state {} is missing or invalid; initialize an empty state explicitly: {error}",
                state_path.display()
            )
        })?;
        if state.version != AUTHORIZATION_STATE_VERSION {
            bail!("unsupported authorization state version {}", state.version);
        }
        validate_state_size(&state)?;
        let snapshot = state.snapshot;
        let verified = VerifiedAuthorization::verify(&snapshot, &authorities, &deployment_id)?;
        let credential_requests = verified
            .credentials()
            .map(|credential| credential.enrollment_request_sha256.clone())
            .collect::<HashSet<_>>();
        if !credential_requests.is_subset(&state.used_enrollment_requests) {
            bail!("authorization state omitted a used enrollment request fingerprint");
        }
        Ok(Self {
            deployment_id,
            authorities,
            state_path,
            snapshot,
            verified,
            used_enrollment_requests: state.used_enrollment_requests,
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
        let verified =
            VerifiedAuthorization::verify(&proposed, &self.authorities, &self.deployment_id)?;
        self.persist(&proposed, &self.used_enrollment_requests)?;
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
        if self
            .used_enrollment_requests
            .contains(&credential.enrollment_request_sha256)
        {
            bail!(
                "enrollment request {} was already used; create a new enrollment request",
                credential.enrollment_request_id
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
        let verified =
            VerifiedAuthorization::verify(&proposed, &self.authorities, &self.deployment_id)?;
        let mut used_enrollment_requests = self.used_enrollment_requests.clone();
        used_enrollment_requests.insert(credential.enrollment_request_sha256);
        self.persist(&proposed, &used_enrollment_requests)?;
        self.snapshot = proposed;
        self.verified = verified;
        self.used_enrollment_requests = used_enrollment_requests;
        Ok(true)
    }

    fn persist(
        &self,
        snapshot: &TravelAuthorizationSnapshot,
        used_enrollment_requests: &HashSet<String>,
    ) -> Result<()> {
        let state = PersistentAuthorizationState {
            version: AUTHORIZATION_STATE_VERSION,
            snapshot: snapshot.clone(),
            used_enrollment_requests: used_enrollment_requests.clone(),
        };
        validate_state_size(&state)?;
        store_json_atomic(&self.state_path, &state)
    }
}

fn validate_state_size(state: &PersistentAuthorizationState) -> Result<()> {
    if state.snapshot.credentials.len() > MAX_CREDENTIALS
        || state.snapshot.revocations.len() > MAX_REVOCATIONS
    {
        bail!(
            "authorization state exceeds credential/revocation limits ({MAX_CREDENTIALS}/{MAX_REVOCATIONS})"
        );
    }
    let encoded = serde_json::to_vec(state)?;
    if encoded.len() > MAX_STATE_BYTES {
        bail!("authorization state exceeds {MAX_STATE_BYTES} bytes");
    }
    Ok(())
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
        SignedTravelCredential, TravelCredential, TravelCredentialScope, TrustedTravelAuthority,
        store_json_atomic,
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
            epoch: 1,
            home_id: "home-1".to_owned(),
            public_key: hex::encode(key.public_key().as_ref()),
        }]
    }

    fn fixture_credential(travel_id: &str, management: &str, business: &str) -> TravelCredential {
        let enrollment_request_id = Uuid::new_v4();
        TravelCredential {
            version: flowsplice_core::authorization::TRAVEL_CREDENTIAL_VERSION,
            object_type: flowsplice_core::authorization::TRAVEL_CREDENTIAL_OBJECT_TYPE.to_owned(),
            deployment_id: "deployment-1".to_owned(),
            deployment_trust_sha256: "aa".repeat(32),
            credential_id: Uuid::new_v4(),
            authority_id: "home-1-authority".to_owned(),
            authority_epoch: 1,
            enrollment_request_id,
            enrollment_nonce: "aa".repeat(32),
            enrollment_request_sha256: hex::encode(enrollment_request_id.as_bytes()).repeat(2),
            travel_id: travel_id.to_owned(),
            management_spki_sha256: management.repeat(32),
            business_spki_sha256: business.repeat(32),
            management_ca_sha256: "cc".repeat(32),
            business_ca_sha256: "dd".repeat(32),
            management_certificate_sha256: "ee".repeat(32),
            business_certificate_sha256: "ff".repeat(32),
            scope: TravelCredentialScope::Home {
                home_id: "home-1".to_owned(),
            },
            not_before_unix_secs: 1,
            not_after_unix_secs: u64::MAX,
        }
    }

    fn store_state_with_key(
        path: &std::path::Path,
        key: &EcdsaKeyPair,
        credentials: Vec<SignedTravelCredential>,
    ) -> Result<()> {
        let authority = authorities(key);
        let used_enrollment_requests = credentials
            .iter()
            .map(|credential| {
                credential
                    .verify(&authority[0])
                    .map(|value| value.enrollment_request_sha256)
            })
            .collect::<Result<HashSet<_>>>()?;
        store_json_atomic(
            path,
            &PersistentAuthorizationState {
                version: AUTHORIZATION_STATE_VERSION,
                snapshot: TravelAuthorizationSnapshot {
                    generation: 1,
                    credentials,
                    revocations: Vec::new(),
                },
                used_enrollment_requests,
            },
        )
    }

    #[test]
    fn revocation_is_durable_monotonic_and_idempotent() -> Result<()> {
        let directory = std::env::temp_dir().join(format!("flowsplice-auth-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory)?;
        let state_path = directory.join("authorization.json");
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate fixture key"))?;
        let credential = fixture_credential("travel-1", "11", "22");
        store_state_with_key(&state_path, &key, vec![signed(&key, &credential)?])?;
        let authorities = authorities(&key);
        let mut authorization = ServerAuthorization::load(
            "deployment-1".to_owned(),
            authorities.clone(),
            state_path.clone(),
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

        let reloaded =
            ServerAuthorization::load("deployment-1".to_owned(), authorities, state_path.clone())?;
        assert_eq!(reloaded.snapshot(), authorization.snapshot());
        let persisted: PersistentAuthorizationState = load_json(&state_path)?;
        assert_eq!(persisted.snapshot.generation, 2);

        let second = fixture_credential("travel-2", "33", "44");
        assert!(authorization.import_credential(signed(&key, &second)?, "home-1")?);
        assert!(!authorization.import_credential(signed(&key, &second)?, "home-1")?);
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
        let state_path = directory.join("authorization.json");
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate fixture key"))?;
        store_state_with_key(&state_path, &key, Vec::new())?;
        let authorities = authorities(&key);
        let mut authorization = ServerAuthorization::load(
            "deployment-1".to_owned(),
            authorities.clone(),
            state_path.clone(),
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
        let reloaded =
            ServerAuthorization::load("deployment-1".to_owned(), authorities, state_path)?;
        assert_eq!(reloaded.snapshot(), authorization.snapshot());
        fs::remove_dir_all(directory).ok();
        Ok(())
    }

    #[test]
    fn enrollment_request_cannot_be_reused_after_revocation() -> Result<()> {
        let directory = std::env::temp_dir().join(format!("flowsplice-reuse-{}", Uuid::new_v4()));
        fs::create_dir_all(&directory)?;
        let state_path = directory.join("authorization.json");
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate fixture key"))?;
        store_state_with_key(&state_path, &key, Vec::new())?;
        let mut authorization =
            ServerAuthorization::load("deployment-1".to_owned(), authorities(&key), state_path)?;
        let credential = fixture_credential("travel-reuse", "77", "88");
        assert!(authorization.import_credential(signed(&key, &credential)?, "home-1")?);
        assert!(authorization.revoke_from_home(
            credential.credential_id,
            "rotated".to_owned(),
            "home-1"
        )?);

        let mut replay = credential.clone();
        replay.credential_id = Uuid::new_v4();
        assert!(
            authorization
                .import_credential(signed(&key, &replay)?, "home-1")
                .is_err()
        );
        fs::remove_dir_all(directory).ok();
        Ok(())
    }

    #[test]
    fn missing_authorization_state_fails_closed() -> Result<()> {
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate fixture key"))?;
        let missing = std::env::temp_dir().join(format!("flowsplice-missing-{}", Uuid::new_v4()));
        assert!(
            ServerAuthorization::load("deployment-1".to_owned(), authorities(&key), missing,)
                .is_err()
        );
        Ok(())
    }
}
