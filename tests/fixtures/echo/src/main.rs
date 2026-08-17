#![forbid(unsafe_code)]

use anyhow::Result;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
};

#[tokio::main]
async fn main() -> Result<()> {
    tokio::try_join!(tcp_echo(), udp_echo())?;
    Ok(())
}

async fn tcp_echo() -> Result<()> {
    let listener = TcpListener::bind("0.0.0.0:7001").await?;
    loop {
        let (mut socket, _) = listener.accept().await?;
        tokio::spawn(async move {
            let mut buffer = vec![0_u8; 64 * 1024];
            loop {
                let count = socket.read(&mut buffer).await?;
                if count == 0 {
                    socket.shutdown().await?;
                    return Ok::<_, std::io::Error>(());
                }
                socket.write_all(&buffer[..count]).await?;
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
