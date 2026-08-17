#![forbid(unsafe_code)]

use std::{future::pending, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::Parser;
use flowsplice_core::{
    CONTROL_FRAME_LIMIT, DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    config::load_toml,
    frame::{read_json, write_json},
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
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    sync::Semaphore,
    time::{interval, sleep, timeout},
};
use tokio_rustls::{client::TlsStream as ClientTlsStream, server::TlsStream as ServerTlsStream};
use tracing::{info, warn};
use uuid::Uuid;

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

#[tokio::main]
async fn main() -> Result<()> {
    init_crypto();
    tracing_subscriber::fmt()
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
    let config = Arc::new(config);
    let permits = Arc::new(Semaphore::new(config.max_active_flows));
    loop {
        if let Err(error) = run_control(Arc::clone(&config), Arc::clone(&permits)).await {
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

async fn run_control(config: Arc<Config>, permits: Arc<Semaphore>) -> Result<()> {
    let connector = client_connector(
        &config.management_cert,
        &config.management_key,
        &config.management_ca,
    )?;
    let socket = TcpStream::connect(&config.server_control_addr).await?;
    let stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        connector.connect(server_name(&config.server_name)?, socket),
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
    run_control_session(stream, config, permits).await
}

async fn run_control_session(
    mut stream: ClientTlsStream<TcpStream>,
    config: Arc<Config>,
    permits: Arc<Semaphore>,
) -> Result<()> {
    write_json(
        &mut stream,
        &ControlMessage::Hello {
            role: Role::Home,
            id: config.id.clone(),
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    match read_json::<_, ControlMessage>(&mut stream, CONTROL_FRAME_LIMIT).await? {
        ControlMessage::Hello { role, id } if role == Role::Server && id == config.server_id => {}
        _ => bail!("server sent an invalid HELLO"),
    }
    write_json(
        &mut stream,
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
    loop {
        tokio::select! {
            message = read_json::<_, ControlMessage>(&mut stream, CONTROL_FRAME_LIMIT) => {
                match message? {
                    ControlMessage::OpenWork { work_id, work_secret } => {
                        let Ok(permit) = Arc::clone(&permits).try_acquire_owned() else {
                            warn!(%work_id, "home active-flow limit reached");
                            continue;
                        };
                        let config = Arc::clone(&config);
                        tokio::spawn(async move {
                            let _permit = permit;
                            if let Err(error) = run_work(config, work_id, work_secret).await {
                                warn!(%work_id, %error, "home work failed");
                            }
                        });
                    }
                    ControlMessage::Heartbeat { nonce } => {
                        write_json(&mut stream, &ControlMessage::HeartbeatAck { nonce }, CONTROL_FRAME_LIMIT).await?;
                    }
                    ControlMessage::HeartbeatAck { .. } => {}
                    _ => bail!("unexpected message from server"),
                }
            }
            _ = heartbeat.tick() => {
                nonce = nonce.wrapping_add(1);
                write_json(&mut stream, &ControlMessage::Heartbeat { nonce }, CONTROL_FRAME_LIMIT).await?;
            }
        }
    }
}

async fn run_work(config: Arc<Config>, work_id: Uuid, work_secret: Vec<u8>) -> Result<()> {
    let mut socket = TcpStream::connect(&config.server_data_addr).await?;
    write_preface(&mut socket, RouteSide::Home, work_id, &work_secret).await?;
    let acceptor = server_acceptor(
        &config.business_cert,
        &config.business_key,
        &config.business_ca,
    )?;
    let mut stream = timeout(
        Duration::from_secs(config.handshake_timeout_secs),
        acceptor.accept(socket),
    )
    .await
    .context("business TLS handshake timed out")??;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Travel, None, &config.travel_spki_pins)?;

    let open = read_json::<_, DataFrame>(&mut stream, DATA_FRAME_LIMIT).await?;
    let DataFrame::Open {
        flow_id,
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
        ServiceProtocol::Tcp => serve_tcp(stream, flow_id, &service).await,
        ServiceProtocol::Udp => serve_udp(stream, flow_id, &service, config.udp_idle_secs).await,
    }
}

async fn serve_tcp(
    stream: ServerTlsStream<TcpStream>,
    flow_id: Uuid,
    service: &Service,
) -> Result<()> {
    let target = TcpStream::connect(&service.target)
        .await
        .with_context(|| format!("failed to connect service {}", service.id))?;
    let (mut tls_reader, mut tls_writer) = tokio::io::split(stream);
    let (mut target_reader, mut target_writer) = target.into_split();
    write_json(
        &mut tls_writer,
        &DataFrame::OpenOk { flow_id },
        DATA_FRAME_LIMIT,
    )
    .await?;

    let mut send_offset = 0_u64;
    let mut receive_offset = 0_u64;
    let mut local_eof = false;
    let mut remote_eof = false;
    let mut buffer = vec![0_u8; MAX_DATA_PAYLOAD];
    while !(local_eof && remote_eof) {
        tokio::select! {
            read = async {
                if local_eof { pending::<std::io::Result<usize>>().await } else { target_reader.read(&mut buffer).await }
            } => {
                let count = read?;
                if count == 0 {
                    local_eof = true;
                    write_json(&mut tls_writer, &DataFrame::Fin { flow_id, final_offset: send_offset }, DATA_FRAME_LIMIT).await?;
                } else {
                    let bytes = buffer[..count].to_vec();
                    write_json(&mut tls_writer, &DataFrame::Data { flow_id, offset: send_offset, bytes }, DATA_FRAME_LIMIT).await?;
                    send_offset += count as u64;
                }
            }
            frame = read_json::<_, DataFrame>(&mut tls_reader, DATA_FRAME_LIMIT) => {
                match frame? {
                    DataFrame::Data { flow_id: id, offset, bytes } if id == flow_id && offset == receive_offset && bytes.len() <= MAX_DATA_PAYLOAD => {
                        target_writer.write_all(&bytes).await?;
                        receive_offset += bytes.len() as u64;
                        write_json(&mut tls_writer, &DataFrame::Ack { flow_id, next_offset: receive_offset }, DATA_FRAME_LIMIT).await?;
                    }
                    DataFrame::Fin { flow_id: id, final_offset } if id == flow_id && final_offset == receive_offset => {
                        target_writer.shutdown().await?;
                        remote_eof = true;
                    }
                    DataFrame::Ack { flow_id: id, .. } if id == flow_id => {}
                    DataFrame::Close { flow_id: id, reason } if id == flow_id => bail!("peer closed flow: {reason}"),
                    _ => bail!("invalid TCP flow frame"),
                }
            }
        }
    }
    Ok(())
}

async fn serve_udp(
    stream: ServerTlsStream<TcpStream>,
    flow_id: Uuid,
    service: &Service,
    idle_secs: u64,
) -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:0").await?;
    socket.connect(&service.target).await?;
    let (mut tls_reader, mut tls_writer) = tokio::io::split(stream);
    write_json(
        &mut tls_writer,
        &DataFrame::OpenOk { flow_id },
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
            frame = timeout(Duration::from_secs(idle_secs), read_json::<_, DataFrame>(&mut tls_reader, DATA_FRAME_LIMIT)) => {
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
