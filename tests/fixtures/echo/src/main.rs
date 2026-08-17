#![forbid(unsafe_code)]

use anyhow::Result;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, UdpSocket},
};

#[tokio::main]
async fn main() -> Result<()> {
    tokio::try_join!(tcp_echo(), udp_echo())?;
    Ok(())
}

async fn tcp_echo() -> Result<()> {
    let listener = TcpListener::bind("0.0.0.0:7001").await?;
    let next_connection = Arc::new(AtomicU64::new(1));
    loop {
        let (socket, _) = listener.accept().await?;
        let connection_id = next_connection.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            let (reader, mut writer) = socket.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = Vec::new();
            loop {
                line.clear();
                let count = reader.read_until(b'\n', &mut line).await?;
                if count == 0 {
                    writer.shutdown().await?;
                    return Ok::<_, std::io::Error>(());
                }
                writer
                    .write_all(format!("{connection_id}:").as_bytes())
                    .await?;
                writer.write_all(&line).await?;
            }
        });
    }
}

async fn udp_echo() -> Result<()> {
    let socket = UdpSocket::bind("0.0.0.0:7002").await?;
    let mut buffer = vec![0_u8; 65_507];
    loop {
        let (count, peer) = socket.recv_from(&mut buffer).await?;
        socket.send_to(&buffer[..count], peer).await?;
    }
}
