//! One durable application identity and verified discovery of matching Business Homes.
use crate::{
    RemoteEnrollmentOptions, RemoteEnrollmentProgress, ServiceBinding, TravelCore,
    business::BusinessEnrollmentOptions, read_public_key, require_trusted_root, sha256_hex,
    validate_bootstrap_trust_continuity,
};
use anyhow::{Context, Result, bail};
use flowsplice_core::{
    authorization::unix_time_secs,
    business::{BUSINESS_VERSION, BusinessService, ServiceClassDescriptor},
    deployment::{DeploymentTrust, SignedDeploymentTrust},
    protocol::Catalog,
};
use flowsplice_enrollment::{business::ServiceClassTravelResponse, load_json};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
use uuid::Uuid;

pub const BINDING_FILE: &str = "approved-service-class-binding.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EnrollmentIntent {
    pub descriptor: ServiceClassDescriptor,
    pub label: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CompletedBinding {
    pub version: u32,
    pub config_sha256: String,
    pub response: ServiceClassTravelResponse,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApprovedServiceClass {
    pub travel_id: String,
    pub request_id: Uuid,
    pub credential_id: Uuid,
    pub not_after_unix_secs: u64,
    pub descriptor: ServiceClassDescriptor,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ServiceClassTarget {
    pub id: String,
    pub home_id: String,
    pub name: String,
    pub service_id: String,
    pub service: BusinessService,
}

/// Enrolls one class intent through the existing resumable remote-enrollment engine.
/// # Errors
/// Returns invalid inputs, trust, approval, password or durable installation errors.
pub async fn enroll<F>(
    options: BusinessEnrollmentOptions,
    descriptor: ServiceClassDescriptor,
    label: String,
    on_progress: F,
) -> Result<()>
where
    F: Fn(RemoteEnrollmentProgress) + Send + Sync,
{
    descriptor.validate()?;
    if options
        .relay_address
        .parse::<std::net::SocketAddr>()
        .is_err()
    {
        bail!("service-class enrollment requires a Relay IP address and port");
    }
    if label.trim().is_empty()
        || label.len() > 64
        || label.chars().any(|c| {
            c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
    {
        bail!("invalid service-class device label");
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
    crate::enroll_remote_intent(
        remote,
        None,
        Some(EnrollmentIntent { descriptor, label }),
        on_progress,
    )
    .await
}

/// Validates installed identity and the original signed class intent.
/// # Errors
/// Rejects changed files, narrower/wider scope, expired authorization or mismatched trust.
pub fn load_binding(
    config: &Path,
    root: &str,
    descriptor: &ServiceClassDescriptor,
) -> Result<ApprovedServiceClass> {
    let parent = config
        .parent()
        .context("class installation has no parent directory")?;
    let completed: CompletedBinding = load_json(&parent.join(BINDING_FILE))?;
    validate_installed_binding(config, &completed, root, descriptor)
}

pub(super) fn validate_installed_binding(
    config_path: &Path,
    completed: &CompletedBinding,
    root: &str,
    descriptor: &ServiceClassDescriptor,
) -> Result<ApprovedServiceClass> {
    if completed.version != BUSINESS_VERSION
        || sha256_hex(&fs::read(config_path)?) != completed.config_sha256
        || &completed.response.request.descriptor != descriptor
    {
        bail!("class installation does not match this application and configuration");
    }
    let now = unix_time_secs()?;
    let (credential, response_trust) = completed.response.validate(root, now)?;
    let config = super::installation_paths::load(config_path)?;
    require_trusted_root(&read_public_key(&config.deployment_root_public_key)?, root)?;
    let signed: SignedDeploymentTrust = load_json(&config.deployment_trust)?;
    let trust = signed.verify(root, now)?;
    validate_bootstrap_trust_continuity(
        &completed.response.response.deployment_trust,
        &response_trust,
        &signed,
        &trust,
    )?;
    completed.response.approval.verify(
        &trust,
        completed
            .response
            .response
            .home_endpoint_credential
            .as_ref(),
        descriptor,
        &flowsplice_core::business::json_digest(&completed.response.request)?,
        &completed.response.response.signed_credential,
        now,
    )?;
    if config.id != credential.travel_id
        || config.service_class.as_ref() != Some(descriptor)
        || !config.homes.is_empty()
        || !config.mappings.is_empty()
        || fs::read_to_string(&config.management_cert)?
            != completed.response.response.management_certificate_pem
        || fs::read_to_string(&config.business_cert)?
            != completed.response.response.business_certificate_pem
    {
        bail!("class installation changed its identity, scope or socket-only configuration");
    }
    Ok(ApprovedServiceClass {
        travel_id: credential.travel_id,
        request_id: credential.enrollment_request_id,
        credential_id: credential.credential_id,
        not_after_unix_secs: credential.not_after_unix_secs,
        descriptor: descriptor.clone(),
    })
}

impl TravelCore {
    /// Starts one listener-free transport shared by all matching Home sockets.
    /// # Errors
    /// Returns installation, credential, trust or startup errors.
    pub async fn start_service_class(
        config: &Path,
        password: &str,
        root: &str,
        descriptor: &ServiceClassDescriptor,
    ) -> Result<(Self, ApprovedServiceClass)> {
        let approved = load_binding(config, root, descriptor)?;
        let runtime = Self::start_in_process(config, password, root).await?;
        Ok((runtime, approved))
    }

    /// Discovers targets from one consistent, previously accepted signed snapshot.
    /// # Errors
    /// Returns expired class identity, stopped runtime, or invalid snapshot errors.
    pub async fn service_class_targets(
        &self,
        approved: &ApprovedServiceClass,
    ) -> Result<Vec<ServiceClassTarget>> {
        if self.stopped.load(std::sync::atomic::Ordering::Acquire)
            || self.state.config.service_class.as_ref() != Some(&approved.descriptor)
            || self.state.config.id != approved.travel_id
            || unix_time_secs()? >= approved.not_after_unix_secs
        {
            bail!("service-class application authorization is unavailable or expired");
        }
        self.class_targets(&approved.descriptor).await
    }

    async fn class_targets(
        &self,
        descriptor: &ServiceClassDescriptor,
    ) -> Result<Vec<ServiceClassTarget>> {
        let snapshot = self
            .state
            .control_trust_state
            .lock()
            .await
            .cached_snapshot
            .clone();
        let Some(snapshot) = snapshot else {
            return Ok(Vec::new());
        };
        // A control outage must not tear down already-authorized sockets. This snapshot
        // already passed subject/high-water checks; verify its signature at issuance,
        // then apply current credential/endpoint/grant validity below. New sockets still
        // require a fresh route authorization through the normal shared transport.
        let verified =
            snapshot.verify_at_issuance_for_migration(&self.state.deployment_root_public_key)?;
        crate::require_control_snapshot_subject(
            &verified,
            &self.state.config.id,
            &self.state.management_spki_sha256,
        )?;
        targets(
            &verified.payload.catalog,
            &verified.trust,
            descriptor,
            unix_time_secs()?,
        )
    }

    pub(super) async fn class_binding_allowed(&self, binding: &ServiceBinding) -> Result<bool> {
        let Some(descriptor) = &self.state.config.service_class else {
            return Ok(false);
        };
        Ok(self.class_targets(descriptor).await?.iter().any(|target| {
            target.home_id == binding.home_id
                && target.service_id == binding.service_id
                && target.service.protocol == binding.protocol
        }))
    }
}

pub(super) async fn home_business_pins(
    state: &crate::AppState,
    descriptor: &ServiceClassDescriptor,
    home_id: &str,
    service_id: &str,
    protocol: flowsplice_core::protocol::ServiceProtocol,
) -> Result<Vec<String>> {
    let snapshot = state
        .control_trust_state
        .lock()
        .await
        .cached_snapshot
        .clone()
        .context("service-class catalog is not available")?;
    let verified = snapshot.verify_at_issuance_for_migration(&state.deployment_root_public_key)?;
    crate::require_control_snapshot_subject(
        &verified,
        &state.config.id,
        &state.management_spki_sha256,
    )?;
    let now = unix_time_secs()?;
    if !targets(&verified.payload.catalog, &verified.trust, descriptor, now)?
        .iter()
        .any(|target| {
            target.home_id == home_id
                && target.service_id == service_id
                && target.service.protocol == protocol
        })
    {
        bail!("service is not in the authorized class catalog");
    }
    let endpoint = verified
        .payload
        .catalog
        .homes
        .iter()
        .find(|home| home.home_id == home_id)
        .and_then(|home| home.endpoint_credential.as_ref());
    crate::trusted_home_business_pins(&verified.trust, home_id, endpoint, now)
}

fn targets(
    catalog: &Catalog,
    trust: &DeploymentTrust,
    descriptor: &ServiceClassDescriptor,
    now: u64,
) -> Result<Vec<ServiceClassTarget>> {
    descriptor.validate()?;
    if now < trust.not_before_unix_secs || now >= trust.not_after_unix_secs {
        bail!("service-class deployment trust expired");
    }
    let mut targets = Vec::new();
    for home in &catalog.homes {
        let (Some(signed), Some(endpoint)) = (&home.service_grant, &home.endpoint_credential)
        else {
            continue;
        };
        let Ok(grant) = signed.verify(trust, endpoint, now) else {
            continue;
        };
        if grant.home_id != home.home_id || grant.validate_catalog_services(&home.services).is_err()
        {
            continue;
        }
        let matching: Vec<_> = home
            .services
            .iter()
            .filter_map(|service| {
                grant
                    .services
                    .iter()
                    .find(|approved| {
                        approved.service_id == service.id
                            && descriptor.scope().allows_business_service(
                                &home.home_id,
                                &service.id,
                                service.protocol,
                                Some(approved),
                            )
                    })
                    .map(|approved| (service, approved))
            })
            .collect();
        for (service, approved) in &matching {
            targets.push(ServiceClassTarget {
                id: serde_json::to_string(&(&home.home_id, &service.id))?,
                home_id: home.home_id.clone(),
                name: if matching.len() > 1 {
                    format!("{} · {}", home.home_alias, service.alias)
                } else {
                    home.home_alias.clone()
                },
                service_id: service.id.clone(),
                service: (*approved).clone(),
            });
        }
    }
    Ok(targets)
}

#[cfg(test)]
#[path = "service_class_tests.rs"]
mod tests;
