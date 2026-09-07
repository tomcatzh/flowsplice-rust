// Stable tracing targets preserve pre-extraction deployment log filters.
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use crate::tasks::{AbortOnDrop, Tasks};
use anyhow::{Context, Result, anyhow, bail};
use bytes::Bytes;
use flowsplice_core::authorization::TravelCredential;
use flowsplice_core::{
    DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    authorization::{VerifiedAuthorization, unix_time_secs},
    frame::{DataFrameCodec, DataFrameReader, write_data_frame, write_json},
    protocol::{DataFrame, Service},
};
use flowsplice_storage::LocalStatistics;
use flowsplice_transport::{ServicePeer, ServiceProvider};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    time::{Instant, interval, sleep_until, timeout},
};
use tokio_rustls::server::TlsStream;
use tracing::{debug, info, warn};
use uuid::Uuid;

#[allow(clippy::struct_field_names)]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct FlowKey {
    credential_id: Uuid,
    travel_id: String,
    flow_id: Uuid,
}

struct FlowHandle {
    instance_id: Uuid,
    service_id: String,
    tx: mpsc::Sender<IncomingCarrier>,
    shutdown: watch::Sender<bool>,
    carrier_slots: Arc<Semaphore>,
}

/// One authenticated encrypted Carrier arriving for a TCP flow.
pub struct IncomingCarrier {
    pub carrier_id: Uuid,
    pub relay_id: String,
    pub stream: TlsStream<TcpStream>,
    pub global_permit: Option<OwnedSemaphorePermit>,
    pub flow_permit: Option<OwnedSemaphorePermit>,
    pub data_codec: DataFrameCodec,
}

/// Active TCP flows and their reliability, capacity and authorization state.
pub struct TcpFlowRegistry {
    pub(crate) tasks: Tasks,
    flows: Mutex<HashMap<FlowKey, FlowHandle>>,
    permits: Arc<Semaphore>,
    heartbeat: Duration,
    carrier_timeout: Duration,
    detach_timeout: Duration,
    max_unacked_bytes: usize,
    carrier_permits: Arc<Semaphore>,
    max_carriers_per_flow: usize,
    statistics: Arc<LocalStatistics>,
    provider: Arc<dyn ServiceProvider>,
}

impl TcpFlowRegistry {
    /// Construct a registry using a business service provider and existing runtime limits.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        permits: Arc<Semaphore>,
        heartbeat: Duration,
        carrier_timeout: Duration,
        detach_timeout: Duration,
        max_unacked_bytes: usize,
        max_active_carriers: usize,
        max_carriers_per_flow: usize,
        statistics: Arc<LocalStatistics>,
        provider: Arc<dyn ServiceProvider>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tasks: Tasks::default(),
            flows: Mutex::new(HashMap::new()),
            permits,
            heartbeat,
            carrier_timeout,
            detach_timeout,
            max_unacked_bytes,
            carrier_permits: Arc::new(Semaphore::new(max_active_carriers)),
            max_carriers_per_flow,
            statistics,
            provider,
        })
    }

    /// Return the shared statistics recorder.
    #[must_use]
    pub fn statistics(&self) -> Arc<LocalStatistics> {
        Arc::clone(&self.statistics)
    }

    /// Return the business provider shared by TCP and UDP flows.
    #[must_use]
    pub fn provider(&self) -> Arc<dyn ServiceProvider> {
        Arc::clone(&self.provider)
    }

    /// Attach a verified Travel carrier to its service flow.
    ///
    /// # Errors
    /// Returns capacity, mismatched service or closed flow errors.
    pub async fn attach(
        self: &Arc<Self>,
        credential: TravelCredential,
        travel_id: String,
        flow_id: Uuid,
        service: Service,
        mut carrier: IncomingCarrier,
        not_after_unix_secs: u64,
    ) -> Result<()> {
        let key = FlowKey {
            credential_id: credential.credential_id,
            travel_id,
            flow_id,
        };
        let (tx, carrier_slots) = {
            let mut flows = self.flows.lock().await;
            if let Some(existing) = flows.get(&key) {
                if existing.service_id != service.id {
                    bail!("flow attempted to change services");
                }
                (existing.tx.clone(), Arc::clone(&existing.carrier_slots))
            } else {
                let permit = Arc::clone(&self.permits)
                    .try_acquire_owned()
                    .map_err(|_| anyhow!("home active-flow limit reached"))?;
                let (tx, rx) = mpsc::channel(16);
                let (shutdown, shutdown_rx) = watch::channel(false);
                let instance_id = Uuid::new_v4();
                let carrier_slots = Arc::new(Semaphore::new(self.max_carriers_per_flow));
                flows.insert(
                    key.clone(),
                    FlowHandle {
                        instance_id,
                        service_id: service.id.clone(),
                        tx: tx.clone(),
                        shutdown,
                        carrier_slots: Arc::clone(&carrier_slots),
                    },
                );
                let registry = Arc::clone(self);
                let spawned = self.tasks.spawn(async move {
                    let outcome_key = key.clone();
                    let outcome_service = service.clone();
                    let result = run_flow(
                        Arc::clone(&registry),
                        key.clone(),
                        instance_id,
                        service,
                        credential,
                        rx,
                        shutdown_rx,
                        not_after_unix_secs,
                        permit,
                    )
                    .await;
                    if let Err(error) = &result {
                        let family = if error.to_string().contains("target") {
                            "target_failure"
                        } else {
                            "home_flow_failed"
                        };
                        record_home_tcp_metric(
                            &registry,
                            &outcome_key,
                            &outcome_service,
                            None,
                            family,
                            1,
                        );
                        warn!(target: "flowsplice_homeagent::tcp_flow", travel_id = %key.travel_id, flow_id = %key.flow_id, %error, "home TCP flow ended");
                    } else {
                        record_home_tcp_metric(
                            &registry,
                            &outcome_key,
                            &outcome_service,
                            None,
                            "home_flow_completed",
                            1,
                        );
                    }
                    registry.remove(&key, instance_id).await;
                });
                if let Err(error) = spawned {
                    flows.retain(|_, flow| flow.instance_id != instance_id);
                    return Err(error);
                }
                (tx, carrier_slots)
            }
        };
        carrier.global_permit = Some(
            Arc::clone(&self.carrier_permits)
                .try_acquire_owned()
                .map_err(|_| anyhow!("home active-Carrier limit reached"))?,
        );
        carrier.flow_permit = Some(
            carrier_slots
                .try_acquire_owned()
                .map_err(|_| anyhow!("home per-flow Carrier limit reached"))?,
        );
        tx.send(carrier)
            .await
            .map_err(|_| anyhow!("home flow closed while attaching carrier"))
    }

    async fn remove(&self, key: &FlowKey, instance_id: Uuid) {
        let mut flows = self.flows.lock().await;
        if flows
            .get(key)
            .is_some_and(|flow| flow.instance_id == instance_id)
        {
            flows.remove(key);
        }
    }

    /// Signal termination for every flow whose credential is no longer authorized.
    pub async fn revoke_inactive(&self, authorization: &VerifiedAuthorization, unix_secs: u64) {
        let flows = self.flows.lock().await;
        for (key, flow) in flows.iter() {
            if !authorization.is_active(key.credential_id, unix_secs) {
                let _ = flow.shutdown.send(true);
                info!(target: "flowsplice_homeagent::tcp_flow", event = "revoked_flow_closed", credential_id = %key.credential_id, travel_id = %key.travel_id, flow_id = %key.flow_id, "Home closed TCP flow for inactive Travel credential");
            }
        }
    }

    pub(crate) async fn shutdown(&self) {
        self.tasks.finish().await;
        self.flows.lock().await.clear();
    }
}

struct Segment {
    offset: u64,
    bytes: Bytes,
    _credit: OwnedSemaphorePermit,
}

enum FlowEvent {
    CarrierFrame {
        carrier_id: Uuid,
        frame: DataFrame,
    },
    CarrierClosed {
        carrier_id: Uuid,
        reason: String,
    },
    TargetData {
        bytes: Bytes,
        credit: OwnedSemaphorePermit,
    },
    TargetEof,
    TargetError(String),
}

type CarrierTasks = HashMap<Uuid, (AbortOnDrop, oneshot::Receiver<()>)>;

async fn drain_carriers(tasks: CarrierTasks, events: &mut mpsc::Receiver<FlowEvent>) {
    let draining = async move {
        for (_, (task, finished)) in tasks {
            let _task = task;
            let _ = finished.await;
        }
    };
    tokio::pin!(draining);
    loop {
        tokio::select! {
            biased;
            () = &mut draining => break,
            // Readers must not block on the bounded event queue while their
            // writers finish already-queued acknowledgements.
            Some(_) = events.recv() => {}
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_flow(
    registry: Arc<TcpFlowRegistry>,
    key: FlowKey,
    _instance_id: Uuid,
    service: Service,
    credential: TravelCredential,
    mut incoming: mpsc::Receiver<IncomingCarrier>,
    mut shutdown_rx: watch::Receiver<bool>,
    not_after_unix_secs: u64,
    _permit: OwnedSemaphorePermit,
) -> Result<()> {
    let (lifetime, _lifetime_guard) =
        flowsplice_transport::ServiceLifetime::new(not_after_unix_secs);
    let authorization_ended = async move {
        loop {
            if *shutdown_rx.borrow_and_update() {
                bail!("Travel credential was revoked");
            }
            shutdown_rx
                .changed()
                .await
                .map_err(|_| anyhow!("Travel authorization publisher closed"))?;
        }
    };
    let operation = async move {
        let target_started = Instant::now();
        let target = timeout(
            registry.carrier_timeout,
            registry.provider.connect_tcp(
                &service,
                ServicePeer {
                    credential,
                    travel_id: key.travel_id.clone(),
                    flow_id: key.flow_id,
                    lifetime,
                },
            ),
        )
        .await
        .context("target TCP connection timed out")?
        .with_context(|| format!("failed to connect service {}", service.id))?;
        let target_latency_ms =
            u64::try_from(target_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        record_home_tcp_metric_sample(
            &registry,
            &key,
            &service,
            None,
            "target_connection_latency_ms",
            target_latency_ms,
            Some(target_latency_ms),
        );
        record_home_tcp_metric(&registry, &key, &service, None, "home_flow_accepted", 1);
        let (mut target_reader, mut target_writer) = tokio::io::split(target);
        let (event_tx, mut events) = mpsc::channel::<FlowEvent>(32);
        let send_credit = Arc::new(Semaphore::new(registry.max_unacked_bytes));
        let target_events = event_tx.clone();
        let target_reader_task = registry.tasks.spawn(async move {
            let mut buffer = vec![0_u8; MAX_DATA_PAYLOAD];
            loop {
                match target_reader.read(&mut buffer).await {
                    Ok(0) => {
                        let _ = target_events.send(FlowEvent::TargetEof).await;
                        return;
                    }
                    Ok(count) => {
                        let Ok(count_u32) = u32::try_from(count) else {
                            let _ = target_events
                                .send(FlowEvent::TargetError(
                                    "target read size overflow".to_owned(),
                                ))
                                .await;
                            return;
                        };
                        let Ok(credit) =
                            Arc::clone(&send_credit).acquire_many_owned(count_u32).await
                        else {
                            return;
                        };
                        if target_events
                            .send(FlowEvent::TargetData {
                                bytes: Bytes::copy_from_slice(&buffer[..count]),
                                credit,
                            })
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = target_events
                            .send(FlowEvent::TargetError(error.to_string()))
                            .await;
                        return;
                    }
                }
            }
        })?;
        let _target_reader_task = AbortOnDrop(target_reader_task);

        let mut carriers: HashMap<Uuid, mpsc::Sender<DataFrame>> = HashMap::new();
        let mut carrier_tasks = CarrierTasks::new();
        let mut carrier_relays = HashMap::<Uuid, String>::new();
        let mut active_carrier = None;
        let mut receive_offset = 0_u64;
        let mut send_offset = 0_u64;
        let mut send_acked = 0_u64;
        let mut unacked = VecDeque::<Segment>::new();
        let mut unacked_bytes = 0_usize;
        let mut race_winners = HashMap::<Uuid, Uuid>::new();
        let mut race_order = VecDeque::<Uuid>::new();
        let mut detached_deadline = Some(Instant::now() + registry.detach_timeout);
        let mut target_eof = false;
        let mut target_fin_acked = false;
        let mut travel_eof = false;
        let mut selected_once = false;

        info!(target: "flowsplice_homeagent::tcp_flow", travel_id = %key.travel_id, flow_id = %key.flow_id, service_id = %service.id, "home TCP flow created");
        loop {
            if travel_eof && target_eof && target_fin_acked && unacked.is_empty() {
                // Final FIN acknowledgements may still be queued in a Carrier. A normal
                // close drains them; cancellation/error drops the abort guards immediately.
                drop(carriers);
                timeout(
                    registry.carrier_timeout,
                    drain_carriers(carrier_tasks, &mut events),
                )
                .await
                .context("Carrier final acknowledgement drain timed out")?;
                return Ok(());
            }
            tokio::select! {
                carrier = incoming.recv() => {
                    let Some(mut carrier) = carrier else { bail!("flow carrier channel closed"); };
                    if carriers.contains_key(&carrier.carrier_id) {
                        continue;
                    }
                    carrier_relays.insert(carrier.carrier_id, carrier.relay_id.clone());
                    write_json(
                        &mut carrier.stream,
                        &DataFrame::OpenOk {
                            flow_id: key.flow_id,
                            carrier_id: carrier.carrier_id,
                            receive_offset,
                            send_offset: send_acked,
                            data_protocol_version: carrier.data_codec.version(),
                        },
                        DATA_FRAME_LIMIT,
                    )
                    .await?;
                    info!(target: "flowsplice_homeagent::tcp_flow",
                        event = "carrier_attached",
                        travel_id = %key.travel_id,
                        flow_id = %key.flow_id,
                        carrier_id = %carrier.carrier_id,
                        receive_offset,
                        send_acked,
                        "home attached a carrier to the TCP flow"
                    );
                    let (tx, rx) = mpsc::channel(128);
                    carriers.insert(carrier.carrier_id, tx);
                    let carrier_id = carrier.carrier_id;
                    let (completed, finished) = oneshot::channel();
                    let running = run_carrier(key.flow_id, carrier, rx, event_tx.clone(), registry.heartbeat, registry.carrier_timeout);
                    let task = registry.tasks.spawn(async move {
                        running.await;
                        let _ = completed.send(());
                    })?;
                    carrier_tasks.insert(carrier_id, (AbortOnDrop(task), finished));
                }
                event = events.recv() => {
                    let Some(event) = event else { bail!("flow event channel closed"); };
                    match event {
                        FlowEvent::CarrierFrame { carrier_id, frame } => {
                            match frame {
                                DataFrame::Race { flow_id, race_id, next_offset }
                                    if flow_id == key.flow_id && next_offset <= receive_offset =>
                                {
                                    if let Some(winner) = race_winners.get(&race_id).copied() {
                                        debug!(target: "flowsplice_homeagent::tcp_flow", event = "carrier_race_duplicate", travel_id = %key.travel_id, flow_id = %key.flow_id, %race_id, %carrier_id, winner_carrier_id = %winner, "home rejected a slower carrier candidate");
                                        send_to(
                                            &carriers,
                                            carrier_id,
                                            DataFrame::RaceDuplicate {
                                                flow_id: key.flow_id,
                                                race_id,
                                                winner_carrier_id: winner,
                                            },
                                        )
                                        .await;
                                    } else {
                                        let resumed = selected_once && active_carrier.is_none();
                                        race_winners.insert(race_id, carrier_id);
                                        race_order.push_back(race_id);
                                        while race_order.len() > 32 {
                                            if let Some(old) = race_order.pop_front() {
                                                race_winners.remove(&old);
                                            }
                                        }
                                        active_carrier = Some(carrier_id);
                                        selected_once = true;
                                        detached_deadline = None;
                                        send_to(
                                            &carriers,
                                            carrier_id,
                                            DataFrame::RaceAck {
                                                flow_id: key.flow_id,
                                                race_id,
                                                winner_carrier_id: carrier_id,
                                            },
                                        )
                                        .await;
                                        retransmit(
                                            &carriers,
                                            carrier_id,
                                            key.flow_id,
                                            &unacked,
                                            target_eof,
                                            send_offset,
                                        )
                                        .await;
                                        info!(target: "flowsplice_homeagent::tcp_flow", event = "carrier_selected", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, %race_id, resumed, receive_offset, send_acked, "home selected carrier");
                                    }
                                }
                                DataFrame::Data { flow_id, offset, bytes }
                                    if flow_id == key.flow_id && bytes.len() <= MAX_DATA_PAYLOAD =>
                                {
                                    let end = offset.saturating_add(bytes.len() as u64);
                                    if offset == receive_offset && active_carrier == Some(carrier_id) {
                                        timeout(
                                            registry.carrier_timeout,
                                            target_writer.write_all(&bytes),
                                        )
                                        .await
                                        .context("target TCP write timed out")??;
                                        receive_offset = end;
                                        record_home_tcp_metric(
                                            &registry,
                                            &key,
                                            &service,
                                            carrier_relays.get(&carrier_id).map(String::as_str),
                                            "delivered_upload_bytes",
                                            bytes.len() as u64,
                                        );
                                        debug!(target: "flowsplice_homeagent::tcp_flow", event = "tcp_data_accepted", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, offset, next_offset = receive_offset, bytes = bytes.len(), "accepted Travel-to-Home TCP data");
                                        send_to(
                                            &carriers,
                                            carrier_id,
                                            DataFrame::Ack { flow_id: key.flow_id, next_offset: receive_offset },
                                        )
                                        .await;
                                    } else if end <= receive_offset {
                                        debug!(target: "flowsplice_homeagent::tcp_flow", event = "tcp_data_duplicate", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, offset, next_offset = receive_offset, bytes = bytes.len(), "received duplicate Travel-to-Home TCP data");
                                        send_to(
                                            &carriers,
                                            carrier_id,
                                            DataFrame::Duplicate {
                                                flow_id: key.flow_id,
                                                next_offset: receive_offset,
                                                winner_carrier_id: active_carrier.unwrap_or(carrier_id),
                                            },
                                        )
                                        .await;
                                    } else {
                                        send_to(
                                            &carriers,
                                            carrier_id,
                                            DataFrame::Close {
                                                flow_id: key.flow_id,
                                                reason: "non-contiguous TCP data".to_owned(),
                                            },
                                        )
                                        .await;
                                    }
                                }
                                DataFrame::Ack { flow_id, next_offset }
                                    if flow_id == key.flow_id && next_offset >= send_acked && next_offset <= send_offset =>
                                {
                                    acknowledge(&mut unacked, &mut unacked_bytes, &mut send_acked, next_offset)?;
                                    debug!(target: "flowsplice_homeagent::tcp_flow", event = "tcp_ack_received", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, next_offset, unacked_bytes, "received ACK for Home-to-Travel TCP data");
                                }
                                DataFrame::Duplicate { flow_id, next_offset, .. }
                                    if flow_id == key.flow_id && next_offset >= send_acked && next_offset <= send_offset =>
                                {
                                    acknowledge(&mut unacked, &mut unacked_bytes, &mut send_acked, next_offset)?;
                                    debug!(target: "flowsplice_homeagent::tcp_flow", event = "tcp_duplicate_received", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, next_offset, unacked_bytes, "received DUP for Home-to-Travel TCP data");
                                }
                                DataFrame::Fin { flow_id, final_offset }
                                    if flow_id == key.flow_id && final_offset == receive_offset =>
                                {
                                    if !travel_eof {
                                        timeout(registry.carrier_timeout, target_writer.shutdown())
                                            .await
                                            .context("target TCP shutdown timed out")??;
                                        travel_eof = true;
                                    }
                                    send_to(
                                        &carriers,
                                        carrier_id,
                                        DataFrame::FinAck { flow_id: key.flow_id, final_offset },
                                    )
                                    .await;
                                }
                                DataFrame::FinAck { flow_id, final_offset }
                                    if flow_id == key.flow_id && target_eof && final_offset == send_offset =>
                                {
                                    target_fin_acked = true;
                                }
                                DataFrame::Close { flow_id, reason } if flow_id == key.flow_id => {
                                    bail!("travel closed flow: {reason}");
                                }
                                _ => {
                                    send_to(
                                        &carriers,
                                        carrier_id,
                                        DataFrame::Close {
                                            flow_id: key.flow_id,
                                            reason: "invalid TCP flow frame".to_owned(),
                                        },
                                    )
                                    .await;
                                }
                            }
                        }
                        FlowEvent::CarrierClosed { carrier_id, reason } => {
                            carriers.remove(&carrier_id);
                            carrier_tasks.remove(&carrier_id);
                            carrier_relays.remove(&carrier_id);
                            if active_carrier == Some(carrier_id) {
                                active_carrier = None;
                                detached_deadline = Some(Instant::now() + registry.detach_timeout);
                                warn!(target: "flowsplice_homeagent::tcp_flow", event = "flow_detached", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, %reason, detach_timeout_ms = registry.detach_timeout.as_millis(), "home flow detached from carrier and retained target TCP");
                            } else {
                                debug!(target: "flowsplice_homeagent::tcp_flow", event = "carrier_closed_inactive", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, %reason, "inactive home carrier closed");
                            }
                        }
                        FlowEvent::TargetData { bytes, credit } => {
                            if unacked_bytes.saturating_add(bytes.len()) > registry.max_unacked_bytes {
                                bail!("home unacknowledged-data limit reached");
                            }
                            let segment = Segment {
                                offset: send_offset,
                                bytes,
                                _credit: credit,
                            };
                            record_home_tcp_metric(
                                &registry,
                                &key,
                                &service,
                                active_carrier.and_then(|carrier_id| carrier_relays.get(&carrier_id).map(String::as_str)),
                                "home_flow_download_observed_bytes",
                                segment.bytes.len() as u64,
                            );
                            send_offset = send_offset.saturating_add(segment.bytes.len() as u64);
                            unacked_bytes += segment.bytes.len();
                            debug!(target: "flowsplice_homeagent::tcp_flow", event = "tcp_data_buffered", travel_id = %key.travel_id, flow_id = %key.flow_id, offset = segment.offset, bytes = segment.bytes.len(), unacked_bytes, "buffered Home-to-Travel TCP data");
                            if let Some(carrier_id) = active_carrier {
                                send_to(
                                    &carriers,
                                    carrier_id,
                                    DataFrame::Data {
                                        flow_id: key.flow_id,
                                        offset: segment.offset,
                                        bytes: segment.bytes.clone(),
                                    },
                                )
                                .await;
                            }
                            unacked.push_back(segment);
                        }
                        FlowEvent::TargetEof => {
                            target_eof = true;
                            if let Some(carrier_id) = active_carrier {
                                send_to(
                                    &carriers,
                                    carrier_id,
                                    DataFrame::Fin { flow_id: key.flow_id, final_offset: send_offset },
                                )
                                .await;
                            }
                        }
                        FlowEvent::TargetError(reason) => bail!("target TCP failed: {reason}"),
                    }
                }
                () = wait_for_deadline(detached_deadline), if detached_deadline.is_some() => {
                    bail!("home detached-flow timeout expired");
                }
            }
        }
    };
    crate::lifetime::guard(operation, authorization_ended, not_after_unix_secs).await
}

fn record_home_tcp_metric(
    registry: &TcpFlowRegistry,
    key: &FlowKey,
    service: &Service,
    relay_id: Option<&str>,
    family: &str,
    value: u64,
) {
    record_home_tcp_metric_sample(registry, key, service, relay_id, family, value, None);
}

fn record_home_tcp_metric_sample(
    registry: &TcpFlowRegistry,
    key: &FlowKey,
    service: &Service,
    relay_id: Option<&str>,
    family: &str,
    value: u64,
    histogram_sample: Option<u64>,
) {
    let mut dimensions = BTreeMap::new();
    dimensions.insert("travel_id".to_owned(), key.travel_id.clone());
    dimensions.insert("service_id".to_owned(), service.id.clone());
    dimensions.insert("protocol".to_owned(), "tcp".to_owned());
    if let Some(relay_id) = relay_id {
        dimensions.insert("relay_id".to_owned(), relay_id.to_owned());
    }
    if let Ok(now) = unix_time_secs() {
        registry
            .statistics
            .record(now, family, dimensions, value, histogram_sample);
    }
}

async fn run_carrier(
    flow_id: Uuid,
    carrier: IncomingCarrier,
    mut outgoing: mpsc::Receiver<DataFrame>,
    events: mpsc::Sender<FlowEvent>,
    heartbeat_period: Duration,
    timeout_period: Duration,
) {
    let IncomingCarrier {
        carrier_id,
        stream,
        data_codec,
        global_permit,
        flow_permit,
        ..
    } = carrier;
    let _carrier_permits = (global_permit, flow_permit);
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = DataFrameReader::new(reader, DATA_FRAME_LIMIT, data_codec);
    let mut heartbeat = interval(heartbeat_period);
    let mut nonce = 0_u64;
    let mut last_received = Instant::now();
    let result: Result<()> = async {
            loop {
                tokio::select! {
                    frame = outgoing.recv() => {
                        let Some(frame) = frame else { return Ok(()); };
                        write_data_frame(&mut writer, &frame, DATA_FRAME_LIMIT, data_codec).await?;
                    }
                    frame = reader.read() => {
                        last_received = Instant::now();
                        match frame? {
                            DataFrame::Ping { nonce } => {
                                write_data_frame(&mut writer, &DataFrame::Pong { nonce }, DATA_FRAME_LIMIT, data_codec).await?;
                            }
                            DataFrame::Pong { .. } => {}
                            frame => {
                                events.send(FlowEvent::CarrierFrame { carrier_id, frame }).await
                                    .map_err(|_| anyhow!("home flow event receiver closed"))?;
                            }
                        }
                    }
                    _ = heartbeat.tick() => {
                        if last_received.elapsed() > timeout_period {
                            bail!("carrier heartbeat timed out");
                        }
                        nonce = nonce.wrapping_add(1);
                        write_data_frame(&mut writer, &DataFrame::Ping { nonce }, DATA_FRAME_LIMIT, data_codec).await?;
                    }
                }
            }
        }
        .await;
    let reason = result
        .err()
        .map_or_else(|| "carrier closed".to_owned(), |error| error.to_string());
    let _ = events
        .send(FlowEvent::CarrierClosed { carrier_id, reason })
        .await;
    info!(target: "flowsplice_homeagent::tcp_flow", %flow_id, %carrier_id, "home carrier stopped");
}

async fn send_to(
    carriers: &HashMap<Uuid, mpsc::Sender<DataFrame>>,
    carrier_id: Uuid,
    frame: DataFrame,
) {
    if let Some(tx) = carriers.get(&carrier_id) {
        let _ = tx.send(frame).await;
    }
}

async fn retransmit(
    carriers: &HashMap<Uuid, mpsc::Sender<DataFrame>>,
    carrier_id: Uuid,
    flow_id: Uuid,
    unacked: &VecDeque<Segment>,
    target_eof: bool,
    send_offset: u64,
) {
    let unacked_bytes: usize = unacked.iter().map(|segment| segment.bytes.len()).sum();
    debug!(target: "flowsplice_homeagent::tcp_flow",
        event = "tcp_retransmit",
        %flow_id,
        %carrier_id,
        segment_count = unacked.len(),
        unacked_bytes,
        "retransmitting unacknowledged Home-to-Travel TCP data"
    );
    for segment in unacked {
        send_to(
            carriers,
            carrier_id,
            DataFrame::Data {
                flow_id,
                offset: segment.offset,
                bytes: segment.bytes.clone(),
            },
        )
        .await;
    }
    if target_eof {
        send_to(
            carriers,
            carrier_id,
            DataFrame::Fin {
                flow_id,
                final_offset: send_offset,
            },
        )
        .await;
    }
}

fn acknowledge(
    unacked: &mut VecDeque<Segment>,
    unacked_bytes: &mut usize,
    send_acked: &mut u64,
    next_offset: u64,
) -> Result<()> {
    while let Some(segment) = unacked.front() {
        let end = segment.offset.saturating_add(segment.bytes.len() as u64);
        if end > next_offset {
            break;
        }
        let segment = unacked
            .pop_front()
            .ok_or_else(|| anyhow!("segment vanished"))?;
        *unacked_bytes = unacked_bytes.saturating_sub(segment.bytes.len());
    }
    if unacked
        .front()
        .is_some_and(|segment| segment.offset < next_offset)
    {
        bail!("ACK split a TCP data frame");
    }
    *send_acked = next_offset;
    Ok(())
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    if let Some(deadline) = deadline {
        sleep_until(deadline).await;
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;

    struct Released(Option<oneshot::Sender<()>>);
    impl Drop for Released {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn graceful_drain_relives_event_pressure_and_delivers_final_ack() -> Result<()> {
        let flow_id = Uuid::new_v4();
        let carrier_id = Uuid::new_v4();
        let codec = DataFrameCodec::negotiate(1);
        let (producer, mut outgoing) = mpsc::channel(1);
        producer
            .send(DataFrame::FinAck {
                flow_id,
                final_offset: 913,
            })
            .await?;
        let (event_sender, mut events) = mpsc::channel(1);
        event_sender.send(FlowEvent::TargetEof).await?;
        let (mut writer, reader) = tokio::io::duplex(1);
        let (finished, completion) = oneshot::channel();
        let (released, release) = oneshot::channel();
        let child = tokio::spawn(async move {
            let resource = Released(Some(released));
            // The event queue is initially full; only drain_carriers can release it.
            event_sender.send(FlowEvent::TargetEof).await?;
            while let Some(frame) = outgoing.recv().await {
                write_data_frame(&mut writer, &frame, DATA_FRAME_LIMIT, codec).await?;
            }
            writer.shutdown().await?;
            drop(resource);
            let _ = finished.send(());
            Ok::<_, anyhow::Error>(())
        });
        let tasks = HashMap::from([(carrier_id, (AbortOnDrop(child.abort_handle()), completion))]);
        drop(producer);
        let received = async {
            let mut reader = DataFrameReader::new(reader, DATA_FRAME_LIMIT, codec);
            let frame = reader.read().await?;
            assert!(
                matches!(frame, DataFrame::FinAck { flow_id: id, final_offset: 913 } if id == flow_id)
            );
            Ok::<_, anyhow::Error>(())
        };
        timeout(Duration::from_secs(1), async {
            let ((), frame) = tokio::join!(drain_carriers(tasks, &mut events), received);
            frame?;
            release.await?;
            child.await??;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    #[tokio::test]
    async fn cancelling_drain_aborts_blocked_writer_and_releases_resource() -> Result<()> {
        let codec = DataFrameCodec::negotiate(1);
        let (producer, mut outgoing) = mpsc::channel(1);
        producer
            .send(DataFrame::FinAck {
                flow_id: Uuid::new_v4(),
                final_offset: 19,
            })
            .await?;
        drop(producer);
        let (_event_sender, mut events) = mpsc::channel(1);
        let (mut writer, mut reader) = tokio::io::duplex(1);
        let (finished, completion) = oneshot::channel();
        let (released, release) = oneshot::channel();
        let child = tokio::spawn(async move {
            let _resource = Released(Some(released));
            while let Some(frame) = outgoing.recv().await {
                write_data_frame(&mut writer, &frame, DATA_FRAME_LIMIT, codec).await?;
            }
            let _ = finished.send(());
            Ok::<_, anyhow::Error>(())
        });
        timeout(Duration::from_secs(1), reader.read_exact(&mut [0])).await??;
        let tasks = HashMap::from([(
            Uuid::new_v4(),
            (AbortOnDrop(child.abort_handle()), completion),
        )]);
        let mut draining = Box::pin(drain_carriers(tasks, &mut events));
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(draining.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(draining);
        timeout(Duration::from_secs(1), release).await??;
        assert!(
            timeout(Duration::from_secs(1), child)
                .await?
                .is_err_and(|error| error.is_cancelled())
        );
        drop(reader);
        Ok(())
    }
}
