use std::{
    collections::{HashMap, HashSet},
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::{
    digest,
    signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    protocol::ServiceProtocol,
    tls::{PeerIdentity, validate_spki_pin},
};

pub const TRAVEL_CREDENTIAL_VERSION: u32 = 1;
pub const TRAVEL_CREDENTIAL_OBJECT_TYPE: &str = "flowsplice.travel_credential";

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TravelCredentialScope {
    Global,
    Home {
        home_id: String,
    },
    Service {
        home_id: String,
        service_id: String,
        protocol: ServiceProtocol,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TrustedTravelAuthority {
    Global {
        id: String,
        epoch: u64,
        home_id: String,
        public_key: String,
    },
    Home {
        id: String,
        epoch: u64,
        home_id: String,
        public_key: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TravelCredential {
    pub version: u32,
    pub object_type: String,
    pub deployment_id: String,
    pub deployment_trust_sha256: String,
    pub credential_id: Uuid,
    pub authority_id: String,
    pub authority_epoch: u64,
    pub enrollment_request_id: Uuid,
    pub enrollment_nonce: String,
    pub enrollment_request_sha256: String,
    pub travel_id: String,
    pub management_spki_sha256: String,
    pub business_spki_sha256: String,
    pub management_ca_sha256: String,
    pub business_ca_sha256: String,
    pub management_certificate_sha256: String,
    pub business_certificate_sha256: String,
    pub scope: TravelCredentialScope,
    pub not_before_unix_secs: u64,
    pub not_after_unix_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedTravelCredential {
    pub authority_id: String,
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
    pub snapshot_sha256: String,
    pub revoked_credentials: HashSet<Uuid>,
}

#[derive(Clone, Debug)]
pub struct VerifiedAuthorization {
    generation: u64,
    snapshot_sha256: String,
    credentials: HashMap<Uuid, TravelCredential>,
    management_index: HashMap<(String, String), Vec<Uuid>>,
    business_index: HashMap<(String, String), Vec<Uuid>>,
    revoked_credentials: HashSet<Uuid>,
}

impl SignedTravelCredential {
    /// Verifies the detached signature against one deployment-trusted authority.
    ///
    /// # Errors
    ///
    /// Returns an error when the public key, payload, signature, or credential is invalid.
    pub fn verify(&self, authority: &TrustedTravelAuthority) -> Result<TravelCredential> {
        if self.authority_id != authority.id() {
            bail!("signed Travel credential authority does not match the selected authority");
        }
        let credential = self.verify_public_key(authority.public_key())?;
        if credential.authority_id != self.authority_id {
            bail!("signed Travel credential payload has a different authority id");
        }
        if credential.authority_epoch != authority.epoch() {
            bail!("signed Travel credential uses the wrong authority epoch");
        }
        authority.validate_scope(&credential.scope)?;
        Ok(credential)
    }

    /// Verifies the signature with an explicitly supplied P-256 public key.
    ///
    /// This is used by the enrolling Travel device to detect a corrupt response. Runtime access
    /// decisions must instead use [`Self::verify`] with a deployment-trusted authority.
    ///
    /// # Errors
    ///
    /// Returns an error when the key, payload, signature, or credential is invalid.
    pub fn verify_public_key(&self, authority_public_key_hex: &str) -> Result<TravelCredential> {
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

impl TrustedTravelAuthority {
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Global { id, .. } | Self::Home { id, .. } => id,
        }
    }

    #[must_use]
    pub fn public_key(&self) -> &str {
        match self {
            Self::Global { public_key, .. } | Self::Home { public_key, .. } => public_key,
        }
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        match self {
            Self::Global { epoch, .. } | Self::Home { epoch, .. } => *epoch,
        }
    }

    #[must_use]
    pub fn home_id(&self) -> Option<&str> {
        match self {
            Self::Global { home_id, .. } | Self::Home { home_id, .. } => Some(home_id),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.id().is_empty() || self.epoch() == 0 {
            bail!("Travel authority id must be non-empty");
        }
        if self.home_id().is_some_and(str::is_empty) {
            bail!("Travel authority must name its publishing Home id");
        }
        validate_authority_public_key(self.public_key())
    }

    fn validate_scope(&self, scope: &TravelCredentialScope) -> Result<()> {
        match self {
            Self::Global { .. } => Ok(()),
            Self::Home { home_id, .. }
                if scope
                    .home_id()
                    .is_some_and(|scope_home| scope_home == home_id) =>
            {
                Ok(())
            }
            Self::Home { .. } => {
                bail!("Home Travel authority attempted to sign outside its own Home")
            }
        }
    }
}

/// Validates that deployment-trusted Travel authorities are usable and uniquely named.
///
/// # Errors
///
/// Returns an error for an empty set, malformed authority, or duplicate authority id.
pub fn validate_trusted_authorities(authorities: &[TrustedTravelAuthority]) -> Result<()> {
    if authorities.is_empty() {
        bail!("at least one trusted Travel authority is required");
    }
    let mut ids = HashSet::new();
    for authority in authorities {
        authority.validate()?;
        if !ids.insert(authority.id()) {
            bail!("duplicate Travel authority id {}", authority.id());
        }
    }
    Ok(())
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
        if self.version != TRAVEL_CREDENTIAL_VERSION
            || self.object_type != TRAVEL_CREDENTIAL_OBJECT_TYPE
            || self.deployment_id.is_empty()
            || self.credential_id.is_nil()
            || self.authority_id.is_empty()
            || self.authority_epoch == 0
            || self.enrollment_request_id.is_nil()
            || self.travel_id.is_empty()
        {
            bail!("Travel credential id, authority id, and Travel id must be non-empty");
        }
        self.scope.validate()?;
        validate_spki_pin(&self.management_spki_sha256, "Travel management")?;
        validate_spki_pin(&self.business_spki_sha256, "Travel business")?;
        for (digest, label) in [
            (&self.deployment_trust_sha256, "deployment trust"),
            (&self.enrollment_nonce, "enrollment nonce"),
            (&self.enrollment_request_sha256, "enrollment request"),
            (&self.management_ca_sha256, "management CA"),
            (&self.business_ca_sha256, "business CA"),
            (
                &self.management_certificate_sha256,
                "management certificate",
            ),
            (&self.business_certificate_sha256, "business certificate"),
        ] {
            validate_spki_pin(digest, label)?;
        }
        if self.not_before_unix_secs >= self.not_after_unix_secs {
            bail!("Travel credential validity interval is empty");
        }
        Ok(())
    }

    #[must_use]
    pub const fn active_at(&self, unix_secs: u64) -> bool {
        unix_secs >= self.not_before_unix_secs && unix_secs < self.not_after_unix_secs
    }

    #[must_use]
    pub fn allows_home(&self, home_id: &str) -> bool {
        self.scope.allows_home(home_id)
    }

    #[must_use]
    pub fn allows_service(
        &self,
        home_id: &str,
        service_id: &str,
        protocol: ServiceProtocol,
    ) -> bool {
        self.scope.allows_service(home_id, service_id, protocol)
    }
}

impl TravelCredentialScope {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Global => Ok(()),
            Self::Home { home_id } if !home_id.is_empty() => Ok(()),
            Self::Service {
                home_id,
                service_id,
                ..
            } if !home_id.is_empty() && !service_id.is_empty() => Ok(()),
            Self::Home { .. } => bail!("Home-scoped Travel credential has an empty Home id"),
            Self::Service { .. } => {
                bail!("service-scoped Travel credential has an empty Home or service id")
            }
        }
    }

    #[must_use]
    pub fn home_id(&self) -> Option<&str> {
        match self {
            Self::Global => None,
            Self::Home { home_id } | Self::Service { home_id, .. } => Some(home_id),
        }
    }

    #[must_use]
    pub fn allows_home(&self, home_id: &str) -> bool {
        match self {
            Self::Global => true,
            Self::Home {
                home_id: allowed_home,
            }
            | Self::Service {
                home_id: allowed_home,
                ..
            } => allowed_home == home_id,
        }
    }

    #[must_use]
    pub fn allows_service(
        &self,
        home_id: &str,
        service_id: &str,
        protocol: ServiceProtocol,
    ) -> bool {
        match self {
            Self::Global => true,
            Self::Home {
                home_id: allowed_home,
            } => allowed_home == home_id,
            Self::Service {
                home_id: allowed_home,
                service_id: allowed_service,
                protocol: allowed_protocol,
            } => {
                allowed_home == home_id
                    && allowed_service == service_id
                    && *allowed_protocol == protocol
            }
        }
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
        authorities: &[TrustedTravelAuthority],
        deployment_id: &str,
    ) -> Result<Self> {
        if deployment_id.is_empty() {
            bail!("Travel authorization deployment id must be non-empty");
        }
        if snapshot.generation == 0 {
            bail!("Travel authorization generation must be positive");
        }
        validate_trusted_authorities(authorities)?;
        let authorities_by_id = authorities
            .iter()
            .map(|authority| (authority.id(), authority))
            .collect::<HashMap<_, _>>();
        let mut credentials = HashMap::new();
        let mut management_index = HashMap::new();
        let mut business_index = HashMap::new();
        for signed in &snapshot.credentials {
            let authority = authorities_by_id
                .get(signed.authority_id.as_str())
                .ok_or_else(|| anyhow!("unknown Travel authority {}", signed.authority_id))?;
            let credential = signed.verify(authority)?;
            if credential.deployment_id != deployment_id {
                bail!("Travel credential belongs to a different deployment");
            }
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
            management_index
                .entry(management_key)
                .or_insert_with(Vec::new)
                .push(credential_id);
            let business_key = (
                credential.travel_id.clone(),
                credential.business_spki_sha256.to_ascii_lowercase(),
            );
            business_index
                .entry(business_key)
                .or_insert_with(Vec::new)
                .push(credential_id);
        }
        let mut revoked_credentials = HashSet::new();
        for revocation in &snapshot.revocations {
            if revocation.revoked_at_unix_secs == 0
                || revocation.reason.is_empty()
                || revocation.reason.len() > 256
            {
                bail!("Travel revocation has an invalid time or reason");
            }
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
        let snapshot_bytes = serde_json::to_vec(snapshot)
            .context("failed to encode Travel authorization snapshot")?;
        Ok(Self {
            generation: snapshot.generation,
            snapshot_sha256: hex::encode(digest::digest(&digest::SHA256, &snapshot_bytes).as_ref()),
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
    pub fn snapshot_sha256(&self) -> &str {
        &self.snapshot_sha256
    }

    #[must_use]
    pub fn revoked_credentials(&self) -> &HashSet<Uuid> {
        &self.revoked_credentials
    }

    #[must_use]
    pub fn credential(&self, credential_id: Uuid) -> Option<&TravelCredential> {
        self.credentials.get(&credential_id)
    }

    pub fn credentials(&self) -> impl Iterator<Item = &TravelCredential> {
        self.credentials.values()
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
        self.authorize_all(identity, unix_secs, &self.management_index)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Travel certificate is not covered by an active credential"))
    }

    /// Resolves a management TLS identity to every active signed credential for that identity.
    ///
    /// # Errors
    ///
    /// Returns an error when no active credential binds the Travel identity and SPKI.
    pub fn authorize_management_all(
        &self,
        identity: &PeerIdentity,
        unix_secs: u64,
    ) -> Result<Vec<&TravelCredential>> {
        self.authorize_all(identity, unix_secs, &self.management_index)
    }

    /// Selects one active management credential that permits routing to `home_id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity has no active authorization for the Home.
    pub fn authorize_management_for_home(
        &self,
        identity: &PeerIdentity,
        home_id: &str,
        unix_secs: u64,
    ) -> Result<&TravelCredential> {
        self.authorize_management_all(identity, unix_secs)?
            .into_iter()
            .filter(|credential| credential.allows_home(home_id))
            .max_by_key(|credential| credential.not_after_unix_secs)
            .ok_or_else(|| anyhow!("Travel identity is not authorized for Home {home_id}"))
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
        self.authorize_all(identity, unix_secs, &self.business_index)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("Travel certificate is not covered by an active credential"))
    }

    /// Resolves a business identity against the exact credential selected for a route.
    ///
    /// # Errors
    ///
    /// Returns an error when the selected credential is inactive or does not bind the identity.
    pub fn authorize_business_credential(
        &self,
        identity: &PeerIdentity,
        credential_id: Uuid,
        unix_secs: u64,
    ) -> Result<&TravelCredential> {
        self.authorize_all(identity, unix_secs, &self.business_index)?
            .into_iter()
            .find(|credential| credential.credential_id == credential_id)
            .ok_or_else(|| {
                anyhow!("business certificate does not match the routed Travel credential")
            })
    }

    fn authorize_all<'a>(
        &'a self,
        identity: &PeerIdentity,
        unix_secs: u64,
        index: &HashMap<(String, String), Vec<Uuid>>,
    ) -> Result<Vec<&'a TravelCredential>> {
        let key = (
            identity.id.clone(),
            identity.spki_sha256.to_ascii_lowercase(),
        );
        let credential_ids = index
            .get(&key)
            .ok_or_else(|| anyhow!("Travel certificate is not covered by a signed credential"))?;
        if !identity.active_at(unix_secs) {
            bail!("Travel certificate is expired or not yet valid");
        }
        let active = credential_ids
            .iter()
            .filter(|credential_id| self.is_active(**credential_id, unix_secs))
            .map(|credential_id| {
                self.credentials
                    .get(credential_id)
                    .ok_or_else(|| anyhow!("Travel credential index is inconsistent"))
            })
            .collect::<Result<Vec<_>>>()?;
        if active.is_empty() {
            bail!("Travel credential is revoked, expired, or not yet valid");
        }
        Ok(active)
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
        if self.generation != 0
            && authorization.generation() == self.generation
            && authorization.snapshot_sha256() != self.snapshot_sha256
        {
            bail!("Travel authorization content changed without a generation increase");
        }
        if !self
            .revoked_credentials
            .is_subset(authorization.revoked_credentials())
        {
            bail!("Travel authorization snapshot removed a prior revocation");
        }
        Ok(Self {
            generation: authorization.generation(),
            snapshot_sha256: authorization.snapshot_sha256().to_owned(),
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
            authority_id: credential.authority_id.clone(),
            payload_hex: hex::encode(payload),
            signature_hex: hex::encode(signature.as_ref()),
        })
    }

    fn fixture() -> Result<(
        TrustedTravelAuthority,
        TravelCredential,
        SignedTravelCredential,
    )> {
        let key = EcdsaKeyPair::generate(&ECDSA_P256_SHA256_ASN1_SIGNING)
            .map_err(|_| anyhow!("failed to generate test key"))?;
        let credential = TravelCredential {
            version: TRAVEL_CREDENTIAL_VERSION,
            object_type: TRAVEL_CREDENTIAL_OBJECT_TYPE.to_owned(),
            deployment_id: "deployment-1".to_owned(),
            deployment_trust_sha256: "aa".repeat(32),
            credential_id: Uuid::new_v4(),
            authority_id: "home-1-authority".to_owned(),
            authority_epoch: 1,
            enrollment_request_id: Uuid::new_v4(),
            enrollment_nonce: "aa".repeat(32),
            enrollment_request_sha256: "bb".repeat(32),
            travel_id: "travel-1".to_owned(),
            management_spki_sha256: "11".repeat(32),
            business_spki_sha256: "22".repeat(32),
            management_ca_sha256: "cc".repeat(32),
            business_ca_sha256: "dd".repeat(32),
            management_certificate_sha256: "ee".repeat(32),
            business_certificate_sha256: "ff".repeat(32),
            scope: TravelCredentialScope::Home {
                home_id: "home-1".to_owned(),
            },
            not_before_unix_secs: 100,
            not_after_unix_secs: 200,
        };
        let signed = signed(&key, &credential)?;
        Ok((
            TrustedTravelAuthority::Home {
                id: "home-1-authority".to_owned(),
                epoch: 1,
                home_id: "home-1".to_owned(),
                public_key: hex::encode(key.public_key().as_ref()),
            },
            credential,
            signed,
        ))
    }

    #[test]
    fn signed_credentials_bind_both_tls_identities_and_validity() -> Result<()> {
        let (authority, credential, signed) = fixture()?;
        let snapshot = TravelAuthorizationSnapshot {
            generation: 1,
            credentials: vec![signed],
            revocations: vec![],
        };
        let authorization = VerifiedAuthorization::verify(&snapshot, &[authority], "deployment-1")?;
        let management = PeerIdentity {
            role: Role::Travel,
            id: credential.travel_id.clone(),
            certificate_sha256: credential.management_certificate_sha256.clone(),
            spki_sha256: credential.management_spki_sha256.clone(),
            not_before_unix_secs: 100,
            not_after_unix_secs: 200,
        };
        let business = PeerIdentity {
            role: Role::Travel,
            id: credential.travel_id.clone(),
            certificate_sha256: credential.business_certificate_sha256.clone(),
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

        // A later credential for the same Travel keys may carry freshly issued leaf
        // certificates. Runtime authorization therefore follows the signed Travel ID +
        // SPKI, while the exact leaf hashes remain bound into each enrollment response
        // to prevent response splicing during import.
        let mut reissued_management_leaf = management.clone();
        reissued_management_leaf.certificate_sha256 = "aa".repeat(32);
        assert_eq!(
            authorization
                .authorize_management(&reissued_management_leaf, 150)?
                .credential_id,
            credential.credential_id
        );
        Ok(())
    }

    #[test]
    fn tampering_revocation_and_rollback_fail_closed() -> Result<()> {
        let (authority, credential, mut signed) = fixture()?;
        signed.payload_hex.replace_range(0..2, "00");
        assert!(signed.verify(&authority).is_err());

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
                &[authority],
                "deployment-1",
            )
            .is_err()
        );

        let (authority, credential, valid) = fixture()?;
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
            std::slice::from_ref(&authority),
            "deployment-1",
        )?;
        let cache = AuthorizationCache::default().accept(&revoked)?;
        let rollback = VerifiedAuthorization::verify(
            &TravelAuthorizationSnapshot {
                generation: 2,
                credentials: vec![valid],
                revocations: vec![],
            },
            &[authority],
            "deployment-1",
        )?;
        assert!(cache.accept(&rollback).is_err());
        let (same_authority, credential, valid) = fixture()?;
        let same_generation_different_content = VerifiedAuthorization::verify(
            &TravelAuthorizationSnapshot {
                generation: 3,
                credentials: vec![valid],
                revocations: vec![TravelRevocation {
                    credential_id: credential.credential_id,
                    revoked_at_unix_secs: 151,
                    reason: "different signed state".to_owned(),
                }],
            },
            std::slice::from_ref(&same_authority),
            "deployment-1",
        )?;
        assert!(cache.accept(&same_generation_different_content).is_err());
        Ok(())
    }
}
