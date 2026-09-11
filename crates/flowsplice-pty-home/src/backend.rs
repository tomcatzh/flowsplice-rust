use crate::{
    session::{Connection, SessionState},
    tmux::{Tmux, TmuxConfig},
};
use anyhow::{Context, Result, anyhow, bail};
use flowsplice_core::{
    business::HomeServiceGrant,
    protocol::{Service, ServiceProtocol},
};
use flowsplice_pty_protocol::{
    APPLICATION_PROTOCOL, ClientMessage, Operation, PROTOCOL_VERSION, Reply, ServerMessage,
    read_message, write_server_message,
};
use flowsplice_transport::{BoxStream, DatagramIo, IoFuture, ServicePeer, ServiceProvider};
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::{Semaphore, mpsc, watch};
use uuid::Uuid;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PtyDomainConfig {
    pub service_id: String,
    pub tmux: TmuxConfig,
}
struct Domain {
    tmux: Arc<Tmux>,
    sessions: Mutex<BTreeMap<Uuid, Arc<SessionState>>>,
    control: tokio::sync::Mutex<()>,
    can_write: AtomicBool,
}
impl Domain {
    async fn refresh(&self) -> Result<Vec<flowsplice_pty_protocol::Session>> {
        let active = self.tmux.list().await?;
        let missing = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| anyhow!("session registry poisoned"))?;
            active
                .iter()
                .filter(|(id, _)| !sessions.contains_key(id))
                .copied()
                .collect::<Vec<_>>()
        };
        let mut discovered = Vec::new();
        for (id, created) in missing {
            let (name, last) = self.tmux.metadata(id).await?;
            discovered.push((
                id,
                SessionState::with_metadata(id, created, name, last, Arc::clone(&self.tmux)),
            ));
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| anyhow!("session registry poisoned"))?;
        // Session existence follows tmux. Never recreate an absent session automatically.
        sessions.retain(|id, _| active.iter().any(|(active, _)| active == id));
        for (id, session) in discovered {
            sessions.entry(id).or_insert(session);
        }
        sessions.values().map(|s| s.snapshot()).collect()
    }
    fn session(&self, id: Uuid) -> Result<Arc<SessionState>> {
        self.sessions
            .lock()
            .map_err(|_| anyhow!("session registry poisoned"))?
            .get(&id)
            .cloned()
            .context("session ended; refresh the list")
    }
    fn attachment(&self, connection: Uuid, id: Uuid) -> Result<Arc<SessionState>> {
        for session in self
            .sessions
            .lock()
            .map_err(|_| anyhow!("session registry poisoned"))?
            .values()
        {
            if session.owns(connection, id)? {
                return Ok(Arc::clone(session));
            }
        }
        bail!("attachment is not available on this connection")
    }
    fn detach_connection(&self, id: Uuid) {
        if let Ok(sessions) = self.sessions.lock() {
            for session in sessions.values() {
                session.detach_connection(id);
            }
        }
    }
    async fn new_session(
        &self,
        connection: &Connection,
        name: Option<&str>,
        columns: u16,
        rows: u16,
    ) -> Result<Reply> {
        if !connection.can_write {
            bail!("creating a session requires write permission");
        }
        let _control = self.control.lock().await;
        if self.refresh().await?.len() >= 128 {
            bail!("session limit reached");
        }
        if !connection.active() {
            bail!("business connection ended");
        }
        let id = Uuid::new_v4();
        self.tmux.new_session(id, columns, rows).await?;
        if let Some(name) = name {
            self.tmux.set_display_name(id, name).await?;
        }
        self.refresh().await?;
        self.session(id)?
            .attach(
                connection,
                flowsplice_pty_protocol::Mode::ReadWrite,
                columns,
                rows,
            )
            .await
    }
    #[allow(clippy::too_many_lines)] // Dispatch stays exhaustive over the wire operations.
    async fn operation(&self, connection: &Connection, operation: Operation) -> Result<Reply> {
        if !connection.active() {
            bail!("business connection ended");
        }
        match operation {
            Operation::List => {
                let _control = self.control.lock().await;
                Ok(Reply::Sessions {
                    sessions: self.refresh().await?,
                })
            }
            Operation::ListDetails => {
                let _control = self.control.lock().await;
                self.refresh().await?;
                let sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| anyhow!("session registry poisoned"))?
                    .values()
                    .map(|session| session.details())
                    .collect::<Result<Vec<_>>>()?;
                Ok(Reply::SessionDetails { sessions })
            }
            Operation::New { columns, rows } => {
                self.new_session(connection, None, columns, rows).await
            }
            Operation::Rename { session_id, name } => {
                flowsplice_pty_protocol::validate_session_name(&name)?;
                if !connection.can_write {
                    bail!("renaming a session requires write permission");
                }
                let _control = self.control.lock().await;
                self.refresh().await?;
                self.session(session_id)?.rename(connection, &name).await?;
                Ok(Reply::Ok)
            }
            Operation::NewNamed {
                name,
                columns,
                rows,
            } => {
                flowsplice_pty_protocol::validate_session_name(&name)?;
                self.new_session(connection, Some(&name), columns, rows)
                    .await
            }
            Operation::Join {
                session_id,
                mode,
                columns,
                rows,
            } => {
                {
                    let _control = self.control.lock().await;
                    self.refresh().await?;
                }
                self.session(session_id)?
                    .attach(connection, mode, columns, rows)
                    .await
            }
            Operation::SetMode {
                attachment_id,
                mode,
                force,
                expected_epoch,
            } => {
                self.attachment(connection.id, attachment_id)?
                    .set_mode(connection, attachment_id, mode, force, expected_epoch)
                    .await
            }
            Operation::Resize {
                attachment_id,
                columns,
                rows,
            } => {
                self.attachment(connection.id, attachment_id)?
                    .resize(connection, attachment_id, columns, rows)
                    .await?;
                Ok(Reply::Ok)
            }
            Operation::Input {
                attachment_id,
                writer_epoch,
                data,
            } => {
                self.attachment(connection.id, attachment_id)?
                    .input(connection, attachment_id, writer_epoch, &data)
                    .await?;
                Ok(Reply::Ok)
            }
            Operation::Detach { attachment_id } => {
                self.attachment(connection.id, attachment_id)?
                    .detach(connection.id, attachment_id)?;
                Ok(Reply::Ok)
            }
            Operation::History {
                attachment_id,
                capture_id,
                before,
            } => {
                self.attachment(connection.id, attachment_id)?
                    .history(connection, attachment_id, capture_id, before)
                    .await
            }
        }
    }
}
struct ConnectionCleanup {
    domain: Arc<Domain>,
    id: Uuid,
    stop: watch::Sender<bool>,
}
impl Drop for ConnectionCleanup {
    fn drop(&mut self) {
        self.stop.send_replace(true);
        self.domain.detach_connection(self.id);
    }
}
struct Inner {
    domains: BTreeMap<String, Arc<Domain>>,
    ready: AtomicBool,
    capacity: Arc<Semaphore>,
    stop: watch::Sender<bool>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}
/// Issuer-free terminal service provider. Each logical service has a private tmux domain.
pub struct PtyBackend {
    inner: Arc<Inner>,
}
impl PtyBackend {
    /// Opens independent tmux domains without creating any sessions.
    ///
    /// # Errors
    /// Rejects invalid configuration, unavailable tmux or a domain already in use.
    pub async fn open(configs: Vec<PtyDomainConfig>) -> Result<Self> {
        if configs.is_empty() || configs.len() > 256 {
            bail!("configure one or more PTY service domains");
        }
        let mut domains = BTreeMap::new();
        for config in configs {
            if config.service_id.is_empty() || domains.contains_key(&config.service_id) {
                bail!("PTY service identifiers must be unique");
            }
            let tmux = Arc::new(Tmux::open(config.tmux).await?);
            domains.insert(
                config.service_id,
                Arc::new(Domain {
                    tmux,
                    sessions: Mutex::new(BTreeMap::new()),
                    control: tokio::sync::Mutex::new(()),
                    can_write: AtomicBool::new(false),
                }),
            );
        }
        Ok(Self {
            inner: Arc::new(Inner {
                domains,
                ready: AtomicBool::new(false),
                capacity: Arc::new(Semaphore::new(128)),
                stop: watch::channel(false).0,
                tasks: Mutex::new(Vec::new()),
            }),
        })
    }
    /// Enables service admission only from the grant already verified by `HomeRuntime`.
    ///
    /// # Errors
    /// Rejects mismatched services, application protocols or capabilities.
    pub fn authorize(&self, grant: &HomeServiceGrant) -> Result<()> {
        if grant.services.len() != self.inner.domains.len() {
            bail!("PTY domains must exactly match the approved Home service grant");
        }
        for service in &grant.services {
            let domain = self
                .inner
                .domains
                .get(&service.service_id)
                .context("approved service has no PTY domain")?;
            if service.protocol != ServiceProtocol::Tcp
                || service.application_protocol != APPLICATION_PROTOCOL
                || !service.capabilities.iter().any(|c| c == "read")
                || service
                    .capabilities
                    .iter()
                    .any(|c| c != "read" && c != "write")
            {
                bail!("unsupported PTY business service capabilities");
            }
            domain.can_write.store(
                service.capabilities.iter().any(|c| c == "write"),
                Ordering::Release,
            );
        }
        self.inner.ready.store(true, Ordering::Release);
        Ok(())
    }
    pub async fn shutdown(&self) {
        self.inner.stop.send_replace(true);
        let tasks = self
            .inner
            .tasks
            .lock()
            .map(|mut tasks| std::mem::take(&mut *tasks))
            .unwrap_or_default();
        for task in tasks {
            let _ = task.await;
        }
    }
}
impl Drop for PtyBackend {
    fn drop(&mut self) {
        self.inner.stop.send_replace(true);
    }
}
impl ServiceProvider for PtyBackend {
    fn connect_tcp<'a>(
        &'a self,
        service: &'a Service,
        peer: ServicePeer,
    ) -> IoFuture<'a, BoxStream> {
        Box::pin(async move {
            if !self.inner.ready.load(Ordering::Acquire)
                || *self.inner.stop.borrow()
                || !peer.lifetime.is_active()
            {
                bail!("PTY backend is not accepting connections");
            }
            let domain = self
                .inner
                .domains
                .get(&service.id)
                .cloned()
                .context("unknown PTY service")?;
            if service.protocol != ServiceProtocol::Tcp {
                bail!("PTY requires TCP");
            }
            let permit = Arc::clone(&self.inner.capacity)
                .try_acquire_owned()
                .context("PTY connection limit reached")?;
            let (application, stream) = tokio::io::duplex(65_536);
            let mut stop = self.inner.stop.subscribe();
            let mut tasks = self
                .inner
                .tasks
                .lock()
                .map_err(|_| anyhow!("PTY task registry poisoned"))?;
            if *stop.borrow() {
                bail!("PTY backend is stopping");
            }
            let task = tokio::spawn(async move {
                let _permit = permit;
                if *stop.borrow() {
                    return;
                }
                tokio::select! {
                    biased;
                    _ = stop.changed() => {},
                    result = serve(domain, Box::new(stream), peer) => {
                        if result.is_err() { tracing::debug!("PTY business connection ended"); }
                    }
                }
            });
            tasks.retain(|task| !task.is_finished());
            tasks.push(task);
            Ok(Box::new(application) as BoxStream)
        })
    }
    fn connect_udp<'a>(
        &'a self,
        _: &'a Service,
        _: ServicePeer,
    ) -> IoFuture<'a, Arc<dyn DatagramIo>> {
        Box::pin(async { bail!("PTY requires TCP") })
    }
}
async fn serve(domain: Arc<Domain>, stream: BoxStream, peer: ServicePeer) -> Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let hello: ClientMessage = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        read_message(&mut reader),
    )
    .await??
    .context("missing PTY hello")?;
    hello.validate()?;
    let ClientMessage::Hello { label, .. } = hello else {
        bail!("first PTY message must be hello");
    };
    let (events, mut outgoing) = mpsc::channel(64);
    let connection = Connection {
        id: Uuid::new_v4(),
        travel_id: peer.travel_id,
        label,
        lifetime: peer.lifetime,
        events,
        stop: watch::channel(false).0,
        can_write: domain.can_write.load(Ordering::Acquire),
    };
    let _cleanup = ConnectionCleanup {
        domain: Arc::clone(&domain),
        id: connection.id,
        stop: connection.stop.clone(),
    };
    connection.send(ServerMessage::Hello {
        version: PROTOCOL_VERSION,
        can_write: connection.can_write,
    })?;
    let requests = async {
        while let Some(message) = read_message::<ClientMessage>(&mut reader).await? {
            message.validate()?;
            let ClientMessage::Request {
                request_id,
                operation,
            } = message
            else {
                bail!("duplicate PTY hello");
            };
            let result = domain
                .operation(&connection, operation)
                .await
                .unwrap_or_else(|error| Reply::Error {
                    code: "operation_failed".into(),
                    message: error.to_string().chars().take(512).collect(),
                });
            let attached = if let Reply::Attached { attachment_id, .. } = &result {
                Some(*attachment_id)
            } else {
                None
            };
            connection.send(ServerMessage::Response { request_id, result })?;
            if let Some(id) = attached {
                domain.attachment(connection.id, id)?.start_output(id)?;
            }
        }
        Ok::<(), anyhow::Error>(())
    };
    let responses = async {
        while let Some(message) = outgoing.recv().await {
            write_server_message(&mut writer, &message).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! { biased; () = connection.ended() => Ok(()), result = requests => result, result = responses => result }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use flowsplice_core::authorization::unix_time_secs;
    use flowsplice_transport::ServiceLifetime;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[allow(clippy::too_many_lines)] // One real tmux lifecycle, including restart recovery.
    async fn named_sessions_validate_rename_preserve_attachments_and_recover_metadata() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let binary = if cfg!(target_os = "macos") {
            "/opt/homebrew/bin/tmux"
        } else {
            "/usr/bin/tmux"
        };
        let socket = directory.path().join("domain/tmux.sock");
        let config = TmuxConfig {
            binary: binary.into(),
            socket: socket.clone(),
            shell: "/bin/sh".into(),
            working_directory: directory.path().into(),
        };
        let tmux = Arc::new(Tmux::open(config.clone()).await?);
        let domain = |tmux| Domain {
            tmux,
            sessions: Mutex::new(BTreeMap::new()),
            control: tokio::sync::Mutex::new(()),
            can_write: AtomicBool::new(true),
        };
        let backend = domain(Arc::clone(&tmux));
        let (lifetime, _guard) = ServiceLifetime::new(unix_time_secs()? + 60);
        let (events, _receiver) = mpsc::channel(64);
        let connection = Connection {
            id: Uuid::new_v4(),
            travel_id: "metadata-test".into(),
            label: "test".into(),
            lifetime,
            events,
            stop: watch::channel(false).0,
            can_write: true,
        };
        assert!(
            backend
                .operation(
                    &connection,
                    Operation::NewNamed {
                        name: " \n ".into(),
                        columns: 80,
                        rows: 24
                    }
                )
                .await
                .is_err()
        );
        assert!(tmux.list().await?.is_empty());
        let reply = backend
            .operation(
                &connection,
                Operation::NewNamed {
                    name: "  中文 $(touch nope); 😀  ".into(),
                    columns: 80,
                    rows: 24,
                },
            )
            .await?;
        let Reply::Attached {
            attachment_id,
            session,
            ..
        } = reply
        else {
            bail!("missing attachment");
        };
        let before = backend.session(session.id)?.details()?;
        assert_eq!(before.name, "中文 $(touch nope); 😀");
        let rename = |name: &str| Operation::Rename {
            session_id: session.id,
            name: name.into(),
        };
        let observer = Connection {
            id: Uuid::new_v4(),
            can_write: false,
            stop: watch::channel(false).0,
            ..connection.clone()
        };
        assert!(
            backend
                .operation(&observer, rename("forbidden"))
                .await
                .is_err()
        );
        for invalid in [
            " ".to_owned(),
            "a\nb".into(),
            "x".repeat(65),
            "😀".repeat(65),
        ] {
            assert!(
                backend
                    .operation(&connection, rename(&invalid))
                    .await
                    .is_err()
            );
        }
        assert!(
            backend
                .operation(
                    &connection,
                    Operation::Rename {
                        session_id: Uuid::new_v4(),
                        name: "absent".into(),
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(backend.session(session.id)?.details()?, before);
        let name = "renamed #{session_id}; $(touch nope) 中文 😀";
        assert_eq!(
            backend
                .operation(&connection, rename(&format!("  {name}  ")))
                .await?,
            Reply::Ok
        );
        let mut expected = before.clone();
        expected.name = name.into();
        assert_eq!(backend.session(session.id)?.details()?, expected);
        assert!(
            backend
                .session(session.id)?
                .owns(connection.id, attachment_id)?
        );
        assert_eq!(
            tmux.list().await?,
            vec![(session.id, before.created_at_unix_secs)]
        );
        // A separate list connection sees the committed name without acquiring
        // the writer lease or creating an attachment.
        assert_eq!(
            backend.operation(&observer, Operation::ListDetails).await?,
            Reply::SessionDetails {
                sessions: vec![expected]
            }
        );
        backend
            .operation(&connection, Operation::Detach { attachment_id })
            .await?;
        let Reply::SessionDetails { sessions } = backend
            .operation(&connection, Operation::ListDetails)
            .await?
        else {
            bail!("missing details");
        };
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, name);
        assert_eq!(sessions[0].connection_count, 0);
        assert!(sessions[0].last_connected_at_unix_secs.is_some());
        assert!(!directory.path().join("nope").exists());
        // Management from the session list needs business write permission but
        // does not require being attached or owning terminal input.
        assert_eq!(
            backend
                .operation(&connection, rename("Detached session"))
                .await?,
            Reply::Ok
        );
        let mut expected_recovered = sessions[0].clone();
        expected_recovered.name = "Detached session".into();
        connection.stop.send_replace(true);
        assert!(
            backend
                .operation(&connection, rename("inactive"))
                .await
                .is_err()
        );
        assert_eq!(backend.session(session.id)?.details()?, expected_recovered);
        drop(backend);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while Arc::strong_count(&tmux) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await?;
        drop(tmux);
        let recovered = domain(Arc::new(Tmux::open(config).await?));
        assert_eq!(recovered.refresh().await?[0].id, session.id);
        assert_eq!(
            recovered.session(session.id)?.details()?,
            expected_recovered
        );
        let _ = tokio::process::Command::new(binary)
            .arg("-S")
            .arg(socket)
            .arg("kill-server")
            .output()
            .await?;
        assert!(
            recovered
                .operation(
                    &Connection {
                        can_write: true,
                        ..observer
                    },
                    rename("ended")
                )
                .await
                .is_err()
        );
        assert!(recovered.tmux.list().await?.is_empty());
        Ok(())
    }
}
