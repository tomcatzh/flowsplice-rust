use super::{
    session::{Connection, SessionState},
    tmux::{Tmux, TmuxConfig},
};
use anyhow::{Context, Result, bail};
use flowsplice_core::authorization::unix_time_secs;
use flowsplice_pty_protocol::{Mode, Reply, ServerMessage};
use flowsplice_transport::{ServiceLifetime, ServiceLifetimeGuard};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

struct Fixture {
    directory: tempfile::TempDir,
    tmux: Arc<Tmux>,
    socket: PathBuf,
    binary: PathBuf,
}
impl Fixture {
    async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let binary = PathBuf::from(if cfg!(target_os = "macos") {
            "/opt/homebrew/bin/tmux"
        } else {
            "/usr/bin/tmux"
        });
        let socket = directory.path().join("history/tmux.sock");
        let tmux = Arc::new(
            Tmux::open(TmuxConfig {
                binary: binary.clone(),
                socket: socket.clone(),
                shell: "/bin/sh".into(),
                working_directory: directory.path().to_owned(),
            })
            .await?,
        );
        Ok(Self {
            directory,
            tmux,
            socket,
            binary,
        })
    }
    async fn session(&self) -> Result<Arc<SessionState>> {
        let id = Uuid::new_v4();
        self.tmux.new_session(id, 80, 24).await?;
        Ok(SessionState::new(id, 0, Arc::clone(&self.tmux)))
    }
    async fn command(&self, args: &[&str]) -> Result<String> {
        let output = tokio::process::Command::new(&self.binary)
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "tmux: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?)
    }
    async fn generate(&self, session: &SessionState, count: u32, marker: &str) -> Result<()> {
        // Output originates in the fixture-owned shell, never a flooded business event queue.
        let wrap = "W".repeat(112);
        let script = format!(
            "i=0\nwhile [ $i -lt {count} ]; do printf '\\033[32mROW-%05d-中文\\033[0m\\n' \"$i\"; i=$((i+1)); done\nprintf 'WRAP-{wrap}\\n'\nprintf '{marker}\\n'\n"
        );
        std::fs::write(self.directory.path().join("output.sh"), script)?;
        let target = format!("=fs-{}:", session.id);
        self.command(&["send-keys", "-t", &target, "sh output.sh", "Enter"])
            .await?;
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let screen = self.command(&["capture-pane", "-p", "-t", &target]).await?;
                if screen.lines().any(|line| line.trim() == marker) {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .context("shell output completion marker timed out")??;
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::process::Command::new(&self.binary)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}
fn connection() -> Result<(Connection, ServiceLifetimeGuard)> {
    let (events, mut receiver) = mpsc::channel::<ServerMessage>(64);
    tokio::spawn(async move { while receiver.recv().await.is_some() {} });
    let (lifetime, guard) = ServiceLifetime::new(unix_time_secs()? + 60);
    Ok((
        Connection {
            id: Uuid::new_v4(),
            travel_id: "history-test".into(),
            label: "history".into(),
            lifetime,
            events,
            stop: watch::channel(false).0,
            can_write: true,
            history_budget: crate::history::connection_budget(),
        },
        guard,
    ))
}
async fn attach(
    session: &Arc<SessionState>,
    connection: &Connection,
    mode: Mode,
) -> Result<(Uuid, u64)> {
    let Reply::Attached {
        attachment_id,
        writer_epoch,
        ..
    } = session.attach(connection, mode, 80, 24).await?
    else {
        bail!("expected attachment")
    };
    session.start_output(attachment_id)?;
    Ok((attachment_id, writer_epoch))
}
fn page(reply: &Reply) -> Result<(u32, u32, &[String])> {
    let Reply::History {
        total_lines,
        start,
        lines,
        ..
    } = reply
    else {
        bail!("expected history")
    };
    anyhow::ensure!(lines.len() <= 256);
    anyhow::ensure!(serde_json::to_vec(lines)?.len() <= 96 * 1024);
    Ok((*total_lines, *start, lines))
}
async fn all(
    session: &SessionState,
    connection: &Connection,
    attachment: Uuid,
    capture: Uuid,
    first: Reply,
) -> Result<Vec<String>> {
    let (total, mut start, lines) = page(&first)?;
    assert_eq!(start + u32::try_from(lines.len())?, total);
    let mut result = lines.to_vec();
    while start > 0 {
        let reply = session
            .history(connection, attachment, capture, Some(start))
            .await?;
        let (next_total, next_start, lines) = page(&reply)?;
        assert_eq!(next_total, total);
        assert_eq!(next_start + u32::try_from(lines.len())?, start);
        assert!(next_start < start);
        result.splice(0..0, lines.iter().cloned());
        start = next_start;
    }
    assert_eq!(result.len(), usize::try_from(total)?);
    Ok(result)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_pages_are_immutable_contiguous_and_read_only() -> Result<()> {
    let fixture = Fixture::new().await?;
    let session = fixture.session().await?;
    let (writer, _wg) = connection()?;
    let (wid, epoch) = attach(&session, &writer, Mode::ReadWrite).await?;
    let (mut reader, _rg) = connection()?;
    reader.can_write = false;
    let (rid, _) = attach(&session, &reader, Mode::ReadOnly).await?;
    fixture.generate(&session, 900, "INITIAL-DONE").await?;
    let before_writer = session.snapshot()?.writer;
    let capture = Uuid::new_v4();
    let first = session.history(&reader, rid, capture, None).await?;
    assert_eq!(session.history(&reader, rid, capture, None).await?, first);
    let baseline = all(&session, &reader, rid, capture, first.clone()).await?;
    let numbered: Vec<_> = baseline
        .iter()
        .filter(|line| line.contains("ROW-"))
        .collect();
    assert_eq!(numbered.len(), 900);
    assert!(
        baseline
            .iter()
            .any(|line| line.contains(&format!("WRAP-{}", "W".repeat(75))))
    );
    assert!(
        baseline
            .iter()
            .any(|line| line.contains(&"W".repeat(37)) && !line.contains("WRAP-"))
    );
    for (index, line) in numbered.iter().enumerate() {
        assert!(line.contains(&format!("ROW-{index:05}-中文")));
    }
    assert!(
        baseline.iter().any(|line| line.contains("\u{1b}[32m")),
        "ANSI color lost"
    );
    fixture.generate(&session, 30, "APPENDED-DONE").await?;
    session.resize(&writer, wid, 40, 12).await?;
    let target = format!("=fs-{}:", session.id);
    fixture.command(&["clear-history", "-t", &target]).await?;
    assert_eq!(all(&session, &reader, rid, capture, first).await?, baseline);
    assert_eq!(session.snapshot()?.writer, before_writer);
    assert_eq!(
        fixture
            .command(&[
                "display-message",
                "-p",
                "-t",
                &target,
                "#{window_width}x#{window_height}:#{pane_in_mode}"
            ])
            .await?
            .trim(),
        "40x12:0"
    );
    session.input(&writer, wid, epoch, b"\r").await?;
    assert!(session.input(&reader, rid, epoch, b"\r").await.is_err());
    let newer = Uuid::new_v4();
    let newest = session.history(&reader, rid, newer, None).await?;
    assert!(
        all(&session, &reader, rid, newer, newest)
            .await?
            .iter()
            .any(|line| line.contains("APPENDED-DONE"))
    );
    assert!(
        session
            .history(&reader, rid, capture, Some(1))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_authorization_cursor_detach_and_session_deletion() -> Result<()> {
    let fixture = Fixture::new().await?;
    let session = fixture.session().await?;
    let (a, guard) = connection()?;
    let (aid, _) = attach(&session, &a, Mode::ReadOnly).await?;
    let (b, _bg) = connection()?;
    let (bid, _) = attach(&session, &b, Mode::ReadOnly).await?;
    let capture = Uuid::new_v4();
    let expired = session
        .history(&a, aid, capture, Some(1))
        .await
        .err()
        .context("invalid history cursor unexpectedly returned a page")?;
    assert!(expired.is::<crate::history::SnapshotExpired>());
    let message = expired.to_string();
    assert!(
        matches!(crate::backend::operation_error(&expired), Reply::Error { code, message: actual } if code == "history_snapshot_expired" && actual == message)
    );
    assert!(
        matches!(crate::backend::operation_error(&anyhow::anyhow!(message)), Reply::Error { code, .. } if code == "operation_failed")
    );
    let initial = session.history(&a, aid, capture, None).await?;
    let (total, _, _) = page(&initial)?;
    let invalid_cursor = session
        .history(&a, aid, capture, Some(total + 1))
        .await
        .err()
        .context("invalid history cursor unexpectedly returned a page")?;
    assert!(!invalid_cursor.is::<crate::history::SnapshotExpired>());
    assert!(
        matches!(crate::backend::operation_error(&invalid_cursor), Reply::Error { code, .. } if code == "operation_failed")
    );
    assert!(session.history(&b, aid, capture, None).await.is_err());
    assert!(session.history(&b, bid, capture, Some(1)).await.is_err());
    drop(guard);
    assert!(session.history(&a, aid, capture, None).await.is_err());
    let other_capture = Uuid::new_v4();
    session.history(&b, bid, other_capture, None).await?;
    session.detach(b.id, bid)?;
    assert!(session.history(&b, bid, other_capture, None).await.is_err());
    let (c, _cg) = connection()?;
    let (cid, _) = attach(&session, &c, Mode::ReadOnly).await?;
    fixture
        .command(&["kill-session", "-t", &format!("=fs-{}", session.id)])
        .await?;
    assert!(
        session
            .history(&c, cid, Uuid::new_v4(), None)
            .await
            .is_err()
    );
    assert!(fixture.tmux.list().await?.is_empty());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn history_retains_fifty_thousand_rows_and_evicts_oldest() -> Result<()> {
    let fixture = Fixture::new().await?;
    let session = fixture.session().await?;
    let target = format!("=fs-{}:", session.id);
    assert_eq!(
        fixture
            .command(&["display-message", "-p", "-t", &target, "#{history_limit}"])
            .await?
            .trim(),
        "50000"
    );
    fixture.generate(&session, 55_100, "RETENTION-DONE").await?;
    let (reader, _guard) = connection()?;
    let (attachment, _) = attach(&session, &reader, Mode::ReadOnly).await?;
    let capture = Uuid::new_v4();
    let first = session.history(&reader, attachment, capture, None).await?;
    let (total, _, _) = page(&first)?;
    assert!(
        (45_000..=50_256).contains(&total),
        "unexpected retained row count {total}"
    );
    let lines = all(&session, &reader, attachment, capture, first).await?;
    assert!(!lines.iter().any(|line| line.contains("ROW-00000-")));
    assert!(lines.iter().any(|line| line.contains("ROW-55099-中文")));
    assert!(lines.iter().any(|line| line.contains("RETENTION-DONE")));
    let numbers: Vec<u32> = lines
        .iter()
        .filter_map(|line| line.split("ROW-").nth(1)?.get(..5)?.parse().ok())
        .collect();
    assert!(numbers.windows(2).all(|pair| pair[1] == pair[0] + 1));
    Ok(())
}
