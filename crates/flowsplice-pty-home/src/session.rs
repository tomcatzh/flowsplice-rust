use crate::{pty::PtyProcess, tmux::Tmux};
use anyhow::{Context, Result, anyhow, bail};
use flowsplice_pty_protocol::{Mode, Reply, ServerMessage, Session, SessionDetails, Writer};
use flowsplice_transport::ServiceLifetime;
use std::{
    collections::BTreeMap,
    io,
    sync::{Arc, Mutex, MutexGuard},
};
use tokio::sync::{mpsc, watch};
use uuid::Uuid;

#[derive(Clone)]
pub(crate) struct Connection {
    pub id: Uuid,
    pub travel_id: String,
    pub label: String,
    pub lifetime: ServiceLifetime,
    pub events: mpsc::Sender<ServerMessage>,
    pub stop: watch::Sender<bool>,
    pub can_write: bool,
}
impl Connection {
    pub fn active(&self) -> bool {
        self.lifetime.is_active() && !*self.stop.borrow()
    }
    pub fn send(&self, event: ServerMessage) -> Result<()> {
        if !self.active() {
            bail!("business connection ended");
        }
        if self.events.try_send(event).is_err() {
            self.stop.send_replace(true);
            bail!("terminal consumer is too slow or disconnected");
        }
        Ok(())
    }
    pub async fn ended(&self) {
        let mut stop = self.stop.subscribe();
        tokio::select! {
            () = self.lifetime.ended() => {},
            () = async { while !*stop.borrow_and_update() { if stop.changed().await.is_err() { break; } } } => {},
        }
    }
}
struct Attachment {
    connection: Connection,
    pty: Arc<PtyProcess>,
    columns: u16,
    rows: u16,
    started: watch::Sender<bool>,
    history: Option<Arc<crate::history::Snapshot>>,
}
struct Ownership {
    name: String,
    last_connected: Option<u64>,
    epoch: u64,
    writer: Option<Uuid>,
    attachments: BTreeMap<Uuid, Attachment>,
}
pub(crate) struct SessionState {
    pub id: Uuid,
    created: u64,
    metadata_update: tokio::sync::Mutex<()>,
    tmux: Arc<Tmux>,
    ownership: Mutex<Ownership>,
    transition: tokio::sync::Mutex<()>,
    changes: watch::Sender<u64>,
}
impl SessionState {
    #[cfg(test)]
    pub fn new(id: Uuid, created: u64, tmux: Arc<Tmux>) -> Arc<Self> {
        Self::with_metadata(id, created, Tmux::fallback_name(id), None, tmux)
    }
    pub fn with_metadata(
        id: Uuid,
        created: u64,
        name: String,
        last_connected: Option<u64>,
        tmux: Arc<Tmux>,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            created,
            metadata_update: tokio::sync::Mutex::new(()),
            tmux,
            ownership: Mutex::new(Ownership {
                name,
                last_connected,
                epoch: 1,
                writer: None,
                attachments: BTreeMap::new(),
            }),
            transition: tokio::sync::Mutex::new(()),
            changes: watch::channel(1).0,
        })
    }
    fn lock(&self) -> Result<MutexGuard<'_, Ownership>> {
        self.ownership
            .lock()
            .map_err(|_| anyhow!("session ownership lock poisoned"))
    }
    fn writer(state: &Ownership) -> Option<Writer> {
        let id = state.writer?;
        let attachment = state.attachments.get(&id)?;
        Some(Writer {
            attachment_id: id,
            travel_id: attachment.connection.travel_id.clone(),
            label: attachment.connection.label.clone(),
        })
    }
    pub fn snapshot(&self) -> Result<Session> {
        Ok(Session {
            id: self.id,
            created_at_unix_secs: self.created,
            writer: Self::writer(&*self.lock()?),
        })
    }
    pub fn details(&self) -> Result<SessionDetails> {
        let state = self.lock()?;
        Ok(SessionDetails {
            id: self.id,
            name: state.name.clone(),
            created_at_unix_secs: self.created,
            last_connected_at_unix_secs: state.last_connected,
            connection_count: u32::try_from(state.attachments.len())?,
            writer: Self::writer(&state),
        })
    }
    pub async fn rename(self: &Arc<Self>, connection: &Connection, name: &str) -> Result<()> {
        let session = Arc::clone(self);
        let connection = connection.clone();
        let name = name.to_owned();
        // Once admitted, finish the bounded metadata write even if the caller
        // disconnects. Otherwise tmux could commit while the registry stays stale.
        tokio::spawn(async move {
            let _update = session.metadata_update.lock().await;
            if !connection.active() || !connection.can_write {
                bail!("renaming requires an active business connection with write permission");
            }
            // Keep the stable tmux identity and all attachments. Persist first so a
            // failed write cannot advertise an uncommitted name to other clients.
            session.tmux.set_display_name(session.id, &name).await?;
            name.trim().clone_into(&mut session.lock()?.name);
            Ok(())
        })
        .await?
    }
    async fn record_attachment(&self, connection: &Connection, id: Uuid) -> Result<()> {
        let _update = self.metadata_update.lock().await;
        if !connection.active() || !self.owns(connection.id, id)? {
            bail!("attachment ended");
        }
        let timestamp = flowsplice_core::authorization::unix_time_secs()?
            .max(self.lock()?.last_connected.unwrap_or(0));
        self.tmux.set_last_connected(self.id, timestamp).await?;
        self.lock()?.last_connected = Some(timestamp);
        Ok(())
    }
    fn notify(&self, state: &Ownership) {
        self.changes.send_replace(state.epoch);
        let event = ServerMessage::Ownership {
            session_id: self.id,
            epoch: state.epoch,
            writer: Self::writer(state),
        };
        for attachment in state.attachments.values() {
            let _ = attachment.connection.send(event.clone());
        }
    }
    pub async fn attach(
        self: &Arc<Self>,
        connection: &Connection,
        mode: Mode,
        columns: u16,
        rows: u16,
    ) -> Result<Reply> {
        if !connection.active() {
            bail!("business connection ended");
        }
        let process = crate::pty::spawn(self.tmux.attach_command(self.id), columns, rows)?;
        let tty = process
            .tty_name()
            .context("tmux attach lacks terminal identity")?;
        // list-clients can expose a client before it has switched its terminal into
        // raw mode. Wait for terminal output before admitting input; startup flushes
        // must not silently discard the first user command.
        let mut initial = vec![0; 16_384];
        let count = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            process.read(&mut initial),
        )
        .await
        .context("tmux terminal initialization timed out")??;
        if count == 0 {
            bail!("tmux attachment ended during initialization");
        }
        initial.truncate(count);
        self.tmux.wait_attached(&tty).await?;
        if !connection.active() {
            bail!("business connection ended");
        }
        let id = Uuid::new_v4();
        let (started, start_rx) = watch::channel(false);
        {
            let mut state = self.lock()?;
            if state.attachments.len() >= 32 {
                bail!("session attachment limit reached");
            }
            state.attachments.insert(
                id,
                Attachment {
                    connection: connection.clone(),
                    pty: Arc::clone(&process),
                    columns,
                    rows,
                    started,
                    history: None,
                },
            );
        }
        if mode == Mode::ReadWrite && connection.can_write {
            // An occupied session is attached read-only. The UI can request an explicit takeover.
            if let Err(error) = self
                .set_mode(connection, id, Mode::ReadWrite, false, None)
                .await
            {
                self.detach(connection.id, id)?;
                return Err(error);
            }
        }
        if let Err(error) = self.record_attachment(connection, id).await {
            self.detach(connection.id, id)?;
            return Err(error);
        }
        let state = self.lock()?;
        let reply = Reply::Attached {
            session: Session {
                id: self.id,
                created_at_unix_secs: self.created,
                writer: Self::writer(&state),
            },
            attachment_id: id,
            mode: if state.writer == Some(id) {
                Mode::ReadWrite
            } else {
                Mode::ReadOnly
            },
            writer_epoch: state.epoch,
        };
        drop(state);
        let session = Arc::clone(self);
        let connection = connection.clone();
        tokio::spawn(async move {
            session
                .output(id, process, connection, start_rx, initial)
                .await;
        });
        Ok(reply)
    }
    pub fn start_output(&self, id: Uuid) -> Result<()> {
        if let Some(attachment) = self.lock()?.attachments.get(&id) {
            attachment.started.send_replace(true);
        }
        Ok(())
    }
    async fn output(
        self: Arc<Self>,
        id: Uuid,
        process: Arc<PtyProcess>,
        connection: Connection,
        mut start: watch::Receiver<bool>,
        initial: Vec<u8>,
    ) {
        let output = async {
            while !*start.borrow_and_update() {
                start.changed().await?;
            }
            connection.send(ServerMessage::Output {
                attachment_id: id,
                data: initial,
            })?;
            let mut buffer = vec![0; 16_384];
            loop {
                let count = process.read(&mut buffer).await?;
                if count == 0 {
                    break;
                }
                connection.send(ServerMessage::Output {
                    attachment_id: id,
                    data: buffer[..count].to_vec(),
                })?;
            }
            Ok::<(), anyhow::Error>(())
        };
        tokio::select! { biased; () = connection.ended() => {}, _ = output => {} }
        let _ = self.detach(connection.id, id);
        if let Ok(sessions) = self.tmux.list().await {
            if sessions.iter().any(|(session, _)| *session == self.id) {
                let _ = connection.send(ServerMessage::Detached {
                    attachment_id: id,
                    reason: "attachment_closed".into(),
                });
            } else {
                let _ = connection.send(ServerMessage::SessionEnded {
                    session_id: self.id,
                });
            }
        }
    }
    pub async fn set_mode(
        &self,
        connection: &Connection,
        id: Uuid,
        mode: Mode,
        force: bool,
        expected: Option<u64>,
    ) -> Result<Reply> {
        let _transition = self.transition.lock().await;
        let (process, columns, rows) = {
            let mut state = self.lock()?;
            let attachment = state
                .attachments
                .get(&id)
                .context("attachment is no longer available")?;
            if attachment.connection.id != connection.id || !connection.active() {
                bail!("attachment does not belong to this active connection");
            }
            if mode == Mode::ReadWrite && !connection.can_write {
                bail!("business permission is read-only");
            }
            if (mode == Mode::ReadWrite && state.writer == Some(id))
                || (mode == Mode::ReadOnly && state.writer != Some(id))
            {
                return Ok(Reply::Ok);
            }
            if mode == Mode::ReadWrite
                && let Some(writer) = Self::writer(&state)
                && (!force || expected != Some(state.epoch))
            {
                return Ok(Reply::TakeoverRequired {
                    session_id: self.id,
                    epoch: state.epoch,
                    writer,
                });
            }
            // Ownership is invalidated before asynchronous terminal resizing. No writes
            // are admitted while the transition is in flight. Input admission lives
            // here because tmux read-only flags cannot be removed once set.
            let process = Arc::clone(&attachment.pty);
            let columns = attachment.columns;
            let rows = attachment.rows;
            state.writer = None;
            state.epoch = state
                .epoch
                .checked_add(1)
                .context("writer epoch exhausted")?;
            self.notify(&state);
            (process, columns, rows)
        };
        if mode == Mode::ReadWrite {
            if let Err(error) = self.tmux.resize(self.id, columns, rows).await {
                close_process(process);
                return Err(error);
            }
            let admitted = {
                let mut state = self.lock()?;
                if connection.active() && state.attachments.contains_key(&id) {
                    state.writer = Some(id);
                    self.notify(&state);
                    true
                } else {
                    false
                }
            };
            if !admitted {
                bail!("connection ended during writer handoff");
            }
        }
        Ok(Reply::Ok)
    }
    pub async fn input(
        &self,
        connection: &Connection,
        id: Uuid,
        epoch: u64,
        data: &[u8],
    ) -> Result<()> {
        let mut offset = 0;
        let mut changes = self.changes.subscribe();
        while offset < data.len() {
            let process = {
                let state = self.lock()?;
                let attachment = state.attachments.get(&id).context("attachment ended")?;
                if !connection.active()
                    || attachment.connection.id != connection.id
                    || !attachment.connection.active()
                    || state.writer != Some(id)
                    || state.epoch != epoch
                {
                    bail!("input rejected: writer ownership changed or connection ended");
                }
                match attachment.pty.try_write(&data[offset..]) {
                    Ok(0) => bail!("terminal input closed"),
                    Ok(count) => {
                        offset += count;
                        continue;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        Arc::clone(&attachment.pty)
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            tokio::select! {
                biased;
                () = connection.ended() => bail!("connection ended"),
                _ = changes.changed() => {},
                result = process.writable() => result?,
            }
            // Recheck both attachment and epoch after every await and partial write.
        }
        Ok(())
    }
    pub async fn resize(
        &self,
        connection: &Connection,
        id: Uuid,
        columns: u16,
        rows: u16,
    ) -> Result<()> {
        let _transition = self.transition.lock().await;
        let writer = {
            let mut state = self.lock()?;
            let attachment = state.attachments.get_mut(&id).context("attachment ended")?;
            if attachment.connection.id != connection.id || !connection.active() {
                bail!("attachment does not belong to this active connection");
            }
            attachment.pty.resize(columns, rows)?;
            attachment.columns = columns;
            attachment.rows = rows;
            state.writer == Some(id)
        };
        if writer {
            self.tmux.resize(self.id, columns, rows).await?;
        }
        Ok(())
    }
    pub fn detach(&self, connection: Uuid, id: Uuid) -> Result<()> {
        let process = {
            let mut state = self.lock()?;
            if let Some(attachment) = state.attachments.get(&id) {
                if attachment.connection.id != connection {
                    bail!("attachment belongs to a different connection");
                }
            } else {
                return Ok(());
            }
            let removed = state.attachments.remove(&id).context("attachment ended")?;
            if state.writer == Some(id) {
                state.writer = None;
                state.epoch = state
                    .epoch
                    .checked_add(1)
                    .context("writer epoch exhausted")?;
                self.notify(&state);
            }
            removed.pty
        };
        close_process(process);
        Ok(())
    }
    pub fn detach_connection(&self, connection: Uuid) {
        let ids = self
            .lock()
            .map(|state| {
                state
                    .attachments
                    .iter()
                    .filter(|(_, a)| a.connection.id == connection)
                    .map(|(id, _)| *id)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for id in ids {
            let _ = self.detach(connection, id);
        }
    }
    pub fn owns(&self, connection: Uuid, id: Uuid) -> Result<bool> {
        Ok(self
            .lock()?
            .attachments
            .get(&id)
            .is_some_and(|a| a.connection.id == connection))
    }
    pub async fn history(
        &self,
        connection: &Connection,
        id: Uuid,
        capture_id: Uuid,
        before: Option<u32>,
    ) -> Result<Reply> {
        {
            let state = self.lock()?;
            let attachment = state.attachments.get(&id).context("attachment ended")?;
            if attachment.connection.id != connection.id || !connection.active() {
                bail!("history requires an active attachment owned by this connection");
            }
            if let Some(snapshot) = &attachment.history
                && snapshot.id == capture_id
            {
                return snapshot.page(id, before);
            }
            if before.is_some() {
                bail!("history snapshot expired; start a new capture");
            }
        }
        let _slot = tokio::select! {
            biased;
            () = connection.ended() => bail!("connection ended"),
            permit = crate::history::CAPTURE_SLOT.acquire() => permit?,
        };
        {
            let mut state = self.lock()?;
            let attachment = state.attachments.get_mut(&id).context("attachment ended")?;
            if attachment.connection.id != connection.id || !connection.active() {
                bail!("connection ended");
            }
            if let Some(snapshot) = &attachment.history
                && snapshot.id == capture_id
            {
                return snapshot.page(id, before);
            }
            // Release the prior cache budget before replacing it. Failure remains
            // retryable and never changes the PTY attachment or writer ownership.
            attachment.history = None;
        }
        let captured = tokio::select! {
            biased;
            () = connection.ended() => bail!("connection ended"),
            result = self.tmux.capture_history(self.id) => result?,
        };
        let snapshot = Arc::new(crate::history::Snapshot::parse(capture_id, &captured)?);
        let mut state = self.lock()?;
        let attachment = state
            .attachments
            .get_mut(&id)
            .context("attachment ended during history capture")?;
        if attachment.connection.id != connection.id || !connection.active() {
            bail!("connection ended");
        }
        let result = snapshot.page(id, before)?;
        attachment.history = Some(snapshot);
        Ok(result)
    }
}
fn close_process(process: Arc<PtyProcess>) {
    // Child termination may wait for the operating system; ownership was already removed.
    tokio::task::spawn_blocking(move || {
        let _ = process.close();
    });
}

#[cfg(test)]
mod rename_cancellation_tests {
    use super::*;
    use crate::tmux::TmuxConfig;
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

    #[tokio::test]
    async fn admitted_rename_finishes_after_caller_cancellation() -> Result<()> {
        let directory = tempfile::Builder::new()
            .prefix("fs-rename-")
            .tempdir_in("/tmp")?;
        let binary = directory.path().join("tmux");
        let id = Uuid::new_v4();
        // Only the generated UUID is interpolated. All paths are derived from
        // the executable location and every shell expansion is quoted.
        fs::write(
            &binary,
            format!(
                r#"#!/bin/sh
set -eu
if [ "$#" -eq 1 ] && [ "$1" = '-V' ]; then printf 'tmux 3.3\n'; exit 0; fi
base=${{0%/*}}
[ "$#" -eq 10 ]
[ "$1" = '-u' ] && [ "$2" = '-S' ] && [ "$3" = "$base/domain/tmux.sock" ]
[ "$4" = '-f' ] && [ "$5" = "$base/domain/flowsplice-tmux.conf" ]
shift 5
[ "$1" = 'set-option' ] && [ "$2" = '-t' ] && [ "$3" = 'fs-{id}' ]
[ "$4" = '@flowsplice-name-hex' ] && [ "$5" = '72656e616d6564' ]
: > "$base/entered"
count=0
while [ ! -f "$base/release" ]; do
    count=$((count + 1))
    [ "$count" -lt 200 ] || exit 1
    sleep 0.01
done
: > "$base/committed"
"#
            ),
        )?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))?;
        let tmux = Arc::new(
            Tmux::open(TmuxConfig {
                binary,
                socket: directory.path().join("domain/tmux.sock"),
                shell: "/bin/sh".into(),
                working_directory: directory.path().to_path_buf(),
            })
            .await?,
        );
        let session = SessionState::with_metadata(id, 123, "original".into(), Some(456), tmux);
        let before = session.details()?;
        let (lifetime, _guard) =
            ServiceLifetime::new(flowsplice_core::authorization::unix_time_secs()? + 60);
        let (events, _receiver) = mpsc::channel(4);
        let connection = Connection {
            id: Uuid::new_v4(),
            travel_id: "rename-test".into(),
            label: "test".into(),
            lifetime,
            events,
            stop: watch::channel(false).0,
            can_write: true,
        };
        let caller_session = Arc::clone(&session);
        let caller =
            tokio::spawn(async move { caller_session.rename(&connection, "renamed").await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !directory.path().join("entered").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        assert_eq!(
            session.details()?,
            before,
            "uncommitted name became visible"
        );
        caller.abort();
        assert!(matches!(caller.await, Err(error) if error.is_cancelled()));
        fs::write(directory.path().join("release"), [])?;
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if session.details()?.name == "renamed" {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await??;
        assert!(directory.path().join("committed").exists());
        let mut expected = before;
        expected.name = "renamed".into();
        assert_eq!(session.details()?, expected);
        assert_eq!(session.lock()?.epoch, 1);
        Ok(())
    }
}
