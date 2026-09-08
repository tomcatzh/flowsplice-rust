//! One cancellable application actor shared by native hosts; no local network listeners.
use anyhow::{Result, bail};
use flowsplice_core::business::BusinessDescriptor;
use flowsplice_pty_client::{ConnectOptions, PtyClient};
use flowsplice_pty_protocol::{Operation, ServerMessage};
use flowsplice_travel_core::business::{BusinessEnrollmentOptions, enroll};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeOptions {
    pub install_dir: PathBuf,
    pub root_public_key: String,
    pub descriptor: BusinessDescriptor,
    pub travel_id: String,
    pub label: String,
}
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Enroll {
        relay: String,
        password: String,
    },
    Connect {
        password: String,
    },
    Disconnect,
    Operation {
        operation: Operation,
    },
    ConnectHome {
        home_id: String,
    },
    DisconnectHome {
        home_id: String,
    },
    OperationHome {
        home_id: String,
        operation: Operation,
    },
}
pub(crate) struct Outbox {
    pub(crate) queue: Mutex<VecDeque<Value>>,
    pub(crate) cancel: watch::Sender<bool>,
    pub(crate) overflow: watch::Sender<bool>,
}
impl Outbox {
    pub(crate) fn emit(&self, value: Value) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *self.cancel.borrow() {
            return;
        }
        if queue.len() >= 128 {
            queue.clear();
            queue.push_back(json!({"type":"error","message":"Terminal output exceeded the native queue; reconnect from the session list."}));
            queue
                .push_back(json!({"type":"state","installed":true,"connected":false,"busy":false}));
            self.overflow.send_replace(true);
        } else {
            queue.push_back(value);
        }
    }
    fn state(&self, options: &NativeOptions, connected: bool, busy: bool) {
        self.emit(json!({"type":"state","installed":options.install_dir.join("approved-business-binding.json").exists(),"connected":connected,"busy":busy}));
    }
    pub(crate) fn error(&self, error: &anyhow::Error) {
        self.emit(json!({"type":"error","message":error.to_string().chars().take(512).collect::<String>()}));
    }
}
/// An opaque native session. Platform code owns keychain storage and visibility policy.
pub struct NativeSession {
    pub(crate) actions: mpsc::Sender<Action>,
    pub(crate) outbox: Arc<Outbox>,
    pub(crate) task: Mutex<Option<JoinHandle<()>>>,
}
impl NativeSession {
    /// Starts the native actor in the current Tokio runtime.
    ///
    /// # Errors
    /// Returns an error if the immutable private configuration is invalid.
    pub fn open(options: NativeOptions) -> Result<Self> {
        if !options.install_dir.is_absolute()
            || options.travel_id.is_empty()
            || options.travel_id.len() > 128
            || options.label.len() > 64
            || options.label.chars().any(char::is_control)
            || options.root_public_key.is_empty()
        {
            bail!("invalid private terminal configuration");
        }
        let (actions, receiver) = mpsc::channel(32);
        let (cancel, cancellation) = watch::channel(false);
        let outbox = Arc::new(Outbox {
            queue: Mutex::new(VecDeque::new()),
            cancel,
            overflow: watch::channel(false).0,
        });
        let actor_outbox = Arc::clone(&outbox);
        let task = tokio::spawn(actor(options, receiver, actor_outbox, cancellation));
        Ok(Self {
            actions,
            outbox,
            task: Mutex::new(Some(task)),
        })
    }
    /// Enqueues a user action without waiting for network I/O.
    ///
    /// # Errors
    /// Returns an error if the actor is closed or its queue is full.
    pub fn send(&self, action: Action) -> Result<()> {
        if *self.outbox.cancel.borrow() {
            bail!("native terminal session closed");
        }
        if self.actions.try_send(action).is_err() {
            self.outbox.overflow.send_replace(true);
            bail!("native action queue unavailable; connection closing");
        }
        Ok(())
    }
    pub fn poll(&self) -> Vec<Value> {
        let mut queue = self
            .outbox
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = queue.len().min(64);
        queue.drain(..count).collect()
    }
    pub fn close(&self) {
        self.outbox.cancel.send_replace(true);
    }
    pub async fn shutdown(&self) {
        self.close();
        let task = self
            .task
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }
}
impl Drop for NativeSession {
    fn drop(&mut self) {
        self.close();
    }
}

enum JobResult {
    Enrolled(Result<()>),
    Connected(Box<Result<(PtyClient, mpsc::Receiver<ServerMessage>)>>),
}
async fn actor(
    options: NativeOptions,
    mut actions: mpsc::Receiver<Action>,
    outbox: Arc<Outbox>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut client: Option<PtyClient> = None;
    let mut events: Option<mpsc::Receiver<ServerMessage>> = None;
    let mut job: Option<JoinHandle<JobResult>> = None;
    outbox.state(&options, false, false);
    let mut overflow = outbox.overflow.subscribe();
    loop {
        tokio::select! {
            biased;
            () = async { let _ = cancel.wait_for(|cancelled| *cancelled).await; } => break,
            () = async { let _ = overflow.wait_for(|full| *full).await; } => {
                stop_job(&mut job).await;
                if let Some(client) = client.take() { client.shutdown().await; }
                events = None;
                outbox.overflow.send_replace(false);
                while actions.try_recv().is_ok() {}
                outbox.state(&options, false, false);
            },
            action = actions.recv() => {
                let Some(action) = action else { break; };
                match action {
                    Action::Disconnect => {
                        stop_job(&mut job).await;
                        if let Some(client) = client.take() { client.shutdown().await; }
                        events = None;
                        outbox.state(&options, false, false);
                    },
                    Action::Enroll { relay, password } => {
                        if job.is_some() || client.is_some() { outbox.error(&anyhow::anyhow!("another connection operation is active")); continue; }
                        let config = options.clone(); let output = Arc::clone(&outbox);
                        outbox.state(&options, false, true);
                        job = Some(tokio::spawn(async move {
                            JobResult::Enrolled(enroll(BusinessEnrollmentOptions {
                                travel_id: config.travel_id, install_dir: config.install_dir, relay_address: relay,
                                deployment_root_public_key: config.root_public_key, private_key_password: password, wait_timeout_secs: 3600,
                            }, config.descriptor, move |progress| output.emit(json!({"type":"progress","progress":progress}))).await)
                        }));
                    },
                    Action::Connect { password } => {
                        if job.is_some() || client.is_some() { outbox.error(&anyhow::anyhow!("another connection operation is active")); continue; }
                        let config = options.clone(); outbox.state(&options, false, true);
                        job = Some(tokio::spawn(async move { JobResult::Connected(Box::new(PtyClient::connect(ConnectOptions {
                            config_path: config.install_dir.join("travelagent.toml"), password, root_public_key: config.root_public_key,
                            descriptor: config.descriptor, label: config.label,
                        }).await)) }));
                    },
                    Action::ConnectHome { .. } | Action::DisconnectHome { .. } | Action::OperationHome { .. } => {
                        outbox.error(&anyhow::anyhow!("class actions require a service-class installation"));
                    },
                    Action::Operation { operation } => {
                        if let Some(client) = &client {
                            match client.send(operation.clone()).await {
                                Ok(request_id) => {
                                    if !matches!(operation, Operation::Input { .. } | Operation::Resize { .. }) {
                                        outbox.emit(json!({"type":"submitted","request_id":request_id,"operation":operation}));
                                    }
                                },
                                Err(error) => outbox.error(&error),
                            }
                        } else { outbox.error(&anyhow::anyhow!("connect before opening a terminal session")); }
                    },
                }
            },
            result = async { if let Some(job) = &mut job { Some(job.await) } else { std::future::pending().await } } => {
                job = None;
                match result {
                    Some(Ok(JobResult::Enrolled(result))) => if let Err(error) = result { outbox.error(&error); },
                    Some(Ok(JobResult::Connected(result))) => match *result {
                        Ok((connected, receiver)) => { client = Some(connected); events = Some(receiver); },
                        Err(error) => outbox.error(&error),
                    },
                    _ => outbox.error(&anyhow::anyhow!("terminal operation was interrupted")),
                }
                outbox.state(&options, client.is_some(), false);
            },
            event = async { if let Some(events) = &mut events { events.recv().await } else { std::future::pending().await } } => {
                if let Some(message) = event { outbox.emit(json!({"type":"protocol","message":message})); }
                else {
                    events = None;
                    if let Some(client) = client.take() { client.shutdown().await; }
                    outbox.error(&anyhow::anyhow!("Connection closed. Refresh the session list after reconnecting."));
                    outbox.state(&options, false, false);
                }
            },
        }
    }
    stop_job(&mut job).await;
    if let Some(client) = client {
        client.shutdown().await;
    }
}
async fn stop_job(job: &mut Option<JoinHandle<JobResult>>) {
    if let Some(job) = job.take() {
        job.abort();
        let _ = job.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flowsplice_core::{
        business::SignedHomeServiceGrant, deployment::SignedHomeEndpointCredential,
    };
    use std::time::Duration;
    pub(super) fn options() -> NativeOptions {
        NativeOptions {
            install_dir: std::env::temp_dir()
                .join(format!("flowsplice-pty-absent-{}", uuid::Uuid::new_v4())),
            root_public_key: "unconfigured-test-root".into(),
            travel_id: "test".into(),
            label: "test".into(),
            descriptor: BusinessDescriptor {
                version: 1,
                approving_home_id: "test".into(),
                service_id: "test".into(),
                endpoint: SignedHomeEndpointCredential {
                    authority_id: "test".into(),
                    payload_hex: String::new(),
                    signature_hex: String::new(),
                },
                grant: SignedHomeServiceGrant {
                    authority_id: "test".into(),
                    payload_hex: String::new(),
                    signature_hex: String::new(),
                },
            },
        }
    }
    #[tokio::test]
    async fn failed_connect_returns_to_list_state_and_shutdown_cancels_queued_work() -> Result<()> {
        let options = options();
        let path = options.install_dir.clone();
        let native = NativeSession::open(options)?;
        native.send(Action::Connect {
            password: String::new(),
        })?;
        tokio::time::timeout(Duration::from_secs(2), async {
            let mut error = false;
            loop {
                for event in native.poll() {
                    error |= event["type"] == "error";
                    if error && event["type"] == "state" && event["busy"] == false {
                        return;
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        native.send(Action::Disconnect)?;
        tokio::time::timeout(Duration::from_secs(2), native.shutdown()).await?;
        assert!(
            native
                .send(Action::Connect {
                    password: String::new()
                })
                .is_err()
        );
        assert!(!path.exists());
        Ok(())
    }
    #[tokio::test]
    async fn full_action_queue_and_full_event_queue_remain_bounded_and_cancellable() -> Result<()> {
        let native = NativeSession::open(options())?;
        for _ in 0..32 {
            native.send(Action::Disconnect)?;
        }
        assert!(native.send(Action::Disconnect).is_err());
        for _ in 0..1000 {
            native
                .outbox
                .emit(json!({"type":"protocol","message":"fixture"}));
        }
        assert!(native.poll().len() <= 64);
        assert!(
            native
                .outbox
                .queue
                .lock()
                .map_err(|_| anyhow::anyhow!("queue poisoned"))?
                .len()
                <= 128
        );
        tokio::time::timeout(Duration::from_secs(2), native.shutdown()).await?;
        Ok(())
    }
}
