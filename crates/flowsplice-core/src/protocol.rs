use aws_lc_rs::digest;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::authorization::{SignedTravelCredential, TravelAuthorizationSnapshot};
use crate::deployment::{SignedControlSnapshot, SignedHomeEndpointCredential};
use crate::statistics::SignedStatisticsReport;

pub const CONTROL_PROTOCOL_VERSION: u32 = 2;

/// Returns the short human comparison code for one first-enrollment request and its private
/// retrieval token. This code is not an authentication secret; it lets the Home operator confirm
/// that the locally visible request is the one displayed by the new Travel.
#[must_use]
pub fn bootstrap_verification_code(request_json: &[u8], retrieval_token: &[u8]) -> String {
    let mut material = Vec::with_capacity(request_json.len().saturating_add(retrieval_token.len()));
    material.extend_from_slice(request_json);
    material.extend_from_slice(retrieval_token);
    let encoded = hex::encode_upper(digest::digest(&digest::SHA256, &material).as_ref());
    format!("{}-{}-{}", &encoded[0..4], &encoded[4..8], &encoded[8..12])
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Server,
    Relay,
    Home,
    Travel,
}

impl Role {
    #[must_use]
    pub const fn as_uri_part(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Relay => "relay",
            Self::Home => "home",
            Self::Travel => "travel",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceProtocol {
    Tcp,
    Udp,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TravelConnectionPurpose {
    Catalog,
    Route,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Service {
    pub id: String,
    pub alias: String,
    pub protocol: ServiceProtocol,
    pub target: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HomeCatalog {
    pub home_id: String,
    pub home_alias: String,
    #[serde(default)]
    pub endpoint_credential: Option<SignedHomeEndpointCredential>,
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Catalog {
    pub generation: u64,
    pub homes: Vec<HomeCatalog>,
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct RelayEndpoint {
    pub id: String,
    pub management_addr: String,
    pub data_public_addr: String,
    pub management_spki_sha256: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RelayDirectory {
    pub generation: u64,
    pub relays: Vec<RelayEndpoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    Hello {
        protocol_version: u32,
        role: Role,
        id: String,
    },
    TravelHello {
        protocol_version: u32,
        id: String,
        session_id: Uuid,
        purpose: TravelConnectionPurpose,
    },
    TravelHelloAccepted {
        relay_id: String,
    },
    TravelHelloDenied {
        reason: String,
    },
    Heartbeat {
        nonce: u64,
    },
    HeartbeatAck {
        nonce: u64,
    },
    HomeRegister {
        home: HomeCatalog,
    },
    ControlSnapshot {
        snapshot: SignedControlSnapshot,
    },
    RouteRequest {
        request_id: Uuid,
        travel_id: String,
        travel_session_id: Uuid,
        credential_id: Uuid,
        home_id: String,
    },
    TravelRouteRequest {
        request_id: Uuid,
        travel_id: String,
        travel_session_id: Uuid,
        home_id: String,
    },
    TravelSessionAuthorize {
        request_id: Uuid,
        travel_id: String,
        travel_session_id: Uuid,
        credential_id: Uuid,
        lease_id: Option<Uuid>,
    },
    TravelSessionAccepted {
        request_id: Uuid,
        snapshot: SignedControlSnapshot,
    },
    TravelSessionRelease {
        travel_id: String,
        travel_session_id: Uuid,
        lease_id: Uuid,
    },
    TravelSessionDenied {
        request_id: Uuid,
        reason: String,
    },
    ServerRelayGrant {
        request_id: Uuid,
        work_id: Uuid,
        work_secret: Vec<u8>,
        credential_id: Uuid,
        home_id: String,
        expires_at_unix_secs: u64,
    },
    RelayWorkReady {
        request_id: Uuid,
        work_id: Uuid,
    },
    RouteGrant {
        request_id: Uuid,
        route_id: Uuid,
        route_secret: Vec<u8>,
        data_addr: String,
    },
    RouteDenied {
        request_id: Uuid,
        reason: String,
    },
    OpenRelayWork {
        work_id: Uuid,
        work_secret: Vec<u8>,
        credential_id: Uuid,
        relay_id: String,
        relay_data_addr: String,
        expires_at_unix_secs: u64,
    },
    TravelAuthorizationSnapshot {
        snapshot: TravelAuthorizationSnapshot,
    },
    TravelAuthorizationAck {
        generation: u64,
    },
    PublishTravelCredential {
        request_id: Uuid,
        credential: SignedTravelCredential,
    },
    PublishTravelCredentialResult {
        request_id: Uuid,
        accepted: bool,
        generation: u64,
        error: Option<String>,
    },
    RevokeTravelCredential {
        request_id: Uuid,
        credential_id: Uuid,
        reason: String,
    },
    RevokeTravelCredentialResult {
        request_id: Uuid,
        accepted: bool,
        generation: u64,
        error: Option<String>,
    },
    StatisticsReport {
        report: SignedStatisticsReport,
    },
    StatisticsReportAck {
        digest_sha256: String,
        accepted: bool,
        error: Option<String>,
    },
    TravelEnrollmentSubmit {
        request_id: Uuid,
        travel_id: String,
        travel_session_id: Uuid,
        home_id: String,
        request_json: Vec<u8>,
    },
    RemoteEnrollmentSubmit {
        request_id: Uuid,
        travel_id: String,
        travel_session_id: Uuid,
        credential_id: Uuid,
        home_id: String,
        request_json: Vec<u8>,
    },
    RemoteEnrollmentResult {
        request_id: Uuid,
        accepted: bool,
        response_json: Option<Vec<u8>>,
        error: Option<String>,
    },
    BootstrapEnrollmentSubmit {
        protocol_version: u32,
        request_id: Uuid,
        travel_id: String,
        home_id: String,
        retrieval_token: Vec<u8>,
        request_json: Vec<u8>,
    },
    BootstrapEnrollmentResult {
        request_id: Uuid,
        accepted: bool,
        response_json: Option<Vec<u8>>,
        seed_relays: Vec<String>,
        error: Option<String>,
    },
    HomeBootstrapEnrollmentSubmit {
        protocol_version: u32,
        request_id: Uuid,
        home_id: String,
        retrieval_token: Vec<u8>,
        request_json: Vec<u8>,
    },
    HomeBootstrapEnrollmentResult {
        request_id: Uuid,
        accepted: bool,
        response_json: Option<Vec<u8>>,
        error: Option<String>,
    },
    HomeEnrollmentSubmit {
        request_id: Uuid,
        home_id: String,
        retrieval_token: Vec<u8>,
        request_json: Vec<u8>,
    },
    HomeEnrollmentResult {
        request_id: Uuid,
        accepted: bool,
        response_json: Option<Vec<u8>>,
        error: Option<String>,
    },
    RemoteEnrollmentInstalled {
        request_id: Uuid,
        travel_id: String,
        travel_session_id: Uuid,
        credential_id: Uuid,
        home_id: String,
    },
    RemoteEnrollmentInstalledAck {
        request_id: Uuid,
        accepted: bool,
        error: Option<String>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DataFrame {
    Open {
        flow_id: Uuid,
        carrier_id: Uuid,
        service_id: String,
        protocol: ServiceProtocol,
    },
    OpenOk {
        flow_id: Uuid,
        carrier_id: Uuid,
        receive_offset: u64,
        send_offset: u64,
    },
    OpenError {
        flow_id: Uuid,
        carrier_id: Uuid,
        reason: String,
    },
    Race {
        flow_id: Uuid,
        race_id: Uuid,
        next_offset: u64,
    },
    RaceAck {
        flow_id: Uuid,
        race_id: Uuid,
        winner_carrier_id: Uuid,
    },
    RaceDuplicate {
        flow_id: Uuid,
        race_id: Uuid,
        winner_carrier_id: Uuid,
    },
    Data {
        flow_id: Uuid,
        offset: u64,
        bytes: Vec<u8>,
    },
    Ack {
        flow_id: Uuid,
        next_offset: u64,
    },
    Duplicate {
        flow_id: Uuid,
        next_offset: u64,
        winner_carrier_id: Uuid,
    },
    Datagram {
        flow_id: Uuid,
        sequence: u64,
        bytes: Vec<u8>,
    },
    Fin {
        flow_id: Uuid,
        final_offset: u64,
    },
    FinAck {
        flow_id: Uuid,
        final_offset: u64,
    },
    Close {
        flow_id: Uuid,
        reason: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::{
        CONTROL_PROTOCOL_VERSION, Catalog, ControlMessage, DataFrame, HomeCatalog, Service,
        ServiceProtocol, TravelConnectionPurpose,
    };
    use crate::deployment::{SignedControlSnapshot, SignedDeploymentTrust};
    use uuid::Uuid;

    #[test]
    fn signed_control_snapshot_round_trips_without_reencoding_payloads()
    -> Result<(), serde_json::Error> {
        let message = ControlMessage::ControlSnapshot {
            snapshot: SignedControlSnapshot {
                trust: SignedDeploymentTrust {
                    payload_hex: "0102".to_owned(),
                    signature_hex: "0304".to_owned(),
                },
                payload_hex: "0506".to_owned(),
                signature_hex: "0708".to_owned(),
            },
        };
        let encoded = serde_json::to_vec(&message)?;
        let decoded: ControlMessage = serde_json::from_slice(&encoded)?;
        match decoded {
            ControlMessage::ControlSnapshot { snapshot } => {
                assert_eq!(snapshot.trust.payload_hex, "0102");
                assert_eq!(snapshot.payload_hex, "0506");
                assert_eq!(snapshot.signature_hex, "0708");
            }
            _ => panic!("wrong control message variant"),
        }
        Ok(())
    }

    #[test]
    fn carrier_race_duplicate_preserves_winner() -> Result<(), serde_json::Error> {
        let flow_id = Uuid::new_v4();
        let race_id = Uuid::new_v4();
        let winner_carrier_id = Uuid::new_v4();
        let frame = DataFrame::RaceDuplicate {
            flow_id,
            race_id,
            winner_carrier_id,
        };
        let encoded = serde_json::to_vec(&frame)?;
        let decoded: DataFrame = serde_json::from_slice(&encoded)?;
        match decoded {
            DataFrame::RaceDuplicate {
                flow_id: decoded_flow,
                race_id: decoded_race,
                winner_carrier_id: decoded_winner,
            } => {
                assert_eq!(decoded_flow, flow_id);
                assert_eq!(decoded_race, race_id);
                assert_eq!(decoded_winner, winner_carrier_id);
            }
            _ => panic!("wrong data frame variant"),
        }
        Ok(())
    }

    #[test]
    fn travel_hello_carries_process_session_and_purpose() -> Result<(), serde_json::Error> {
        let session_id = Uuid::new_v4();
        let message = ControlMessage::TravelHello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            id: "travel-1".to_owned(),
            session_id,
            purpose: TravelConnectionPurpose::Catalog,
        };
        let encoded = serde_json::to_vec(&message)?;
        let decoded: ControlMessage = serde_json::from_slice(&encoded)?;
        match decoded {
            ControlMessage::TravelHello {
                protocol_version,
                id,
                session_id: decoded_session,
                purpose,
            } => {
                assert_eq!(protocol_version, CONTROL_PROTOCOL_VERSION);
                assert_eq!(id, "travel-1");
                assert_eq!(decoded_session, session_id);
                assert_eq!(purpose, TravelConnectionPurpose::Catalog);
            }
            _ => panic!("wrong control message variant"),
        }
        Ok(())
    }

    #[test]
    fn catalog_preserves_services_with_the_same_id_on_different_homes()
    -> Result<(), serde_json::Error> {
        let service = |target: &str| Service {
            id: "ssh".to_owned(),
            alias: "SSH".to_owned(),
            protocol: ServiceProtocol::Tcp,
            target: target.to_owned(),
        };
        let catalog = Catalog {
            generation: 9,
            homes: vec![
                HomeCatalog {
                    home_id: "home-1".to_owned(),
                    home_alias: "Home One".to_owned(),
                    services: vec![service("127.0.0.1:22")],
                    endpoint_credential: None,
                },
                HomeCatalog {
                    home_id: "home-2".to_owned(),
                    home_alias: "Home Two".to_owned(),
                    services: vec![service("127.0.0.1:22")],
                    endpoint_credential: None,
                },
            ],
        };
        let encoded = serde_json::to_vec(&catalog)?;
        let catalog: Catalog = serde_json::from_slice(&encoded)?;
        assert_eq!(catalog.generation, 9);
        assert_eq!(catalog.homes.len(), 2);
        assert_eq!(catalog.homes[0].services[0].id, "ssh");
        assert_eq!(catalog.homes[1].services[0].id, "ssh");
        Ok(())
    }

    #[test]
    fn route_request_carries_the_selected_home() -> Result<(), serde_json::Error> {
        let message = ControlMessage::RouteRequest {
            request_id: Uuid::new_v4(),
            travel_id: "travel-1".to_owned(),
            travel_session_id: Uuid::new_v4(),
            credential_id: Uuid::new_v4(),
            home_id: "home-2".to_owned(),
        };
        let encoded = serde_json::to_vec(&message)?;
        let decoded: ControlMessage = serde_json::from_slice(&encoded)?;
        let ControlMessage::RouteRequest { home_id, .. } = decoded else {
            panic!("wrong control message variant");
        };
        assert_eq!(home_id, "home-2");
        Ok(())
    }
}
