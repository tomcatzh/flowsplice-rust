//! Exercise the production UDP pump and its whole-operation cancellation boundary.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use super::*;
use tokio::io::AsyncReadExt;

const CODEC: DataFrameCodec = DataFrameCodec::BinaryV1;
const DEADLINE: Duration = Duration::from_secs(2);

// The notification is emitted inside the actual 65th send future, immediately
// before it awaits capacity in datagram_pair(64).
struct DeliveryBarrier {
    endpoint: Arc<dyn flowsplice_transport::DatagramIo>,
    attempts: std::sync::atomic::AtomicUsize,
    saturated: Arc<tokio::sync::Notify>,
}

impl flowsplice_transport::DatagramIo for DeliveryBarrier {
    fn send<'a>(&'a self, bytes: &'a [u8]) -> flowsplice_transport::IoFuture<'a, ()> {
        Box::pin(async move {
            if self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                == 64
            {
                self.saturated.notify_one();
            }
            self.endpoint.send(bytes).await
        })
    }

    fn recv(&self) -> flowsplice_transport::IoFuture<'_, Vec<u8>> {
        self.endpoint.recv()
    }
}

async fn assert_pending<F: std::future::Future>(future: std::pin::Pin<&mut F>) {
    let mut future = future;
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "operation unexpectedly completed"
        );
        std::task::Poll::Ready(())
    })
    .await;
}

async fn send_datagram(
    stream: &mut tokio::io::DuplexStream,
    id: Uuid,
    sequence: u64,
    bytes: &[u8],
) {
    write_data_frame(
        stream,
        &DataFrame::Datagram {
            flow_id: id,
            sequence,
            bytes: bytes.to_vec().into(),
        },
        DATA_FRAME_LIMIT,
        CODEC,
    )
    .await
    .expect("test operation succeeds");
}

async fn saturated_download(stop: Option<bool>) {
    let (application, endpoint) =
        flowsplice_transport::datagram_pair(64).expect("test operation succeeds");
    let saturated = Arc::new(tokio::sync::Notify::new());
    let endpoint = Arc::new(DeliveryBarrier {
        endpoint,
        attempts: std::sync::atomic::AtomicUsize::new(0),
        saturated: Arc::clone(&saturated),
    });
    let (business, mut remote) = tokio::io::duplex(128 * 1024);
    let id = Uuid::new_v4();
    for sequence in 0..65 {
        send_datagram(
            &mut remote,
            id,
            sequence,
            &[u8::try_from(sequence).expect("sequence fits u8")],
        )
        .await;
    }
    let (shutdown_tx, mut shutdown) = watch::channel(false);
    let (network_tx, mut network) = watch::channel(0);
    let permits = Arc::new(Semaphore::new(1));
    let permit = permits
        .clone()
        .acquire_owned()
        .await
        .expect("test operation succeeds");
    let mut counts = (0, 0);
    let observed = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let result = {
        let operation = async {
            let _permit = permit;
            pump_udp_association(
                business,
                endpoint,
                id,
                CODEC,
                Duration::from_secs(60),
                &mut counts,
                |_, downloaded, _| {
                    observed.store(downloaded, std::sync::atomic::Ordering::Relaxed);
                    Ok(std::future::ready(()))
                },
            )
            .await
        };
        let operation = cancel_udp_association(&mut shutdown, &mut network, operation);
        tokio::pin!(operation);
        timeout(DEADLINE, async {
            tokio::select! {
                result = &mut operation => panic!("association ended before saturation: {result:?}"),
                () = saturated.notified() => {}
            }
        }).await.expect("pump did not reach the 65th delivery");
        assert_pending(operation.as_mut()).await;
        assert_eq!(observed.load(std::sync::atomic::Ordering::Relaxed), 64);
        match stop {
            Some(true) => {
                shutdown_tx.send(true).expect("test operation succeeds");
            }
            Some(false) => {
                network_tx.send(1).expect("test operation succeeds");
            }
            None => tokio::time::advance(Duration::from_secs(60)).await,
        }
        timeout(DEADLINE, operation)
            .await
            .expect("association stuck")
    };
    if stop.is_none() {
        assert_eq!(
            result.unwrap_err().to_string(),
            "UDP application write timed out"
        );
    } else {
        result.expect("test operation succeeds");
    }
    assert_eq!(counts, (0, 64));
    assert_eq!(permits.available_permits(), 1);
    for sequence in 0..64 {
        assert_eq!(
            application.recv().await.expect("test operation succeeds"),
            vec![u8::try_from(sequence).expect("sequence fits u8")]
        );
    }
    assert!(
        application.recv().await.is_err(),
        "undelivered datagram was queued"
    );
    assert_eq!(
        remote
            .read(&mut [0; 1])
            .await
            .expect("test operation succeeds"),
        0,
        "carrier was not dropped"
    );
}

#[tokio::test(start_paused = true)]
async fn udp_application_write_timeout_preserves_delivered_counts() {
    saturated_download(None).await;
}
#[tokio::test(start_paused = true)]
async fn udp_network_change_cancels_blocked_application_delivery() {
    saturated_download(Some(false)).await;
}
#[tokio::test(start_paused = true)]
async fn udp_shutdown_cancels_blocked_application_delivery() {
    saturated_download(Some(true)).await;
}

#[tokio::test(start_paused = true)]
async fn udp_ordered_and_empty_datagrams_round_trip() {
    let (application, endpoint) =
        flowsplice_transport::datagram_pair(64).expect("test operation succeeds");
    let (business, mut remote) = tokio::io::duplex(128 * 1024);
    let id = Uuid::new_v4();
    let mut counts = (0, 0);
    let pump = pump_udp_association(
        business,
        endpoint,
        id,
        CODEC,
        DEADLINE,
        &mut counts,
        |_, _, _| Ok(std::future::ready(())),
    );
    let peer = async {
        for (sequence, bytes) in [b"first".as_slice(), b"", b"last"].into_iter().enumerate() {
            application
                .send(bytes)
                .await
                .expect("test operation succeeds");
            let frame = DataFrameReader::new(&mut remote, DATA_FRAME_LIMIT, CODEC)
                .read()
                .await
                .expect("test operation succeeds");
            assert!(
                matches!(frame, DataFrame::Datagram { sequence: s, bytes: b, .. } if s == sequence as u64 && b.as_ref() == bytes)
            );
            send_datagram(&mut remote, id, sequence as u64, bytes).await;
            assert_eq!(
                application.recv().await.expect("test operation succeeds"),
                bytes
            );
        }
        drop(application);
    };
    let (result, ()) = timeout(DEADLINE, async { tokio::join!(pump, peer) })
        .await
        .expect("test operation succeeds");
    result.expect("test operation succeeds");
    assert_eq!(counts, (9, 9));
}

#[tokio::test(start_paused = true)]
async fn udp_cancellation_interrupts_blocked_frame_write_and_releases_saturated_upload() {
    for network_change in [false, true] {
        let (application, endpoint) =
            flowsplice_transport::datagram_pair(64).expect("test operation succeeds");
        for _ in 0..64 {
            application
                .send(b"queued")
                .await
                .expect("test operation succeeds");
        }
        let (business, mut remote) = tokio::io::duplex(1);
        let (shutdown_tx, mut shutdown) = watch::channel(false);
        let (network_tx, mut network) = watch::channel(0);
        let mut counts = (0, 0);
        {
            let operation = cancel_udp_association(
                &mut shutdown,
                &mut network,
                pump_udp_association(
                    business,
                    endpoint,
                    Uuid::new_v4(),
                    CODEC,
                    DEADLINE,
                    &mut counts,
                    |_, _, _| Ok(std::future::ready(())),
                ),
            );
            tokio::pin!(operation);
            // Reading one byte proves the selected handler entered its frame write.
            // The one-byte stream cannot fit the rest of even the frame header.
            let mut first_byte = [0; 1];
            timeout(DEADLINE, async {
                tokio::select! {
                    result = &mut operation => panic!("association ended before frame write: {result:?}"),
                    result = remote.read_exact(&mut first_byte) => { result.expect("frame started"); }
                }
            }).await.expect("pump did not begin frame write");
            assert_pending(operation.as_mut()).await;
            // The first datagram is in the blocked frame writer; refill its queue slot.
            application
                .send(b"refill")
                .await
                .expect("test operation succeeds");
            let blocked_send = application.send(b"blocked");
            tokio::pin!(blocked_send);
            assert_pending(blocked_send.as_mut()).await;
            if network_change {
                network_tx.send(1).expect("test operation succeeds");
            } else {
                shutdown_tx.send(true).expect("test operation succeeds");
            }
            timeout(DEADLINE, operation)
                .await
                .expect("test operation succeeds")
                .expect("test operation succeeds");
        }
        assert!(application.send(b"closed").await.is_err());
        assert_eq!(counts, (6, 0)); // Upload metric observes input, not remote delivery.
        let mut remaining = Vec::new();
        timeout(DEADLINE, remote.read_to_end(&mut remaining))
            .await
            .expect("test operation succeeds")
            .expect("test operation succeeds");
        assert_eq!(remaining.len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn udp_network_change_cancels_pending_carrier_open() {
    let (_shutdown_tx, mut shutdown) = watch::channel(false);
    let (network_tx, mut network) = watch::channel(0);
    let permits = Arc::new(Semaphore::new(1));
    let permit = permits
        .clone()
        .acquire_owned()
        .await
        .expect("test operation succeeds");
    let operation = async {
        let _permit = permit;
        std::future::pending::<Result<()>>().await
    };
    let operation = cancel_udp_association(&mut shutdown, &mut network, operation);
    tokio::pin!(operation);
    assert_pending(operation.as_mut()).await;
    network_tx.send(1).expect("test operation succeeds");
    timeout(DEADLINE, operation)
        .await
        .expect("test operation succeeds")
        .expect("test operation succeeds");
    assert_eq!(permits.available_permits(), 1);
}
