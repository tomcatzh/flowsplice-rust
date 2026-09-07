//! Real encrypted, in-process socket acceptance against the disposable E2E topology.
use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use flowsplice_core::protocol::ServiceProtocol;
use flowsplice_home_core::{HomeRuntime, HomeRuntimeConfig};
use flowsplice_transport::SocketServices;
use flowsplice_travel_core::{ServiceBinding, TravelCore};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

#[tokio::main]
async fn main() -> Result<()> {
    flowsplice_core::init_crypto();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("home") if args.len() == 3 => serve(Path::new(&args[2])).await,
        Some("travel") if args.len() == 6 => tokio::time::timeout(
            Duration::from_secs(120),
            probe(&args[2], &args[3], &args[4], &args[5]),
        )
        .await
        .context("socket acceptance timed out")?,
        _ => {
            bail!("usage: socket-probe home CONFIG | travel CONFIG PASSWORD_FILE ROOT_FILE HOME_ID")
        }
    }
}

fn no_listeners() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        // /proc/net includes the container's DNS proxy and every other socket in its
        // network namespace. Only descriptors owned by this process belong to Travel.
        let sockets: std::collections::HashSet<String> = std::fs::read_dir("/proc/self/fd")?
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter_map(|path| {
                let value = path.to_str()?;
                Some(
                    value
                        .strip_prefix("socket:[")?
                        .strip_suffix(']')?
                        .to_owned(),
                )
            })
            .collect();
        for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
            for line in std::fs::read_to_string(path)?.lines().skip(1) {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.get(3) == Some(&"0A")
                    && fields.get(9).is_some_and(|inode| sockets.contains(*inode))
                {
                    bail!("in-process Travel opened a TCP listener");
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = std::process::Command::new("lsof")
            .args([
                "-a",
                "-p",
                &std::process::id().to_string(),
                "-iTCP",
                "-sTCP:LISTEN",
                "-F",
                "n",
            ])
            .output()?;
        if output.status.success() && !output.stdout.is_empty() {
            bail!("in-process Travel opened a TCP listener");
        }
        if !output.status.success() && output.status.code() != Some(1) {
            bail!("listener inspection failed");
        }
    }
    Ok(())
}

async fn probe(config: &str, password_file: &str, root_file: &str, home: &str) -> Result<()> {
    let password = std::fs::read_to_string(password_file)?;
    let root = std::fs::read_to_string(root_file)?;
    let runtime =
        TravelCore::start_in_process(Path::new(config), password.trim_end(), root.trim()).await?;
    no_listeners()?;
    let binding = ServiceBinding {
        home_id: home.to_owned(),
        service_id: "tcp-echo".to_owned(),
        protocol: ServiceProtocol::Tcp,
    };
    let stream = runtime.connect_tcp(&binding).await?;
    no_listeners()?;
    let payload: Vec<u8> = (0..131_071)
        .map(|i| if i % 2 == 0 { 0 } else { 255 })
        .chain(*b"\n")
        .collect();
    let (mut reader, mut writer) = tokio::io::split(stream);
    let send = async {
        writer.write_all(&payload).await?;
        writer.shutdown().await
    };
    let receive = async {
        let mut received = Vec::new();
        reader.read_to_end(&mut received).await?;
        if !received.ends_with(&payload) {
            bail!("TCP byte stream or half-close corrupted");
        }
        Ok::<_, anyhow::Error>(())
    };
    let (write_result, read_result) = tokio::join!(send, receive);
    write_result?;
    read_result?;
    drop(reader);
    drop(writer);
    concurrent_roundtrips(&runtime, &binding).await?;
    let missing = ServiceBinding {
        service_id: "socket-probe-unknown-service".to_owned(),
        ..binding.clone()
    };
    if tokio::time::timeout(Duration::from_secs(10), runtime.connect_tcp(&missing))
        .await
        .context("unknown service lookup did not finish")?
        .is_ok()
    {
        bail!("unknown service unexpectedly connected");
    }
    tcp_roundtrip(&runtime, &binding, 5).await?;
    let udp = runtime
        .connect_udp(&ServiceBinding {
            service_id: "udp-echo".to_owned(),
            protocol: ServiceProtocol::Udp,
            ..binding.clone()
        })
        .await?;
    for payload in [vec![], vec![0, 255, 1, 0], vec![42; 65_507]] {
        udp.send(&payload).await?;
        if udp.recv().await? != payload {
            bail!("UDP message boundary or bytes corrupted");
        }
    }
    no_listeners()?;
    drop(udp);
    shutdown_with_backpressure(&runtime, &binding).await?;
    if runtime.status().await.active_flows != 0 {
        bail!("socket shutdown retained active flows");
    }
    no_listeners()?;
    println!(
        "{{\"checkpoint\":\"in-process-encrypted-sockets\",\"tcp_half_close\":true,\"udp_empty_and_maximum\":true,\"listeners\":0,\"concurrent_tcp\":true,\"unknown_service_recovery\":true,\"blocked_writer_shutdown\":true}}"
    );
    Ok(())
}

async fn tcp_roundtrip(runtime: &TravelCore, binding: &ServiceBinding, tag: u8) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let stream = runtime.connect_tcp(binding).await?;
        let payload: Vec<u8> = [0, 255, tag]
            .into_iter()
            .cycle()
            .take(32_766)
            .chain(*b"\n")
            .collect();
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = reader.take(u64::try_from(payload.len())? + 257);
        let send = async {
            writer.write_all(&payload).await?;
            writer.shutdown().await
        };
        let receive = async {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await?;
            if bytes.len() > payload.len() + 256 || !bytes.ends_with(&payload) {
                bail!("concurrent TCP flow {tag} returned mixed or corrupt bytes");
            }
            Ok::<_, anyhow::Error>(())
        };
        let (write_result, read_result) = tokio::join!(send, receive);
        write_result?;
        read_result?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("TCP roundtrip timed out")?
}

async fn concurrent_roundtrips(runtime: &TravelCore, binding: &ServiceBinding) -> Result<()> {
    tokio::try_join!(
        tcp_roundtrip(runtime, binding, 1),
        tcp_roundtrip(runtime, binding, 2),
        tcp_roundtrip(runtime, binding, 3),
        tcp_roundtrip(runtime, binding, 4)
    )?;
    Ok(())
}

async fn shutdown_with_backpressure(runtime: &TravelCore, binding: &ServiceBinding) -> Result<()> {
    let stream = runtime.connect_tcp(binding).await?;
    // Hold the read half open without draining it: only runtime shutdown can unblock the writer.
    let (reader, mut writer) = tokio::io::split(stream);
    let (progress, mut observed) = tokio::sync::watch::channel(0_usize);
    let mut writing: tokio::task::JoinHandle<Result<()>> = tokio::spawn(async move {
        let mut chunk = vec![b'x'; 16_384];
        *chunk.last_mut().context("empty backpressure chunk")? = b'\n';
        for count in 1..=2_048 {
            writer.write_all(&chunk).await?;
            progress.send_replace(count);
        }
        bail!("backpressure did not block within the 32 MiB attempt limit")
    });
    let blocked = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match tokio::time::timeout(Duration::from_secs(1), observed.changed()).await {
                Ok(result) => {
                    result.context("writer ended before backpressure")?;
                }
                Err(_) if *observed.borrow() > 0 && !writing.is_finished() => {
                    return Ok::<_, anyhow::Error>(());
                }
                Err(_) => bail!("writer made no initial progress"),
            }
        }
    })
    .await
    .context("backpressure detection timed out");
    if let Err(error) = blocked.and_then(|result| result) {
        writing.abort();
        let _ = writing.await;
        return Err(error);
    }
    no_listeners()?;
    tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .context("shutdown blocked behind unread application output")?;
    let stopped = tokio::time::timeout(Duration::from_secs(2), &mut writing).await;
    if stopped.is_err() {
        writing.abort();
        let _ = writing.await;
        bail!("application writer survived runtime shutdown");
    }
    let failure = stopped?
        .context("application writer task panicked")?
        .err()
        .context("writer unexpectedly completed")?;
    if failure.downcast_ref::<std::io::Error>().is_none() {
        return Err(failure.context("writer did not terminate with closed stream I/O"));
    }
    drop(reader);
    Ok(())
}

async fn serve(path: &Path) -> Result<()> {
    let mut config: toml::Value = toml::from_str(&std::fs::read_to_string(path)?)?;
    let table = config
        .as_table_mut()
        .context("Home configuration must be a table")?;
    table.remove("ui_listen");
    table.remove("issuer");
    let mut config: HomeRuntimeConfig = config.try_into()?;
    let services = Arc::new(SocketServices::default());
    let mut handlers = tokio::task::JoinSet::new();
    for service in &mut config.services {
        "in-process".clone_into(&mut service.target);
        match service.protocol {
            ServiceProtocol::Tcp => {
                let mut listener = services.bind_tcp(service.id.clone(), 16)?;
                handlers.spawn(async move {
                    let mut connections = tokio::task::JoinSet::new();
                    loop {
                        let (stream, _peer) = listener.accept().await?;
                        while connections.try_join_next().is_some() {}
                        connections.spawn(async move {
                            let (reader, mut writer) = tokio::io::split(stream);
                            let mut reader = BufReader::new(reader);
                            let mut line = Vec::new();
                            loop {
                                line.clear();
                                if reader.read_until(b'\n', &mut line).await? == 0 {
                                    writer.shutdown().await?;
                                    return Ok::<_, anyhow::Error>(());
                                }
                                writer.write_all(b"in-process:").await?;
                                writer.write_all(&line).await?;
                            }
                        });
                    }
                    #[allow(unreachable_code)]
                    Ok::<_, anyhow::Error>(())
                });
            }
            ServiceProtocol::Udp => {
                let mut listener = services.bind_udp(service.id.clone(), 16)?;
                handlers.spawn(async move {
                    let mut connections = tokio::task::JoinSet::new();
                    loop {
                        let (socket, _peer) = listener.accept().await?;
                        while connections.try_join_next().is_some() {}
                        connections.spawn(async move {
                            loop {
                                let bytes = socket.recv().await?;
                                socket.send(&bytes).await?;
                            }
                            #[allow(unreachable_code)]
                            Ok::<_, anyhow::Error>(())
                        });
                    }
                    #[allow(unreachable_code)]
                    Ok::<_, anyhow::Error>(())
                });
            }
        }
    }
    let runtime = HomeRuntime::load(config.clone(), services.clone())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    {
        let running = runtime.run_serving();
        tokio::pin!(running);
        tokio::select! {
            result = &mut running => return result,
            _ = terminate.recv() => {}
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            runtime.shutdown().await;
            running.await
        })
        .await
        .context("Home shutdown retained background work")??;
    }
    drop(runtime);
    // Reopening in this same process proves that completed work released its state store.
    let reopened = HomeRuntime::load(config, services)?;
    drop(reopened);
    no_listeners()?;
    println!("{{\"checkpoint\":\"in-process-home-shutdown-store-reopened\"}}");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn listener_inspection_detects_and_releases_an_owned_listener() -> anyhow::Result<()> {
        super::no_listeners()?;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        assert!(super::no_listeners().is_err());
        drop(listener);
        super::no_listeners()
    }
}
