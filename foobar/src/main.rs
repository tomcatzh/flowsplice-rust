#![forbid(unsafe_code)]

use std::{
    io,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::{sleep, timeout},
};
use uuid::Uuid;

const MAGIC: &[u8; 8] = b"FSFOOBAR";
const PROBE_SIZE: usize = 64;

#[derive(Parser)]
#[command(
    version,
    about = "Low-rate, single-connection FlowSplice continuity probe"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the TCP loopback target.
    Serve {
        /// Address exposed to Home Agent as a TCP service target.
        #[arg(long, default_value = "127.0.0.1:18080")]
        listen: SocketAddr,
        /// Required to bind the target outside loopback.
        #[arg(long)]
        allow_remote_listen: bool,
    },
    /// Probe a Travel Agent TCP mapping over one connection without reconnecting.
    Probe {
        /// Travel Agent local TCP mapping.
        #[arg(long, default_value = "127.0.0.1:10080")]
        addr: String,
        /// Delay after one successful response before the next probe.
        #[arg(long, default_value_t = 5)]
        interval_secs: u64,
        /// Maximum time allowed for connect and for each complete echo exchange.
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        /// Number of probes before a successful exit; zero runs until Ctrl-C.
        #[arg(long, default_value_t = 0)]
        count: u64,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().command {
        Command::Serve {
            listen,
            allow_remote_listen,
        } => serve(listen, allow_remote_listen).await,
        Command::Probe {
            addr,
            interval_secs,
            timeout_secs,
            count,
        } => probe(&addr, interval_secs, timeout_secs, count).await,
    }
}

async fn serve(listen: SocketAddr, allow_remote_listen: bool) -> Result<()> {
    if !listen.ip().is_loopback() && !allow_remote_listen {
        bail!("refusing non-loopback listen {listen} without --allow-remote-listen");
    }
    let listener = TcpListener::bind(listen)
        .await
        .with_context(|| format!("failed to bind foobar target {listen}"))?;
    let local_address = listener.local_addr()?;
    let next_connection = Arc::new(AtomicU64::new(1));
    println!("event=listening address={local_address}");

    loop {
        let (socket, peer) = listener.accept().await?;
        socket.set_nodelay(true)?;
        let connection_id = next_connection.fetch_add(1, Ordering::Relaxed);
        println!("event=accepted connection={connection_id} peer={peer}");
        tokio::spawn(async move {
            match echo_stream(socket).await {
                Ok(bytes) => {
                    println!("event=closed connection={connection_id} peer={peer} bytes={bytes}");
                }
                Err(error) => eprintln!(
                    "event=connection_error connection={connection_id} peer={peer} error={error}"
                ),
            }
        });
    }
}

async fn echo_stream<S>(stream: S) -> io::Result<u64>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let bytes = tokio::io::copy(&mut reader, &mut writer).await?;
    writer.shutdown().await?;
    Ok(bytes)
}

async fn probe(addr: &str, interval_secs: u64, timeout_secs: u64, count: u64) -> Result<()> {
    if interval_secs == 0 {
        bail!("--interval-secs must be positive");
    }
    if timeout_secs == 0 {
        bail!("--timeout-secs must be positive");
    }
    let interval = Duration::from_secs(interval_secs);
    let response_timeout = Duration::from_secs(timeout_secs);
    let mut socket = timeout(response_timeout, TcpStream::connect(addr))
        .await
        .with_context(|| format!("connection to {addr} timed out after {timeout_secs}s"))?
        .with_context(|| format!("failed to connect to {addr}"))?;
    socket.set_nodelay(true)?;

    let session_id = Uuid::new_v4();
    let started = Instant::now();
    let mut completed = 0_u64;
    let mut maximum_rtt = Duration::ZERO;
    let shutdown = tokio::signal::ctrl_c();
    tokio::pin!(shutdown);
    println!(
        "event=connected address={addr} session={} interval_secs={interval_secs} timeout_secs={timeout_secs}",
        session_id.simple()
    );

    loop {
        let sequence = completed + 1;
        let expected = probe_frame(session_id, sequence);
        let mut actual = [0_u8; PROBE_SIZE];
        let sent_at = Instant::now();
        let exchange = timeout(response_timeout, async {
            socket.write_all(&expected).await?;
            socket.read_exact(&mut actual).await?;
            Ok::<(), io::Error>(())
        })
        .await
        .with_context(|| format!("probe {sequence} timed out after {timeout_secs}s"))?;
        exchange.with_context(|| format!("probe {sequence} connection failed"))?;
        let rtt = sent_at.elapsed();

        if actual != expected {
            let offset = actual
                .iter()
                .zip(expected.iter())
                .position(|(left, right)| left != right)
                .unwrap_or(PROBE_SIZE);
            bail!("probe {sequence} returned corrupt or out-of-order data at byte {offset}");
        }

        completed = sequence;
        maximum_rtt = maximum_rtt.max(rtt);
        println!(
            "event=probe_ok sequence={sequence} rtt_us={} max_rtt_us={}",
            rtt.as_micros(),
            maximum_rtt.as_micros()
        );
        if count != 0 && completed >= count {
            print_summary(session_id, completed, started.elapsed(), maximum_rtt);
            return Ok(());
        }

        tokio::select! {
            result = &mut shutdown => {
                result.context("failed to listen for Ctrl-C")?;
                print_summary(session_id, completed, started.elapsed(), maximum_rtt);
                return Ok(());
            }
            () = sleep(interval) => {}
        }
    }
}

fn probe_frame(session_id: Uuid, sequence: u64) -> [u8; PROBE_SIZE] {
    let mut frame = [0_u8; PROBE_SIZE];
    frame[..MAGIC.len()].copy_from_slice(MAGIC);
    frame[8..24].copy_from_slice(session_id.as_bytes());
    frame[24..32].copy_from_slice(&sequence.to_be_bytes());
    for chunk in frame[32..].chunks_exact_mut(8) {
        chunk.copy_from_slice(&sequence.to_le_bytes());
    }
    frame
}

fn print_summary(session_id: Uuid, completed: u64, elapsed: Duration, maximum_rtt: Duration) {
    println!(
        "event=summary session={} completed={completed} elapsed_secs={} max_rtt_us={}",
        session_id.simple(),
        elapsed.as_secs(),
        maximum_rtt.as_micros()
    );
}

#[cfg(test)]
mod tests {
    use super::{Args, Command, MAGIC, PROBE_SIZE, echo_stream, probe_frame};
    use anyhow::{Result, bail};
    use clap::Parser;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use uuid::Uuid;

    #[test]
    fn frame_carries_magic_session_and_sequence() {
        let session = Uuid::from_u128(0x0011_2233_4455_6677_8899_aabb_ccdd_eeff);
        let frame = probe_frame(session, 42);
        assert_eq!(&frame[..8], MAGIC);
        assert_eq!(&frame[8..24], session.as_bytes());
        assert_eq!(&frame[24..32], &42_u64.to_be_bytes());
        assert_eq!(frame.len(), PROBE_SIZE);
    }

    #[test]
    fn probe_defaults_to_five_second_low_rate() -> Result<()> {
        let args = Args::try_parse_from(["flowsplice-foobar", "probe"])?;
        let Command::Probe {
            addr,
            interval_secs,
            timeout_secs,
            count,
        } = args.command
        else {
            bail!("probe subcommand was not selected");
        };
        assert_eq!(addr, "127.0.0.1:10080");
        assert_eq!(interval_secs, 5);
        assert_eq!(timeout_secs, 30);
        assert_eq!(count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn one_connection_echoes_multiple_probes_exactly() -> Result<()> {
        let (server_stream, mut client) = tokio::io::duplex(PROBE_SIZE * 4);
        let server = tokio::spawn(echo_stream(server_stream));
        let session = Uuid::new_v4();
        for sequence in 1..=2 {
            let expected = probe_frame(session, sequence);
            let mut actual = [0_u8; PROBE_SIZE];
            client.write_all(&expected).await?;
            client.read_exact(&mut actual).await?;
            assert_eq!(actual, expected);
        }
        client.shutdown().await?;
        let echoed = server.await??;
        assert_eq!(echoed, (PROBE_SIZE * 2) as u64);
        Ok(())
    }
}
