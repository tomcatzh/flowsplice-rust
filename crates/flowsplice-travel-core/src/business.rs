//! Exact, durable business enrollment and in-process startup.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use flowsplice_core::{
    authorization::unix_time_secs,
    business::{BUSINESS_VERSION, BusinessDescriptor, BusinessService, json_digest},
    deployment::SignedDeploymentTrust,
};
use flowsplice_enrollment::{
    business::{BUSINESS_BINDING_FILE, BusinessTravelResponse},
    load_json,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    RemoteEnrollmentOptions, RemoteEnrollmentProgress, ServiceBinding, TravelCore, read_public_key,
    require_trusted_root, sha256_hex, validate_bootstrap_trust_continuity,
};

pub(super) const INSTALL_JOURNAL_FILE: &str = "business-installation.pending.json";

/// Business applications select a private descriptor, never an interactive Home/service choice.
pub struct BusinessEnrollmentOptions {
    pub travel_id: String,
    pub install_dir: PathBuf,
    pub relay_address: String,
    pub deployment_root_public_key: String,
    pub private_key_password: String,
    pub wait_timeout_secs: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApprovedBusiness {
    pub travel_id: String,
    pub request_id: Uuid,
    pub credential_id: Uuid,
    pub binding: ServiceBinding,
    pub service: BusinessService,
    pub not_after_unix_secs: u64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct InstallJournal {
    pub response: flowsplice_enrollment::business::TravelResponseEnvelope,
    pub config_toml: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompletedBinding {
    pub version: u32,
    pub config_sha256: String,
    pub response: BusinessTravelResponse,
}

/// Enrolls and commits an exact business binding using the shared first-enrollment engine.
///
/// # Errors
/// Returns trust, input, password, transport, approval or durable-installation errors.
pub async fn enroll<F>(
    options: BusinessEnrollmentOptions,
    descriptor: BusinessDescriptor,
    on_progress: F,
) -> Result<()>
where
    F: Fn(RemoteEnrollmentProgress) + Send + Sync,
{
    if options
        .relay_address
        .parse::<std::net::SocketAddr>()
        .is_err()
    {
        bail!("business enrollment requires a Relay IP address and port");
    }
    let remote = RemoteEnrollmentOptions {
        travel_id: options.travel_id,
        home_id: descriptor.approving_home_id.clone(),
        install_dir: options.install_dir,
        bootstrap_config: None,
        trusted_deployment_root_public_key: Some(options.deployment_root_public_key),
        selected_relay: Some(options.relay_address),
        ui_listen: None,
        private_key_password: options.private_key_password,
        wait_timeout_secs: options.wait_timeout_secs,
        #[cfg(feature = "e2e-remote-ui")]
        test_allow_remote_listen: false,
        #[cfg(feature = "e2e-remote-ui")]
        test_admin_token: None,
    };
    crate::enroll_remote_inner(remote, Some(descriptor), on_progress).await
}

/// Loads a completed binding and verifies it against the private app descriptor and current trust.
///
/// # Errors
/// Returns an error for partial installation, altered files, a different descriptor or expired trust.
pub fn load_binding(
    config_path: &Path,
    root: &str,
    descriptor: &BusinessDescriptor,
) -> Result<ApprovedBusiness> {
    let directory = config_path
        .parent()
        .context("business configuration has no installation directory")?;
    let completed: CompletedBinding = load_json(&directory.join(BUSINESS_BINDING_FILE))?;
    validate_installed_binding(config_path, &completed, root, descriptor)
}

impl TravelCore {
    /// Starts the socket runtime only after exact business installation has completed.
    ///
    /// # Errors
    /// Returns binding, credential, password, trust or runtime initialization errors.
    pub async fn start_business(
        config_path: &Path,
        password: &str,
        root: &str,
        descriptor: &BusinessDescriptor,
    ) -> Result<(Self, ApprovedBusiness)> {
        let approved = load_binding(config_path, root, descriptor)?;
        let runtime = Self::start_in_process(config_path, password, root).await?;
        Ok((runtime, approved))
    }
}

pub(super) fn validate_installed_binding(
    config_path: &Path,
    completed: &CompletedBinding,
    root: &str,
    descriptor: &BusinessDescriptor,
) -> Result<ApprovedBusiness> {
    if completed.version != BUSINESS_VERSION
        || sha256_hex(&fs::read(config_path)?) != completed.config_sha256
        || json_digest(&completed.response.request.descriptor)? != json_digest(descriptor)?
    {
        bail!("installed business binding does not match this application and configuration");
    }
    let now = unix_time_secs()?;
    let (credential, response_trust) = completed.response.validate(root, now)?;
    let config = super::installation_paths::load(config_path)?;
    require_trusted_root(&read_public_key(&config.deployment_root_public_key)?, root)?;
    let installed_signed: SignedDeploymentTrust = load_json(&config.deployment_trust)?;
    let installed_trust = installed_signed.verify(root, now)?;
    validate_bootstrap_trust_continuity(
        &completed.response.response.deployment_trust,
        &response_trust,
        &installed_signed,
        &installed_trust,
    )?;
    let target = descriptor.verify(&installed_trust, now)?;
    completed.response.approval.verify(
        &installed_trust,
        completed
            .response
            .response
            .home_endpoint_credential
            .as_ref(),
        descriptor,
        &json_digest(&completed.response.request)?,
        &completed.response.response.signed_credential,
        now,
    )?;
    if config.id != credential.travel_id
        || config.homes.len() != 1
        || config.homes[0].id != target.home_id
        || fs::read_to_string(&config.management_cert)?
            != completed.response.response.management_certificate_pem
        || fs::read_to_string(&config.business_cert)?
            != completed.response.response.business_certificate_pem
    {
        bail!("installed business configuration or certificates changed their approved identity");
    }
    Ok(ApprovedBusiness {
        travel_id: credential.travel_id,
        request_id: credential.enrollment_request_id,
        credential_id: credential.credential_id,
        binding: ServiceBinding {
            home_id: target.home_id,
            service_id: target.service.service_id.clone(),
            protocol: target.service.protocol,
        },
        service: target.service,
        not_after_unix_secs: credential
            .not_after_unix_secs
            .min(target.not_after_unix_secs),
    })
}

pub(super) fn write_atomic_private(path: &Path, data: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .context("private installation file has no parent")?;
    let temporary = parent.join(format!(".business-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
