//! Real-TLS regressions for event-queue backpressure in the carrier task.
use super::*;
use anyhow::Context;
use flowsplice_core::{
    frame::DataFrameCodec,
    tls::{
        identity_server_auth_connector_from_ca_pem, identity_server_name,
        optional_client_server_acceptor,
    },
};
use std::path::PathBuf;
use tokio::net::TcpListener;
use tokio_rustls::server::TlsStream;

const DEADLINE: Duration = Duration::from_secs(3);
const MARKER: u64 = 0xface_beef;
const CODEC: DataFrameCodec = DataFrameCodec::BinaryV1;
type RemoteReader = DataFrameReader<ReadHalf<TlsStream<TcpStream>>>;

struct Harness {
    tasks: JoinSet<()>,
    handle: CarrierHandle,
    events: mpsc::Receiver<FlowEvent>,
    network: watch::Sender<u64>,
    permits: Arc<Semaphore>,
    remote: RemoteReader,
    _remote_writer: WriteHalf<TlsStream<TcpStream>>,
    flow_id: Uuid,
    carrier_id: Uuid,
    event_capacity: usize,
    permit_capacity: u32,
}

async fn tls_pair() -> Result<(
    tokio_rustls::client::TlsStream<TcpStream>,
    TlsStream<TcpStream>,
)> {
    let certs = PathBuf::from(std::env::var("FLOWSPLICE_REVIEW_FIXTURES").context(
        "set FLOWSPLICE_REVIEW_FIXTURES using tests/check-travel-review-regressions.sh",
    )?)
    .join("legit/generated/certs");
    let acceptor = optional_client_server_acceptor(
        &certs.join("relay1.crt"),
        &certs.join("relay1.key"),
        &certs.join("management-ca.crt"),
    )?;
    let connector = identity_server_auth_connector_from_ca_pem(&std::fs::read_to_string(
        certs.join("management-ca.crt"),
    )?)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    timeout(DEADLINE, async {
        tokio::try_join!(
            async {
                Ok::<_, anyhow::Error>(
                    connector
                        .connect(identity_server_name()?, TcpStream::connect(address).await?)
                        .await?,
                )
            },
            async { Ok::<_, anyhow::Error>(acceptor.accept(listener.accept().await?.0).await?) },
        )
    })
    .await
    .context("TLS handshake deadline")?
}

impl Harness {
    async fn blocked(event_capacity: usize, permits: Arc<Semaphore>) -> Result<Self> {
        let permit_capacity = u32::try_from(permits.available_permits())?;
        assert!(permit_capacity > 0);
        let (client, server) = tls_pair().await?;
        let (events_tx, events) = mpsc::channel(event_capacity);
        for _ in 0..event_capacity {
            events_tx
                .send(FlowEvent::LocalEof)
                .await
                .map_err(|_| anyhow!("event queue closed"))?;
        }
        let (network, network_rx) = watch::channel(0);
        let flow_id = Uuid::new_v4();
        let carrier_id = Uuid::new_v4();
        let mut tasks = JoinSet::new();
        let handle = spawn_carrier(
            &mut tasks,
            flow_id,
            BusinessCarrier {
                carrier_id,
                relay_id: "relay-1".into(),
                stream: client,
                home_receive_offset: 0,
                home_send_offset: 0,
                data_codec: CODEC,
            },
            events_tx,
            Duration::from_secs(60),
            Duration::from_secs(120),
            network_rx,
            permits.clone().acquire_owned().await?,
        );
        let (reader, mut writer) = tokio::io::split(server);
        let mut remote = DataFrameReader::new(reader, DATA_FRAME_LIMIT, CODEC);
        // A wire-level round trip proves the spawned task has entered its read loop.
        write_data_frame(
            &mut writer,
            &DataFrame::Ping { nonce: MARKER },
            DATA_FRAME_LIMIT,
            CODEC,
        )
        .await?;
        timeout(DEADLINE, async {
            loop {
                match remote.read().await? {
                    DataFrame::Pong { nonce: MARKER } => return Ok::<_, anyhow::Error>(()),
                    DataFrame::Ping { .. } => {}
                    _ => bail!("unexpected frame before marker"),
                }
            }
        })
        .await
        .context("carrier marker deadline")??;
        write_data_frame(
            &mut writer,
            &DataFrame::Data {
                flow_id,
                offset: 0,
                bytes: Bytes::from_static(b"inbound"),
            },
            DATA_FRAME_LIMIT,
            CODEC,
        )
        .await?;
        // These current-thread tests give the ready socket/carrier bounded scheduling
        // turns to read the frame and block on the deliberately full event queue.
        for _ in 0..64 {
            tokio::task::yield_now().await;
        }
        assert_eq!(events.len(), event_capacity);
        Ok(Self {
            tasks,
            handle,
            events,
            network,
            permits,
            remote,
            _remote_writer: writer,
            flow_id,
            carrier_id,
            event_capacity,
            permit_capacity,
        })
    }

    async fn finish(&mut self, expected_reason: &str) -> Result<()> {
        // Resource retirement must happen BEFORE the consumer drains its events.
        let permit = timeout(
            DEADLINE,
            self.permits
                .clone()
                .acquire_many_owned(self.permit_capacity),
        )
        .await
        .context("carrier permit retained behind full event queue")??;
        timeout(DEADLINE, self.handle.tx.closed())
            .await
            .context("outgoing receiver stayed open")?;
        assert_eq!(self.events.len(), self.event_capacity);
        drop(permit);
        timeout(DEADLINE, async {
            loop {
                match self.events.recv().await {
                    Some(FlowEvent::CarrierClosed { carrier_id, reason }) => {
                        assert_eq!(carrier_id, self.carrier_id);
                        assert!(
                            reason.contains(expected_reason),
                            "unexpected close reason: {reason}"
                        );
                        break;
                    }
                    Some(_) => {}
                    None => bail!("missing CarrierClosed event"),
                }
            }
            while let Some(result) = self.tasks.join_next().await {
                result?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("carrier did not finish after events drained")??;
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires generated review TLS fixtures"]
async fn review_regressions_carrier_full_events_preserves_outgoing_tls_order() -> Result<()> {
    let mut harness = Harness::blocked(1, Arc::new(Semaphore::new(1))).await?;
    verify_outgoing_order(&mut harness).await?;
    harness.handle.shutdown.send(true)?;
    harness.finish("carrier closed").await
}

async fn verify_outgoing_order(harness: &mut Harness) -> Result<()> {
    assert_eq!(harness.handle.tx.max_capacity(), 128);
    let flow_id = harness.flow_id;
    timeout(DEADLINE, async {
        let send = async {
            for index in 0..256_u64 {
                let frame = if index % 2 == 0 {
                    DataFrame::Data {
                        flow_id,
                        offset: index,
                        bytes: Bytes::copy_from_slice(&index.to_be_bytes()),
                    }
                } else {
                    DataFrame::Ack {
                        flow_id,
                        next_offset: index,
                    }
                };
                harness
                    .handle
                    .tx
                    .send(frame)
                    .await
                    .map_err(|_| anyhow!("outgoing carrier closed"))?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let receive = async {
            for expected in 0..256_u64 {
                let frame = loop {
                    let frame = harness.remote.read().await?;
                    if !matches!(frame, DataFrame::Ping { .. }) {
                        break frame;
                    }
                };
                match frame {
                    DataFrame::Data {
                        flow_id: actual_flow,
                        offset,
                        bytes,
                    } if expected % 2 == 0 => {
                        assert_eq!(actual_flow, flow_id);
                        assert_eq!(offset, expected);
                        assert_eq!(bytes.as_ref(), expected.to_be_bytes());
                    }
                    DataFrame::Ack {
                        flow_id: actual_flow,
                        next_offset,
                    } if expected % 2 == 1 => {
                        assert_eq!(actual_flow, flow_id);
                        assert_eq!(next_offset, expected);
                    }
                    _ => bail!("unexpected outgoing frame at position {expected}"),
                }
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::try_join!(send, receive)?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("outgoing TLS stalled behind full event queue")??;
    assert_eq!(harness.events.len(), harness.event_capacity);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires generated review TLS fixtures"]
async fn review_regressions_carrier_full_events_network_change_releases_resources() -> Result<()> {
    let mut harness = Harness::blocked(1, Arc::new(Semaphore::new(1))).await?;
    harness.network.send(1)?;
    harness.finish("default network changed").await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires generated review TLS fixtures"]
async fn review_regressions_carrier_full_events_shutdown_sender_drop_releases_resources()
-> Result<()> {
    let mut harness = Harness::blocked(1, Arc::new(Semaphore::new(1))).await?;
    let (replacement, _) = watch::channel(false);
    drop(std::mem::replace(&mut harness.handle.shutdown, replacement));
    harness.finish("carrier closed").await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires generated review TLS fixtures"]
async fn review_regressions_carrier_production_event32_outgoing128_pressure() -> Result<()> {
    let mut harness = Harness::blocked(32, Arc::new(Semaphore::new(1))).await?;
    verify_outgoing_order(&mut harness).await?;
    harness.handle.shutdown.send(true)?;
    harness.finish("carrier closed").await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires generated review TLS fixtures"]
async fn review_regressions_carrier_repeated_network_changes_restore_shared_budget() -> Result<()> {
    let permits = Arc::new(Semaphore::new(4));
    for generation in 1..=64 {
        let mut harness = Harness::blocked(32, permits.clone()).await?;
        assert_eq!(permits.available_permits(), 3);
        harness.network.send(generation)?;
        harness.finish("default network changed").await?;
        assert_eq!(
            permits.available_permits(),
            4,
            "leaked permit at generation {generation}"
        );
        assert!(
            harness.tasks.is_empty(),
            "leaked task at generation {generation}"
        );
    }
    assert_eq!(permits.available_permits(), 4);
    Ok(())
}
