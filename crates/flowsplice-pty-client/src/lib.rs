//! In-process terminal client with bounded queues and no automatic session creation.
use anyhow::{Context, Result, bail};
use flowsplice_core::{business::BusinessDescriptor, protocol::ServiceProtocol};
use flowsplice_pty_protocol::{
    APPLICATION_PROTOCOL, ClientMessage, Operation, PROTOCOL_VERSION, Reply, ServerMessage,
    read_message, write_message,
};
use flowsplice_travel_core::{ServiceBinding, TravelCore};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, watch},
    task::JoinHandle,
};
use uuid::Uuid;

const CAPACITY: usize = 64;

/// Exact installation and optional terminal display label.
pub struct ConnectOptions {
    pub config_path: PathBuf,
    pub password: String,
    pub root_public_key: String,
    pub descriptor: BusinessDescriptor,
    pub label: String,
}

/// One shared transport for all terminal attachments on this connection.
pub struct PtyClient {
    requests: mpsc::Sender<ClientMessage>,
    pending_new: Arc<Mutex<Option<Uuid>>>,
    cancel: watch::Sender<bool>,
    finished: watch::Receiver<bool>,
    task: Mutex<Option<JoinHandle<()>>>,
    runtime: Option<TravelCore>,
}

impl PtyClient {
    /// Validates the installed business, performs Hello and List, then starts event delivery.
    ///
    /// # Errors
    /// Returns installation, unsupported service, transport or handshake errors.
    pub async fn connect(options: ConnectOptions) -> Result<(Self, mpsc::Receiver<ServerMessage>)> {
        let approved = flowsplice_travel_core::business::load_binding(
            &options.config_path,
            &options.root_public_key,
            &options.descriptor,
        )?;
        if approved
            .service
            .capabilities
            .iter()
            .any(|capability| capability != "read" && capability != "write")
            || approved.service.application_protocol != APPLICATION_PROTOCOL
            || approved.binding.protocol != ServiceProtocol::Tcp
            || !approved
                .service
                .capabilities
                .iter()
                .any(|capability| capability == "read")
        {
            bail!("business descriptor does not permit this terminal protocol and read capability");
        }
        let (runtime, approved) = TravelCore::start_business(
            &options.config_path,
            &options.password,
            &options.root_public_key,
            &options.descriptor,
        )
        .await?;
        let result = async {
            let stream = runtime.connect_tcp(&approved.binding).await?;
            Self::from_stream(stream, options.label).await
        }
        .await;
        match result {
            Ok((mut client, events)) => {
                client.runtime = Some(runtime);
                Ok((client, events))
            }
            Err(error) => {
                runtime.shutdown().await;
                Err(error)
            }
        }
    }

    /// Opens a PTY stream on an existing runtime without owning its lifetime.
    ///
    /// # Errors
    /// Returns binding, transport, or terminal handshake errors.
    pub async fn connect_shared(
        runtime: &TravelCore,
        binding: &ServiceBinding,
        label: String,
    ) -> Result<(Self, mpsc::Receiver<ServerMessage>)> {
        let stream = runtime.connect_tcp(binding).await?;
        Self::from_stream(stream, label).await
    }

    async fn from_stream<S>(
        mut stream: S,
        label: String,
    ) -> Result<(Self, mpsc::Receiver<ServerMessage>)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (events, receiver) = mpsc::channel(CAPACITY);
        tokio::time::timeout(Duration::from_secs(15), async {
            let hello = ClientMessage::Hello { version: PROTOCOL_VERSION, label };
            hello.validate()?;
            write_message(&mut stream, &hello).await?;
            let hello: ServerMessage = read_message(&mut stream).await?.context("EOF before server Hello")?;
            hello.validate()?;
            if !matches!(hello, ServerMessage::Hello { version: PROTOCOL_VERSION, .. }) { bail!("expected server Hello"); }
            events.try_send(hello)?;
            let request_id = Uuid::new_v4();
            write_message(&mut stream, &ClientMessage::Request { request_id, operation: Operation::List }).await?;
            let response: ServerMessage = read_message(&mut stream).await?.context("EOF before session list")?;
            response.validate()?;
            if !matches!(&response, ServerMessage::Response { request_id: id, result: Reply::Sessions { .. } } if *id == request_id) { bail!("expected matching session list response"); }
            events.try_send(response)?;
            Ok::<_,anyhow::Error>(())
        }).await.context("PTY handshake timed out")??;
        let (requests, outgoing) = mpsc::channel(CAPACITY);
        let (cancel, cancellation) = watch::channel(false);
        let (done, finished) = watch::channel(false);
        let pending_new = Arc::new(Mutex::new(None));
        let pending = Arc::clone(&pending_new);
        let task = tokio::spawn(async move {
            run(stream, outgoing, events, pending, cancellation).await;
            done.send_replace(true);
        });
        Ok((
            Self {
                requests,
                pending_new,
                cancel,
                finished,
                task: Mutex::new(Some(task)),
                runtime: None,
            },
            receiver,
        ))
    }

    /// Queues one validated operation, preserving the caller's attachment and epoch.
    ///
    /// # Errors
    /// Returns invalid operation, closed/full queue or already-pending New errors.
    // Keep the async SDK contract while full queues fail promptly without an I/O wait.
    #[allow(clippy::unused_async)]
    pub async fn send(&self, operation: Operation) -> Result<Uuid> {
        if *self.cancel.borrow() || *self.finished.borrow() {
            bail!("PTY connection closed");
        }
        let is_new = matches!(
            operation,
            Operation::New { .. } | Operation::NewNamed { .. }
        );
        let request_id = Uuid::new_v4();
        let message = ClientMessage::Request {
            request_id,
            operation,
        };
        message.validate()?;
        let mut pending = self
            .pending_new
            .lock()
            .map_err(|_| anyhow::anyhow!("pending New lock poisoned"))?;
        if is_new && pending.is_some() {
            bail!("a New request is already pending");
        }
        if self.requests.try_send(message).is_err() {
            self.cancel.send_replace(true);
            bail!("PTY request queue unavailable; connection closing");
        }
        if is_new {
            *pending = Some(request_id);
        }
        Ok(request_id)
    }

    /// Cancels I/O, joins its task within five seconds, then shuts down shared transport.
    pub async fn shutdown(&self) {
        self.cancel.send_replace(true);
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(mut task) = task {
            if tokio::time::timeout(Duration::from_secs(5), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        } else {
            let mut finished = self.finished.clone();
            let _ =
                tokio::time::timeout(Duration::from_secs(5), finished.wait_for(|done| *done)).await;
        }
        if let Some(runtime) = &self.runtime {
            runtime.shutdown().await;
        }
    }
}

impl Drop for PtyClient {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

async fn run<S>(
    stream: S,
    mut outgoing: mpsc::Receiver<ClientMessage>,
    events: mpsc::Sender<ServerMessage>,
    pending: Arc<Mutex<Option<Uuid>>>,
    mut cancellation: watch::Receiver<bool>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let receive = async {
        while let Some(message) = read_message::<ServerMessage>(&mut reader).await? {
            message.validate()?;
            if let ServerMessage::Response { request_id, .. } = &message {
                let mut pending = pending
                    .lock()
                    .map_err(|_| anyhow::anyhow!("pending New lock poisoned"))?;
                if pending.as_ref() == Some(request_id) {
                    *pending = None;
                }
            }
            events
                .try_send(message)
                .context("PTY event queue unavailable")?;
        }
        Err::<(), _>(anyhow::anyhow!("PTY EOF"))
    };
    let transmit = async {
        while let Some(message) = outgoing.recv().await {
            write_message(&mut writer, &message).await?;
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        _ = receive => {},
        _ = transmit => {},
        _ = cancellation.wait_for(|cancelled| *cancelled) => {},
        () = events.closed() => {},
    }
    // A dropped Carrier is recoverable, so dropping the socket alone leaves Home's
    // application connection alive. Send application EOF and wait for Home to close
    // its half after releasing attachments. Never replay queued terminal input.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::io::AsyncWriteExt::shutdown(&mut writer).await?;
        tokio::io::copy(&mut reader, &mut tokio::io::sink()).await?;
        Ok::<(), std::io::Error>(())
    })
    .await;
    if let Ok(mut pending) = pending.lock() {
        *pending = None;
    }
}

#[cfg(test)]
mod tests;
