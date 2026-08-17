use serde::{Deserialize, Serialize};
use uuid::Uuid;

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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Service {
    pub id: String,
    pub alias: String,
    pub protocol: ServiceProtocol,
    pub target: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Catalog {
    pub home_id: String,
    pub home_alias: String,
    pub generation: u64,
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMessage {
    Hello {
        role: Role,
        id: String,
    },
    Heartbeat {
        nonce: u64,
    },
    HeartbeatAck {
        nonce: u64,
    },
    HomeRegister {
        catalog: Catalog,
    },
    Catalog {
        catalog: Catalog,
    },
    RouteRequest {
        request_id: Uuid,
        travel_id: String,
    },
    ServerRouteGrant {
        request_id: Uuid,
        work_id: Uuid,
        work_secret: Vec<u8>,
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
        service_id: String,
        protocol: ServiceProtocol,
    },
    OpenOk {
        flow_id: Uuid,
    },
    OpenError {
        flow_id: Uuid,
        reason: String,
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
    Datagram {
        flow_id: Uuid,
        sequence: u64,
        bytes: Vec<u8>,
    },
    Fin {
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
