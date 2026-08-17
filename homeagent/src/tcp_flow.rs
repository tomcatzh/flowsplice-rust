use std::{
    collections::{HashMap, VecDeque},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use flowsplice_core::{
    DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    authorization::{VerifiedAuthorization, unix_time_secs},
    frame::{JsonFrameReader, write_json},
    protocol::{DataFrame, Service},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, watch},
    time::{Instant, interval, sleep_until},
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
}

pub struct IncomingCarrier {
    pub carrier_id: Uuid,
    pub stream: TlsStream<TcpStream>,
}

pub struct TcpFlowRegistry {
    flows: Mutex<HashMap<FlowKey, FlowHandle>>,
    permits: Arc<Semaphore>,
    heartbeat: Duration,
    carrier_timeout: Duration,
    detach_timeout: Duration,
    max_unacked_bytes: usize,
}

impl TcpFlowRegistry {
    pub fn new(
        permits: Arc<Semaphore>,
        heartbeat: Duration,
        carrier_timeout: Duration,
        detach_timeout: Duration,
        max_unacked_bytes: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            flows: Mutex::new(HashMap::new()),
            permits,
            heartbeat,
            carrier_timeout,
            detach_timeout,
            max_unacked_bytes,
        })
    }

    pub async fn attach(
        self: &Arc<Self>,
        credential_id: Uuid,
        travel_id: String,
        flow_id: Uuid,
        service: Service,
        carrier: IncomingCarrier,
        not_after_unix_secs: u64,
    ) -> Result<()> {
        let key = FlowKey {
            credential_id,
            travel_id,
            flow_id,
        };
        let tx = {
            let mut flows = self.flows.lock().await;
            if let Some(existing) = flows.get(&key) {
                if existing.service_id != service.id {
                    bail!("flow attempted to change services");
                }
                existing.tx.clone()
            } else {
                let permit = Arc::clone(&self.permits)
                    .try_acquire_owned()
                    .map_err(|_| anyhow!("home active-flow limit reached"))?;
                let (tx, rx) = mpsc::channel(16);
                let (shutdown, shutdown_rx) = watch::channel(false);
                let instance_id = Uuid::new_v4();
                flows.insert(
                    key.clone(),
                    FlowHandle {
                        instance_id,
                        service_id: service.id.clone(),
                        tx: tx.clone(),
                        shutdown,
                    },
                );
                let registry = Arc::clone(self);
                tokio::spawn(async move {
                    if let Err(error) = run_flow(
                        Arc::clone(&registry),
                        key.clone(),
                        instance_id,
                        service,
                        rx,
                        shutdown_rx,
                        not_after_unix_secs,
                        permit,
                    )
                    .await
                    {
                        warn!(travel_id = %key.travel_id, flow_id = %key.flow_id, %error, "home TCP flow ended");
                    }
                    registry.remove(&key, instance_id).await;
                });
                tx
            }
        };
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

    pub async fn revoke_inactive(&self, authorization: &VerifiedAuthorization, unix_secs: u64) {
        let flows = self.flows.lock().await;
        for (key, flow) in flows.iter() {
            if !authorization.is_active(key.credential_id, unix_secs) {
                let _ = flow.shutdown.send(true);
                info!(event = "revoked_flow_closed", credential_id = %key.credential_id, travel_id = %key.travel_id, flow_id = %key.flow_id, "Home closed TCP flow for inactive Travel credential");
            }
        }
    }
}

#[derive(Clone)]
struct Segment {
    offset: u64,
    bytes: Vec<u8>,
}

enum FlowEvent {
    CarrierFrame { carrier_id: Uuid, frame: DataFrame },
    CarrierClosed { carrier_id: Uuid, reason: String },
    TargetData(Vec<u8>),
    TargetEof,
    TargetError(String),
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_flow(
    registry: Arc<TcpFlowRegistry>,
    key: FlowKey,
    _instance_id: Uuid,
    service: Service,
    mut incoming: mpsc::Receiver<IncomingCarrier>,
    mut shutdown_rx: watch::Receiver<bool>,
    not_after_unix_secs: u64,
    _permit: OwnedSemaphorePermit,
) -> Result<()> {
    let target = TcpStream::connect(&service.target)
        .await
        .with_context(|| format!("failed to connect service {}", service.id))?;
    target.set_nodelay(true)?;
    let (mut target_reader, mut target_writer) = target.into_split();
    let (event_tx, mut events) = mpsc::channel::<FlowEvent>(256);
    let target_events = event_tx.clone();
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; MAX_DATA_PAYLOAD];
        loop {
            match target_reader.read(&mut buffer).await {
                Ok(0) => {
                    let _ = target_events.send(FlowEvent::TargetEof).await;
                    return;
                }
                Ok(count) => {
                    if target_events
                        .send(FlowEvent::TargetData(buffer[..count].to_vec()))
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
    });

    let mut carriers: HashMap<Uuid, mpsc::Sender<DataFrame>> = HashMap::new();
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

    info!(travel_id = %key.travel_id, flow_id = %key.flow_id, service_id = %service.id, "home TCP flow created");
    loop {
        if travel_eof && target_eof && target_fin_acked && unacked.is_empty() {
            return Ok(());
        }
        tokio::select! {
            carrier = incoming.recv() => {
                let Some(mut carrier) = carrier else { bail!("flow carrier channel closed"); };
                if carriers.contains_key(&carrier.carrier_id) {
                    continue;
                }
                write_json(
                    &mut carrier.stream,
                    &DataFrame::OpenOk {
                        flow_id: key.flow_id,
                        carrier_id: carrier.carrier_id,
                        receive_offset,
                        send_offset: send_acked,
                    },
                    DATA_FRAME_LIMIT,
                )
                .await?;
                info!(
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
                spawn_carrier(
                    key.flow_id,
                    carrier,
                    rx,
                    event_tx.clone(),
                    registry.heartbeat,
                    registry.carrier_timeout,
                );
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
                                    debug!(event = "carrier_race_duplicate", travel_id = %key.travel_id, flow_id = %key.flow_id, %race_id, %carrier_id, winner_carrier_id = %winner, "home rejected a slower carrier candidate");
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
                                    info!(event = "carrier_selected", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, %race_id, resumed, receive_offset, send_acked, "home selected carrier");
                                }
                            }
                            DataFrame::Data { flow_id, offset, bytes }
                                if flow_id == key.flow_id && bytes.len() <= MAX_DATA_PAYLOAD =>
                            {
                                let end = offset.saturating_add(bytes.len() as u64);
                                if offset == receive_offset && active_carrier == Some(carrier_id) {
                                    target_writer.write_all(&bytes).await?;
                                    receive_offset = end;
                                    debug!(event = "tcp_data_accepted", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, offset, next_offset = receive_offset, bytes = bytes.len(), "accepted Travel-to-Home TCP data");
                                    send_to(
                                        &carriers,
                                        carrier_id,
                                        DataFrame::Ack { flow_id: key.flow_id, next_offset: receive_offset },
                                    )
                                    .await;
                                } else if end <= receive_offset {
                                    debug!(event = "tcp_data_duplicate", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, offset, next_offset = receive_offset, bytes = bytes.len(), "received duplicate Travel-to-Home TCP data");
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
                                debug!(event = "tcp_ack_received", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, next_offset, unacked_bytes, "received ACK for Home-to-Travel TCP data");
                            }
                            DataFrame::Duplicate { flow_id, next_offset, .. }
                                if flow_id == key.flow_id && next_offset >= send_acked && next_offset <= send_offset =>
                            {
                                acknowledge(&mut unacked, &mut unacked_bytes, &mut send_acked, next_offset)?;
                                debug!(event = "tcp_duplicate_received", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, next_offset, unacked_bytes, "received DUP for Home-to-Travel TCP data");
                            }
                            DataFrame::Fin { flow_id, final_offset }
                                if flow_id == key.flow_id && final_offset == receive_offset =>
                            {
                                if !travel_eof {
                                    target_writer.shutdown().await?;
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
                        if active_carrier == Some(carrier_id) {
                            active_carrier = None;
                            detached_deadline = Some(Instant::now() + registry.detach_timeout);
                            warn!(event = "flow_detached", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, %reason, detach_timeout_ms = registry.detach_timeout.as_millis(), "home flow detached from carrier and retained target TCP");
                        } else {
                            debug!(event = "carrier_closed_inactive", travel_id = %key.travel_id, flow_id = %key.flow_id, %carrier_id, %reason, "inactive home carrier closed");
                        }
                    }
                    FlowEvent::TargetData(bytes) => {
                        if unacked_bytes.saturating_add(bytes.len()) > registry.max_unacked_bytes {
                            bail!("home unacknowledged-data limit reached");
                        }
                        let segment = Segment { offset: send_offset, bytes };
                        send_offset = send_offset.saturating_add(segment.bytes.len() as u64);
                        unacked_bytes += segment.bytes.len();
                        debug!(event = "tcp_data_buffered", travel_id = %key.travel_id, flow_id = %key.flow_id, offset = segment.offset, bytes = segment.bytes.len(), unacked_bytes, "buffered Home-to-Travel TCP data");
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
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    bail!("Travel credential was revoked");
                }
            }
            () = sleep_until_unix(not_after_unix_secs) => {
                bail!("Travel credential expired");
            }
        }
    }
}

fn spawn_carrier(
    flow_id: Uuid,
    carrier: IncomingCarrier,
    mut outgoing: mpsc::Receiver<DataFrame>,
    events: mpsc::Sender<FlowEvent>,
    heartbeat_period: Duration,
    timeout_period: Duration,
) {
    tokio::spawn(async move {
        let carrier_id = carrier.carrier_id;
        let (reader, mut writer) = tokio::io::split(carrier.stream);
        let mut reader = JsonFrameReader::new(reader, DATA_FRAME_LIMIT);
        let mut heartbeat = interval(heartbeat_period);
        let mut nonce = 0_u64;
        let mut last_received = Instant::now();
        let result: Result<()> = async {
            loop {
                tokio::select! {
                    frame = outgoing.recv() => {
                        let Some(frame) = frame else { return Ok(()); };
                        write_json(&mut writer, &frame, DATA_FRAME_LIMIT).await?;
                    }
                    frame = reader.read::<DataFrame>() => {
                        last_received = Instant::now();
                        match frame? {
                            DataFrame::Ping { nonce } => {
                                write_json(&mut writer, &DataFrame::Pong { nonce }, DATA_FRAME_LIMIT).await?;
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
                        write_json(&mut writer, &DataFrame::Ping { nonce }, DATA_FRAME_LIMIT).await?;
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
        info!(%flow_id, %carrier_id, "home carrier stopped");
    });
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
    debug!(
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

async fn sleep_until_unix(not_after_unix_secs: u64) {
    let now = unix_time_secs().unwrap_or(not_after_unix_secs);
    tokio::time::sleep(Duration::from_secs(not_after_unix_secs.saturating_sub(now))).await;
}
