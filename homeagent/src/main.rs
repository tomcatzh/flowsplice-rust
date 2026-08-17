#![forbid(unsafe_code)]

use std::{
    io::{self, IsTerminal},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use flowsplice_core::{
    CONTROL_FRAME_LIMIT, DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    config::load_toml,
    frame::{JsonFrameReader, write_json},
    init_crypto,
    protocol::{Catalog, ControlMessage, DataFrame, Role, Service, ServiceProtocol},
    route::{RouteSide, write_preface},
    tls::{
        client_connector, peer_identity, require_peer, server_acceptor, server_name,
        validate_spki_pins,
    },
};
use serde::Deserialize;
use tokio::{
    net::{TcpStream, UdpSocket},
    sync::Semaphore,
    time::{interval, sleep, timeout},
};
use tokio_rustls::{
    TlsAcceptor, TlsConnector, client::TlsStream as ClientTlsStream,
    server::TlsStream as ServerTlsStream,
};
use tracing::{info, warn};
use uuid::Uuid;

mod tcp_flow;

use tcp_flow::{IncomingCarrier, TcpFlowRegistry};

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FLOWSPLICE_CONFIG", default_value = "homeagent.toml")]
    config: PathBuf,
}

#[derive(Clone, Deserialize)]
struct Config {
    id: String,
    alias: String,
    server_control_addr: String,
    server_data_addr: String,
    server_name: String,
    server_id: String,
    management_cert: PathBuf,
    management_key: PathBuf,
    management_ca: PathBuf,
    business_cert: PathBuf,
    business_key: PathBuf,
    business_ca: PathBuf,
    #[serde(default)]
    server_spki_pins: Vec<String>,
    #[serde(default)]
    travel_spki_pins: Vec<String>,
    services: Vec<Service>,
    #[serde(default = "default_handshake_timeout")]
    handshake_timeout_secs: u64,
    #[serde(default = "default_udp_idle")]
    udp_idle_secs: u64,
    #[serde(default = "default_max_active_flows")]
    max_active_flows: usize,
    #[serde(default = "default_carrier_heartbeat")]
    carrier_heartbeat_secs: u64,
    #[serde(default = "default_carrier_timeout")]
    carrier_timeout_secs: u64,
    #[serde(default = "default_flow_detach_timeout")]
    flow_detach_timeout_secs: u64,
    #[serde(default = "default_max_unacked_bytes")]
    max_unacked_bytes: usize,
}

const fn default_handshake_timeout() -> u64 {
    10
}

const fn default_udp_idle() -> u64 {
    60
}

const fn default_max_active_flows() -> usize {
    128
}

const fn default_carrier_heartbeat() -> u64 {
    5
}

const fn default_carrier_timeout() -> u64 {
    30
}

const fn default_flow_detach_timeout() -> u64 {
    120
}

const fn default_max_unacked_bytes() -> usize {
    1_048_576
}

#[derive(Clone)]
struct TlsMaterial {
    management_connector: TlsConnector,
    business_acceptor: TlsAcceptor,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_crypto();
    tracing_subscriber::fmt()
        .with_ansi(io::stdout().is_terminal())
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "flowsplice_homeagent=info".into()),
        )
        .init();
    let args = Args::parse();
    let config: Config = load_toml(&args.config)?;
    validate_services(&config.services)?;
    validate_spki_pins(&config.server_spki_pins, "server")?;
    validate_spki_pins(&config.travel_spki_pins, "travel")?;
    if config.carrier_heartbeat_secs == 0
        || config.carrier_timeout_secs <= config.carrier_heartbeat_secs
        || config.flow_detach_timeout_secs <= config.carrier_timeout_secs
        || config.max_unacked_bytes < MAX_DATA_PAYLOAD
    {
        bail!("carrier/flow timeout or unacknowledged-data limits are invalid");
    }
    let tls = Arc::new(TlsMaterial {
        management_connector: client_connector(
            &config.management_cert,
            &config.management_key,
            &config.management_ca,
        )?,
        business_acceptor: server_acceptor(
            &config.business_cert,
            &config.business_key,
            &config.business_ca,
        )?,
    });
    let config = Arc::new(config);
    let permits = Arc::new(Semaphore::new(config.max_active_flows));
    let tcp_flows = TcpFlowRegistry::new(
        Arc::clone(&permits),
        Duration::from_secs(config.carrier_heartbeat_secs),
        Duration::from_secs(config.carrier_timeout_secs),
        Duration::from_secs(config.flow_detach_timeout_secs),
        config.max_unacked_bytes,
    );
    loop {
        if let Err(error) = run_control(
            Arc::clone(&config),
            Arc::clone(&permits),
            Arc::clone(&tls),
            Arc::clone(&tcp_flows),
        )
        .await
        {
            warn!(%error, "server control disconnected; reconnecting");
        }
        sleep(Duration::from_secs(1)).await;
    }
}

fn validate_services(services: &[Service]) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for service in services {
        if service.id.is_empty() || !ids.insert(&service.id) {
            bail!("service ids must be non-empty and unique");
        }
        let valid_target = service.target.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p != 0)
        });
        if !valid_target {
            bail!("service {} target must be host:port", service.id);
        }
    }
    Ok(())
}

async fn run_control(
    config: Arc<Config>,
    permits: Arc<Semaphore>,
    tls: Arc<TlsMaterial>,
    tcp_flows: Arc<TcpFlowRegistry>,
) -> Result<()> {
    let socket = TcpStream::connect(&config.server_control_addr).await?;
    let stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        tls.management_connector
            .connect(server_name(&config.server_name)?, socket),
    )
    .await
    .context("server TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(
        &identity,
        Role::Server,
        Some(&config.server_id),
        &config.server_spki_pins,
    )?;
    run_control_session(stream, config, permits, tls, tcp_flows).await
}

async fn run_control_session(
    stream: ClientTlsStream<TcpStream>,
    config: Arc<Config>,
    permits: Arc<Semaphore>,
    tls: Arc<TlsMaterial>,
    tcp_flows: Arc<TcpFlowRegistry>,
) -> Result<()> {
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = JsonFrameReader::new(reader, CONTROL_FRAME_LIMIT);
    write_json(
        &mut writer,
        &ControlMessage::Hello {
            role: Role::Home,
            id: config.id.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    match reader
        .read_with_timeout::<ControlMessage>(Duration::from_secs(config.handshake_timeout_secs))
        .await?
    {
        ControlMessage::Hello { role, id } if role == Role::Server && id == config.server_id => {}
        _ => bail!("server sent an invalid HELLO"),
    }
    write_json(
        &mut writer,
        &ControlMessage::HomeRegister {
            catalog: Catalog {
                home_id: config.id.clone(),
                home_alias: config.alias.clone(),
                generation: 1,
                services: config.services.clone(),
            },
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    info!(server = %config.server_control_addr, "home agent registered");

    let mut heartbeat = interval(Duration::from_secs(10));
    let mut nonce = 0_u64;
    let mut last_received = Instant::now();
    loop {
        tokio::select! {
            message = reader.read::<ControlMessage>() => {
                last_received = Instant::now();
                match message? {
                    ControlMessage::OpenWork { work_id, work_secret } => {
                        let config = Arc::clone(&config);
                        let tls = Arc::clone(&tls);
                        let permits = Arc::clone(&permits);
                        let tcp_flows = Arc::clone(&tcp_flows);
                        tokio::spawn(async move {
                            if let Err(error) = run_work(config, tls, permits, tcp_flows, work_id, work_secret).await {
                                warn!(%work_id, %error, "home work failed");
                            }
                        });
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(&mut writer, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
                    _ => bail!("unexpected message from server"),
                }
            }
            _ = heartbeat.tick() => {
                if last_received.elapsed() > Duration::from_secs(30) {
                    bail!("server control heartbeat timed out");
                }
                nonce = nonce.wrapping_add(1);
                write_json(&mut writer, &ControlMessage::Heartbeat { nonce }, CONTROL_FRAME_LIMIT).await?;
            }
        }
    }
}

async fn run_work(
    config: Arc<Config>,
    tls: Arc<TlsMaterial>,
    permits: Arc<Semaphore>,
    tcp_flows: Arc<TcpFlowRegistry>,
    work_id: Uuid,
    work_secret: Vec<u8>,
) -> Result<()> {
    let mut socket = TcpStream::connect(&config.server_data_addr).await?;
    write_preface(&mut socket, RouteSide::Home, work_id, &work_secret).await?;
    let mut stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        tls.business_acceptor.accept(socket),
    )
    .await
    .context("business TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Travel, None, &config.travel_spki_pins)?;

    let open = JsonFrameReader::new(&mut stream, DATA_FRAME_LIMIT)
        .read_with_timeout::<DataFrame>(Duration::from_secs(config.handshake_timeout_secs))
        .await?;
    let DataFrame::Open {
        flow_id,
        carrier_id,
        service_id,
        protocol,
    } = open
    else {
        bail!("first business frame must be OPEN");
    };
    let service = config
        .services
        .iter()
        .find(|candidate| candidate.id == service_id && candidate.protocol == protocol)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("unknown or mismatched service"))?;

    match protocol {
        ServiceProtocol::Tcp => {
            tcp_flows
                .attach(
                    identity.id,
                    flow_id,
                    service,
                    IncomingCarrier { carrier_id, stream },
                )
                .await
        }
        ServiceProtocol::Udp => {
            let _permit = permits
                .try_acquire_owned()
                .map_err(|_| anyhow::anyhow!("home active-flow limit reached"))?;
            serve_udp(stream, flow_id, carrier_id, &service, config.udp_idle_secs).await
        }
    }
}

async fn serve_udp(
    stream: ServerTlsStream<TcpStream>,
    flow_id: Uuid,
    carrier_id: Uuid,
    service: &Service,
    idle_secs: u64,
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(&service.target).await?;
    let (mut tls_reader, mut tls_writer) = tokio::io::split(stream);
    let mut tls_reader = JsonFrameReader::new(&mut tls_reader, DATA_FRAME_LIMIT);
    write_json(
        &mut tls_writer,
        &DataFrame::OpenOk {
            flow_id,
            carrier_id,
            receive_offset: 0,
            send_offset: 0,
        },
        DATA_FRAME_LIMIT,
    )
    .await?;
    let mut send_sequence = 0_u64;
    let mut receive_sequence = 0_u64;
    let mut buffer = vec![0_u8; 65_507];
    loop {
        tokio::select! {
            response = timeout(Duration::from_secs(idle_secs), socket.recv(&mut buffer)) => {
                let count = response.context("UDP association idle timeout")??;
                write_json(&mut tls_writer, &DataFrame::Datagram { flow_id, sequence: send_sequence, bytes: buffer[..count].to_vec() }, DATA_FRAME_LIMIT).await?;
                send_sequence = send_sequence.wrapping_add(1);
            }
            frame = timeout(Duration::from_secs(idle_secs), tls_reader.read::<DataFrame>()) => {
                match frame.context("UDP association idle timeout")?? {
                    DataFrame::Datagram { flow_id: id, sequence, bytes } if id == flow_id && sequence >= receive_sequence && bytes.len() <= 65_507 => {
                        if sequence == receive_sequence {
                            socket.send(&bytes).await?;
                            receive_sequence = receive_sequence.wrapping_add(1);
                        }
                    }
                    DataFrame::Close { flow_id: id, .. } if id == flow_id => return Ok(()),
                    _ => bail!("invalid UDP flow frame"),
                }
            }
        }
    }
}
