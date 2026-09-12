//! Compile and link a third-party consumer using only the two supported SDKs.
#![allow(dead_code)]

use flowsplice_home_core as home;
use flowsplice_travel_core as travel;
use std::sync::Arc;

fn home_runtime(runtime: &home::HomeRuntime) {
    let _: Option<&home::HomeServiceGrant> = runtime.business_service_grant();
    let _: Option<&home::SignedHomeEndpointCredential> = runtime.endpoint_credential();
    let _: Arc<home::LocalStatistics> = runtime.statistics().local;
    let _: anyhow::Result<Vec<home::MetricPoint>> = runtime.statistics().local.query(0, 0);
    let _: Option<Arc<home::VerifiedAuthorization>> = runtime.authorization().current();
}

fn home_peer(peer: &home::ServicePeer) {
    let _: &home::TravelCredential = &peer.credential;
    match &peer.credential.scope {
        home::TravelCredentialScope::Global
        | home::TravelCredentialScope::Home { .. }
        | home::TravelCredentialScope::Service { .. }
        | home::TravelCredentialScope::ServiceClass { .. } => {}
    }
}

fn home_trust(trust: &home::DeploymentTrust, endpoint: &home::SignedHomeEndpointCredential) {
    let _: &[home::ServerControlKey] = &trust.server_control_keys;
    let _: &[home::HomeEndpointTrust] = &trust.home_endpoints;
    let _: &[home::TrustedHomeEnrollmentAuthority] = &trust.home_enrollment_authorities;
    let _: &[home::TrustedTravelAuthority] = &trust.travel_authorities;
    let _: anyhow::Result<home::HomeEndpointCredential> = endpoint.verify(trust, 0);
}

fn metric(point: &home::MetricPoint) {
    let _: &home::MetricIdentity = &point.identity;
    let _: &home::MetricValue = &point.value;
}

fn signed_trust(home: &home::SignedDeploymentTrust, travel: &travel::SignedDeploymentTrust) {
    let _: anyhow::Result<home::DeploymentTrust> = home.verify("invalid", 0);
    let _: anyhow::Result<travel::DeploymentTrust> = travel.verify("invalid", 0);
}

async fn travel_catalog(runtime: &travel::TravelCore) {
    let catalog: travel::Catalog = runtime.catalog().await;
    let _: &[travel::HomeCatalog] = &catalog.homes;
    for entry in &catalog.homes {
        let _: &[travel::Service] = &entry.services;
        let _: &Option<travel::SignedHomeEndpointCredential> = &entry.endpoint_credential;
        let _: &Option<travel::SignedHomeServiceGrant> = &entry.service_grant;
    }
    let directory: travel::RelayDirectory = runtime.relay_directory().await;
    let _: &[travel::RelayEndpoint] = &directory.relays;
}

fn travel_descriptor(
    descriptor: &travel::BusinessDescriptor,
    class: &travel::ServiceClassDescriptor,
    trust: &travel::DeploymentTrust,
) {
    let _: &travel::SignedHomeEndpointCredential = &descriptor.endpoint;
    let _: &travel::SignedHomeServiceGrant = &descriptor.grant;
    let _: anyhow::Result<travel::VerifiedBusinessDescriptor> = descriptor.verify(trust, 0);
    let _: anyhow::Result<travel::HomeServiceGrant> =
        descriptor.grant.verify(trust, &descriptor.endpoint, 0);
    let _: anyhow::Result<travel::HomeEndpointCredential> = descriptor.endpoint.verify(trust, 0);
    let _: travel::TravelCredentialScope = class.scope();
    let _: anyhow::Result<()> = class.verify_with_issuer(trust, Some(&descriptor.endpoint), 0);
    let _: &[travel::ServerControlKey] = &trust.server_control_keys;
    let _: &[travel::HomeEndpointTrust] = &trust.home_endpoints;
    let _: &[travel::TrustedHomeEnrollmentAuthority] = &trust.home_enrollment_authorities;
    let _: &[travel::TrustedTravelAuthority] = &trust.travel_authorities;
}

fn main() {
    let (lifetime, guard): (home::ServiceLifetime, home::ServiceLifetimeGuard) =
        home::ServiceLifetime::new(u64::MAX);
    assert!(lifetime.is_active());
    drop(guard);
    assert!(!lifetime.is_active());
    println!("SDK consumer linked; public types and service lifetime verified");
}
