//! Disposable E2E for one service-class identity shared across multiple Home sockets.
use anyhow::{Context, Result, bail, ensure};
use flowsplice_core::business::ServiceClassDescriptor;
use flowsplice_core::protocol::ServiceProtocol;
use flowsplice_enrollment::load_json;
use flowsplice_pty_client::PtyClient;
use flowsplice_pty_protocol::{Mode, Operation, Reply, ServerMessage, Session, SessionDetails};
use flowsplice_storage::{StateStore, Table, WriteBatch};
use flowsplice_travel_core::{
    ServiceBinding, TravelCore,
    business::BusinessEnrollmentOptions,
    service_class::{self, ApprovedServiceClass, ServiceClassTarget},
};
use std::{collections::BTreeMap, path::Path, time::Duration};
use tokio::sync::mpsc;
use uuid::Uuid;

struct Probe {
    client: PtyClient,
    events: mpsc::Receiver<ServerMessage>,
    output: Vec<u8>,
    ownership: BTreeMap<Uuid, (u64, Option<Uuid>)>,
    initial: Vec<Session>,
}
impl Probe {
    async fn connect(runtime: &TravelCore, target: &ServiceClassTarget) -> Result<Self> {
        let (client, events) =
            PtyClient::connect_shared(runtime, &binding(target), "class probe".into()).await?;
        let mut this = Self {
            client,
            events,
            output: Vec::new(),
            ownership: BTreeMap::new(),
            initial: Vec::new(),
        };
        ensure!(
            matches!(this.next().await?, ServerMessage::Hello { .. }),
            "missing initial Hello"
        );
        let ServerMessage::Response {
            result: Reply::Sessions { sessions },
            ..
        } = this.next().await?
        else {
            bail!("missing actionable session list");
        };
        this.initial = sessions;
        no_listeners()?;
        Ok(this)
    }
    async fn next(&mut self) -> Result<ServerMessage> {
        let message = tokio::time::timeout(Duration::from_secs(15), self.events.recv())
            .await?
            .context("terminal connection ended")?;
        match &message {
            ServerMessage::Output { data, .. } => {
                self.output.extend(data);
                if self.output.len() > 1024 * 1024 {
                    self.output.drain(..self.output.len() - 1024 * 1024);
                }
            }
            ServerMessage::Ownership {
                session_id,
                epoch,
                writer,
            } => {
                self.ownership.insert(
                    *session_id,
                    (*epoch, writer.as_ref().map(|w| w.attachment_id)),
                );
            }
            ServerMessage::Response {
                result:
                    Reply::Attached {
                        session,
                        writer_epoch,
                        ..
                    },
                ..
            } => {
                self.ownership.insert(
                    session.id,
                    (
                        *writer_epoch,
                        session.writer.as_ref().map(|w| w.attachment_id),
                    ),
                );
            }
            _ => {}
        }
        Ok(message)
    }
    async fn request(&mut self, operation: Operation) -> Result<Reply> {
        let id = self.client.send(operation).await?;
        loop {
            if let ServerMessage::Response { request_id, result } = self.next().await?
                && request_id == id
            {
                return Ok(result);
            }
        }
    }
    async fn ok(&mut self, operation: Operation) -> Result<()> {
        ensure!(
            self.request(operation).await? == Reply::Ok,
            "operation did not succeed"
        );
        Ok(())
    }
    async fn text(&mut self, marker: &str) -> Result<()> {
        let result = tokio::time::timeout(Duration::from_secs(15), async {
            while !String::from_utf8_lossy(&self.output).contains(marker) {
                self.next().await?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .with_context(|| format!("terminal marker timed out: {marker}"))?;
        result.with_context(|| format!("waiting for terminal marker: {marker}"))
    }
    async fn input(&mut self, attachment: Uuid, epoch: u64, text: &str) -> Result<()> {
        self.ok(Operation::Input {
            attachment_id: attachment,
            writer_epoch: epoch,
            data: text.as_bytes().to_vec(),
        })
        .await
    }
    async fn details(&mut self) -> Result<Vec<SessionDetails>> {
        let Reply::SessionDetails { sessions } = self.request(Operation::ListDetails).await? else {
            bail!("details failed");
        };
        Ok(sessions)
    }
    async fn wait_empty(&mut self) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.list().await?.is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }
    async fn list(&mut self) -> Result<Vec<Session>> {
        let Reply::Sessions { sessions } = self.request(Operation::List).await? else {
            bail!("list failed");
        };
        Ok(sessions)
    }
}
fn attachment(reply: Reply) -> Result<(Uuid, Uuid, Mode, u64)> {
    if let Reply::Attached {
        session,
        attachment_id,
        mode,
        writer_epoch,
    } = reply
    {
        Ok((session.id, attachment_id, mode, writer_epoch))
    } else {
        bail!("join/new failed: {reply:?}");
    }
}
#[allow(clippy::format_collect)]
fn print_command(marker: &str) -> String {
    // Octal encoding prevents a command echo from satisfying an output assertion.
    let encoded: String = marker
        .as_bytes()
        .iter()
        .map(|byte| format!("\\{byte:03o}"))
        .collect();
    format!("printf '{encoded}\\n'\r")
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
                    "PTY Travel owns a TCP listener or UDP socket"
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
            let homes: std::collections::HashSet<_> = targets.iter().map(|t| &t.home_id).collect();
            if homes.len() >= 2 {
                return Ok(targets);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .context("two class Homes were not discovered")?
}
async fn new_named(probe: &mut Probe) -> Result<(Uuid, Uuid, u64)> {
    let (session, attachment, mode, epoch) = attachment(
        probe
            .request(Operation::NewNamed {
                name: "同名 class 终端".into(),
                columns: 80,
                rows: 24,
            })
            .await?,
    )?;
    ensure!(mode == Mode::ReadWrite, "new class session is not writable");
    let details = probe.details().await?;
    let detail = details
        .iter()
        .find(|detail| detail.id == session)
        .context("new metadata absent")?;
    ensure!(
        detail.name == "同名 class 终端"
            && detail.connection_count == 1
            && detail.last_connected_at_unix_secs.is_some(),
        "class session metadata incorrect"
    );
    Ok((session, attachment, epoch))
}
async fn marker(probe: &mut Probe, attachment: Uuid, epoch: u64) -> Result<()> {
    let value = format!("类授权-中文-{}", Uuid::new_v4());
    probe
        .input(attachment, epoch, &print_command(&value))
        .await?;
    probe.text(&value).await
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
    let mut probe = Probe::connect(runtime, &target).await?;
    ensure!(
        probe.initial.is_empty(),
        "future connect created an automatic session"
    );
    let (_, id, epoch) = new_named(&mut probe).await?;
    marker(&mut probe, id, epoch).await?;
    probe.input(id, epoch, "exit\r").await?;
    probe.wait_empty().await?;
    probe.client.shutdown().await;
    println!(
        "{}",
        serde_json::json!({"checkpoint":"service-class-future-complete","home_id":home})
    );
    Ok(())
}
async fn access_end(
    first: &mut Probe,
    second: &mut Probe,
    aid: Uuid,
    bid: Uuid,
    ae: u64,
    be: u64,
) -> Result<()> {
    println!("{{\"checkpoint\":\"service-class-access-ready\"}}");
    tokio::time::timeout(Duration::from_secs(180), async {
        tokio::join!(
            async { while first.events.recv().await.is_some() {} },
            async { while second.events.recv().await.is_some() {} }
        );
    })
    .await
    .context("class authorization loss did not close both sockets")?;
    for (probe, id, epoch) in [(first, aid, ae), (second, bid, be)] {
        // Event EOF precedes publication of worker completion. send() reports
        // local queue admission, not remote execution; join teardown before
        // asserting that a completed connection rejects further operations.
        probe.client.shutdown().await;
        ensure!(
            probe
                .client
                .send(Operation::Input {
                    attachment_id: id,
                    writer_epoch: epoch,
                    data: b"forbidden\r".to_vec()
                })
                .await
                .is_err(),
            "closed class socket accepted input"
        );
    }
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
    let found = targets(&runtime, &approved).await?;
    ensure!(
        found
            .iter()
            .all(
                |target| target.service.application_protocol == descriptor.application_protocol
                    && target.service.protocol == descriptor.protocol
            ),
        "discovery included a wrong class"
    );
    let first_target = &found[0];
    let second_target = found
        .iter()
        .find(|target| target.home_id != first_target.home_id)
        .context("second distinct Home missing")?;
    let wrong = ServiceBinding {
        home_id: args[4].clone(),
        service_id: args[5].clone(),
        protocol: ServiceProtocol::Tcp,
    };
    ensure!(
        runtime.connect_tcp(&wrong).await.is_err(),
        "guessed nonclass service was admitted"
    );
    let (a, b) = tokio::join!(
        Probe::connect(&runtime, first_target),
        Probe::connect(&runtime, second_target)
    );
    let (mut first, mut second) = (a?, b?);
    ensure!(
        first.initial.is_empty() && second.initial.is_empty(),
        "connect created a session or fixture not empty"
    );
    let (sid_a, aid, ae) = new_named(&mut first).await?;
    let (sid_b, bid, be) = new_named(&mut second).await?;
    ensure!(sid_a != sid_b, "two Homes reused session identity");
    marker(&mut first, aid, ae).await?;
    marker(&mut second, bid, be).await?;
    let state = serde_json::json!({"request_id":approved.request_id,"credential_id":approved.credential_id,"travel_id":approved.travel_id,
        "sessions":[{"home_id":first_target.home_id,"service_id":first_target.service_id,"session_id":sid_a},
        {"home_id":second_target.home_id,"service_id":second_target.service_id,"session_id":sid_b}]});
    std::fs::write(&args[6], serde_json::to_vec(&state)?)?;
    println!(
        "{}",
        serde_json::json!({"checkpoint":"service-class-two-homes-ready","state":state})
    );
    no_listeners()?;
    if mode == "access-end" {
        access_end(&mut first, &mut second, aid, bid, ae, be).await?;
    } else {
        first.ok(Operation::Detach { attachment_id: aid }).await?;
        first.client.shutdown().await;
        marker(&mut second, bid, be).await?;
        let mut rejoined = Probe::connect(&runtime, first_target).await?;
        ensure!(
            rejoined.initial.len() == 1 && rejoined.initial[0].id == sid_a,
            "reconnect created or lost shell"
        );
        let (_, id, joined_mode, epoch) = attachment(
            rejoined
                .request(Operation::Join {
                    session_id: sid_a,
                    mode: Mode::ReadWrite,
                    columns: 80,
                    rows: 24,
                })
                .await?,
        )?;
        ensure!(joined_mode == Mode::ReadWrite, "detached writer survived");
        marker(&mut rejoined, id, epoch).await?;
        if mode == "future" {
            future(&runtime, &approved, Path::new(&args[7]), &found).await?;
        }
        rejoined.input(id, epoch, "exit\r").await?;
        second.input(bid, be, "exit\r").await?;
        rejoined.wait_empty().await?;
        second.wait_empty().await?;
        rejoined.client.shutdown().await;
    }
    first.client.shutdown().await;
    second.client.shutdown().await;
    runtime.shutdown().await;
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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = arguments(&args)?;
    tokio::time::timeout(Duration::from_secs(240), async {
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
