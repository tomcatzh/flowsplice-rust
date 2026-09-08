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

struct DomainFixture {
    directory: tempfile::TempDir,
    tmux: Arc<Tmux>,
    socket: PathBuf,
    binary: PathBuf,
}
impl DomainFixture {
    async fn new() -> Result<Self> {
        let directory = tempfile::tempdir()?;
        let binary = if cfg!(target_os = "macos") {
            PathBuf::from("/opt/homebrew/bin/tmux")
        } else {
            PathBuf::from("/usr/bin/tmux")
        };
        let socket = directory.path().join("domain/tmux.sock");
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
        let (_, created) = self
            .tmux
            .list()
            .await?
            .into_iter()
            .find(|(session, _)| *session == id)
            .context("new session missing")?;
        Ok(SessionState::new(id, created, Arc::clone(&self.tmux)))
    }
}
impl Drop for DomainFixture {
    fn drop(&mut self) {
        // Test-owned private domain only; there is no corresponding production operation.
        let _ = std::process::Command::new(&self.binary)
            .arg("-S")
            .arg(&self.socket)
            .arg("kill-server")
            .output();
    }
}
fn connection(
    name: &str,
    expires: u64,
) -> (
    Connection,
    mpsc::Receiver<ServerMessage>,
    ServiceLifetimeGuard,
) {
    let (events, receiver) = mpsc::channel(64);
    let (lifetime, guard) = ServiceLifetime::new(expires);
    (
        Connection {
            id: Uuid::new_v4(),
            travel_id: name.into(),
            label: name.into(),
            lifetime,
            events,
            stop: watch::channel(false).0,
            can_write: true,
        },
        receiver,
        guard,
    )
}
#[allow(clippy::needless_pass_by_value)]
fn attached(reply: Reply) -> Result<(Uuid, Mode, u64)> {
    if let Reply::Attached {
        attachment_id,
        mode,
        writer_epoch,
        ..
    } = reply
    {
        Ok((attachment_id, mode, writer_epoch))
    } else {
        bail!("expected attachment")
    }
}
async fn output_contains(events: &mut mpsc::Receiver<ServerMessage>, marker: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut all = Vec::new();
        while let Some(event) = events.recv().await {
            if let ServerMessage::Output { data, .. } = event {
                all.extend(data);
                if String::from_utf8_lossy(&all).contains(marker) {
                    return Ok(());
                }
            }
        }
        bail!("output closed before marker")
    })
    .await
    .context("terminal output marker timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_tmux_takeover_read_only_resize_detach_and_natural_exit() -> Result<()> {
    let fixture = DomainFixture::new().await?;
    assert!(
        fixture.tmux.list().await?.is_empty(),
        "opening Home created an unsolicited session"
    );
    let session = fixture.session().await?;
    let (a, mut ae, _ag) = connection("first", unix_time_secs()? + 60);
    let (aid, mode, epoch) = attached(session.attach(&a, Mode::ReadWrite, 80, 24).await?)?;
    assert_eq!(mode, Mode::ReadWrite);
    session.start_output(aid)?;
    session
        .input(
            &a,
            aid,
            epoch,
            b"printf '\\146\\151\\162\\163\\164-OK-\\344\\270\\255\\346\\226\\207\\n'\r",
        )
        .await?;
    output_contains(&mut ae, "first-OK-中文").await?;
    let (b, mut be, bg) = connection("second", unix_time_secs()? + 60);
    let (bid, mode, _) = attached(session.attach(&b, Mode::ReadWrite, 100, 30).await?)?;
    assert_eq!(mode, Mode::ReadOnly);
    session.start_output(bid)?;
    assert!(session.input(&b, bid, epoch, b"blocked\r").await.is_err());
    let Reply::TakeoverRequired {
        epoch: current,
        writer,
        ..
    } = session
        .set_mode(&b, bid, Mode::ReadWrite, false, None)
        .await?
    else {
        bail!("missing takeover warning");
    };
    assert_eq!(writer.attachment_id, aid);
    assert!(matches!(
        session
            .set_mode(&b, bid, Mode::ReadWrite, true, Some(current - 1))
            .await?,
        Reply::TakeoverRequired { .. }
    ));
    assert_eq!(
        session
            .set_mode(&b, bid, Mode::ReadWrite, true, Some(current))
            .await?,
        Reply::Ok
    );
    assert!(session.input(&a, aid, epoch, b"stale\r").await.is_err());
    let next = current + 1;
    session
        .input(
            &b,
            bid,
            next,
            b"printf '\\163\\145\\143\\157\\156\\144-OK\\n'\r",
        )
        .await?;
    output_contains(&mut be, "second-OK").await?;
    session.resize(&a, aid, 50, 10).await?;
    let dimensions = tokio::process::Command::new(&fixture.binary)
        .arg("-S")
        .arg(&fixture.socket)
        .args(["display-message", "-p", "-t"])
        .arg(format!("=fs-{}:", session.id))
        .arg("#{window_width}x#{window_height}")
        .output()
        .await?;
    assert_eq!(String::from_utf8(dimensions.stdout)?.trim(), "100x30");
    drop(bg);
    assert!(session.input(&b, bid, next, b"expired\r").await.is_err());
    session.detach(a.id, aid)?;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        fixture.tmux.list().await?.len(),
        1,
        "detach/authorization loss killed tmux session"
    );
    let (c, mut ce, _cg) = connection("third", unix_time_secs()? + 60);
    let (cid, mode, epoch) = attached(session.attach(&c, Mode::ReadWrite, 80, 24).await?)?;
    assert_eq!(mode, Mode::ReadWrite);
    session.start_output(cid)?;
    output_contains(&mut ce, "second-OK").await?;
    session.input(&c, cid, epoch, b"exit\r").await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !fixture.tmux.list().await?.is_empty() {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    assert!(fixture.directory.path().exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_attach_does_not_submit_partial_input_or_end_shell() -> Result<()> {
    let fixture = DomainFixture::new().await?;
    let session = fixture.session().await?;
    let (a, mut ae, _ag) = connection("first", unix_time_secs()? + 60);
    let (aid, _, epoch) = attached(session.attach(&a, Mode::ReadWrite, 80, 24).await?)?;
    session.start_output(aid)?;
    session
        .input(&a, aid, epoch, b"touch must-not-exist")
        .await?;
    output_contains(&mut ae, "touch must-not-exist").await?;
    session.detach(a.id, aid)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!fixture.directory.path().join("must-not-exist").exists());
    assert_eq!(fixture.tmux.list().await?.len(), 1);
    let (b, mut be, _bg) = connection("second", unix_time_secs()? + 60);
    let (bid, _, epoch) = attached(session.attach(&b, Mode::ReadWrite, 80, 24).await?)?;
    session.start_output(bid)?;
    session
        .input(
            &b,
            bid,
            epoch,
            b"\x03printf '\\162\\145\\163\\165\\155\\145-OK\\n'\r",
        )
        .await?;
    output_contains(&mut be, "resume-OK").await?;
    assert!(!fixture.directory.path().join("must-not-exist").exists());
    session.detach(b.id, bid)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_old_input_is_cancelled_by_takeover_without_waiting_for_pty_space() -> Result<()> {
    let fixture = DomainFixture::new().await?;
    let session = fixture.session().await?;
    let (a, mut ae, _ag) = connection("blocked-first", unix_time_secs()? + 60);
    let (aid, _, epoch) = attached(session.attach(&a, Mode::ReadWrite, 80, 24).await?)?;
    session.start_output(aid)?;
    session
        .input(
            &a,
            aid,
            epoch,
            b"printf '\\142\\154\\157\\143\\153-ready\\n'\r",
        )
        .await?;
    output_contains(&mut ae, "block-ready").await?;
    // Keep consuming output so this test isolates a blocked input queue.
    let drain = tokio::spawn(async move { while ae.recv().await.is_some() {} });
    let pid = tokio::process::Command::new(&fixture.binary)
        .arg("-S")
        .arg(&fixture.socket)
        .args(["list-clients", "-F", "#{client_pid}"])
        .output()
        .await?;
    let pid: u32 = String::from_utf8(pid.stdout)?.trim().parse()?;
    assert!(
        std::process::Command::new("/bin/kill")
            .args(["-STOP", &pid.to_string()])
            .status()?
            .success()
    );
    let stopped = Arc::clone(&session);
    let old = a.clone();
    let mut input = tokio::spawn(async move {
        stopped
            .input(&old, aid, epoch, &vec![b'x'; 512 * 1024])
            .await
    });
    if let Ok(result) = tokio::time::timeout(Duration::from_millis(150), &mut input).await {
        bail!("test did not fill stopped PTY: {result:?}");
    }
    let (b, mut be, _bg) = connection("new-writer", unix_time_secs()? + 60);
    let (bid, mode, _) = attached(session.attach(&b, Mode::ReadOnly, 80, 24).await?)?;
    assert_eq!(mode, Mode::ReadOnly);
    session.start_output(bid)?;
    let drain_b = tokio::spawn(async move { while be.recv().await.is_some() {} });
    let Reply::TakeoverRequired {
        epoch: expected, ..
    } = session
        .set_mode(&b, bid, Mode::ReadWrite, false, None)
        .await?
    else {
        bail!("missing warning");
    };
    tokio::time::timeout(
        Duration::from_secs(2),
        session.set_mode(&b, bid, Mode::ReadWrite, true, Some(expected)),
    )
    .await??;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), input)
            .await??
            .is_err(),
        "stale input survived takeover"
    );
    session.detach(a.id, aid)?;
    session.detach(b.id, bid)?;
    drain.abort();
    drain_b.abort();
    Ok(())
}

async fn wait_detached(session: &SessionState, connection: Uuid, attachment: Uuid) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while session.owns(connection, attachment)? {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("idle attachment was not removed")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_takeover_accepts_exactly_one_warned_epoch() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = DomainFixture::new().await?;
        let session = fixture.session().await?;
        let (original, _events, _guard) = connection("original", unix_time_secs()? + 60);
        let (original_id, _, epoch) = attached(session.attach(&original, Mode::ReadWrite, 80, 24).await?)?;
        let (first, _events_first, _guard_first) = connection("first-observer", unix_time_secs()? + 60);
        let (second, _events_second, _guard_second) = connection("second-observer", unix_time_secs()? + 60);
        let (first_id, _, _) = attached(session.attach(&first, Mode::ReadOnly, 80, 24).await?)?;
        let (second_id, _, _) = attached(session.attach(&second, Mode::ReadOnly, 80, 24).await?)?;
        for (peer, id) in [(&first, first_id), (&second, second_id)] {
            assert!(matches!(session.set_mode(peer,id,Mode::ReadWrite,false,None).await?, Reply::TakeoverRequired { epoch: warned, .. } if warned == epoch));
        }
        let (first_result, second_result) = tokio::join!(
            session.set_mode(&first, first_id, Mode::ReadWrite, true, Some(epoch)),
            session.set_mode(&second, second_id, Mode::ReadWrite, true, Some(epoch))
        );
        let (winner, loser) = match (first_result?, second_result?) {
            (Reply::Ok, warning @ Reply::TakeoverRequired { .. }) => (first_id, warning),
            (warning @ Reply::TakeoverRequired { .. }, Reply::Ok) => (second_id, warning),
            result => bail!("expected one successful takeover: {result:?}"),
        };
        let Reply::TakeoverRequired { epoch: next, writer, .. } = loser else { bail!("missing updated warning"); };
        assert!(next > epoch);
        assert_eq!(writer.attachment_id, winner);
        assert_eq!(session.snapshot()?.writer.context("missing winner")?.attachment_id, winner);
        assert!(session.input(&original, original_id, epoch, b"forbidden\r").await.is_err());
        let winning_peer = if winner == first_id { &first } else { &second };
        assert!(session.input(winning_peer, winner, epoch, b"stale\r").await.is_err());
        session.detach_connection(original.id);
        session.detach_connection(first.id);
        session.detach_connection(second.id);
        Ok::<_, anyhow::Error>(())
    }).await.context("takeover race timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_business_permission_cannot_be_promoted_or_inject_input() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = DomainFixture::new().await?;
        let session = fixture.session().await?;
        let (mut peer, _events, _guard) = connection("restricted", unix_time_secs()? + 60);
        peer.can_write = false;
        let (id, mode, epoch) = attached(session.attach(&peer, Mode::ReadWrite, 80, 24).await?)?;
        assert_eq!(mode, Mode::ReadOnly);
        assert!(session.snapshot()?.writer.is_none());
        assert!(
            session
                .set_mode(&peer, id, Mode::ReadWrite, false, None)
                .await
                .is_err()
        );
        assert!(
            session
                .set_mode(&peer, id, Mode::ReadWrite, true, Some(epoch))
                .await
                .is_err()
        );
        assert!(
            session
                .input(&peer, id, epoch, b"touch forbidden\r")
                .await
                .is_err()
        );
        assert!(!fixture.directory.path().join("forbidden").exists());
        session.detach(peer.id, id)?;
        assert_eq!(fixture.tmux.list().await?.len(), 1);
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("read-only test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn actual_output_overflow_detaches_only_slow_observer() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let fixture = DomainFixture::new().await?;
        let session = fixture.session().await?;
        let (writer, mut events, _guard) = connection("drained-writer", unix_time_secs()? + 60);
        let (writer_id, _, epoch) = attached(session.attach(&writer, Mode::ReadWrite, 80, 24).await?)?;
        session.start_output(writer_id)?;
        let (observer, _undrained, _observer_guard) = connection("slow-observer", unix_time_secs()? + 60);
        let (observer_id, _, _) = attached(session.attach(&observer, Mode::ReadOnly, 80, 24).await?)?;
        session.start_output(observer_id)?;
        let (marker, mut marker_seen) = watch::channel(false);
        let drain = tokio::spawn(async move {
            let mut output = Vec::new();
            while let Some(event) = events.recv().await {
                if let ServerMessage::Output { data, .. } = event {
                    output.extend(data);
                    if output.windows(b"writer-still-usable".len()).any(|part| part == b"writer-still-usable") { marker.send_replace(true); }
                    if output.len() > 65536 { output.drain(..output.len()-32768); }
                }
            }
        });
        session.input(&writer, writer_id, epoch, b"i=0; while [ $i -lt 300 ]; do printf '%08000d\\n' 0; i=$((i+1)); sleep 0.01; done\r").await?;
        let mut stopped = observer.stop.subscribe();
        tokio::time::timeout(Duration::from_secs(10), stopped.wait_for(|value| *value)).await.context("observer did not reach output overflow")??;
        wait_detached(&session, observer.id, observer_id).await?;
        assert!(session.owns(writer.id, writer_id)?);
        assert!(writer.active());
        session.input(&writer, writer_id, epoch, b"\x03printf '\\167\\162\\151\\164\\145\\162-still-usable\\n'\r").await?;
        tokio::time::timeout(Duration::from_secs(3), marker_seen.wait_for(|value| *value)).await.context("writer stopped after observer overflow")??;
        assert_eq!(fixture.tmux.list().await?.len(), 1);
        session.detach(writer.id, writer_id)?;
        drain.abort();
        Ok::<_, anyhow::Error>(())
    }).await.context("overflow test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn idle_expiry_and_guard_drop_remove_attachment_without_ending_session() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let fixture = DomainFixture::new().await?;
        let session = fixture.session().await?;
        let (expiring, _events, _guard) = connection("expiring", unix_time_secs()? + 2);
        let (id, _, _) = attached(session.attach(&expiring, Mode::ReadWrite, 80, 24).await?)?;
        session.start_output(id)?;
        wait_detached(&session, expiring.id, id).await?;
        assert!(session.snapshot()?.writer.is_none());
        assert_eq!(fixture.tmux.list().await?.len(), 1);
        let (revoked, _revoked_events, guard) = connection("revoked", unix_time_secs()? + 60);
        let (id, mode, _) = attached(session.attach(&revoked, Mode::ReadWrite, 80, 24).await?)?;
        assert_eq!(mode, Mode::ReadWrite);
        session.start_output(id)?;
        drop(guard);
        wait_detached(&session, revoked.id, id).await?;
        assert!(session.snapshot()?.writer.is_none());
        assert_eq!(fixture.tmux.list().await?.len(), 1);
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("idle lifetime test timed out")?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn metadata_survives_reconstruction_and_counts_attachments() -> Result<()> {
    let fixture = DomainFixture::new().await?;
    let session = fixture.session().await?;
    let name = " 部署 $(touch injected); ' | #{session_name} 😀 ";
    fixture.tmux.set_display_name(session.id, name).await?;
    let (saved, last) = fixture.tmux.metadata(session.id).await?;
    assert_eq!(saved, name.trim());
    assert_eq!(last, None);
    assert!(!fixture.directory.path().join("injected").exists());
    let session = SessionState::with_metadata(
        session.id,
        session.snapshot()?.created_at_unix_secs,
        saved,
        last,
        Arc::clone(&fixture.tmux),
    );
    assert_eq!(session.details()?.connection_count, 0);
    let (first, _events, _guard) = connection("first", unix_time_secs()? + 60);
    let (a, _, _) = attached(session.attach(&first, Mode::ReadWrite, 80, 24).await?)?;
    let first_time = session
        .details()?
        .last_connected_at_unix_secs
        .context("missing connect time")?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (second, _events2, _guard2) = connection("second", unix_time_secs()? + 60);
    let (b, _, _) = attached(session.attach(&second, Mode::ReadOnly, 80, 24).await?)?;
    let details = session.details()?;
    assert_eq!(details.connection_count, 2);
    assert!(
        details
            .last_connected_at_unix_secs
            .context("missing latest connect time")?
            > first_time
    );
    session.detach(first.id, a)?;
    assert_eq!(session.details()?.connection_count, 1);
    session.detach(second.id, b)?;
    assert_eq!(session.details()?.connection_count, 0);
    let (name, last) = fixture.tmux.metadata(session.id).await?;
    let recovered = SessionState::with_metadata(
        session.id,
        details.created_at_unix_secs,
        name,
        last,
        Arc::clone(&fixture.tmux),
    );
    assert_eq!(recovered.details()?.name, details.name);
    assert_eq!(
        recovered.details()?.last_connected_at_unix_secs,
        details.last_connected_at_unix_secs
    );
    assert_eq!(recovered.details()?.connection_count, 0);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn external_tmux_session_deletion_notifies_all_connections_and_preserves_other_io()
-> Result<()> {
    let fixture = DomainFixture::new().await?;
    let deleted = fixture.session().await?;
    let unrelated = fixture.session().await?;
    let (writer, mut writer_events, _writer_guard) = connection("writer", unix_time_secs()? + 60);
    let (observer, mut observer_events, _observer_guard) =
        connection("observer", unix_time_secs()? + 60);
    let (other, mut other_events, _other_guard) = connection("unrelated", unix_time_secs()? + 60);
    let (writer_id, _, epoch) = attached(deleted.attach(&writer, Mode::ReadWrite, 80, 24).await?)?;
    let (observer_id, mode, _) =
        attached(deleted.attach(&observer, Mode::ReadOnly, 80, 24).await?)?;
    assert_eq!(mode, Mode::ReadOnly);
    let (other_id, _, other_epoch) =
        attached(unrelated.attach(&other, Mode::ReadWrite, 80, 24).await?)?;
    deleted.start_output(writer_id)?;
    deleted.start_output(observer_id)?;
    unrelated.start_output(other_id)?;
    deleted
        .input(
            &writer,
            writer_id,
            epoch,
            b"printf '\\144\\145\\154-ready\\n'\r",
        )
        .await?;
    output_contains(&mut writer_events, "del-ready").await?;
    output_contains(&mut observer_events, "del-ready").await?;
    unrelated
        .input(
            &other,
            other_id,
            other_epoch,
            b"printf '\\157\\164\\150-before\\n'\r",
        )
        .await?;
    output_contains(&mut other_events, "oth-before").await?;
    let killed = tokio::process::Command::new(&fixture.binary)
        .arg("-S")
        .arg(&fixture.socket)
        .args(["kill-session", "-t"])
        .arg(format!("=fs-{}", deleted.id))
        .output()
        .await?;
    assert!(
        killed.status.success(),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
    for events in [&mut writer_events, &mut observer_events] {
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = events.recv().await {
                if let ServerMessage::SessionEnded { session_id } = event {
                    assert_eq!(session_id, deleted.id);
                    return Ok(());
                }
            }
            bail!("connection closed without SessionEnded")
        })
        .await
        .context("deleted session did not notify every connection")??;
    }
    assert!(!deleted.owns(writer.id, writer_id)?);
    assert!(!deleted.owns(observer.id, observer_id)?);
    assert!(writer.active() && observer.active() && other.active());
    // A stale restoration request must not implicitly recreate the deleted UUID.
    assert!(
        deleted
            .attach(&writer, Mode::ReadWrite, 80, 24)
            .await
            .is_err()
    );
    unrelated
        .input(
            &other,
            other_id,
            other_epoch,
            b"printf '\\157\\164\\150-after\\n'\r",
        )
        .await?;
    output_contains(&mut other_events, "oth-after").await?;
    let remaining = fixture.tmux.list().await?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].0, unrelated.id);
    unrelated.detach(other.id, other_id)?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restored_attach_respects_existing_writer_and_original_read_only_mode() -> Result<()> {
    let fixture = DomainFixture::new().await?;
    let session = fixture.session().await?;
    let (original, _original_events, _original_guard) =
        connection("original", unix_time_secs()? + 60);
    let (original_id, _, _) = attached(session.attach(&original, Mode::ReadWrite, 80, 24).await?)?;
    session.detach(original.id, original_id)?;
    let (writer, _writer_events, _writer_guard) = connection("new-writer", unix_time_secs()? + 60);
    let (writer_id, _, _) = attached(session.attach(&writer, Mode::ReadWrite, 80, 24).await?)?;
    let (restored, mode, epoch) =
        attached(session.attach(&original, Mode::ReadWrite, 80, 24).await?)?;
    assert_eq!(mode, Mode::ReadOnly);
    assert_eq!(
        session
            .snapshot()?
            .writer
            .context("writer missing")?
            .attachment_id,
        writer_id
    );
    assert!(
        session
            .input(&original, restored, epoch, b"blocked\r")
            .await
            .is_err()
    );
    session.detach(original.id, restored)?;
    session.detach(writer.id, writer_id)?;
    let (read_only, mode, _) = attached(session.attach(&original, Mode::ReadOnly, 80, 24).await?)?;
    assert_eq!(mode, Mode::ReadOnly);
    session.detach(original.id, read_only)?;
    let (restored_read_only, mode, epoch) =
        attached(session.attach(&original, Mode::ReadOnly, 80, 24).await?)?;
    assert_eq!(mode, Mode::ReadOnly);
    assert!(session.snapshot()?.writer.is_none());
    assert!(
        session
            .input(&original, restored_read_only, epoch, b"blocked\r")
            .await
            .is_err()
    );
    session.detach(original.id, restored_read_only)?;
    Ok(())
}
