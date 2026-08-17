use std::{
    collections::{HashMap, HashSet, VecDeque},
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use flowsplice_core::{
    DATA_FRAME_LIMIT, MAX_DATA_PAYLOAD,
    frame::{JsonFrameReader, write_json},
    protocol::{DataFrame, ServiceProtocol},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{mpsc, watch},
    task::JoinSet,
    time::{Instant, interval, sleep, sleep_until},
};
use tracing::{debug, info, warn};
use uuid::Uuid;

use super::{AppState, BusinessCarrier, Mapping, open_business_on, relay_candidates};

#[derive(Clone)]
struct Segment {
    offset: u64,
    bytes: Vec<u8>,
}

struct CarrierHandle {
    relay_id: String,
    tx: mpsc::Sender<DataFrame>,
    shutdown: watch::Sender<bool>,
}

enum FlowEvent {
    LocalData(Vec<u8>),
    LocalEof,
    LocalError(String),
    CarrierFrame { carrier_id: Uuid, frame: DataFrame },
    CarrierClosed { carrier_id: Uuid, reason: String },
}

struct TransferState {
    flow_id: Uuid,
    local_writer: OwnedWriteHalf,
    send_offset: u64,
    send_acked: u64,
    receive_offset: u64,
    unacked: VecDeque<Segment>,
    unacked_bytes: usize,
    max_unacked_bytes: usize,
    local_eof: bool,
    local_fin_acked: bool,
    remote_eof: bool,
}

pub async fn run(state: AppState, mapping: Mapping, local: TcpStream) -> Result<()> {
    let flow_id = Uuid::new_v4();
    info!(
        event = "tcp_flow_started",
        %flow_id,
        service_id = %mapping.service_id,
        "travel TCP flow started"
    );
    let result = run_inner(&state, &mapping, local, flow_id).await;
    state.flow_relays.lock().await.remove(&flow_id);
    if result.is_ok() {
        info!(event = "tcp_flow_finished", %flow_id, "travel TCP flow finished");
    }
    result
}

#[allow(clippy::too_many_lines)]
async fn run_inner(
    state: &AppState,
    mapping: &Mapping,
    local: TcpStream,
    flow_id: Uuid,
) -> Result<()> {
    local.set_nodelay(true)?;
    let (local_reader, local_writer) = local.into_split();
    let (events_tx, mut events) = mpsc::channel(512);
    spawn_local_reader(local_reader, events_tx.clone());

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
    };
    let mut carriers = HashMap::<Uuid, CarrierHandle>::new();
    let mut active = None;
    let mut recovery_started = Instant::now();
    let mut retry_backoff = Duration::from_millis(250);
    let mut reevaluate_secs = state.config.carrier_reevaluate_secs;
    let mut next_reevaluation = Instant::now();

    loop {
        if transfer.local_eof
            && transfer.remote_eof
            && transfer.local_fin_acked
            && transfer.unacked.is_empty()
        {
            return Ok(());
        }

        if active.is_none() || Instant::now() >= next_reevaluation {
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
                &events_tx,
                &mut events,
                &mut carriers,
                &mut active,
                &mut transfer,
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
                    info!(event = "carrier_selected", %flow_id, carrier_id = %winner, %relay_id, "travel selected carrier");
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
                    warn!(
                        event = "carrier_race_retry",
                        %flow_id,
                        retry_after_ms = retry_backoff.as_millis(),
                        "carrier race found no usable relay"
                    );
                    sleep(retry_backoff).await;
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
                    &mut transfer,
                    &mut carriers,
                    active,
                    None,
                )
                .await?;
                if closed_active {
                    active = None;
                    state.flow_relays.lock().await.remove(&flow_id);
                    recovery_started = Instant::now();
                    reevaluate_secs = state.config.carrier_reevaluate_secs;
                    info!(
                        event = "carrier_recovery_started",
                        %flow_id,
                        recovery_timeout_secs = state.config.carrier_recovery_timeout_secs,
                        "travel started immediate carrier recovery"
                    );
                }
            }
            () = sleep_until(next_reevaluation) => {}
        }
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
) -> Result<Option<Uuid>> {
    let race_id = Uuid::new_v4();
    let race_deadline =
        Instant::now() + Duration::from_secs(state.config.carrier_race_timeout_secs);
    let old_active = *active;
    let old_relay_id = old_active
        .and_then(|carrier_id| carriers.get(&carrier_id))
        .map(|carrier| carrier.relay_id.clone());
    let mut candidate_ids = HashSet::new();
    let mut opens = JoinSet::new();
    let candidates = relay_candidates(state).await;
    let relay_ids: Vec<_> = candidates.iter().map(|relay| relay.id.as_str()).collect();
    info!(
        event = "carrier_race_started",
        %flow_id,
        %race_id,
        candidate_count = candidates.len(),
        ?relay_ids,
        had_active_carrier = old_active.is_some(),
        "travel started carrier race"
    );
    for relay in candidates {
        if old_relay_id.as_deref() == Some(relay.id.as_str()) {
            continue;
        }
        let state = state.clone();
        let service_id = mapping.service_id.clone();
        opens.spawn(async move {
            let carrier_id = Uuid::new_v4();
            let result = open_business_on(
                &state,
                &relay,
                flow_id,
                carrier_id,
                &service_id,
                ServiceProtocol::Tcp,
            )
            .await;
            (relay.id, result)
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
                        (relay_id, Ok(carrier)) => {
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
                                flow_id,
                                carrier,
                                events_tx.clone(),
                                Duration::from_secs(state.config.carrier_heartbeat_secs),
                                Duration::from_secs(state.config.carrier_timeout_secs),
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
                        (relay_id, Err(error)) => {
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
    opens.abort_all();

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
        *active = Some(winner);
        retransmit(transfer, carriers, winner).await;
        Ok(Some(winner))
    } else {
        warn!(
            event = "carrier_race_timed_out",
            %flow_id,
            %race_id,
            had_active_carrier = old_active.is_some(),
            "carrier race timed out"
        );
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
        FlowEvent::LocalData(bytes) => {
            if transfer.unacked_bytes.saturating_add(bytes.len()) > transfer.max_unacked_bytes {
                bail!("travel unacknowledged-data limit reached");
            }
            let segment = Segment {
                offset: transfer.send_offset,
                bytes,
            };
            transfer.send_offset = transfer
                .send_offset
                .saturating_add(segment.bytes.len() as u64);
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
                    transfer.local_writer.write_all(&bytes).await?;
                    transfer.receive_offset = end;
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
                    transfer.local_writer.shutdown().await?;
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

fn spawn_local_reader(mut reader: OwnedReadHalf, events: mpsc::Sender<FlowEvent>) {
    tokio::spawn(async move {
        let mut buffer = vec![0_u8; MAX_DATA_PAYLOAD];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) => {
                    let _ = events.send(FlowEvent::LocalEof).await;
                    return;
                }
                Ok(count) => {
                    if events
                        .send(FlowEvent::LocalData(buffer[..count].to_vec()))
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

fn spawn_carrier(
    flow_id: Uuid,
    carrier: BusinessCarrier,
    events: mpsc::Sender<FlowEvent>,
    heartbeat_period: Duration,
    timeout_period: Duration,
) -> CarrierHandle {
    let BusinessCarrier {
        carrier_id,
        relay_id,
        stream,
        home_receive_offset: _,
        home_send_offset: _,
    } = carrier;
    let handle_relay_id = relay_id.clone();
    let (tx, mut outgoing) = mpsc::channel(128);
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    tokio::spawn(async move {
        let (reader, mut writer) = tokio::io::split(stream);
        let mut reader = JsonFrameReader::new(reader, DATA_FRAME_LIMIT);
        let mut heartbeat = interval(heartbeat_period);
        let mut nonce = 0_u64;
        let mut last_received = Instant::now();
        let result: Result<()> = async {
            loop {
                tokio::select! {
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            return Ok(());
                        }
                    }
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
                                    .map_err(|_| anyhow!("travel flow event receiver closed"))?;
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
    transfer: &TransferState,
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
