use anyhow::{Context, Result, bail, ensure};
use flowsplice_home_core::{HomeRuntime, HomeRuntimeConfig, SocketServices};
use flowsplice_travel_core::{BusinessDescriptor, TravelCore, business};
use std::{fs, path::Path, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const DEADLINE: Duration = Duration::from_secs(120);

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["home", config, service_id] => home(config, service_id).await,
        [
            "enroll",
            relay,
            descriptor,
            root,
            install_dir,
            password,
            travel_id,
        ] => {
            let options = business::BusinessEnrollmentOptions {
                travel_id: travel_id.to_string(),
                install_dir: install_dir.into(),
                relay_address: relay.to_string(),
                deployment_root_public_key: fs::read_to_string(root)?.trim().to_owned(),
                private_key_password: read_password(password)?,
                wait_timeout_secs: 110,
            };
            tokio::time::timeout(
                DEADLINE,
                business::enroll(options, read_descriptor(descriptor)?, |progress| {
                    println!(
                        "enrollment phase={:?} verification_code={:?}",
                        progress.phase, progress.verification_code
                    );
                }),
            )
            .await
            .context("enrollment exceeded 120 seconds")??;
            println!("PASS enrollment");
            Ok(())
        }
        ["travel", config, password, root, descriptor] => {
            travel(config, password, root, descriptor).await
        }
        _ => bail!(
            "usage: home CONFIG SERVICE_ID | enroll RELAY DESCRIPTOR ROOT_FILE INSTALL_DIR PASSWORD_FILE TRAVEL_ID | travel CONFIG PASSWORD_FILE ROOT_FILE DESCRIPTOR"
        ),
    }
}

fn read_password(path: &str) -> Result<String> {
    Ok(fs::read_to_string(path)?
        .trim_end_matches(['\r', '\n'])
        .to_owned())
}

fn read_descriptor(path: &str) -> Result<BusinessDescriptor> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

async fn home(config: &str, service_id: &str) -> Result<()> {
    let config: HomeRuntimeConfig = toml::from_str(&fs::read_to_string(config)?)?;
    let services = Arc::new(SocketServices::default());
    let mut listener = services.bind_tcp(service_id.to_owned(), 16)?;
    let runtime = Arc::new(HomeRuntime::load(config, services)?);
    let running = Arc::clone(&runtime);
    let mut task = tokio::spawn(async move { running.run_serving().await });
    let work = async {
        let (mut stream, peer) = listener.accept().await?;
        ensure!(peer.lifetime.is_active(), "peer authorization inactive");
        let mut request = [0; 6];
        stream.read_exact(&mut request).await?;
        ensure!(&request == b"foobar", "unexpected request");
        ensure!(peer.lifetime.is_active(), "peer authorization ended");
        stream.write_all(b"FOOBAR").await?;
        let mut ack = [0; 3];
        stream.read_exact(&mut ack).await?;
        ensure!(&ack == b"ACK", "response was not acknowledged");
        let mut extra = [0; 1];
        ensure!(
            stream.read(&mut extra).await? == 0,
            "unexpected trailing request data"
        );
        stream.shutdown().await?;
        // Keep the serving task alive until transport finishes the flow.
        peer.lifetime.ended().await;
        Ok::<_, anyhow::Error>(())
    };
    let result = tokio::select! {
        finished = &mut task => {
            runtime.shutdown().await;
            finished??;
            bail!("Home runtime exited before exchange completed");
        }
        result = tokio::time::timeout(DEADLINE, work) => result.context("Home exchange exceeded 120 seconds").and_then(|result| result),
    };
    runtime.shutdown().await;
    tokio::time::timeout(Duration::from_secs(10), task)
        .await
        .context("Home shutdown timed out")???;
    result?;
    println!("PASS Home: foobar -> FOOBAR, ACK received, runtime stopped");
    Ok(())
}

async fn travel(config: &str, password: &str, root: &str, descriptor: &str) -> Result<()> {
    let password = read_password(password)?;
    let root = fs::read_to_string(root)?;
    let descriptor = read_descriptor(descriptor)?;
    let (runtime, approved) = tokio::time::timeout(
        DEADLINE,
        TravelCore::start_business(Path::new(config), &password, root.trim(), &descriptor),
    )
    .await
    .context("Travel startup exceeded 120 seconds")??;
    let result = tokio::time::timeout(DEADLINE, async {
        let mut stream = runtime.connect_tcp(&approved.binding).await?;
        stream.write_all(b"foobar").await?;
        let mut response = [0; 6];
        stream.read_exact(&mut response).await?;
        ensure!(&response == b"FOOBAR", "unexpected business response");
        stream.write_all(b"ACK").await?;
        stream.shutdown().await?;
        let mut extra = [0; 1];
        ensure!(
            stream.read(&mut extra).await? == 0,
            "unexpected trailing data"
        );
        // Allow the final transport acknowledgement to complete before runtime shutdown.
        while runtime.status().await.active_flows != 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("Travel exchange exceeded 120 seconds");
    tokio::time::timeout(Duration::from_secs(10), runtime.shutdown())
        .await
        .context("Travel shutdown timed out")?;
    ensure!(
        runtime.status().await.active_flows == 0,
        "active flows remain after shutdown"
    );
    result??;
    println!("PASS Travel: FOOBAR verified, EOF received, active_flows=0");
    Ok(())
}
