use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use bytes::Bytes;
use flowsplice_core::{
    DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    authorization::unix_time_secs,
    frame::{DataFrameReader, write_data_frame},
    protocol::{DataFrame, ServiceProtocol},
};
use flowsplice_transport::BoxStream;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
    time::{Instant, interval, sleep, sleep_until, timeout},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{
    AppState, BusinessCarrier, Mapping, jittered_retry_delay, open_business_on, relay_candidates,
    uuid_seed,
};

struct Segment {
    offset: u64,
    bytes: Bytes,
    _credit: OwnedSemaphorePermit,
}

struct CarrierHandle {
    relay_id: String,
    tx: mpsc::Sender<DataFrame>,
    shutdown: watch::Sender<bool>,
}

enum FlowEvent {
    LocalData {
        bytes: Bytes,
        credit: OwnedSemaphorePermit,
    },
    LocalEof,
    LocalError(String),
    CarrierFrame {
        carrier_id: Uuid,
        frame: DataFrame,
    },
    CarrierClosed {
        carrier_id: Uuid,
        reason: String,
    },
}

type OpenResult = (
    String,
    OwnedSemaphorePermit,
    Result<BusinessCarrier>,
    Duration,
);

struct TransferState {
    flow_id: Uuid,
    local_writer: WriteHalf<BoxStream>,
    send_offset: u64,
    send_acked: u64,
    receive_offset: u64,
    unacked: VecDeque<Segment>,
    unacked_bytes: usize,
    max_unacked_bytes: usize,
    local_eof: bool,
    local_fin_acked: bool,
    remote_eof: bool,
    io_timeout: Duration,
    uploaded_total: Arc<std::sync::atomic::AtomicU64>,
    downloaded_total: Arc<std::sync::atomic::AtomicU64>,
}

pub async fn run(
    state: AppState,
    mapping: Mapping,
    local: TcpStream,
    shutdown: watch::Receiver<bool>,
) -> Result<()> {
    local.set_nodelay(true)?;
    run_io(state, mapping, Box::new(local), shutdown, None, None).await
}

#[allow(clippy::too_many_lines)]
pub(super) async fn run_io(
    state: AppState,
    mapping: Mapping,
    local: BoxStream,
    mut shutdown: watch::Receiver<bool>,
    mut connected: Option<oneshot::Sender<std::result::Result<(), String>>>,
    failure: Option<watch::Sender<Option<String>>>,
) -> Result<()> {
    let flow_id = Uuid::new_v4();
    state.begin_route_flow(flow_id, &mapping).await;
    info!(
        event = "tcp_flow_started",
        %flow_id,
        home_id = %mapping.home_id,
        service_id = %mapping.service_id,
        "travel TCP flow started"
    );
    let mut io_tasks = JoinSet::new();
    let mut opens = JoinSet::new();
    let (local_reader, local_writer) = tokio::io::split(local);
    let (events_tx, mut events) = mpsc::channel(32);
    let send_credit = Arc::new(Semaphore::new(state.config.max_unacked_bytes));
    spawn_local_reader(&mut io_tasks, local_reader, events_tx.clone(), send_credit);

    let mut transfer = TransferState {
        flow_id,
        local_writer,
        send_offset: 0,
        send_acked: 0,
        receive_offset: 0,
        unacked: VecDeque::new(),
        unacked_bytes: 0,
        max_unacked_bytes: state.config.max_unacked_bytes,
        local_eof: false,
        local_fin_acked: false,
        remote_eof: false,
        io_timeout: Duration::from_secs(state.config.carrier_timeout_secs),
        uploaded_total: Arc::clone(&state.uploaded_bytes),
        downloaded_total: Arc::clone(&state.downloaded_bytes),
    };
    let result = if *shutdown.borrow() {
        Err(anyhow!("mapping stopped"))
    } else {
        tokio::select! {
            biased;
            _ = shutdown.changed() => Err(anyhow!("mapping stopped")),
            result = run_inner(&state, &mapping, flow_id, &events_tx, &mut events, &mut transfer, &mut io_tasks, &mut opens, &mut connected) => result,
        }
    };
    if let (Err(error), Some(failure)) = (&result, failure) {
        failure.send_replace(Some(error.to_string()));
    }
    if let Some(connected) = connected.take() {
        let reason = result.as_ref().err().map_or_else(
            || "flow closed before a Carrier was selected".to_owned(),
            ToString::to_string,
        );
        let _ = connected.send(Err(reason));
    }
    // Own and drain all readers, carriers, and in-progress opens before releasing the flow.
    opens.shutdown().await;
    io_tasks.shutdown().await;
    let (uploaded_bytes, downloaded_bytes) = (transfer.send_offset, transfer.receive_offset);
    drop(transfer);
    state
        .finish_route_flow(
            flow_id,
            uploaded_bytes,
            downloaded_bytes,
            result.as_ref().err(),
        )
        .await;
    let relay_id = state.flow_relays.lock().await.remove(&flow_id);
    if relay_id.is_some() {
        state.mark_status_changed();
    }
    record_flow_metric(
        &state,
        &mapping,
        relay_id.as_deref(),
        "travel_flow_upload_observed_bytes",
        uploaded_bytes,
    );
    record_flow_metric(
        &state,
        &mapping,
        relay_id.as_deref(),
        "delivered_download_bytes",
        downloaded_bytes,
    );
    if result.is_ok() {
        record_flow_metric(
            &state,
            &mapping,
            relay_id.as_deref(),
            "travel_flow_completed",
            1,
        );
        info!(event = "tcp_flow_finished", %flow_id, "travel TCP flow finished");
    } else {
        record_flow_metric(
            &state,
            &mapping,
            relay_id.as_deref(),
            "travel_flow_failed",
            1,
        );
    }
    result
}

#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
async fn run_inner(
    state: &AppState,
    mapping: &Mapping,
    flow_id: Uuid,
    events_tx: &mpsc::Sender<FlowEvent>,
    events: &mut mpsc::Receiver<FlowEvent>,
    transfer: &mut TransferState,
    io_tasks: &mut JoinSet<()>,
    opens: &mut JoinSet<OpenResult>,
    connected: &mut Option<oneshot::Sender<std::result::Result<(), String>>>,
) -> Result<()> {
    let mut carriers = HashMap::<Uuid, CarrierHandle>::new();
    let mut active = None;
    let mut recovery_started = Instant::now();
    let mut retry_backoff = Duration::from_millis(250);
    let mut recovery_jitter = None;
    let mut reevaluate_secs = state.config.carrier_reevaluate_secs;
    let mut next_reevaluation = Instant::now();

    loop {
        while io_tasks.try_join_next().is_some() {}
        if transfer.local_eof
            && transfer.remote_eof
            && transfer.local_fin_acked
            && transfer.unacked.is_empty()
        {
            drain_carriers(carriers, io_tasks, events, transfer.io_timeout).await?;
            return Ok(());
        }

        if active.is_none() || Instant::now() >= next_reevaluation {
            if active.is_none()
                && let Some(delay) = recovery_jitter.take()
            {
                sleep(delay).await;
            }
            let previous_active = active;
            if active.is_none()
                && recovery_started.elapsed()
                    > Duration::from_secs(state.config.carrier_recovery_timeout_secs)
            {
                bail!("carrier recovery deadline expired");
            }
            match perform_race(
                state,
                mapping,
                flow_id,
                events_tx,
                events,
                &mut carriers,
                &mut active,
                transfer,
                io_tasks,
                opens,
            )
            .await?
            {
                Some(winner) => {
                    let relay_id = carriers
                        .get(&winner)
                        .map(|carrier| carrier.relay_id.clone())
                        .ok_or_else(|| anyhow!("winning carrier vanished"))?;
                    active = Some(winner);
                    state
                        .flow_relays
                        .lock()
                        .await
                        .insert(flow_id, relay_id.clone());
                    state.mark_status_changed();
                    info!(event = "carrier_selected", %flow_id, carrier_id = %winner, %relay_id, "travel selected carrier");
                    if let Some(connected) = connected.take() {
                        let _ = connected.send(Ok(()));
                    }
                    recovery_started = Instant::now();
                    retry_backoff = Duration::from_millis(250);
                    let stable = previous_active == Some(winner);
                    let switched = previous_active.is_some() && !stable;
                    let (delay_secs, next_delay_secs) = advance_reevaluation(
                        reevaluate_secs,
                        state.config.carrier_reevaluate_secs,
                        state.config.max_carrier_reevaluate_secs,
                        stable,
                    );
                    next_reevaluation = Instant::now() + Duration::from_secs(delay_secs);
                    reevaluate_secs = next_delay_secs;
                    info!(
                        event = "carrier_reevaluation_scheduled",
                        %flow_id,
                        stable,
                        switched,
                        delay_secs,
                        next_delay_secs,
                        "scheduled next carrier race"
                    );
                }
                None if active.is_some() => {
                    let (delay_secs, next_delay_secs) = advance_reevaluation(
                        reevaluate_secs,
                        state.config.carrier_reevaluate_secs,
                        state.config.max_carrier_reevaluate_secs,
                        false,
                    );
                    warn!(
                        event = "carrier_race_no_winner",
                        %flow_id,
                        delay_secs,
                        "carrier race produced no winner; retained current carrier and reset reevaluation"
                    );
                    next_reevaluation = Instant::now() + Duration::from_secs(delay_secs);
                    reevaluate_secs = next_delay_secs;
                }
                None => {
                    state
                        .mark_route_recovering(flow_id, Some("No Relay won the carrier race"))
                        .await;
                    warn!(
                        event = "carrier_race_retry",
                        %flow_id,
                        retry_after_ms = retry_backoff.as_millis(),
                        "carrier race found no usable relay"
                    );
                    let generation = *state.network_generation.borrow();
                    sleep(jittered_retry_delay(
                        retry_backoff,
                        uuid_seed(flow_id) ^ generation,
                    ))
                    .await;
                    retry_backoff = retry_backoff.saturating_mul(2).min(Duration::from_secs(5));
                    continue;
                }
            }
        }

        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { bail!("flow event channel closed"); };
                let closed_active = handle_event(
                    event,
                    transfer,
                    &mut carriers,
                    active,
                    None,
                )
                .await?;
                if closed_active {
                    active = None;
                    state
                        .mark_route_recovering(flow_id, Some("The active Carrier closed"))
                        .await;
                    let generation = *state.network_generation.borrow();
                    recovery_jitter = Some(Duration::from_millis(
                        (uuid_seed(flow_id) ^ generation) % 251,
                    ));
                    if state.flow_relays.lock().await.remove(&flow_id).is_some() {
                        state.mark_status_changed();
                    }
                    recovery_started = Instant::now();
                    reevaluate_secs = state.config.carrier_reevaluate_secs;
                    info!(
                        event = "carrier_recovery_started",
                        %flow_id,
                        recovery_timeout_secs = state.config.carrier_recovery_timeout_secs,
                        "travel started immediate carrier recovery"
                    );
                }
                state
                    .update_route_flow_counters(
                        flow_id,
                        transfer.send_offset,
                        transfer.receive_offset,
                    )
                    .await;
            }
            () = sleep_until(next_reevaluation) => {}
        }
    }
}

fn record_flow_metric(
    state: &AppState,
    mapping: &Mapping,
    relay_id: Option<&str>,
    family: &str,
    value: u64,
) {
    if value == 0 {
        return;
    }
    record_flow_metric_sample(state, mapping, relay_id, family, value, None, None);
}

fn record_flow_metric_sample(
    state: &AppState,
    mapping: &Mapping,
    relay_id: Option<&str>,
    family: &str,
    value: u64,
    result: Option<&str>,
    histogram_sample: Option<u64>,
) {
    let mut dimensions = super::travel_flow_metric_dimensions(mapping);
    if let Some(relay_id) = relay_id {
        dimensions.insert("relay_id".to_owned(), relay_id.to_owned());
    }
    if let Some(result) = result {
        dimensions.insert("result".to_owned(), result.to_owned());
    }
    if let Ok(now) = unix_time_secs() {
        state
            .statistics
            .record(now, family, dimensions, value, histogram_sample);
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn perform_race(
    state: &AppState,
    mapping: &Mapping,
    flow_id: Uuid,
    events_tx: &mpsc::Sender<FlowEvent>,
    events: &mut mpsc::Receiver<FlowEvent>,
    carriers: &mut HashMap<Uuid, CarrierHandle>,
    active: &mut Option<Uuid>,
    transfer: &mut TransferState,
    io_tasks: &mut JoinSet<()>,
    opens: &mut JoinSet<OpenResult>,
) -> Result<Option<Uuid>> {
    let race_id = Uuid::new_v4();
    let race_started = Instant::now();
    let race_deadline =
        Instant::now() + Duration::from_secs(state.config.carrier_race_timeout_secs);
    let old_active = *active;
    let old_relay_id = old_active
        .and_then(|carrier_id| carriers.get(&carrier_id))
        .map(|carrier| carrier.relay_id.clone());
    let mut candidate_ids = HashSet::new();
    let candidates = relay_candidates(state).await;
    let relay_ids: Vec<_> = candidates
        .iter()
        .map(super::RelayCandidate::label)
        .collect();
    info!(
        event = "carrier_race_started",
        %flow_id,
        %race_id,
        home_id = %mapping.home_id,
        service_id = %mapping.service_id,
        candidate_count = candidates.len(),
        ?relay_ids,
        had_active_carrier = old_active.is_some(),
        "travel started carrier race"
    );
    state
        .record_route_attempt(flow_id, None, ServiceProtocol::Tcp, "started", None, None)
        .await;
    let available_slots = state
        .config
        .max_carriers_per_flow
        .saturating_sub(usize::from(old_active.is_some()));
    for relay in candidates.into_iter().take(available_slots) {
        if relay
            .expected_id
            .as_deref()
            .is_some_and(|relay_id| old_relay_id.as_deref() == Some(relay_id))
        {
            continue;
        }
        let relay_label = relay.label().to_owned();
        let state = state.clone();
        let Ok(carrier_permit) = Arc::clone(&state.carrier_permits).try_acquire_owned() else {
            warn!(%flow_id, "travel active-Carrier limit reached during race");
            break;
        };
        let home_id = mapping.home_id.clone();
        let service_id = mapping.service_id.clone();
        opens.spawn(async move {
            let carrier_id = Uuid::new_v4();
            let attempt_started = Instant::now();
            let result = open_business_on(
                &state,
                &relay,
                flow_id,
                carrier_id,
                &service_id,
                ServiceProtocol::Tcp,
                &home_id,
            )
            .await;
            (
                relay_label,
                carrier_permit,
                result,
                attempt_started.elapsed(),
            )
        });
    }
    if let Some(carrier_id) = old_active {
        debug!(
            event = "active_carrier_race_sent",
            %flow_id,
            %race_id,
            %carrier_id,
            relay_id = old_relay_id.as_deref().unwrap_or("unknown"),
            "sent periodic race on active carrier"
        );
        send_to(
            carriers,
            carrier_id,
            DataFrame::Race {
                flow_id,
                race_id,
                next_offset: transfer.send_acked,
            },
        )
        .await;
    }

    let winner = loop {
        if Instant::now() >= race_deadline {
            break None;
        }
        tokio::select! {
            opened = opens.join_next(), if !opens.is_empty() => {
                if let Some(result) = opened {
                    match result? {
                        (relay_label, carrier_permit, Ok(carrier), elapsed) => {
                            let relay_id = carrier.relay_id.clone();
                            let latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                            record_flow_metric_sample(
                                state,
                                mapping,
                                Some(&relay_id),
                                "carrier_attempt",
                                1,
                                Some("accepted"),
                                None,
                            );
                            state
                                .record_route_attempt(
                                    flow_id,
                                    Some(relay_id.clone()),
                                    ServiceProtocol::Tcp,
                                    "succeeded",
                                    Some(latency_ms),
                                    None,
                                )
                                .await;
                            record_flow_metric_sample(
                                state,
                                mapping,
                                Some(&relay_id),
                                "carrier_setup_latency_ms",
                                latency_ms,
                                Some("accepted"),
                                Some(latency_ms),
                            );
                            debug!(%flow_id, %relay_label, %latency_ms, "carrier candidate setup measured");
                            if carrier.home_receive_offset > transfer.send_offset {
                                bail!("Home acknowledged unsent Travel data");
                            }
                            if carrier.home_receive_offset < transfer.send_acked {
                                bail!("Home acknowledgement moved backwards");
                            }
                            acknowledge(transfer, carrier.home_receive_offset)?;
                            if carrier.home_send_offset > transfer.receive_offset {
                                warn!(%flow_id, %relay_id, home_send_offset = carrier.home_send_offset, receive_offset = transfer.receive_offset, "Home reports reverse data that Travel has not received yet");
                            }
                            let carrier_id = carrier.carrier_id;
                            debug!(
                                event = "carrier_candidate_ready",
                                %flow_id,
                                %race_id,
                                %carrier_id,
                                %relay_id,
                                home_receive_offset = carrier.home_receive_offset,
                                home_send_offset = carrier.home_send_offset,
                                "carrier candidate completed OPEN"
                            );
                            let handle = spawn_carrier(
                                io_tasks,
                                flow_id,
                                carrier,
                                events_tx.clone(),
                                Duration::from_secs(state.config.carrier_heartbeat_secs),
                                Duration::from_secs(state.config.carrier_timeout_secs),
                                state.network_generation.subscribe(),
                                carrier_permit,
                            );
                            carriers.insert(carrier_id, handle);
                            candidate_ids.insert(carrier_id);
                            send_to(
                                carriers,
                                carrier_id,
                                DataFrame::Race {
                                    flow_id,
                                    race_id,
                                    next_offset: transfer.send_acked,
                                },
                            )
                            .await;
                        }
                        (relay_id, _carrier_permit, Err(error), elapsed) => {
                            let latency_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                            record_flow_metric_sample(
                                state,
                                mapping,
                                Some(&relay_id),
                                "carrier_attempt",
                                1,
                                Some("failed"),
                                None,
                            );
                            state
                                .record_route_attempt(
                                    flow_id,
                                    Some(relay_id.clone()),
                                    ServiceProtocol::Tcp,
                                    "failed",
                                    Some(latency_ms),
                                    Some(&error.to_string()),
                                )
                                .await;
                            record_flow_metric_sample(
                                state,
                                mapping,
                                Some(&relay_id),
                                "carrier_setup_latency_ms",
                                latency_ms,
                                Some("failed"),
                                Some(latency_ms),
                            );
                            warn!(%flow_id, %relay_id, %error, "carrier race attempt failed");
                        }
                    }
                    if active.is_none() && opens.is_empty() && candidate_ids.is_empty() {
                        break None;
                    }
                }
            }
            event = events.recv() => {
                let Some(event) = event else { bail!("flow event channel closed during race"); };
                let closed_candidate = match &event {
                    FlowEvent::CarrierClosed { carrier_id, .. } => Some(*carrier_id),
                    _ => None,
                };
                if let FlowEvent::CarrierFrame { carrier_id, frame } = &event {
                    match frame {
                        DataFrame::RaceAck { flow_id: id, race_id: id_race, winner_carrier_id }
                            if *id == flow_id && *id_race == race_id =>
                        {
                            debug!(event = "carrier_race_ack", %flow_id, %race_id, %carrier_id, %winner_carrier_id, "received carrier race ACK");
                            break Some(*winner_carrier_id);
                        }
                        DataFrame::RaceDuplicate { flow_id: id, race_id: id_race, winner_carrier_id }
                            if *id == flow_id && *id_race == race_id =>
                        {
                            debug!(event = "carrier_race_duplicate", %flow_id, %race_id, %carrier_id, %winner_carrier_id, "received carrier race DUP");
                            break Some(*winner_carrier_id);
                        }
                        _ => {}
                    }
                }
                let closed_active = handle_event(
                    event,
                    transfer,
                    carriers,
                    *active,
                    Some(race_id),
                )
                .await?;
                if closed_active {
                    *active = None;
                }
                if let Some(carrier_id) = closed_candidate {
                    candidate_ids.remove(&carrier_id);
                    if active.is_none() && opens.is_empty() && candidate_ids.is_empty() {
                        break None;
                    }
                }
            }
            () = sleep_until(race_deadline) => break None,
        }
    };
    opens.shutdown().await;

    if let Some(winner) = winner {
        if !carriers.contains_key(&winner) {
            close_all(carriers);
            *active = None;
            return Ok(None);
        }
        let losers: Vec<_> = carriers
            .keys()
            .copied()
            .filter(|id| *id != winner)
            .collect();
        for loser in losers {
            close_carrier(carriers, loser);
        }
        let winner_relay_id = carriers
            .get(&winner)
            .map(|carrier| carrier.relay_id.as_str());
        record_flow_metric_sample(
            state,
            mapping,
            winner_relay_id,
            "carrier_winner",
            1,
            None,
            None,
        );
        if old_active.is_some() && old_active != Some(winner) {
            record_flow_metric_sample(
                state,
                mapping,
                winner_relay_id,
                "carrier_handover",
                1,
                None,
                None,
            );
        }
        let race_latency_ms = u64::try_from(race_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        record_flow_metric_sample(
            state,
            mapping,
            winner_relay_id,
            "carrier_race_latency_ms",
            race_latency_ms,
            Some("winner"),
            Some(race_latency_ms),
        );
        *active = Some(winner);
        if let Some(relay_id) = winner_relay_id {
            state
                .select_route_relay(flow_id, relay_id, Some(race_latency_ms))
                .await;
        }
        retransmit(transfer, carriers, winner).await;
        Ok(Some(winner))
    } else {
        record_flow_metric_sample(
            state,
            mapping,
            old_relay_id.as_deref(),
            "carrier_race_failed",
            1,
            Some("no_winner"),
            None,
        );
        warn!(
            event = "carrier_race_timed_out",
            %flow_id,
            %race_id,
            had_active_carrier = old_active.is_some(),
            "carrier race timed out"
        );
        state
            .record_route_attempt(
                flow_id,
                old_relay_id,
                ServiceProtocol::Tcp,
                "failed",
                Some(u64::try_from(race_started.elapsed().as_millis()).unwrap_or(u64::MAX)),
                Some("Carrier race timed out"),
            )
            .await;
        for candidate in candidate_ids {
            close_carrier(carriers, candidate);
        }
        if old_active.is_some_and(|carrier_id| carriers.contains_key(&carrier_id)) {
            *active = old_active;
        } else {
            close_all(carriers);
            *active = None;
        }
        Ok(None)
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_event(
    event: FlowEvent,
    transfer: &mut TransferState,
    carriers: &mut HashMap<Uuid, CarrierHandle>,
    active: Option<Uuid>,
    ignored_race: Option<Uuid>,
) -> Result<bool> {
    match event {
        FlowEvent::LocalData { bytes, credit } => {
            if transfer.unacked_bytes.saturating_add(bytes.len()) > transfer.max_unacked_bytes {
                bail!("travel unacknowledged-data limit reached");
            }
            let segment = Segment {
                offset: transfer.send_offset,
                bytes,
                _credit: credit,
            };
            transfer.send_offset = transfer
                .send_offset
                .saturating_add(segment.bytes.len() as u64);
            transfer.uploaded_total.fetch_add(
                segment.bytes.len() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
            transfer.unacked_bytes += segment.bytes.len();
            debug!(
                event = "tcp_data_buffered",
                flow_id = %transfer.flow_id,
                offset = segment.offset,
                bytes = segment.bytes.len(),
                unacked_bytes = transfer.unacked_bytes,
                "buffered Travel-to-Home TCP data"
            );
            if let Some(carrier_id) = active {
                send_to(
                    carriers,
                    carrier_id,
                    DataFrame::Data {
                        flow_id: transfer.flow_id,
                        offset: segment.offset,
                        bytes: segment.bytes.clone(),
                    },
                )
                .await;
            }
            transfer.unacked.push_back(segment);
        }
        FlowEvent::LocalEof => {
            transfer.local_eof = true;
            if let Some(carrier_id) = active {
                send_to(
                    carriers,
                    carrier_id,
                    DataFrame::Fin {
                        flow_id: transfer.flow_id,
                        final_offset: transfer.send_offset,
                    },
                )
                .await;
            }
        }
        FlowEvent::LocalError(reason) => bail!("local TCP failed: {reason}"),
        FlowEvent::CarrierClosed { carrier_id, reason } => {
            let was_active = active == Some(carrier_id);
            let was_known = carriers.remove(&carrier_id).is_some();
            if was_known {
                warn!(
                    event = "carrier_closed",
                    flow_id = %transfer.flow_id,
                    %carrier_id,
                    %reason,
                    active = was_active,
                    "travel carrier failed"
                );
            } else {
                debug!(
                    event = "carrier_closed_expected",
                    flow_id = %transfer.flow_id,
                    %carrier_id,
                    "travel carrier stopped after cancellation"
                );
            }
            return Ok(was_active);
        }
        FlowEvent::CarrierFrame { carrier_id, frame } => match frame {
            DataFrame::RaceAck { race_id, .. } | DataFrame::RaceDuplicate { race_id, .. }
                if ignored_race == Some(race_id) => {}
            DataFrame::Data {
                flow_id,
                offset,
                bytes,
            } if flow_id == transfer.flow_id && bytes.len() <= MAX_DATA_PAYLOAD => {
                let end = offset.saturating_add(bytes.len() as u64);
                if offset == transfer.receive_offset {
                    let deadline = Instant::now() + transfer.io_timeout;
                    let mut delivered = 0;
                    while delivered < bytes.len() {
                        let count = tokio::time::timeout_at(
                            deadline,
                            transfer.local_writer.write(&bytes[delivered..]),
                        )
                        .await
                        .map_err(|_| anyhow!("local TCP write timed out"))??;
                        if count == 0 {
                            bail!("local TCP write returned zero");
                        }
                        delivered += count;
                        transfer.receive_offset =
                            transfer.receive_offset.saturating_add(count as u64);
                        transfer
                            .downloaded_total
                            .fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
                    }
                    debug!(event = "tcp_data_accepted", flow_id = %transfer.flow_id, %carrier_id, offset, next_offset = transfer.receive_offset, bytes = bytes.len(), "accepted Home-to-Travel TCP data");
                    send_to(
                        carriers,
                        carrier_id,
                        DataFrame::Ack {
                            flow_id: transfer.flow_id,
                            next_offset: transfer.receive_offset,
                        },
                    )
                    .await;
                } else if end <= transfer.receive_offset {
                    debug!(event = "tcp_data_duplicate", flow_id = %transfer.flow_id, %carrier_id, offset, next_offset = transfer.receive_offset, bytes = bytes.len(), "received duplicate Home-to-Travel TCP data");
                    send_to(
                        carriers,
                        carrier_id,
                        DataFrame::Duplicate {
                            flow_id: transfer.flow_id,
                            next_offset: transfer.receive_offset,
                            winner_carrier_id: active.unwrap_or(carrier_id),
                        },
                    )
                    .await;
                } else {
                    bail!("Home sent non-contiguous TCP data");
                }
            }
            DataFrame::Ack {
                flow_id,
                next_offset,
            } if flow_id == transfer.flow_id
                && next_offset >= transfer.send_acked
                && next_offset <= transfer.send_offset =>
            {
                acknowledge(transfer, next_offset)?;
                debug!(event = "tcp_ack_received", flow_id = %transfer.flow_id, %carrier_id, next_offset, unacked_bytes = transfer.unacked_bytes, "received ACK for Travel-to-Home TCP data");
            }
            DataFrame::Duplicate {
                flow_id,
                next_offset,
                ..
            } if flow_id == transfer.flow_id
                && next_offset >= transfer.send_acked
                && next_offset <= transfer.send_offset =>
            {
                acknowledge(transfer, next_offset)?;
                debug!(event = "tcp_duplicate_received", flow_id = %transfer.flow_id, %carrier_id, next_offset, unacked_bytes = transfer.unacked_bytes, "received DUP for Travel-to-Home TCP data");
            }
            DataFrame::Fin {
                flow_id,
                final_offset,
            } if flow_id == transfer.flow_id && final_offset == transfer.receive_offset => {
                if !transfer.remote_eof {
                    timeout(transfer.io_timeout, transfer.local_writer.shutdown())
                        .await
                        .map_err(|_| anyhow!("local TCP shutdown timed out"))??;
                    transfer.remote_eof = true;
                }
                send_to(
                    carriers,
                    carrier_id,
                    DataFrame::FinAck {
                        flow_id: transfer.flow_id,
                        final_offset,
                    },
                )
                .await;
            }
            DataFrame::FinAck {
                flow_id,
                final_offset,
            } if flow_id == transfer.flow_id
                && transfer.local_eof
                && final_offset == transfer.send_offset =>
            {
                transfer.local_fin_acked = true;
            }
            DataFrame::Close { flow_id, reason } if flow_id == transfer.flow_id => {
                bail!("Home closed flow: {reason}");
            }
            DataFrame::RaceAck { .. } | DataFrame::RaceDuplicate { .. } => {}
            _ => bail!("invalid TCP flow frame"),
        },
    }
    Ok(false)
}

fn spawn_local_reader(
    tasks: &mut JoinSet<()>,
    mut reader: ReadHalf<BoxStream>,
    events: mpsc::Sender<FlowEvent>,
    send_credit: Arc<Semaphore>,
) {
    tasks.spawn(async move {
        let mut buffer = vec![0_u8; MAX_DATA_PAYLOAD];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => {
                    let _ = events.send(FlowEvent::LocalEof).await;
                    return;
                }
                Ok(count) => {
                    let Ok(count_u32) = u32::try_from(count) else {
                        let _ = events
                            .send(FlowEvent::LocalError("local read size overflow".to_owned()))
                            .await;
                        return;
                    };
                    let Ok(credit) = Arc::clone(&send_credit).acquire_many_owned(count_u32).await
                    else {
                        return;
                    };
                    if events
                        .send(FlowEvent::LocalData {
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
                    let _ = events.send(FlowEvent::LocalError(error.to_string())).await;
                    return;
                }
            }
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_carrier(
    tasks: &mut JoinSet<()>,
    flow_id: Uuid,
    carrier: BusinessCarrier,
    events: mpsc::Sender<FlowEvent>,
    heartbeat_period: Duration,
    timeout_period: Duration,
    mut network_changes: watch::Receiver<u64>,
    carrier_permit: OwnedSemaphorePermit,
) -> CarrierHandle {
    let BusinessCarrier {
        carrier_id,
        relay_id,
        stream,
        home_receive_offset: _,
        home_send_offset: _,
        data_codec,
    } = carrier;
    let handle_relay_id = relay_id.clone();
    let (tx, mut outgoing) = mpsc::channel(128);
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    tasks.spawn(async move {
        let _carrier_permit = carrier_permit;
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = DataFrameReader::new(reader, DATA_FRAME_LIMIT, data_codec);
        let mut heartbeat = interval(heartbeat_period);
        let mut nonce = 0_u64;
        let mut last_received = Instant::now();
        let result: Result<()> = async {
            loop {
                tokio::select! {
                    changed = network_changes.changed() => {
                        changed.map_err(|_| anyhow!("network change notifier stopped"))?;
                        bail!("default network changed");
                    }
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            return Ok(());
                        }
                    }
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
                                    .map_err(|_| anyhow!("travel flow event receiver closed"))?;
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
        info!(%flow_id, %carrier_id, %relay_id, "travel carrier stopped");
    });
    CarrierHandle {
        relay_id: handle_relay_id,
        tx,
        shutdown,
    }
}

async fn send_to(carriers: &HashMap<Uuid, CarrierHandle>, carrier_id: Uuid, frame: DataFrame) {
    if let Some(carrier) = carriers.get(&carrier_id) {
        let _ = carrier.tx.send(frame).await;
    }
}

async fn retransmit(
    transfer: &mut TransferState,
    carriers: &HashMap<Uuid, CarrierHandle>,
    carrier_id: Uuid,
) {
    debug!(
        event = "tcp_retransmit",
        flow_id = %transfer.flow_id,
        %carrier_id,
        segment_count = transfer.unacked.len(),
        unacked_bytes = transfer.unacked_bytes,
        "retransmitting unacknowledged Travel-to-Home TCP data"
    );
    for segment in &transfer.unacked {
        send_to(
            carriers,
            carrier_id,
            DataFrame::Data {
                flow_id: transfer.flow_id,
                offset: segment.offset,
                bytes: segment.bytes.clone(),
            },
        )
        .await;
    }
    if transfer.local_eof {
        send_to(
            carriers,
            carrier_id,
            DataFrame::Fin {
                flow_id: transfer.flow_id,
                final_offset: transfer.send_offset,
            },
        )
        .await;
    }
}

fn acknowledge(transfer: &mut TransferState, next_offset: u64) -> Result<()> {
    while let Some(segment) = transfer.unacked.front() {
        let end = segment.offset.saturating_add(segment.bytes.len() as u64);
        if end > next_offset {
            break;
        }
        let segment = transfer
            .unacked
            .pop_front()
            .ok_or_else(|| anyhow!("segment vanished"))?;
        transfer.unacked_bytes = transfer.unacked_bytes.saturating_sub(segment.bytes.len());
    }
    if transfer
        .unacked
        .front()
        .is_some_and(|segment| segment.offset < next_offset)
    {
        bail!("ACK split a TCP data frame");
    }
    transfer.send_acked = next_offset;
    Ok(())
}

async fn drain_carriers(
    carriers: HashMap<Uuid, CarrierHandle>,
    tasks: &mut JoinSet<()>,
    events: &mut mpsc::Receiver<FlowEvent>,
    deadline: Duration,
) -> Result<()> {
    // Closing only the outgoing queues lets each Carrier write its final FIN ACK.
    // Keep cancellation senders alive: dropping them would stop the Carrier before
    // it drains that queue. Error and runtime cancellation still abort via run_io.
    let cancellation: Vec<_> = carriers
        .into_values()
        .map(|carrier| carrier.shutdown)
        .collect();
    let result = timeout(deadline, async {
        while !tasks.is_empty() {
            tokio::select! {
                biased;
                finished = tasks.join_next() => {
                    if let Some(finished) = finished {
                        finished.map_err(|error| anyhow!("Carrier drain task failed: {error}"))?;
                    }
                }
                // A Carrier reader must not block its writer on the bounded queue.
                Some(_) = events.recv() => {}
            }
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow!("Carrier final acknowledgement drain timed out"))?;
    drop(cancellation);
    result
}

fn close_carrier(carriers: &mut HashMap<Uuid, CarrierHandle>, carrier_id: Uuid) {
    if let Some(carrier) = carriers.remove(&carrier_id) {
        let _ = carrier.shutdown.send(true);
    }
}

fn close_all(carriers: &mut HashMap<Uuid, CarrierHandle>) {
    for (_, carrier) in carriers.drain() {
        let _ = carrier.shutdown.send(true);
    }
}

fn advance_reevaluation(
    current_secs: u64,
    initial_secs: u64,
    maximum_secs: u64,
    stable: bool,
) -> (u64, u64) {
    let delay_secs = if stable { current_secs } else { initial_secs };
    (delay_secs, delay_secs.saturating_mul(2).min(maximum_secs))
}

#[cfg(test)]
mod tests {
    use super::advance_reevaluation;

    #[test]
    fn stable_carrier_increases_reevaluation_to_cap() {
        assert_eq!(advance_reevaluation(60, 60, 900, true), (60, 120));
        assert_eq!(advance_reevaluation(120, 60, 900, true), (120, 240));
        assert_eq!(advance_reevaluation(480, 60, 900, true), (480, 900));
        assert_eq!(advance_reevaluation(900, 60, 900, true), (900, 900));
    }

    #[test]
    fn unstable_carrier_result_resets_reevaluation() {
        assert_eq!(advance_reevaluation(480, 60, 900, false), (60, 120));
    }
}

#[cfg(test)]
mod close_tests {
    use super::*;
    use flowsplice_core::frame::DataFrameCodec;

    #[tokio::test]
    async fn normal_close_delivers_final_ack_under_event_and_write_pressure() -> Result<()> {
        let flow_id = Uuid::new_v4();
        let carrier_id = Uuid::new_v4();
        let codec = DataFrameCodec::negotiate(1);
        let (tx, mut outgoing) = mpsc::channel(1);
        tx.send(DataFrame::FinAck {
            flow_id,
            final_offset: 9,
        })
        .await?;
        let (shutdown, mut cancelled) = watch::channel(false);
        let (event_tx, mut events) = mpsc::channel(1);
        event_tx.send(FlowEvent::LocalEof).await?;
        let (mut writer, reader) = tokio::io::duplex(1);
        let permits = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&permits).acquire_owned().await?;
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _permit = permit;
            // This blocks until normal-close handling drains the event queue.
            assert!(event_tx.send(FlowEvent::LocalEof).await.is_ok());
            loop {
                tokio::select! {
                    biased;
                    _ = cancelled.changed() => panic!("normal close cancelled queued output"),
                    frame = outgoing.recv() => {
                        let Some(frame) = frame else { break; };
                        assert!(write_data_frame(&mut writer, &frame, DATA_FRAME_LIMIT, codec).await.is_ok());
                    }
                }
            }
            assert!(writer.shutdown().await.is_ok());
        });
        let carriers = HashMap::from([(
            carrier_id,
            CarrierHandle {
                relay_id: "test-relay".into(),
                tx,
                shutdown,
            },
        )]);
        let receiving = async {
            let mut reader = DataFrameReader::new(reader, DATA_FRAME_LIMIT, codec);
            assert!(matches!(reader.read().await?, DataFrame::FinAck {
                flow_id: id, final_offset: 9,
            } if id == flow_id));
            Ok::<_, anyhow::Error>(())
        };
        timeout(Duration::from_secs(2), async {
            tokio::try_join!(
                drain_carriers(carriers, &mut tasks, &mut events, Duration::from_secs(1)),
                receiving,
            )?;
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        assert!(tasks.is_empty());
        assert_eq!(permits.available_permits(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn blocked_close_times_out_and_cancellation_releases_resources() -> Result<()> {
        let (tx, _outgoing) = mpsc::channel(1);
        let (shutdown, _cancelled) = watch::channel(false);
        let (_event_tx, mut events) = mpsc::channel(1);
        let permits = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&permits).acquire_owned().await?;
        let mut tasks = JoinSet::new();
        tasks.spawn(async move {
            let _permit = permit;
            std::future::pending::<()>().await;
        });
        let carriers = HashMap::from([(
            Uuid::new_v4(),
            CarrierHandle {
                relay_id: "blocked-relay".into(),
                tx,
                shutdown,
            },
        )]);
        let failure = drain_carriers(carriers, &mut tasks, &mut events, Duration::from_millis(20))
            .await
            .err()
            .ok_or_else(|| anyhow!("blocked drain unexpectedly completed"))?;
        assert!(
            failure
                .to_string()
                .contains("final acknowledgement drain timed out")
        );
        // run_io retains ownership and aborts the tasks on the error/cancellation path.
        timeout(Duration::from_secs(1), tasks.shutdown()).await?;
        assert!(tasks.is_empty());
        assert_eq!(permits.available_permits(), 1);
        Ok(())
    }
}

#[cfg(test)]
mod live_counter_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tokio::net::TcpListener;

    #[tokio::test]
    #[allow(clippy::too_many_lines)]
    async fn live_payload_counts_once_and_survives_error_close() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut peer = TcpStream::connect(listener.local_addr()?).await?;
        let (local, _) = listener.accept().await?;
        let (_reader, writer) = tokio::io::split(Box::new(local) as BoxStream);
        let upload = Arc::new(AtomicU64::new(0));
        let download = Arc::new(AtomicU64::new(0));
        let flow_id = Uuid::new_v4();
        let mut transfer = TransferState {
            flow_id,
            local_writer: writer,
            send_offset: 0,
            send_acked: 0,
            receive_offset: 0,
            unacked: VecDeque::new(),
            unacked_bytes: 0,
            max_unacked_bytes: 65536,
            local_eof: false,
            local_fin_acked: false,
            remote_eof: false,
            io_timeout: Duration::from_secs(1),
            uploaded_total: Arc::clone(&upload),
            downloaded_total: Arc::clone(&download),
        };
        let mut carriers = HashMap::new();
        let credit = Arc::new(Semaphore::new(4)).acquire_many_owned(4).await?;
        handle_event(
            FlowEvent::LocalData {
                bytes: Bytes::from_static(b"sent"),
                credit,
            },
            &mut transfer,
            &mut carriers,
            None,
            None,
        )
        .await?;
        assert_eq!(upload.load(Ordering::Relaxed), 4);
        let carrier_id = Uuid::new_v4();
        for _ in 0..2 {
            handle_event(
                FlowEvent::CarrierFrame {
                    carrier_id,
                    frame: DataFrame::Data {
                        flow_id,
                        offset: 0,
                        bytes: Bytes::from_static(b"recv"),
                    },
                },
                &mut transfer,
                &mut carriers,
                None,
                None,
            )
            .await?;
        }
        let mut received = [0; 4];
        peer.read_exact(&mut received).await?;
        assert_eq!(&received, b"recv");
        assert_eq!(
            download.load(Ordering::Relaxed),
            4,
            "duplicate carrier payload must not count again"
        );
        handle_event(
            FlowEvent::CarrierFrame {
                carrier_id,
                frame: DataFrame::Ack {
                    flow_id,
                    next_offset: 4,
                },
            },
            &mut transfer,
            &mut carriers,
            None,
            None,
        )
        .await?;
        assert_eq!(
            upload.load(Ordering::Relaxed),
            4,
            "ACK must not count upload again"
        );
        assert!(
            handle_event(
                FlowEvent::LocalError("fixture failure".to_owned()),
                &mut transfer,
                &mut carriers,
                None,
                None
            )
            .await
            .is_err()
        );
        drop(transfer);
        assert_eq!(
            (
                upload.load(Ordering::Relaxed),
                download.load(Ordering::Relaxed)
            ),
            (4, 4)
        );
        Ok(())
    }
}
