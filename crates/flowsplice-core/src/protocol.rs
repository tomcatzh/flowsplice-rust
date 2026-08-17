use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::authorization::TravelAuthorizationSnapshot;

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
    pub server_name: String,
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
        role: Role,
        id: String,
    },
    TravelHello {
        id: String,
        session_id: Uuid,
        purpose: TravelConnectionPurpose,
    },
    TravelHelloAccepted {
        relay_id: String,
        credential_id: Uuid,
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
    Catalog {
        catalog: Catalog,
    },
    RelayDirectory {
        directory: RelayDirectory,
    },
    RouteRequest {
        request_id: Uuid,
        travel_id: String,
        travel_session_id: Uuid,
        credential_id: Uuid,
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
    },
    TravelSessionDenied {
        request_id: Uuid,
        reason: String,
    },
    ServerRouteGrant {
        request_id: Uuid,
        work_id: Uuid,
        work_secret: Vec<u8>,
        credential_id: Uuid,
        home_id: String,
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
    OpenWork {
        work_id: Uuid,
        work_secret: Vec<u8>,
        credential_id: Uuid,
    },
    TravelAuthorizationSnapshot {
        snapshot: TravelAuthorizationSnapshot,
    },
    TravelAuthorizationAck {
        generation: u64,
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
        Catalog, ControlMessage, DataFrame, HomeCatalog, RelayDirectory, RelayEndpoint, Service,
        ServiceProtocol, TravelConnectionPurpose,
    };
    use uuid::Uuid;

    #[test]
    fn relay_directory_round_trips() -> Result<(), serde_json::Error> {
        let message = ControlMessage::RelayDirectory {
            directory: RelayDirectory {
                generation: 7,
                relays: vec![RelayEndpoint {
                    id: "relay-1".to_owned(),
                    management_addr: "relay.example:8443".to_owned(),
                    server_name: "relay.example".to_owned(),
                }],
            },
        };
        let encoded = serde_json::to_vec(&message)?;
        let decoded: ControlMessage = serde_json::from_slice(&encoded)?;
        match decoded {
            ControlMessage::RelayDirectory { directory } => {
                assert_eq!(directory.generation, 7);
                assert_eq!(directory.relays[0].id, "relay-1");
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
            id: "travel-1".to_owned(),
            session_id,
            purpose: TravelConnectionPurpose::Catalog,
        };
        let encoded = serde_json::to_vec(&message)?;
        let decoded: ControlMessage = serde_json::from_slice(&encoded)?;
        match decoded {
            ControlMessage::TravelHello {
                id,
                session_id: decoded_session,
                purpose,
            } => {
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
        let message = ControlMessage::Catalog {
            catalog: Catalog {
                generation: 9,
                homes: vec![
                    HomeCatalog {
                        home_id: "home-1".to_owned(),
                        home_alias: "Home One".to_owned(),
                        services: vec![service("127.0.0.1:22")],
                    },
                    HomeCatalog {
                        home_id: "home-2".to_owned(),
                        home_alias: "Home Two".to_owned(),
                        services: vec![service("127.0.0.1:22")],
                    },
                ],
            },
        };
        let encoded = serde_json::to_vec(&message)?;
        let decoded: ControlMessage = serde_json::from_slice(&encoded)?;
        let ControlMessage::Catalog { catalog } = decoded else {
            panic!("wrong control message variant");
        };
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
