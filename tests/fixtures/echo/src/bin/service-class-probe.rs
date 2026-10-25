//! Disposable E2E for one service-class identity shared across multiple Home sockets.
//!
//! Every matching Home runs the in-process line echo from `socket-probe home`, which
//! answers each received line with the same bytes prefixed by `in-process:`.
use anyhow::{Context, Result, bail, ensure};
use flowsplice_core::business::ServiceClassDescriptor;
use flowsplice_core::protocol::ServiceProtocol;
use flowsplice_enrollment::load_json;
use flowsplice_storage::{StateStore, Table, WriteBatch};
use flowsplice_travel_core::{
    ServiceBinding, SocketStream, TravelCore,
    business::BusinessEnrollmentOptions,
    service_class::{self, ApprovedServiceClass, ServiceClassTarget},
};
use std::{collections::HashSet, path::Path, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use uuid::Uuid;

const IO_TIMEOUT: Duration = Duration::from_secs(15);
/// Admission includes signed route authorization and Carrier races. A runtime that starts with an
/// expired cached snapshot races before the fresh directory arrives and waits out one 10-second
/// race timeout, so allow several races rather than the bare minimum.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Authorization loss must end every open class socket within this bound.
const ACCESS_END_TIMEOUT: Duration = Duration::from_secs(180);
/// Covers the Travel Carrier recovery deadline before a refused connection reports failure.
const REFUSED_CONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// One open line-echo connection to a discovered class target.
struct Echo {
    reader: BufReader<ReadHalf<SocketStream>>,
    writer: WriteHalf<SocketStream>,
}
impl Echo {
    async fn connect(runtime: &TravelCore, target: &ServiceClassTarget) -> Result<Self> {
        let stream = tokio::time::timeout(CONNECT_TIMEOUT, runtime.connect_tcp(&binding(target)))
            .await
            .context("class connection timed out")??;
        let (reader, writer) = tokio::io::split(stream);
        let mut this = Self {
            reader: BufReader::new(reader),
            writer,
        };
        this.roundtrip().await?;
        no_listeners()?;
        Ok(this)
    }
    async fn roundtrip(&mut self) -> Result<()> {
        // Non-ASCII content proves the class stream carries arbitrary UTF-8 bytes unchanged.
        let marker = format!("类授权-class-{}", Uuid::new_v4());
        tokio::time::timeout(IO_TIMEOUT, async {
            self.writer.write_all(marker.as_bytes()).await?;
            self.writer.write_all(b"\n").await?;
            self.writer.flush().await?;
            let mut line = Vec::new();
            self.reader.read_until(b'\n', &mut line).await?;
            ensure!(
                line == format!("in-process:{marker}\n").into_bytes(),
                "class echo returned missing, mixed or corrupt bytes"
            );
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("class echo round-trip timed out")?
    }
    /// Orderly disconnect: half-close, then require the Home's matching EOF.
    async fn close(mut self) -> Result<()> {
        tokio::time::timeout(IO_TIMEOUT, async {
            self.writer.shutdown().await?;
            let mut rest = Vec::new();
            self.reader.read_to_end(&mut rest).await?;
            ensure!(rest.is_empty(), "class echo sent bytes after half-close");
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("class echo half-close timed out")?
    }
    /// Waits, without sending application bytes, until authorization loss ends the stream.
    async fn ended(&mut self) -> Result<()> {
        let mut buffer = [0_u8; 1024];
        match self.reader.read(&mut buffer).await {
            Ok(0) | Err(_) => Ok(()),
            Ok(_) => bail!("class stream delivered unsolicited bytes"),
        }
    }
    /// An ended stream must fail or stay at EOF; it must never echo again.
    async fn rejects_traffic(mut self) -> Result<()> {
        let attempt = async {
            self.writer.write_all(b"forbidden\n").await?;
            self.writer.flush().await?;
            let mut line = Vec::new();
            self.reader.read_until(b'\n', &mut line).await?;
            Ok::<_, std::io::Error>(line)
        };
        match tokio::time::timeout(IO_TIMEOUT, attempt).await {
            Ok(Ok(line)) => ensure!(line.is_empty(), "ended class socket still echoed input"),
            Ok(Err(_)) => {}
            Err(_) => bail!("ended class socket neither failed nor reached EOF"),
        }
        Ok(())
    }
}
#[cfg_attr(not(target_os = "linux"), allow(clippy::unnecessary_wraps))]
fn no_listeners() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let sockets: std::collections::HashSet<_> = std::fs::read_dir("/proc/self/fd")?
            .filter_map(std::result::Result::ok)
            .filter_map(|e| std::fs::read_link(e.path()).ok())
            .filter_map(|p| {
                Some(
                    p.to_str()?
                        .strip_prefix("socket:[")?
                        .strip_suffix(']')?
                        .to_owned(),
                )
            })
            .collect();
        for (file, udp) in [
            ("/proc/net/tcp", false),
            ("/proc/net/tcp6", false),
            ("/proc/net/udp", true),
            ("/proc/net/udp6", true),
        ] {
            for line in std::fs::read_to_string(file)?.lines().skip(1) {
                let f: Vec<_> = line.split_whitespace().collect();
                ensure!(
                    !(udp || f.get(3) == Some(&"0A"))
                        || !f.get(9).is_some_and(|i| sockets.contains(*i)),
                    "service-class Travel owns a TCP listener or UDP socket"
                );
            }
        }
    }
    Ok(())
}
fn binding(target: &ServiceClassTarget) -> ServiceBinding {
    ServiceBinding {
        home_id: target.home_id.clone(),
        service_id: target.service_id.clone(),
        protocol: target.service.protocol,
    }
}
fn summary(target: &ServiceClassTarget) -> serde_json::Value {
    serde_json::json!({"home_id":target.home_id,"service_id":target.service_id})
}
fn arguments(args: &[String]) -> Result<&str> {
    match args.first().map(String::as_str) {
        Some(mode @ ("enroll" | "exercise" | "access-end")) if args.len() == 8 => Ok(mode),
        Some("future") if args.len() == 9 => Ok("future"),
        Some("check-only") if args.len() == 4 => Ok("check-only"),
        Some("resume-fixture") if args.len() == 3 => Ok("resume-fixture"),
        _ => bail!(
            "usage: service-class-probe enroll RELAY DESCRIPTOR ROOT INSTALL PASSWORD TRAVEL_ID LABEL | exercise/access-end CONFIG PASSWORD ROOT DESCRIPTOR WRONG_HOME WRONG_SERVICE STATE | future CONFIG PASSWORD ROOT DESCRIPTOR WRONG_HOME WRONG_SERVICE STATE SIGNAL | check-only CONFIG ROOT DESCRIPTOR | resume-fixture INSTALL BOOTSTRAP_BACKUP"
        ),
    }
}
fn check_only(args: &[String]) -> Result<()> {
    let descriptor = load_json(Path::new(&args[2]))?;
    let root = std::fs::read_to_string(&args[1])?;
    let approved = service_class::load_binding(Path::new(&args[0]), root.trim(), &descriptor)?;
    println!(
        "{}",
        serde_json::json!({"checkpoint":"service-class-binding-verified", "request_id":approved.request_id,"credential_id":approved.credential_id})
    );
    Ok(())
}
fn resume_fixture(directory: &Path, backup: &Path) -> Result<()> {
    let marker = directory.join(service_class::BINDING_FILE);
    let completed: serde_json::Value = load_json(&marker)?;
    let response = completed
        .get("response")
        .context("missing completed response")?;
    ensure!(
        response
            .get("object_type")
            .and_then(serde_json::Value::as_str)
            == Some("flowsplice.service_class_travel_response"),
        "fixture marker is not a class installation"
    );
    let request_id: Uuid = serde_json::from_value(
        response
            .pointer("/request/request/request_id")
            .context("missing request ID")?
            .clone(),
    )?;
    let config_toml = std::fs::read_to_string(directory.join("travelagent.toml"))?;
    let config: toml::Value = toml::from_str(&config_toml)?;
    let configured = config
        .get("state_store")
        .and_then(toml::Value::as_str)
        .context("missing state_store")?;
    let store = StateStore::open(flowsplice_core::config::resolve_path(
        &directory.join("travelagent.toml"),
        Path::new(configured),
    ))?;
    std::fs::write(
        directory.join("business-installation.pending.json"),
        serde_json::to_vec_pretty(
            &serde_json::json!({"response":response,"config_toml":config_toml}),
        )?,
    )?;
    std::fs::copy(backup, directory.join("bootstrap-enrollment.json"))?;
    std::fs::remove_file(marker)?;
    store.apply_immediate(
        WriteBatch::new().delete(Table::EnrollmentOutbox, request_id.as_bytes().to_vec()),
    )?;
    println!("{{\"checkpoint\":\"service-class-resume-fixture-prepared\"}}");
    Ok(())
}
async fn enroll(args: &[String]) -> Result<()> {
    let descriptor = load_json(Path::new(&args[1]))?;
    let root = std::fs::read_to_string(&args[2])?;
    let password = std::fs::read_to_string(&args[4])?;
    service_class::enroll(BusinessEnrollmentOptions {
        travel_id: args[5].clone(), install_dir: args[3].clone().into(), relay_address: args[0].clone(),
        deployment_root_public_key: root.trim().into(), private_key_password: password.trim_end().into(), wait_timeout_secs: 120,
    }, descriptor, args[6].clone(), |progress| {
        println!("{}", serde_json::json!({"phase":progress.phase,"travel_id":progress.travel_id,
            "request_id":progress.request_id,"credential_id":progress.credential_id,"verification_code":progress.verification_code}));
    }).await?;
    no_listeners()?;
    println!("{{\"checkpoint\":\"service-class-enrollment-installed\"}}");
    Ok(())
}
async fn targets(
    runtime: &TravelCore,
    approved: &ApprovedServiceClass,
) -> Result<Vec<ServiceClassTarget>> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let targets = runtime.service_class_targets(approved).await?;
            let homes: HashSet<_> = targets.iter().map(|t| &t.home_id).collect();
            if homes.len() >= 2 {
                return Ok(targets);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("two class Homes were not discovered")?
}
fn matches_class(target: &ServiceClassTarget, descriptor: &ServiceClassDescriptor) -> bool {
    target.service.application_protocol == descriptor.application_protocol
        && target.service.protocol == descriptor.protocol
}
async fn refused(runtime: &TravelCore, binding: &ServiceBinding, timeout: Duration) -> Result<()> {
    match tokio::time::timeout(timeout, runtime.connect_tcp(binding)).await {
        Ok(Err(_)) => Ok(()),
        Ok(Ok(_)) => bail!("unauthorized class binding was admitted"),
        Err(_) => bail!("unauthorized class binding neither connected nor failed within bound"),
    }
}
async fn future(
    runtime: &TravelCore,
    approved: &ApprovedServiceClass,
    signal: &Path,
    original: &[ServiceClassTarget],
) -> Result<()> {
    println!("{{\"checkpoint\":\"service-class-future-ready\"}}");
    let home = tokio::time::timeout(Duration::from_secs(120), async {
        while !signal.exists() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let value: serde_json::Value = load_json(signal)?;
        Ok::<_, anyhow::Error>(
            value["home_id"]
                .as_str()
                .context("future signal requires home_id")?
                .to_owned(),
        )
    })
    .await
    .context("future Home signal timed out")??;
    ensure!(
        !original.iter().any(|target| target.home_id == home),
        "future Home was already present"
    );
    let target = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(target) = runtime
                .service_class_targets(approved)
                .await?
                .into_iter()
                .find(|target| target.home_id == home)
            {
                return Ok::<_, anyhow::Error>(target);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("future matching Home was not discovered")??;
    ensure!(
        matches_class(&target, &approved.descriptor),
        "future discovery included a wrong class"
    );
    let mut echo = Echo::connect(runtime, &target).await?;
    echo.roundtrip().await?;
    echo.close().await?;
    println!(
        "{}",
        serde_json::json!({"checkpoint":"service-class-future-complete","home_id":home,"service_id":target.service_id})
    );
    Ok(())
}
async fn access_end(
    runtime: &TravelCore,
    (mut first, first_target): (Echo, &ServiceClassTarget),
    (mut second, second_target): (Echo, &ServiceClassTarget),
) -> Result<()> {
    println!("{{\"checkpoint\":\"service-class-access-ready\"}}");
    let (a, b) = tokio::time::timeout(ACCESS_END_TIMEOUT, async {
        tokio::join!(first.ended(), second.ended())
    })
    .await
    .context("class authorization loss did not close both sockets")?;
    a?;
    b?;
    let (a, b) = tokio::join!(first.rejects_traffic(), second.rejects_traffic());
    a?;
    b?;
    // A new socket needs a fresh route authorization, which the ended credential cannot obtain.
    let (first_binding, second_binding) = (binding(first_target), binding(second_target));
    let (a, b) = tokio::join!(
        refused(runtime, &first_binding, REFUSED_CONNECT_TIMEOUT),
        refused(runtime, &second_binding, REFUSED_CONNECT_TIMEOUT)
    );
    a.context("fresh class connection after access loss")?;
    b.context("fresh class connection after access loss")?;
    no_listeners()?;
    println!("{{\"checkpoint\":\"service-class-access-ended\"}}");
    Ok(())
}
// Keep the ordered, cross-Home acceptance sequence together.
#[allow(clippy::too_many_lines)]
async fn exercise(mode: &str, args: &[String]) -> Result<()> {
    let root = std::fs::read_to_string(&args[2])?;
    let password = std::fs::read_to_string(&args[1])?;
    let descriptor: ServiceClassDescriptor = load_json(Path::new(&args[3]))?;
    let (runtime, approved) = TravelCore::start_service_class(
        Path::new(&args[0]),
        password.trim_end(),
        root.trim(),
        &descriptor,
    )
    .await?;
    no_listeners()?;
    let found = targets(&runtime, &approved).await?;
    ensure!(
        found
            .iter()
            .all(|target| matches_class(target, &descriptor)),
        "discovery included a wrong class"
    );
    let first_target = &found[0];
    let second_target = found
        .iter()
        .find(|target| target.home_id != first_target.home_id)
        .context("second distinct Home missing")?;
    for wrong in [
        ServiceBinding {
            home_id: args[4].clone(),
            service_id: args[5].clone(),
            protocol: ServiceProtocol::Tcp,
        },
        // A guessed service ID on a discovered class Home is not part of the class.
        ServiceBinding {
            home_id: first_target.home_id.clone(),
            service_id: args[5].clone(),
            protocol: ServiceProtocol::Tcp,
        },
    ] {
        refused(&runtime, &wrong, CONNECT_TIMEOUT)
            .await
            .context("guessed nonclass service")?;
    }
    let (first, second) = tokio::join!(
        Echo::connect(&runtime, first_target),
        Echo::connect(&runtime, second_target)
    );
    let (mut first, mut second) = (first?, second?);
    for _ in 0..3 {
        let (a, b) = tokio::join!(first.roundtrip(), second.roundtrip());
        a?;
        b?;
    }
    let state = serde_json::json!({"request_id":approved.request_id,"credential_id":approved.credential_id,"travel_id":approved.travel_id,
        "discovered":found.iter().map(summary).collect::<Vec<_>>(),
        "connections":[summary(first_target), summary(second_target)]});
    std::fs::write(&args[6], serde_json::to_vec(&state)?)?;
    println!(
        "{}",
        serde_json::json!({"checkpoint":"service-class-two-homes-ready","state":state})
    );
    no_listeners()?;
    if mode == "access-end" {
        access_end(&runtime, (first, first_target), (second, second_target)).await?;
    } else {
        // Disconnecting one Home leaves the shared runtime and other sockets usable.
        first.close().await?;
        second.roundtrip().await?;
        let mut rejoined = Echo::connect(&runtime, first_target).await?;
        let (a, b) = tokio::join!(rejoined.roundtrip(), second.roundtrip());
        a?;
        b?;
        if mode == "future" {
            future(&runtime, &approved, Path::new(&args[7]), &found).await?;
            // Discovering another Home must not disturb already-open sockets.
            let (a, b) = tokio::join!(rejoined.roundtrip(), second.roundtrip());
            a?;
            b?;
        }
        rejoined.close().await?;
        second.close().await?;
    }
    tokio::time::timeout(Duration::from_secs(5), runtime.shutdown())
        .await
        .context("class runtime shutdown timed out")?;
    ensure!(
        runtime.status().await.active_flows == 0,
        "class runtime shutdown retained active flows"
    );
    no_listeners()?;
    println!(
        "{}",
        serde_json::json!({"checkpoint":"service-class-exercise-complete","mode":mode})
    );
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    flowsplice_core::init_crypto();
    // Silent unless RUST_LOG is set; the harness forwards FLOWSPLICE_E2E_PROBE_RUST_LOG.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = arguments(&args)?;
    // Access loss waits for stream teardown and then for a refused fresh connection.
    let limit = if mode == "access-end" { 420 } else { 240 };
    tokio::time::timeout(Duration::from_secs(limit), async {
        match mode {
            "enroll" => enroll(&args[1..]).await,
            "check-only" => check_only(&args[1..]),
            "resume-fixture" => resume_fixture(Path::new(&args[1]), Path::new(&args[2])),
            _ => exercise(mode, &args[1..]).await,
        }
    })
    .await
    .context("service-class probe timed out")?
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_rejects_missing_and_extra_arguments() {
        for (mode, count) in [
            ("enroll", 8),
            ("exercise", 8),
            ("access-end", 8),
            ("future", 9),
            ("check-only", 4),
            ("resume-fixture", 3),
        ] {
            let mut args = vec!["value".to_owned(); count];
            args[0] = mode.into();
            assert_eq!(arguments(&args).ok(), Some(mode));
            args.push("extra".into());
            assert!(arguments(&args).is_err());
            args.truncate(count - 1);
            assert!(arguments(&args).is_err());
        }
        assert!(arguments(&[]).is_err());
    }
}
