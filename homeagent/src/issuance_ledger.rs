use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use aws_lc_rs::digest;
use flowsplice_core::authorization::TravelCredentialScope;
use flowsplice_enrollment::{TravelEnrollmentRequest, TravelEnrollmentResponse};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const LEDGER_VERSION: u32 = 1;
pub const ISSUANCE_LEDGER_FILE: &str = ".flowsplice-issued-enrollments.json";

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IssuanceRecord {
    request_id: Uuid,
    request_sha256: String,
    authority_id: String,
    scope: TravelCredentialScope,
    valid_for_secs: u64,
    enrollment: TravelEnrollmentResponse,
    published_generation: Option<u64>,
}

impl IssuanceRecord {
    #[must_use]
    pub const fn enrollment(&self) -> &TravelEnrollmentResponse {
        &self.enrollment
    }

    #[must_use]
    pub const fn published_generation(&self) -> Option<u64> {
        self.published_generation
    }

    #[must_use]
    pub const fn credential_id(&self) -> Uuid {
        self.enrollment.approval.credential_id
    }

    #[must_use]
    pub fn matches_intent(
        &self,
        authority_id: &str,
        scope: &TravelCredentialScope,
        valid_for_secs: u64,
    ) -> bool {
        self.authority_id == authority_id
            && &self.scope == scope
            && self.valid_for_secs == valid_for_secs
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LedgerState {
    version: u32,
    records: Vec<IssuanceRecord>,
}

pub struct IssuanceLedger {
    path: PathBuf,
    state: LedgerState,
}

impl IssuanceLedger {
    pub fn load(path: PathBuf) -> Result<Self> {
        let state =
            if path.exists() {
                serde_json::from_slice(&fs::read(&path).with_context(|| {
                    format!("failed to read issuance ledger {}", path.display())
                })?)
                .with_context(|| format!("failed to parse issuance ledger {}", path.display()))?
            } else {
                LedgerState {
                    version: LEDGER_VERSION,
                    records: Vec::new(),
                }
            };
        validate_state(&state)?;
        Ok(Self { path, state })
    }

    pub fn find(&self, request: &TravelEnrollmentRequest) -> Result<Option<IssuanceRecord>> {
        let request_sha256 = request_fingerprint(request)?;
        for record in &self.state.records {
            if record.request_id == request.request_id && record.request_sha256 != request_sha256 {
                bail!("enrollment request id was reused with different request content");
            }
        }
        Ok(self
            .state
            .records
            .iter()
            .find(|record| record.request_sha256 == request_sha256)
            .cloned())
    }

    pub fn insert_pending(
        &mut self,
        request: &TravelEnrollmentRequest,
        authority_id: &str,
        scope: &TravelCredentialScope,
        valid_for_secs: u64,
        enrollment: TravelEnrollmentResponse,
    ) -> Result<IssuanceRecord> {
        if self.find(request)?.is_some() {
            bail!("issuance ledger already contains this enrollment request");
        }
        let record = IssuanceRecord {
            request_id: request.request_id,
            request_sha256: request_fingerprint(request)?,
            authority_id: authority_id.to_owned(),
            scope: scope.clone(),
            valid_for_secs,
            enrollment,
            published_generation: None,
        };
        validate_record(&record)?;
        self.state.records.push(record.clone());
        self.persist()?;
        Ok(record)
    }

    pub fn mark_published(&mut self, credential_id: Uuid, generation: u64) -> Result<()> {
        if generation == 0 {
            bail!("published authorization generation must be non-zero");
        }
        let record = self
            .state
            .records
            .iter_mut()
            .find(|record| record.credential_id() == credential_id)
            .ok_or_else(|| {
                anyhow!("issuance ledger does not contain credential {credential_id}")
            })?;
        record.published_generation = Some(generation);
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        store_private_json_atomic(&self.path, &self.state)
    }
}

pub fn ledger_path(management_ca_key: &Path) -> Result<PathBuf> {
    let directory = management_ca_key
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = fs::canonicalize(directory).context("failed to resolve issuer directory")?;
    Ok(directory.join(ISSUANCE_LEDGER_FILE))
}

fn request_fingerprint(request: &TravelEnrollmentRequest) -> Result<String> {
    let encoded = serde_json::to_vec(request).context("failed to encode enrollment request")?;
    Ok(hex::encode(
        digest::digest(&digest::SHA256, &encoded).as_ref(),
    ))
}

fn validate_state(state: &LedgerState) -> Result<()> {
    if state.version != LEDGER_VERSION {
        bail!("unsupported issuance ledger version");
    }
    let mut credential_ids = HashSet::new();
    let mut request_fingerprints = HashSet::new();
    let mut requests = std::collections::HashMap::new();
    for record in &state.records {
        validate_record(record)?;
        if !credential_ids.insert(record.credential_id()) {
            bail!("issuance ledger contains a duplicate credential id");
        }
        if !request_fingerprints.insert(record.request_sha256.as_str()) {
            bail!("issuance ledger contains a duplicate enrollment request");
        }
        if requests
            .insert(record.request_id, record.request_sha256.as_str())
            .is_some_and(|fingerprint| fingerprint != record.request_sha256)
        {
            bail!("issuance ledger reuses a request id for different content");
        }
    }
    Ok(())
}

fn validate_record(record: &IssuanceRecord) -> Result<()> {
    let approval = &record.enrollment.approval;
    if record.request_id.is_nil()
        || record.request_sha256.len() != 64
        || record.authority_id.is_empty()
        || record.valid_for_secs == 0
        || approval.credential_id.is_nil()
        || approval.request.request_id != record.request_id
        || approval.authority_id != record.authority_id
        || approval.scope != record.scope
        || request_fingerprint(&approval.request)? != record.request_sha256
        || record.published_generation == Some(0)
    {
        bail!("issuance ledger contains an invalid record");
    }
    Ok(())
}

fn store_private_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow!("issuance ledger path has no file name"))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        let mut bytes = serde_json::to_vec_pretty(value).context("failed to encode ledger")?;
        bytes.push(b'\n');
        file.write_all(&bytes)?;
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
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    use flowsplice_core::authorization::SignedTravelCredential;
    use flowsplice_enrollment::{ENROLLMENT_VERSION, TravelEnrollmentApproval};

    fn request(request_id: Uuid) -> TravelEnrollmentRequest {
        TravelEnrollmentRequest {
            version: ENROLLMENT_VERSION,
            request_id,
            nonce: "aa".repeat(32),
            travel_id: "travel-test".to_owned(),
            created_at_unix_secs: 1_800_000_000,
            management_csr_pem: "management csr".to_owned(),
            business_csr_pem: "business csr".to_owned(),
        }
    }

    fn response(
        request: TravelEnrollmentRequest,
        authority_id: &str,
        scope: TravelCredentialScope,
    ) -> TravelEnrollmentResponse {
        TravelEnrollmentResponse {
            version: ENROLLMENT_VERSION,
            approval: TravelEnrollmentApproval {
                version: ENROLLMENT_VERSION,
                credential_id: Uuid::new_v4(),
                authority_id: authority_id.to_owned(),
                scope,
                request,
                not_before_unix_secs: 1_799_999_700,
                not_after_unix_secs: 1_800_003_600,
            },
            deployment_trust: flowsplice_core::deployment::SignedDeploymentTrust {
                payload_hex: "deployment trust".to_owned(),
                signature_hex: "root signature".to_owned(),
            },
            management_certificate_pem: "management certificate".to_owned(),
            business_certificate_pem: "business certificate".to_owned(),
            signed_credential: SignedTravelCredential {
                authority_id: authority_id.to_owned(),
                payload_hex: "payload".to_owned(),
                signature_hex: "signature".to_owned(),
            },
        }
    }

    #[test]
    fn enrollment_request_is_single_use_and_survives_restart() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let path = temporary.path().join(ISSUANCE_LEDGER_FILE);
        let mut ledger = IssuanceLedger::load(path.clone())?;
        let request = request(Uuid::new_v4());
        let global = TravelCredentialScope::Global;
        let enrollment = response(request.clone(), "global-authority", global.clone());
        let credential_id = enrollment.approval.credential_id;
        ledger.insert_pending(&request, "global-authority", &global, 3_600, enrollment)?;
        drop(ledger);

        let mut recovered = IssuanceLedger::load(path.clone())?;
        let Some(pending) = recovered.find(&request)? else {
            bail!("pending enrollment request should survive reload");
        };
        assert_eq!(pending.published_generation(), None);
        recovered.mark_published(credential_id, 7)?;

        let reloaded = IssuanceLedger::load(path.clone())?;
        let Some(existing) = reloaded.find(&request)? else {
            bail!("issued enrollment request should exist");
        };
        assert_eq!(existing.credential_id(), credential_id);
        assert_eq!(existing.published_generation(), Some(7));
        assert!(existing.matches_intent("global-authority", &global, 3_600));
        assert!(!existing.matches_intent(
            "home-authority",
            &TravelCredentialScope::Home {
                home_id: "home-1".to_owned(),
            },
            3_600,
        ));
        assert_eq!(fs::metadata(path)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn request_id_cannot_be_reused_with_different_content() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let mut ledger = IssuanceLedger::load(temporary.path().join(ISSUANCE_LEDGER_FILE))?;
        let original = request(Uuid::new_v4());
        let scope = TravelCredentialScope::Global;
        ledger.insert_pending(
            &original,
            "global-authority",
            &scope,
            3_600,
            response(original.clone(), "global-authority", scope.clone()),
        )?;
        let mut changed = original;
        changed.travel_id = "different-travel".to_owned();
        assert!(ledger.find(&changed).is_err());
        Ok(())
    }
}
