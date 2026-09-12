//! Cross-Home authorization for an exact, issuer-approved application protocol.
use super::{
    BUSINESS_VERSION, decode_payload, json_digest, payload_digest, sign,
    validate_application_protocol, validate_token, verify_signature,
};
use crate::authorization::TrustedTravelAuthority;
use crate::{
    authorization::{SignedTravelCredential, TravelCredential, TravelCredentialScope},
    deployment::{DeploymentTrust, SignedHomeEndpointCredential},
    protocol::ServiceProtocol,
    tls::validate_spki_pin,
};
use anyhow::{Result, anyhow, bail};
use aws_lc_rs::signature::EcdsaKeyPair;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const SERVICE_CLASS_APPROVAL_TYPE: &str = "flowsplice.service_class_travel_approval";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceClassDescriptor {
    pub version: u32,
    pub approving_home_id: String,
    pub application_protocol: String,
    pub protocol: ServiceProtocol,
}

impl ServiceClassDescriptor {
    /// Validates an enrollment proposal without treating it as authorization.
    /// # Errors
    /// Rejects unsupported versions and malformed identifiers.
    pub fn validate(&self) -> Result<()> {
        if self.version != BUSINESS_VERSION {
            bail!("unsupported service class descriptor");
        }
        validate_token(&self.approving_home_id, 128)?;
        validate_application_protocol(&self.application_protocol)
    }

    #[must_use]
    pub fn scope(&self) -> TravelCredentialScope {
        TravelCredentialScope::ServiceClass {
            application_protocol: self.application_protocol.clone(),
            protocol: self.protocol,
        }
    }

    /// Checks that the named approver has a root-trusted Global authority.
    /// # Errors
    /// Rejects malformed proposals or an unauthorized approver.
    pub fn verify(&self, trust: &DeploymentTrust, now: u64) -> Result<()> {
        self.verify_with_issuer(trust, None, now)
    }

    /// Also accepts a Global authority delegated by a verified Home endpoint.
    /// # Errors
    /// Rejects invalid endpoints, expired trust or missing Global authority.
    pub fn verify_with_issuer(
        &self,
        trust: &DeploymentTrust,
        issuer: Option<&SignedHomeEndpointCredential>,
        now: u64,
    ) -> Result<()> {
        self.validate()?;
        if now < trust.not_before_unix_secs || now >= trust.not_after_unix_secs {
            bail!("service class deployment trust is not active");
        }
        let authorities = trust.travel_authorities_with_home_delegations(
            &issuer.cloned().into_iter().collect::<Vec<_>>(),
            now,
        )?;
        if !authorities.iter().any(|a| {
            matches!(a,
            TrustedTravelAuthority::Global { home_id, .. } if home_id == &self.approving_home_id)
        }) {
            bail!("service class approval requires the requested Global issuer");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceClassApproval {
    pub version: u32,
    pub object_type: String,
    pub deployment_id: String,
    pub authority_id: String,
    pub authority_epoch: u64,
    pub request_id: Uuid,
    pub request_sha256: String,
    pub descriptor_sha256: String,
    pub credential_payload_sha256: String,
}

impl ServiceClassApproval {
    fn validate(&self) -> Result<()> {
        if self.version != BUSINESS_VERSION
            || self.object_type != SERVICE_CLASS_APPROVAL_TYPE
            || self.deployment_id.is_empty()
            || self.authority_epoch == 0
            || self.request_id.is_nil()
        {
            bail!("invalid service class approval");
        }
        validate_token(&self.authority_id, 128)?;
        for value in [
            &self.request_sha256,
            &self.descriptor_sha256,
            &self.credential_payload_sha256,
        ] {
            validate_spki_pin(value, "service class approval digest")?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedServiceClassApproval {
    pub authority_id: String,
    pub payload_hex: String,
    pub signature_hex: String,
}

impl SignedServiceClassApproval {
    /// Signs the complete class intent, including the device label in its request digest.
    /// # Errors
    /// Rejects invalid payloads or signing errors.
    pub fn sign(payload: &ServiceClassApproval, key: &EcdsaKeyPair) -> Result<Self> {
        payload.validate()?;
        let (payload_hex, signature_hex) = sign(payload, key)?;
        Ok(Self {
            authority_id: payload.authority_id.clone(),
            payload_hex,
            signature_hex,
        })
    }

    /// Verifies exact class, request, label digest, issuer and certificate bindings.
    /// # Errors
    /// Rejects any substitution, non-Global signer, expiry or mismatched scope.
    pub fn verify(
        &self,
        trust: &DeploymentTrust,
        issuer: Option<&SignedHomeEndpointCredential>,
        descriptor: &ServiceClassDescriptor,
        request_sha256: &str,
        credential: &SignedTravelCredential,
        now: u64,
    ) -> Result<TravelCredential> {
        descriptor.verify_with_issuer(trust, issuer, now)?;
        let (payload, bytes): (ServiceClassApproval, _) = decode_payload(&self.payload_hex)?;
        payload.validate()?;
        let authorities = trust.travel_authorities_with_home_delegations(
            &issuer.cloned().into_iter().collect::<Vec<_>>(),
            now,
        )?;
        let authority = authorities
            .iter()
            .find(|a| a.id() == payload.authority_id)
            .ok_or_else(|| anyhow!("untrusted service class signer"))?;
        if !matches!(authority, TrustedTravelAuthority::Global { home_id, .. }
            if home_id == &descriptor.approving_home_id)
        {
            bail!("service class signer must be the requested Global issuer");
        }
        let travel = credential.verify(authority)?;
        if let Some(issuer) = issuer {
            let endpoint = issuer.verify(trust, now)?;
            if endpoint.home_id != descriptor.approving_home_id
                || travel.not_after_unix_secs > endpoint.not_after_unix_secs
            {
                bail!("service class credential exceeds its issuer authorization");
            }
        }
        if self.authority_id != payload.authority_id
            || payload.authority_id != travel.authority_id
            || payload.authority_epoch != authority.epoch()
            || payload.deployment_id != trust.deployment_id
            || travel.deployment_id != trust.deployment_id
            || payload.request_id != travel.enrollment_request_id
            || payload.request_sha256 != request_sha256
            || payload.descriptor_sha256 != json_digest(descriptor)?
            || payload.credential_payload_sha256 != payload_digest(&credential.payload_hex)?
            || !travel.active_at(now)
            || travel.not_before_unix_secs < trust.not_before_unix_secs
            || travel.not_after_unix_secs > trust.not_after_unix_secs
            || travel.scope != descriptor.scope()
        {
            bail!("service class approval does not match the complete requested intent");
        }
        verify_signature(&bytes, &self.signature_hex, authority.public_key())?;
        Ok(travel)
    }
}
