//! Real encrypted PTY acceptance against disposable, separately enrolled Travels.
use anyhow::{Context, Result, bail, ensure};
use flowsplice_enrollment::load_json;
use flowsplice_pty_client::{ConnectOptions, PtyClient};
use flowsplice_pty_protocol::{Mode, Operation, Reply, ServerMessage, Session};
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
    async fn connect(
        config: &str,
        password: &str,
        root: &str,
        descriptor: &str,
        label: &str,
    ) -> Result<Self> {
        let (client, events) = PtyClient::connect(ConnectOptions {
            config_path: config.into(),
            password: std::fs::read_to_string(password)?.trim_end().into(),
            root_public_key: std::fs::read_to_string(root)?.trim().into(),
            descriptor: load_json(Path::new(descriptor))?,
            label: label.into(),
        })
        .await?;
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
    async fn writer(&mut self, session: Uuid, attachment: Uuid) -> Result<u64> {
        loop {
            if let Some((epoch, Some(writer))) = self.ownership.get(&session)
                && *writer == attachment
            {
                return Ok(*epoch);
            }
            self.next().await?;
        }
    }
    async fn input(&mut self, attachment: Uuid, epoch: u64, text: &str) -> Result<()> {
        self.ok(Operation::Input {
            attachment_id: attachment,
            writer_epoch: epoch,
            data: text.as_bytes().to_vec(),
        })
        .await
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
// Keep this ordered E2E scenario readable as one acceptance sequence.
#[allow(clippy::too_many_lines)]
async fn exercise(args: &[String]) -> Result<()> {
    let mut a = Probe::connect(&args[0], &args[2], &args[3], &args[4], "first").await?;
    ensure!(a.initial.is_empty(), "connection created a session");
    let (sid, aid, mode, epoch) = attachment(
        a.request(Operation::New {
            columns: 80,
            rows: 24,
        })
        .await?,
    )?;
    ensure!(mode == Mode::ReadWrite, "new session was not writable");
    let marker = format!("PTY-中文-{}", Uuid::new_v4());
    a.input(aid, epoch, &print_command(&marker)).await?;
    a.text(&marker).await?;
    let mut b = Probe::connect(&args[1], &args[2], &args[3], &args[4], "second").await?;
    ensure!(
        b.initial.len() == 1 && b.initial[0].id == sid,
        "second connection changed session list"
    );
    let (_, bid, mode, _) = attachment(
        b.request(Operation::Join {
            session_id: sid,
            mode: Mode::ReadWrite,
            columns: 100,
            rows: 30,
        })
        .await?,
    )?;
    ensure!(
        mode == Mode::ReadOnly,
        "occupied session admitted a second writer"
    );
    b.text(&marker).await?;
    ensure!(
        matches!(
            b.request(Operation::Input {
                attachment_id: bid,
                writer_epoch: epoch,
                data: b"forbidden\r".to_vec()
            })
            .await?,
            Reply::Error { .. }
        ),
        "observer input accepted"
    );
    b.ok(Operation::Resize {
        attachment_id: bid,
        columns: 100,
        rows: 30,
    })
    .await?;
    a.output.clear();
    a.input(aid, epoch, "stty size\r").await?;
    a.text("24 80").await?;
    let Reply::TakeoverRequired {
        epoch: expected,
        writer,
        ..
    } = b
        .request(Operation::SetMode {
            attachment_id: bid,
            mode: Mode::ReadWrite,
            force: false,
            expected_epoch: None,
        })
        .await?
    else {
        bail!("takeover warning missing");
    };
    ensure!(writer.attachment_id == aid, "wrong displaced writer");
    ensure!(
        matches!(
            b.request(Operation::SetMode {
                attachment_id: bid,
                mode: Mode::ReadWrite,
                force: true,
                expected_epoch: Some(expected - 1)
            })
            .await?,
            Reply::TakeoverRequired { .. }
        ),
        "stale confirmation accepted"
    );
    b.ok(Operation::SetMode {
        attachment_id: bid,
        mode: Mode::ReadWrite,
        force: true,
        expected_epoch: Some(expected),
    })
    .await?;
    let next = b.writer(sid, bid).await?;
    ensure!(
        matches!(
            a.request(Operation::Input {
                attachment_id: aid,
                writer_epoch: epoch,
                data: b"stale\r".to_vec()
            })
            .await?,
            Reply::Error { .. }
        ),
        "old writer input survived takeover"
    );
    b.output.clear();
    b.input(bid, next, "stty size\r").await?;
    b.text("30 100").await?;
    let second = format!("takeover-{}", Uuid::new_v4());
    b.input(bid, next, &print_command(&second)).await?;
    b.text(&second).await?;
    a.text(&second).await?;
    b.ok(Operation::SetMode {
        attachment_id: bid,
        mode: Mode::ReadOnly,
        force: false,
        expected_epoch: None,
    })
    .await?;
    b.ok(Operation::SetMode {
        attachment_id: bid,
        mode: Mode::ReadWrite,
        force: false,
        expected_epoch: None,
    })
    .await?;
    let final_epoch = b.writer(sid, bid).await?;
    ensure!(final_epoch > next, "mode switch reused writer epoch");
    a.ok(Operation::Detach { attachment_id: aid }).await?;
    // Backgrounding closes the whole connection without an explicit tab detach.
    // Carrier recovery must not keep that writer alive after graceful shutdown.
    a.client.shutdown().await;
    b.client.shutdown().await;
    drop(a);
    drop(b);
    let mut c = Probe::connect(&args[0], &args[2], &args[3], &args[4], "reconnected").await?;
    ensure!(
        c.initial.len() == 1 && c.initial[0].id == sid,
        "disconnect removed session"
    );
    ensure!(
        c.initial[0].writer.is_none(),
        "shutdown retained the old writer"
    );
    let (_, cid, mode, epoch) = attachment(
        c.request(Operation::Join {
            session_id: sid,
            mode: Mode::ReadWrite,
            columns: 80,
            rows: 24,
        })
        .await?,
    )?;
    ensure!(mode == Mode::ReadWrite, "reconnect failed writer admission");
    c.text(&second).await?;
    c.input(cid, epoch, "exit\r").await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if c.list().await?.is_empty() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await??;
    no_listeners()?;
    c.client.shutdown().await;
    println!(
        "{{\"checkpoint\":\"encrypted-pty-exercise-complete\",\"checks\":[\"list-before-new\",\"utf8-output\",\"single-writer\",\"observer-size\",\"warned-takeover\",\"stale-epoch-rejected\",\"rw-ro-rw\",\"detach-rejoin-screen\",\"shutdown-releases-writer\",\"natural-shell-exit\",\"no-local-listeners\"]}}"
    );
    Ok(())
}
async fn persistence(mode: &str, args: &[String]) -> Result<()> {
    let mut client = Probe::connect(&args[0], &args[1], &args[2], &args[3], "persistence").await?;
    if mode == "empty" {
        ensure!(
            client.initial.is_empty(),
            "tmux restart did not clear sessions"
        );
        client.client.shutdown().await;
        return Ok(());
    }
    let file = Path::new(&args[4]);
    if mode == "seed" {
        ensure!(client.initial.is_empty(), "seed requires empty list");
        let (sid, aid, _, epoch) = attachment(
            client
                .request(Operation::New {
                    columns: 80,
                    rows: 24,
                })
                .await?,
        )?;
        let marker = format!("persist-{}", Uuid::new_v4());
        client
            .input(
                aid,
                epoch,
                &format!("FS_PERSIST='{marker}'; {}", print_command("seed-ready")),
            )
            .await?;
        client.text("seed-ready").await?;
        std::fs::write(
            file,
            serde_json::to_vec(&serde_json::json!({"session_id":sid,"marker":marker}))?,
        )?;
    } else {
        let value: serde_json::Value = load_json(file)?;
        let sid: Uuid = serde_json::from_value(value["session_id"].clone())?;
        ensure!(
            client.initial.len() == 1 && client.initial[0].id == sid,
            "Home restart lost session"
        );
        let (_, aid, _, epoch) = attachment(
            client
                .request(Operation::Join {
                    session_id: sid,
                    mode: Mode::ReadWrite,
                    columns: 80,
                    rows: 24,
                })
                .await?,
        )?;
        client.output.clear();
        client
            .input(aid, epoch, "printf '%s\\n' \"$FS_PERSIST\"\r")
            .await?;
        client
            .text(
                value["marker"]
                    .as_str()
                    .context("missing persistence marker")?,
            )
            .await?;
    }
    client.client.shutdown().await;
    println!(
        "{}",
        serde_json::json!({"checkpoint":"encrypted-pty-persistence","mode":mode})
    );
    Ok(())
}
async fn access_end(args: &[String]) -> Result<()> {
    let mut client = Probe::connect(&args[0], &args[1], &args[2], &args[3], "access-end").await?;
    ensure!(
        client.initial.is_empty(),
        "access-end requires an empty fixture"
    );
    let (sid, aid, _, epoch) = attachment(
        client
            .request(Operation::New {
                columns: 80,
                rows: 24,
            })
            .await?,
    )?;
    let marker = format!("access-{}", Uuid::new_v4());
    client
        .input(
            aid,
            epoch,
            &format!("FS_PERSIST='{marker}'; {}", print_command("access-ready")),
        )
        .await?;
    client.text("access-ready").await?;
    std::fs::write(
        &args[4],
        serde_json::to_vec(&serde_json::json!({"session_id":sid,"marker":marker}))?,
    )?;
    println!("{{\"checkpoint\":\"encrypted-pty-access-ready\"}}");
    // Revocation closes Home admission immediately, but a Carrier close is
    // deliberately recoverable under the unchanged wire protocol. Allow the
    // configured 90-second recovery window, one race/backoff and EOF drain;
    // the expiry scenario additionally waits for its one-minute credential.
    tokio::time::timeout(Duration::from_secs(170), async {
        while client.events.recv().await.is_some() {}
    })
    .await
    .context("authorization loss did not end active terminal I/O")?;
    ensure!(
        client
            .client
            .send(Operation::Input {
                attachment_id: aid,
                writer_epoch: epoch,
                data: b"forbidden\r".to_vec()
            })
            .await
            .is_err(),
        "closed authorization accepted more input"
    );
    client.client.shutdown().await;
    drop(client);
    let reconnect = tokio::time::timeout(
        Duration::from_secs(25),
        Probe::connect(&args[0], &args[1], &args[2], &args[3], "denied-reconnect"),
    )
    .await;
    ensure!(
        !matches!(reconnect, Ok(Ok(_))),
        "authorization loss allowed a fresh PTY connection"
    );
    no_listeners()?;
    println!("{{\"checkpoint\":\"encrypted-pty-access-ended\"}}");
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    flowsplice_core::init_crypto();
    let args: Vec<String> = std::env::args().collect();
    tokio::time::timeout(Duration::from_secs(210),async {
        match args.get(1).map(String::as_str) {
            Some("exercise") if args.len()==7=>exercise(&args[2..]).await,
            Some(mode @ ("seed"|"resume"|"empty")) if args.len()==7=>persistence(mode,&args[2..]).await,
            Some("access-end") if args.len()==7=>access_end(&args[2..]).await,
            _=>bail!("usage: pty-probe exercise CONFIG_A CONFIG_B PASSWORD ROOT DESCRIPTOR | seed/resume/empty CONFIG PASSWORD ROOT DESCRIPTOR STATE_FILE"),
        }
    }).await.context("PTY acceptance timed out")?
}
