//! Directed enrollment wrappers carried by the existing opaque enrollment transport.

use std::ops::Deref;

use anyhow::{Result, anyhow, bail};
use aws_lc_rs::signature::{ECDSA_P256_SHA256_ASN1_SIGNING, EcdsaKeyPair};
use flowsplice_core::{
    authorization::{TravelCredential, TravelCredentialScope},
    business::{
        BUSINESS_APPROVAL_TYPE, BUSINESS_VERSION, BusinessDescriptor, BusinessService,
        BusinessTravelApproval, HOME_SERVICE_GRANT_TYPE, HomeServiceGrant,
        SERVICE_CLASS_APPROVAL_TYPE, ServiceClassApproval, ServiceClassDescriptor,
        SignedBusinessTravelApproval, SignedHomeServiceGrant, SignedServiceClassApproval,
        json_digest, payload_digest, validate_services,
    },
    deployment::DeploymentTrust,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::{
    TravelEnrollmentApproval, TravelEnrollmentRequest, TravelEnrollmentResponse,
    home::{
        HomeEnrollmentApproval, HomeEnrollmentProfile, HomeEnrollmentRequest,
        HomeEnrollmentResponse, HomeIssuerMaterial, issue_home_enrollment,
        parse_home_enrollment_request, validate_home_enrollment_response,
    },
    issuer::{IssuerMaterial, ProtectedKey, issue_enrollment},
    key, parse_enrollment_request, validate_enrollment_response,
};

pub const BUSINESS_HOME_REQUEST_TYPE: &str = "flowsplice.business_home_request";
pub const BUSINESS_HOME_RESPONSE_TYPE: &str = "flowsplice.business_home_response";
pub const BUSINESS_TRAVEL_REQUEST_TYPE: &str = "flowsplice.business_travel_request";
pub const BUSINESS_TRAVEL_RESPONSE_TYPE: &str = "flowsplice.business_travel_response";
pub const HOME_SERVICE_GRANT_FILE: &str = "home-service-grant.json";
pub const SERVICE_CLASS_TRAVEL_REQUEST_TYPE: &str = "flowsplice.service_class_travel_request";
pub const SERVICE_CLASS_TRAVEL_RESPONSE_TYPE: &str = "flowsplice.service_class_travel_response";
pub const BUSINESS_BINDING_FILE: &str = "approved-business-binding.json";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessHomeRequest {
    pub version: u32,
    pub object_type: String,
    pub request: HomeEnrollmentRequest,
    pub services: Vec<BusinessService>,
}

impl BusinessHomeRequest {
    /// Validates requested services and the unchanged Home CSR proof of possession.
    ///
    /// # Errors
    /// Returns an error for malformed, stale or unsupported requests.
    pub fn validate(&self, now: u64) -> Result<()> {
        validate_kind(self.version, &self.object_type, BUSINESS_HOME_REQUEST_TYPE)?;
        validate_services(&self.services)?;
        parse_home_enrollment_request(&self.request, now)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessHomeResponse {
    pub version: u32,
    pub object_type: String,
    pub request: BusinessHomeRequest,
    pub response: HomeEnrollmentResponse,
    pub grant: SignedHomeServiceGrant,
}

impl BusinessHomeResponse {
    /// Validates TLS identity, requested service subset and the independent signed grant.
    ///
    /// # Errors
    /// Returns an error if any identity, profile, service or signature was changed.
    pub fn validate(&self, root: &str, now: u64) -> Result<DeploymentTrust> {
        validate_kind(self.version, &self.object_type, BUSINESS_HOME_RESPONSE_TYPE)?;
        self.request
            .validate(self.request.request.created_at_unix_secs)?;
        if self.response.approval.request != self.request.request
            || self.response.approval.profile != HomeEnrollmentProfile::ServingOnly
            || self.response.issuer_bundle.is_some()
        {
            bail!(
                "business Home response must install exactly the requested serving-only identity"
            );
        }
        let (_, trust) = validate_home_enrollment_response(&self.response, root, now)?;
        let grant = self
            .grant
            .verify(&trust, &self.response.signed_endpoint_credential, now)?;
        if grant.enrollment_request_sha256 != json_digest(&self.request)? {
            bail!("Home service grant does not cover the complete business request");
        }
        validate_subset(&self.request.services, &grant.services)?;
        Ok(trust)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessTravelRequest {
    pub version: u32,
    pub object_type: String,
    pub request: TravelEnrollmentRequest,
    pub descriptor: BusinessDescriptor,
}

impl BusinessTravelRequest {
    /// Validates the CSR and independently trusted, exact business destination.
    ///
    /// # Errors
    /// Returns an error for an invalid CSR, descriptor, or approval destination.
    pub fn validate(&self, trust: &DeploymentTrust, approving_home: &str, now: u64) -> Result<()> {
        validate_kind(
            self.version,
            &self.object_type,
            BUSINESS_TRAVEL_REQUEST_TYPE,
        )?;
        parse_enrollment_request(&self.request, now)?;
        let descriptor = self.descriptor.verify(trust, now)?;
        if descriptor.approving_home_id != approving_home {
            bail!("directed Travel enrollment targets a different approving Home");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BusinessTravelResponse {
    pub version: u32,
    pub object_type: String,
    pub request: BusinessTravelRequest,
    pub response: TravelEnrollmentResponse,
    pub approval: SignedBusinessTravelApproval,
}

impl BusinessTravelResponse {
    /// Validates both the unchanged enrollment response and the complete business intent.
    ///
    /// # Errors
    /// Returns an error for any untrusted or widened response.
    pub fn validate(&self, root: &str, now: u64) -> Result<(TravelCredential, DeploymentTrust)> {
        validate_kind(
            self.version,
            &self.object_type,
            BUSINESS_TRAVEL_RESPONSE_TYPE,
        )?;
        validate_kind(
            self.request.version,
            &self.request.object_type,
            BUSINESS_TRAVEL_REQUEST_TYPE,
        )?;
        if self.response.approval.request != self.request.request {
            bail!("business response substituted the Travel enrollment request");
        }
        let (credential, trust) = validate_enrollment_response(&self.response, root, now)?;
        let business_credential = self.approval.verify(
            &trust,
            self.response.home_endpoint_credential.as_ref(),
            &self.request.descriptor,
            &json_digest(&self.request)?,
            &self.response.signed_credential,
            now,
        )?;
        if business_credential != credential {
            bail!("business approval and certificate enrollment disagree");
        }
        Ok((credential, trust))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceClassTravelRequest {
    pub version: u32,
    pub object_type: String,
    pub request: TravelEnrollmentRequest,
    pub descriptor: ServiceClassDescriptor,
    pub label: String,
}
impl ServiceClassTravelRequest {
    /// Validate the CSR, class intent and readable client name.
    /// # Errors
    /// Rejects invalid or stale requests, names, classes and approvers.
    pub fn validate(&self, _trust: &DeploymentTrust, approving_home: &str, now: u64) -> Result<()> {
        validate_kind(
            self.version,
            &self.object_type,
            SERVICE_CLASS_TRAVEL_REQUEST_TYPE,
        )?;
        validate_client_label(&self.label)?;
        parse_enrollment_request(&self.request, now)?;
        self.descriptor.validate()?;
        if self.descriptor.approving_home_id != approving_home {
            bail!("service class request targets a different approving Home");
        }
        Ok(())
    }
}
/// Validate a signed-request display name without permitting terminal/bidi controls.
/// # Errors
/// Rejects empty, oversized or control-containing labels.
pub fn validate_client_label(label: &str) -> Result<()> {
    if label.trim().is_empty() || label.len() > 64 || label.chars().any(|c| c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')) {
        bail!("client label must contain 1 to 64 UTF-8 bytes without control characters");
    }
    Ok(())
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceClassTravelResponse {
    pub version: u32,
    pub object_type: String,
    pub request: ServiceClassTravelRequest,
    pub response: TravelEnrollmentResponse,
    pub approval: SignedServiceClassApproval,
}
impl ServiceClassTravelResponse {
    /// Validate identity plus the independent signed class/name request proof.
    /// # Errors
    /// Rejects changed requests, signatures, scope or trust.
    pub fn validate(&self, root: &str, now: u64) -> Result<(TravelCredential, DeploymentTrust)> {
        validate_kind(
            self.version,
            &self.object_type,
            SERVICE_CLASS_TRAVEL_RESPONSE_TYPE,
        )?;
        validate_kind(
            self.request.version,
            &self.request.object_type,
            SERVICE_CLASS_TRAVEL_REQUEST_TYPE,
        )?;
        validate_client_label(&self.request.label)?;
        if self.response.approval.request != self.request.request {
            bail!("service class response substituted the Travel request");
        }
        let (credential, trust) = validate_enrollment_response(&self.response, root, now)?;
        let proved = self.approval.verify(
            &trust,
            self.response.home_endpoint_credential.as_ref(),
            &self.request.descriptor,
            &json_digest(&self.request)?,
            &self.response.signed_credential,
            now,
        )?;
        if proved != credential {
            bail!("service class approval and enrollment disagree");
        }
        Ok((credential, trust))
    }
}

// Untagged envelopes retain the exact legacy serialized object for old installations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum HomeRequestEnvelope {
    Business(Box<BusinessHomeRequest>),
    Legacy(HomeEnrollmentRequest),
}
impl Deref for HomeRequestEnvelope {
    type Target = HomeEnrollmentRequest;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Business(value) => &value.request,
            Self::Legacy(value) => value,
        }
    }
}
impl HomeRequestEnvelope {
    /// Validates a legacy or explicitly versioned business request.
    ///
    /// # Errors
    /// Returns request validation errors.
    pub fn validate(&self, now: u64) -> Result<()> {
        match self {
            Self::Business(request) => request.validate(now),
            Self::Legacy(request) => parse_home_enrollment_request(request, now).map(|_| ()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum HomeResponseEnvelope {
    Business(Box<BusinessHomeResponse>),
    Legacy(Box<HomeEnrollmentResponse>),
}
impl From<HomeEnrollmentResponse> for HomeResponseEnvelope {
    fn from(value: HomeEnrollmentResponse) -> Self {
        Self::Legacy(Box::new(value))
    }
}
impl Deref for HomeResponseEnvelope {
    type Target = HomeEnrollmentResponse;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Business(value) => &value.response,
            Self::Legacy(value) => value,
        }
    }
}
impl HomeResponseEnvelope {
    /// Returns the complete original request for transport replay/mismatch checks.
    #[must_use]
    pub fn request_envelope(&self) -> HomeRequestEnvelope {
        match self {
            Self::Business(response) => {
                HomeRequestEnvelope::Business(Box::new(response.request.clone()))
            }
            Self::Legacy(response) => {
                HomeRequestEnvelope::Legacy(response.approval.request.clone())
            }
        }
    }

    /// Validates the matching legacy or business response format.
    ///
    /// # Errors
    /// Returns trust, identity, signature or scope errors.
    pub fn validate(&self, root: &str, now: u64) -> Result<DeploymentTrust> {
        match self {
            Self::Business(response) => response.validate(root, now),
            Self::Legacy(response) => {
                validate_home_enrollment_response(response, root, now).map(|(_, trust)| trust)
            }
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TravelRequestEnvelope {
    ServiceClass(Box<ServiceClassTravelRequest>),
    Business(Box<BusinessTravelRequest>),
    Legacy(TravelEnrollmentRequest),
}
impl Deref for TravelRequestEnvelope {
    type Target = TravelEnrollmentRequest;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::ServiceClass(value) => &value.request,
            Self::Business(value) => &value.request,
            Self::Legacy(value) => value,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum TravelResponseEnvelope {
    ServiceClass(Box<ServiceClassTravelResponse>),
    Business(Box<BusinessTravelResponse>),
    Legacy(Box<TravelEnrollmentResponse>),
}
impl From<TravelEnrollmentResponse> for TravelResponseEnvelope {
    fn from(value: TravelEnrollmentResponse) -> Self {
        Self::Legacy(Box::new(value))
    }
}
impl Deref for TravelResponseEnvelope {
    type Target = TravelEnrollmentResponse;
    fn deref(&self) -> &Self::Target {
        match self {
            Self::ServiceClass(value) => &value.response,
            Self::Business(value) => &value.response,
            Self::Legacy(value) => value,
        }
    }
}

/// Issues only approved requested services, with no issuer secrets in the result.
///
/// # Errors
/// Returns request, policy, signing or key errors.
pub fn issue_business_home(
    request: BusinessHomeRequest,
    approval: HomeEnrollmentApproval,
    services: Vec<BusinessService>,
    material: &HomeIssuerMaterial<'_>,
    now: u64,
) -> Result<BusinessHomeResponse> {
    request.validate(now)?;
    validate_subset(&request.services, &services)?;
    if approval.request != request.request || approval.profile != HomeEnrollmentProfile::ServingOnly
    {
        bail!("business Home approval must preserve its request and serving-only profile");
    }
    let response = issue_home_enrollment(approval, material, now)?;
    let trust = response
        .deployment_trust
        .verify(material.deployment_root_public_key, now)?;
    let endpoint = response.signed_endpoint_credential.verify(&trust, now)?;
    let payload = HomeServiceGrant {
        version: BUSINESS_VERSION,
        object_type: HOME_SERVICE_GRANT_TYPE.to_owned(),
        deployment_id: endpoint.deployment_id,
        authority_id: endpoint.authority_id,
        authority_epoch: endpoint.authority_epoch,
        home_id: endpoint.home_id,
        endpoint_payload_sha256: payload_digest(&response.signed_endpoint_credential.payload_hex)?,
        enrollment_request_sha256: json_digest(&request)?,
        services,
        not_before_unix_secs: endpoint.not_before_unix_secs,
        not_after_unix_secs: endpoint.not_after_unix_secs,
    };
    let grant = SignedHomeServiceGrant::sign(
        &payload,
        &load_signing_key(&material.home_enrollment_authority_key)?,
    )?;
    let result = BusinessHomeResponse {
        version: BUSINESS_VERSION,
        object_type: BUSINESS_HOME_RESPONSE_TYPE.to_owned(),
        request,
        response,
        grant,
    };
    result.validate(material.deployment_root_public_key, now)?;
    Ok(result)
}

/// Issues an exact Service-scoped Travel credential and its independent intent proof.
///
/// # Errors
/// Returns an error for a substituted request/scope, invalid trust, expired destination or signing failure.
pub fn issue_business_travel(
    request: BusinessTravelRequest,
    mut approval: TravelEnrollmentApproval,
    material: &IssuerMaterial<'_>,
    now: u64,
) -> Result<BusinessTravelResponse> {
    let trust = material
        .deployment_trust
        .verify(material.deployment_root_public_key, now)?;
    request.validate(&trust, &request.descriptor.approving_home_id, now)?;
    let target = request.descriptor.verify(&trust, now)?;
    if approval.request != request.request
        || approval.scope
            != (TravelCredentialScope::Service {
                home_id: target.home_id,
                service_id: target.service.service_id,
                protocol: target.service.protocol,
            })
    {
        bail!("business Travel approval must name exactly its requested Home and service");
    }
    // A default one-year Travel request cannot outlive the already-issued Home grant.
    approval.not_after_unix_secs = approval.not_after_unix_secs.min(target.not_after_unix_secs);
    if let Some(endpoint) = material.home_endpoint_credential {
        approval.not_after_unix_secs = approval
            .not_after_unix_secs
            .min(endpoint.verify(&trust, now)?.not_after_unix_secs);
    }
    approval.not_before_unix_secs = approval
        .not_before_unix_secs
        .max(trust.not_before_unix_secs);
    let response = issue_enrollment(approval, material, now)?;
    let authorities = trust.travel_authorities_with_home_delegations(
        &material
            .home_endpoint_credential
            .cloned()
            .into_iter()
            .collect::<Vec<_>>(),
        now,
    )?;
    let authority = authorities
        .iter()
        .find(|authority| authority.id() == response.signed_credential.authority_id)
        .ok_or_else(|| anyhow!("business Travel authority is unavailable"))?;
    if authority.home_id() != Some(target.approving_home_id.as_str()) {
        bail!("business Travel was approved by an unexpected Home");
    }
    let payload = BusinessTravelApproval {
        version: BUSINESS_VERSION,
        object_type: BUSINESS_APPROVAL_TYPE.to_owned(),
        deployment_id: trust.deployment_id,
        authority_id: authority.id().to_owned(),
        authority_epoch: authority.epoch(),
        request_id: request.request.request_id,
        request_sha256: json_digest(&request)?,
        descriptor_sha256: json_digest(&request.descriptor)?,
        credential_payload_sha256: payload_digest(&response.signed_credential.payload_hex)?,
    };
    let approval = SignedBusinessTravelApproval::sign(
        &payload,
        &load_signing_key(&material.travel_authority_key)?,
    )?;
    let result = BusinessTravelResponse {
        version: BUSINESS_VERSION,
        object_type: BUSINESS_TRAVEL_RESPONSE_TYPE.to_owned(),
        request,
        response,
        approval,
    };
    result.validate(material.deployment_root_public_key, now)?;
    Ok(result)
}

/// Issue exactly one service-class credential and bind its full request, including label.
/// # Errors
/// Rejects widened scopes, invalid requests or signing/trust failures.
pub fn issue_service_class_travel(
    request: ServiceClassTravelRequest,
    mut approval: TravelEnrollmentApproval,
    material: &IssuerMaterial<'_>,
    now: u64,
) -> Result<ServiceClassTravelResponse> {
    let trust = material
        .deployment_trust
        .verify(material.deployment_root_public_key, now)?;
    request.validate(&trust, &request.descriptor.approving_home_id, now)?;
    request
        .descriptor
        .verify_with_issuer(&trust, material.home_endpoint_credential, now)?;
    if approval.request != request.request || approval.scope != request.descriptor.scope() {
        bail!("service class approval must exactly match the requested class");
    }
    approval.not_after_unix_secs = approval.not_after_unix_secs.min(trust.not_after_unix_secs);
    if let Some(endpoint) = material.home_endpoint_credential {
        approval.not_after_unix_secs = approval
            .not_after_unix_secs
            .min(endpoint.verify(&trust, now)?.not_after_unix_secs);
    }
    approval.not_before_unix_secs = approval
        .not_before_unix_secs
        .max(trust.not_before_unix_secs);
    let response = issue_enrollment(approval, material, now)?;
    let authorities = trust.travel_authorities_with_home_delegations(
        &material
            .home_endpoint_credential
            .cloned()
            .into_iter()
            .collect::<Vec<_>>(),
        now,
    )?;
    let authority = authorities
        .iter()
        .find(|authority| authority.id() == response.signed_credential.authority_id)
        .ok_or_else(|| anyhow!("service class Travel authority unavailable"))?;
    if !matches!(
        authority,
        flowsplice_core::authorization::TrustedTravelAuthority::Global { .. }
    ) || authority.home_id() != Some(request.descriptor.approving_home_id.as_str())
    {
        bail!("service class approval requires the requested Global issuer");
    }
    let payload = ServiceClassApproval {
        version: BUSINESS_VERSION,
        object_type: SERVICE_CLASS_APPROVAL_TYPE.into(),
        deployment_id: trust.deployment_id,
        authority_id: authority.id().into(),
        authority_epoch: authority.epoch(),
        request_id: request.request.request_id,
        request_sha256: json_digest(&request)?,
        descriptor_sha256: json_digest(&request.descriptor)?,
        credential_payload_sha256: payload_digest(&response.signed_credential.payload_hex)?,
    };
    let approval = SignedServiceClassApproval::sign(
        &payload,
        &load_signing_key(&material.travel_authority_key)?,
    )?;
    let result = ServiceClassTravelResponse {
        version: BUSINESS_VERSION,
        object_type: SERVICE_CLASS_TRAVEL_RESPONSE_TYPE.into(),
        request,
        response,
        approval,
    };
    result.validate(material.deployment_root_public_key, now)?;
    Ok(result)
}

fn validate_subset(requested: &[BusinessService], approved: &[BusinessService]) -> Result<()> {
    validate_services(requested)?;
    validate_services(approved)?;
    if approved.iter().any(|service| !requested.contains(service)) {
        bail!("approved business services must be an exact subset of the requested services");
    }
    Ok(())
}

fn validate_kind(version: u32, actual: &str, expected: &str) -> Result<()> {
    if version != BUSINESS_VERSION || actual != expected {
        bail!("unsupported business enrollment format");
    }
    Ok(())
}

fn load_signing_key(protected: &ProtectedKey<'_>) -> Result<EcdsaKeyPair> {
    let key = Zeroizing::new(key::load_private_key(
        protected.path,
        protected.password,
        protected.allow_unencrypted,
    )?);
    EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, key.secret_der())
        .map_err(|_| anyhow!("business authorization requires a P-256 PKCS#8 key"))
}
