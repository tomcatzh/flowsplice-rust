#![forbid(unsafe_code)]

use std::{env, fs, path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use flowsplice_core::{
    frame::{JsonFrameReader, write_json},
    protocol::{ControlMessage, Role, TravelConnectionPurpose},
    tls::{
        identity_client_connector_with_private_key, identity_server_name, peer_identity,
        require_peer,
    },
};
use flowsplice_enrollment::key::load_private_key;
use tokio::{io::AsyncReadExt, io::AsyncWriteExt, net::TcpStream, time::timeout};
use uuid::Uuid;

const CONTROL_FRAME_LIMIT: usize = 262_144;

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = env::args().collect::<Vec<_>>();
    if arguments.len() != 4 {
        bail!("usage: flowsplice-travel-login-probe MODE RELAY_ADDR RELAY_ID");
    }
    let mode = &arguments[1];
    let relay_addr = &arguments[2];
    let relay_id = &arguments[3];
    let password = fs::read_to_string("/travel/test-password.txt")
        .context("failed to read test-only Travel password")?;
    let key = load_private_key(
        Path::new("/travel/travel-management.key"),
        Some(password.trim_end().as_bytes()),
        false,
    )?;
    let connector = identity_client_connector_with_private_key(
        Path::new("/travel/travel-management.crt"),
        key,
        Path::new("/certs/management-ca.crt"),
    )?;
    let socket = TcpStream::connect(relay_addr).await?;
    let mut stream = connector.connect(identity_server_name()?, socket).await?;
    let identity = peer_identity(stream.get_ref().1.peer_certificates())?;
    require_peer(&identity, Role::Relay, Some(relay_id), &[])?;
    if mode == "slow-frame" {
        stream.write_all(&[0, 0]).await?;
        let started = tokio::time::Instant::now();
        let mut byte = [0_u8; 1];
        match timeout(Duration::from_secs(13), stream.read(&mut byte)).await {
            Err(_) => bail!("Relay {relay_id} did not close an incomplete control frame"),
            Ok(Ok(0) | Err(_)) => {}
            Ok(Ok(_)) => bail!("Relay {relay_id} sent data for an incomplete control frame"),
        }
        if started.elapsed() >= Duration::from_secs(12) {
            bail!("Relay {relay_id} kept an incomplete control frame open too long");
        }
        println!("slow control frame closed by {relay_id}");
        return Ok(());
    }
    if mode != "duplicate" {
        bail!("unknown probe mode: {mode}");
    }
    write_json(
        &mut stream,
        &ControlMessage::TravelHello {
            id: "travel-1".to_owned(),
            session_id: Uuid::new_v4(),
            purpose: TravelConnectionPurpose::Catalog,
        },
        CONTROL_FRAME_LIMIT,
    )
    .await?;
    let response = JsonFrameReader::new(&mut stream, CONTROL_FRAME_LIMIT)
        .read_with_timeout::<ControlMessage>(Duration::from_secs(10))
        .await?;
    match response {
        ControlMessage::TravelHelloDenied { reason } if reason.contains("already online") => {
            println!("duplicate Travel login rejected by {relay_id}");
            Ok(())
        }
        ControlMessage::TravelHelloDenied { reason } => {
            bail!("Relay {relay_id} rejected duplicate login for an unexpected reason: {reason}")
        }
        _ => bail!("Relay {relay_id} accepted a duplicate Travel login"),
    }
}
