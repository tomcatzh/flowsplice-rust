//! One class identity/runtime with independently cancellable PTY connections.
use crate::{Action, NativeSession, engine::Outbox};
use anyhow::{Result, anyhow, bail};
use flowsplice_core::{business::ServiceClassDescriptor, protocol::ServiceProtocol};
use flowsplice_pty_client::PtyClient;
use flowsplice_pty_protocol::{APPLICATION_PROTOCOL, Operation};
use flowsplice_travel_core::{
    ServiceBinding, TravelCore,
    business::BusinessEnrollmentOptions,
    service_class::{self, ApprovedServiceClass, ServiceClassTarget},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClassNativeOptions {
    pub install_dir: PathBuf,
    pub root_public_key: String,
    pub service_class: ServiceClassDescriptor,
    pub travel_id: String,
    pub label: String,
}

impl NativeSession {
    /// Opens an independent application identity for the PTY service class.
    ///
    /// # Errors
    /// Returns invalid application identity or class configuration errors.
    pub fn open_class(options: ClassNativeOptions) -> Result<Self> {
        options.service_class.validate()?;
        if !options.install_dir.is_absolute()
            || options.root_public_key.is_empty()
            || options.travel_id.is_empty()
            || options.travel_id.len() > 128
            || options.label.is_empty()
            || options.label.len() > 64
            || options
                .label
                .chars()
                .any(flowsplice_pty_protocol::is_display_control)
            || options.service_class.application_protocol != APPLICATION_PROTOCOL
            || options.service_class.protocol != ServiceProtocol::Tcp
        {
            bail!("invalid private PTY service-class configuration");
        }
        let (actions, receiver) = mpsc::channel(32);
        let (cancel, cancellation) = watch::channel(false);
        let outbox = Arc::new(Outbox {
            queue: Mutex::new(VecDeque::new()),
            cancel,
            overflow: watch::channel(false).0,
        });
        let output = Arc::clone(&outbox);
        let task = tokio::spawn(actor(options, receiver, output, cancellation));
        Ok(Self {
            actions,
            outbox,
            task: Mutex::new(Some(task)),
        })
    }
}

impl Outbox {
    /// Scoped queue accounting includes protocol and metadata, reserving 16 root slots.
    /// A false result requires the producer to close only this Home connection.
    fn scoped(&self, id: &str, mut event: Value) -> bool {
        if *self.cancel.borrow() {
            return false;
        }
        event["home_id"] = json!(id);
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Merge only immediately adjacent output from the same attachment. A
        // control message or another stream remains an ordering barrier.
        if let Some(last) = queue.back_mut()
            && last["home_id"].as_str() == Some(id)
            && last["type"] == "protocol"
            && event["type"] == "protocol"
            && last["message"]["type"] == "output"
            && event["message"]["type"] == "output"
            && last["message"]["attachment_id"].is_string()
            && last["message"]["attachment_id"] == event["message"]["attachment_id"]
            && let (Some(previous), Some(incoming)) = (
                last["message"]["data"].as_array_mut(),
                event["message"]["data"].as_array_mut(),
            )
            && previous.len().saturating_add(incoming.len()) <= 16_384
        {
            previous.append(incoming);
            return true;
        }
        let own = queue
            .iter()
            .filter(|item| item["home_id"].as_str() == Some(id))
            .count();
        let scoped = queue
            .iter()
            .filter(|item| item["home_id"].is_string())
            .count();
        if own >= 16 || scoped >= 112 {
            queue.retain(|item| item["home_id"].as_str() != Some(id));
            if queue.len() <= 126 {
                queue.push_back(json!({"type":"error","home_id":id,"message":"Home output exceeded the native queue; reconnect from its session list."}));
                queue.push_back(json!({"type":"state","home_id":id,"installed":true,"connected":false,"busy":false}));
            } else {
                self.overflow.send_replace(true);
            }
            return false;
        }
        if queue.len() >= 128 {
            self.overflow.send_replace(true);
            return false;
        }
        queue.push_back(event);
        true
    }
    fn identity(&self, options: &ClassNativeOptions, connected: bool, busy: bool) {
        self.emit(json!({"type":"identity","installed":options.install_dir.join("approved-service-class-binding.json").exists(),"connected":connected,"busy":busy,"label":options.label}));
    }
}

struct HomeTask {
    stop: watch::Sender<bool>,
    actions: mpsc::Sender<(Operation, Option<uuid::Uuid>)>,
    task: JoinHandle<()>,
}
impl HomeTask {
    async fn shutdown(self) {
        self.stop.send_replace(true);
        let _ = self.task.await;
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Forwarded {
    Sent,
    Unavailable,
    Interrupted,
}

async fn forward_home_action<T>(
    actions: &mpsc::Sender<T>,
    operation: T,
    mut cancel: watch::Receiver<bool>,
    mut overflow: watch::Receiver<bool>,
) -> Forwarded {
    // A short input burst can fill this bounded queue before its consumer is
    // scheduled. Wait for admission instead of disconnecting a healthy Home.
    // Cancellation drops only the operation that has not entered the queue;
    // already-admitted terminal input is never retried.
    tokio::select! {
        biased;
        _ = cancel.wait_for(|value| *value) => Forwarded::Interrupted,
        _ = overflow.wait_for(|value| *value) => Forwarded::Interrupted,
        result = tokio::time::timeout(Duration::from_millis(250), actions.send(operation)) => {
            if matches!(result, Ok(Ok(()))) { Forwarded::Sent } else { Forwarded::Unavailable }
        }
    }
}

#[cfg(test)]
#[path = "class_queue_tests.rs"]
mod queue_tests;

type RuntimeBinding = (TravelCore, ApprovedServiceClass);
enum Job {
    Enrolled(Result<()>),
    Connected(Box<Result<RuntimeBinding>>),
}

async fn stop_job(job: &mut Option<JoinHandle<Job>>) {
    if let Some(task) = job.take() {
        task.abort();
        // A completed connect result still owns a runtime; stop it explicitly.
        if let Ok(Job::Connected(result)) = task.await
            && let Ok((runtime, _)) = *result
        {
            runtime.shutdown().await;
        }
    }
}
async fn stop_all(
    homes: &mut BTreeMap<String, HomeTask>,
    runtime: &mut Option<(Arc<TravelCore>, ApprovedServiceClass)>,
) {
    let stopped = std::mem::take(homes);
    for home in stopped.values() {
        home.stop.send_replace(true);
    }
    for (_, home) in stopped {
        home.shutdown().await;
    }
    if let Some((runtime, _)) = runtime.take() {
        runtime.shutdown().await;
    }
}
fn catalog(outbox: &Outbox, targets: &BTreeMap<String, ServiceClassTarget>) {
    let homes: Vec<_> = targets.values().map(|t| json!({"id":t.id,"home_id":t.home_id,"name":t.name,"service_id":t.service_id,"platform":"host","relay":""})).collect();
    outbox.emit(json!({"type":"homes","homes":homes}));
}
fn disconnected(outbox: &Outbox, id: &str) {
    outbox.scoped(
        id,
        json!({"type":"state","installed":true,"connected":false,"busy":false}),
    );
}

// Keep lifecycle transitions together in one serialized select loop.
#[allow(clippy::too_many_lines)]
async fn actor(
    options: ClassNativeOptions,
    mut actions: mpsc::Receiver<Action>,
    outbox: Arc<Outbox>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut runtime: Option<(Arc<TravelCore>, ApprovedServiceClass)> = None;
    let mut targets: BTreeMap<String, ServiceClassTarget> = BTreeMap::new();
    let mut homes: BTreeMap<String, HomeTask> = BTreeMap::new();
    let mut job: Option<JoinHandle<Job>> = None;
    let mut refresh = tokio::time::interval(Duration::from_secs(1));
    refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut overflow = outbox.overflow.subscribe();
    outbox.identity(&options, false, false);
    loop {
        tokio::select! {
            biased;
            () = async { let _ = cancel.wait_for(|value| *value).await; } => break,
            () = async { let _ = overflow.wait_for(|value| *value).await; } => {
                stop_job(&mut job).await;
                stop_all(&mut homes, &mut runtime).await;
                targets.clear(); catalog(&outbox, &targets);
                outbox.overflow.send_replace(false);
                while actions.try_recv().is_ok() {}
                outbox.identity(&options, false, false);
            },
            result = async { if let Some(task) = &mut job { task.await } else { std::future::pending().await } } => {
                job = None;
                match result {
                    Ok(Job::Enrolled(result)) => if let Err(error) = result { outbox.error(&error); },
                    Ok(Job::Connected(result)) => match *result {
                        Ok((core, approved)) => { runtime = Some((Arc::new(core), approved)); refresh.reset_immediately(); },
                        // Local identity initialization failed before the runtime
                        // started. Retry requires repaired files or a new password;
                        // a timer using the same stored password cannot fix it.
                        Err(error) => outbox.emit(json!({"type":"error","code":"identity_unavailable","message":error.to_string().chars().take(512).collect::<String>()})),
                    },
                    Err(_) => outbox.error(&anyhow!("Class connection operation was interrupted")),
                }
                outbox.identity(&options, runtime.is_some(), false);
            },
            _ = refresh.tick(), if runtime.is_some() => {
                let Some((core, approved)) = runtime.as_ref() else { continue; };
                match core.service_class_targets(approved).await {
                    Ok(discovered) => {
                        let next: BTreeMap<_, _> = discovered.into_iter().map(|target| (target.id.clone(), target)).collect();
                        let removed: Vec<String> = targets.iter().filter(|(id, old)| next.get(*id).is_none_or(|new| new.service != old.service)).map(|(id, _)| id.clone()).collect();
                        for id in removed {
                            if let Some(home) = homes.remove(&id) { home.shutdown().await; }
                            disconnected(&outbox, &id);
                        }
                        let added: Vec<String> = next.keys().filter(|id| !targets.contains_key(*id)).cloned().collect();
                        if targets != next {
                            targets = next;
                            catalog(&outbox, &targets);
                        }
                        for id in added { disconnected(&outbox, &id); }
                        let ended: Vec<_> = homes.iter().filter(|(_, home)| home.task.is_finished()).map(|(id, _)| id.clone()).collect();
                        for id in ended { if let Some(home) = homes.remove(&id) { home.shutdown().await; } }
                    },
                    Err(error) => {
                        outbox.error(&error);
                        stop_all(&mut homes, &mut runtime).await;
                        targets.clear(); catalog(&outbox, &targets);
                        outbox.identity(&options, false, false);
                    }
                }
            },
            action = actions.recv() => {
                let Some(action) = action else { break; };
                match action {
                    Action::Disconnect => {
                        stop_job(&mut job).await;
                        stop_all(&mut homes, &mut runtime).await;
                        targets.clear(); catalog(&outbox, &targets);
                        outbox.identity(&options, false, false);
                    },
                    Action::Enroll { relay, password } => {
                        if job.is_some() || runtime.is_some() { outbox.error(&anyhow!("another class connection operation is active")); continue; }
                        let config = options.clone(); let output = Arc::clone(&outbox);
                        outbox.identity(&options, false, true);
                        job = Some(tokio::spawn(async move { Job::Enrolled(service_class::enroll(BusinessEnrollmentOptions {
                            travel_id:config.travel_id, install_dir:config.install_dir, relay_address:relay, deployment_root_public_key:config.root_public_key,
                            private_key_password:password, wait_timeout_secs:3600,
                        }, config.service_class, config.label, move |progress| output.emit(json!({"type":"progress","progress":progress}))).await) }));
                    },
                    Action::Connect { password } => {
                        if job.is_some() || runtime.is_some() { outbox.error(&anyhow!("another class connection operation is active")); continue; }
                        let config = options.clone(); outbox.identity(&options, false, true);
                        job = Some(tokio::spawn(async move { Job::Connected(Box::new(TravelCore::start_service_class(&config.install_dir.join("travelagent.toml"), &password, &config.root_public_key, &config.service_class).await)) }));
                    },
                    Action::ConnectHome { home_id } => {
                        if homes.get(&home_id).is_some_and(|home| !home.task.is_finished()) { continue; }
                        if let Some(home) = homes.remove(&home_id) { home.shutdown().await; }
                        let Some((core, approved)) = runtime.as_ref() else { outbox.scoped(&home_id,json!({"type":"error","message":"Connect the application identity first"})); continue; };
                        if homes.len() >= 8 { outbox.scoped(&home_id,json!({"type":"error","message":"At most eight Home connections may be open"})); continue; }
                        let found = core.service_class_targets(approved).await.and_then(|targets| targets.into_iter().find(|t| t.id == home_id).ok_or_else(||anyhow!("Home is absent from the verified service-class catalog")));
                        match found {
                            Ok(target) => {
                                let (stop, stopped) = watch::channel(false); let (send, receive) = mpsc::channel(32);
                                let shared = Arc::clone(core); let output = Arc::clone(&outbox); let label = options.label.clone(); let id = home_id.clone();
                                let task = tokio::spawn(home_actor(shared, target, label, receive, stopped, output));
                                homes.insert(id, HomeTask { stop, actions:send, task });
                            },
                            Err(error) => { outbox.scoped(&home_id,json!({"type":"error","message":error.to_string()})); disconnected(&outbox,&home_id); },
                        }
                    },
                    Action::DisconnectHome { home_id } => {
                        if let Some(home) = homes.remove(&home_id) { home.shutdown().await; }
                        disconnected(&outbox, &home_id);
                    },
                    Action::OperationHome { home_id, operation, input_id } => {
                        if let Some(home) = homes.get(&home_id) {
                            let forwarded = forward_home_action(&home.actions, (operation, input_id), cancel.clone(), overflow.clone()).await;
                            if forwarded == Forwarded::Unavailable {
                                outbox.scoped(&home_id,json!({"type":"error","input_id":input_id,"message":"Home action queue busy; operation was not admitted"}));
                            }
                        } else { outbox.scoped(&home_id,json!({"type":"error","input_id":input_id,"message":"Connect this Home before opening a terminal"})); }
                    },
                    Action::Operation { .. } => outbox.error(&anyhow!("Service-class operations require a Home target")),
                }
            }
        }
    }
    stop_job(&mut job).await;
    stop_all(&mut homes, &mut runtime).await;
}

async fn home_actor(
    core: Arc<TravelCore>,
    target: ServiceClassTarget,
    label: String,
    mut actions: mpsc::Receiver<(Operation, Option<uuid::Uuid>)>,
    mut cancel: watch::Receiver<bool>,
    outbox: Arc<Outbox>,
) {
    let id = target.id;
    if !outbox.scoped(
        &id,
        json!({"type":"state","installed":true,"connected":false,"busy":true}),
    ) {
        return;
    }
    let binding = ServiceBinding {
        home_id: target.home_id,
        service_id: target.service_id,
        protocol: target.service.protocol,
    };
    let connected = tokio::select! {
        biased;
        () = async { let _ = cancel.wait_for(|value| *value).await; } => { disconnected(&outbox,&id); return; },
        result = PtyClient::connect_shared(&core,&binding,label) => result,
    };
    let (client, mut events) = match connected {
        Ok(value) => value,
        Err(error) => {
            outbox.scoped(&id, json!({"type":"error","message":error.to_string()}));
            disconnected(&outbox, &id);
            return;
        }
    };
    if !outbox.scoped(
        &id,
        json!({"type":"state","installed":true,"connected":true,"busy":false}),
    ) {
        client.shutdown().await;
        return;
    }
    loop {
        tokio::select! {
            biased;
            () = async { let _ = cancel.wait_for(|value| *value).await; } => break,
            operation = actions.recv() => {
                let Some((operation, input_id)) = operation else { break; };
                let result = tokio::select! { biased; () = async { let _ = cancel.wait_for(|value| *value).await; } => break, result = client.send(operation.clone()) => result };
                let delivered = match result {
                    Ok(request_id) => crate::engine::submitted(request_id, &operation, input_id).is_none_or(|event| outbox.scoped(&id, event)),
                    Err(error) => outbox.scoped(&id,json!({"type":"error","input_id":input_id,"message":error.to_string()})),
                };
                if !delivered { break; }
            },
            message = events.recv() => if let Some(message) = message {
                if !outbox.scoped(&id,json!({"type":"protocol","message":message})) { break; }
            } else {
                outbox.scoped(&id,json!({"type":"error","message":"Home connection closed; reconnect from its session list"})); break;
            }
        }
    }
    client.shutdown().await;
    disconnected(&outbox, &id);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn output() -> Outbox {
        Outbox {
            queue: Mutex::new(VecDeque::new()),
            cancel: watch::channel(false).0,
            overflow: watch::channel(false).0,
        }
    }
    fn chunk(attachment: &str, bytes: Vec<u8>) -> Value {
        json!({"type":"protocol","message":{"type":"output","attachment_id":attachment,"data":bytes.into_iter().map(Value::from).collect::<Vec<_>>()}})
    }
    #[test]
    fn adjacent_tiny_output_preserves_bytes_without_overflow() {
        let out = output();
        for byte in 0..=255 {
            assert!(out.scoped("home", chunk("attachment", vec![byte])));
        }
        let queue = out
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue[0]["message"]["data"],
            json!((0..=255).collect::<Vec<u8>>())
        );
        assert!(!*out.overflow.borrow());
    }
    #[test]
    fn output_coalescing_stops_at_data_limit() {
        let out = output();
        assert!(out.scoped("home", chunk("attachment", vec![1; 16_383])));
        assert!(out.scoped("home", chunk("attachment", vec![2])));
        assert!(out.scoped("home", chunk("attachment", vec![3])));
        let queue = out
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue[0]["message"]["data"].as_array().map(Vec::len),
            Some(16_384)
        );
        assert_eq!(queue[0]["message"]["data"][16_383], 2);
        assert_eq!(queue[1]["message"]["data"], json!([3]));
    }
    #[test]
    fn other_streams_and_control_events_are_coalescing_barriers() {
        for (home, barrier) in [
            ("other", chunk("attachment", vec![2])),
            ("home", chunk("other-attachment", vec![2])),
            (
                "home",
                json!({"type":"protocol","message":{"type":"ownership"}}),
            ),
            (
                "home",
                json!({"type":"protocol","message":{"type":"response"}}),
            ),
        ] {
            let out = output();
            assert!(out.scoped("home", chunk("attachment", vec![1])));
            assert!(out.scoped(home, barrier));
            assert!(out.scoped("home", chunk("attachment", vec![3])));
            let queue = out
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(queue.len(), 3);
            assert_eq!(queue[0]["message"]["data"], json!([1]));
            assert_eq!(queue[2]["message"]["data"], json!([3]));
        }
    }
    #[test]
    fn overflowing_home_preserves_other_homes_and_control_slots() {
        let out = output();
        out.emit(json!({"type":"identity","connected":true}));
        assert!(out.scoped(
            "other",
            json!({"type":"protocol","message":{"type":"output","data":[1]}})
        ));
        for _ in 0..16 {
            assert!(out.scoped("noisy", json!({"type":"protocol"})));
        }
        assert!(!out.scoped("noisy", json!({"type":"protocol"})));
        let queue = out
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(queue.len(), 4);
        assert_eq!(queue[0]["type"], "identity");
        assert_eq!(queue[1]["home_id"], "other");
        assert_eq!(queue[2]["type"], "error");
        assert_eq!(queue[3]["connected"], false);
        assert!(!*out.overflow.borrow());
    }
    #[test]
    fn total_scoped_budget_reserves_root_control_capacity() {
        let out = output();
        for home in 0..7 {
            for _ in 0..16 {
                assert!(out.scoped(&home.to_string(), json!({"type":"protocol"})));
            }
        }
        assert!(!out.scoped("eighth", json!({"type":"protocol"})));
        out.emit(json!({"type":"identity","connected":true}));
        assert_eq!(
            out.queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            115
        );
        assert!(!*out.overflow.borrow());
    }
    #[test]
    fn class_options_and_actions_reject_unknown_fields() {
        let options = json!({"install_dir":"/tmp/class","root_public_key":"root","travel_id":"pty-test","label":"Device · PTY","service_class":{"version":1,"approving_home_id":"issuer","application_protocol":"flowsplice.pty.v1","protocol":"tcp"}});
        assert!(serde_json::from_value::<ClassNativeOptions>(options.clone()).is_ok());
        let mut extra = options;
        extra["descriptor"] = json!({});
        assert!(serde_json::from_value::<ClassNativeOptions>(extra).is_err());
        assert!(
            serde_json::from_value::<Action>(
                json!({"op":"connect_home","home_id":"[\"home\",\"pty\"]"})
            )
            .is_ok()
        );
        assert!(
            serde_json::from_value::<Action>(
                json!({"op":"connect_home","home_id":"home","password":"secret"})
            )
            .is_err()
        );
    }
    #[tokio::test]
    async fn missing_class_config_reports_identity_unavailable_and_allows_retry() -> Result<()> {
        let install_dir =
            std::env::temp_dir().join(format!("pty-missing-class-{}", uuid::Uuid::new_v4()));
        assert!(!install_dir.exists());
        let options = serde_json::from_value::<ClassNativeOptions>(json!({
            "install_dir": install_dir,
            "root_public_key": "dummy-root-not-used-with-missing-config",
            "travel_id": "pty-test",
            "label": "Test PTY",
            "service_class": {"version":1,"approving_home_id":"issuer","application_protocol":"flowsplice.pty.v1","protocol":"tcp"}
        }))?;
        let session = NativeSession::open_class(options)?;
        for password in ["", "manual-retry"] {
            session.send(Action::Connect {
                password: password.to_owned(),
            })?;
            let events = tokio::time::timeout(Duration::from_secs(5), async {
                let mut events = Vec::new();
                loop {
                    events.extend(session.poll());
                    if let Some(error) = events.iter().position(|event| event["type"] == "error")
                        && events[error + 1..]
                            .iter()
                            .any(|event| event["type"] == "identity" && event["busy"] == false)
                    {
                        break events;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await?;
            let errors: Vec<_> = events
                .iter()
                .filter(|event| event["type"] == "error")
                .collect();
            assert_eq!(errors.len(), 1, "{events:?}");
            assert_eq!(errors[0]["code"], "identity_unavailable");
            assert!(
                errors[0]["message"]
                    .as_str()
                    .is_some_and(|message| !message.is_empty())
            );
            let identity = events
                .iter()
                .rev()
                .find(|event| event["type"] == "identity");
            assert!(
                identity.is_some_and(|event| event["connected"] == false && event["busy"] == false)
            );
            assert!(
                events
                    .iter()
                    .any(|event| event["type"] == "identity" && event["busy"] == true)
            );
        }
        session.shutdown().await;
        assert!(!install_dir.exists());
        Ok(())
    }
}
