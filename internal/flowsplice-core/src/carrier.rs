//! Shared bidirectional business-data carrier pump.

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::mpsc,
    time::{Instant, MissedTickBehavior, interval},
};

use crate::{
    DATA_FRAME_LIMIT,
    frame::{DataFrameCodec, DataFrameReader, write_data_frame},
    protocol::DataFrame,
};

/// Pumps carrier frames while keeping outbound progress independent of event backpressure.
///
/// At most one inbound business event is held pending. While its receiver is full, outbound
/// frames and heartbeat ticks remain eligible for processing, and further reads are paused.
/// This function does not spawn tasks or emit a lifecycle event. The caller owns cancellation
/// and must discard the socket afterward: cancellation during a write can leave a partial frame.
///
/// # Errors
///
/// Returns an error on transport read/write failure, heartbeat timeout, or a closed event
/// receiver when delivering a business frame. A drained, closed outbound channel ends normally.
pub async fn run_data_carrier<S, E, F>(
    stream: S,
    outgoing: &mut mpsc::Receiver<DataFrame>,
    events: &mpsc::Sender<E>,
    wrap: F,
    codec: DataFrameCodec,
    heartbeat_period: Duration,
    timeout_period: Duration,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
    F: Fn(DataFrame) -> E,
{
    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = DataFrameReader::new(reader, DATA_FRAME_LIMIT, codec);
    let mut heartbeat = interval(heartbeat_period);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut nonce = 0_u64;
    let mut last_received = Instant::now();
    let mut pending = None;
    loop {
        tokio::select! {
            permit = events.reserve(), if pending.is_some() => {
                let permit = permit.map_err(|_| anyhow!("carrier flow event receiver closed"))?;
                if let Some(event) = pending.take() {
                    permit.send(event);
                }
            }
            frame = outgoing.recv() => {
                let Some(frame) = frame else { return Ok(()); };
                write_data_frame(&mut writer, &frame, DATA_FRAME_LIMIT, codec).await?;
            }
            frame = reader.read(), if pending.is_none() => {
                let frame = frame?;
                last_received = Instant::now();
                match frame {
                    DataFrame::Ping { nonce } => {
                        write_data_frame(&mut writer, &DataFrame::Pong { nonce }, DATA_FRAME_LIMIT, codec).await?;
                    }
                    DataFrame::Pong { .. } => {}
                    frame => pending = Some(wrap(frame)),
                }
            }
            _ = heartbeat.tick() => {
                if last_received.elapsed() > timeout_period {
                    bail!("carrier heartbeat timed out");
                }
                nonce = nonce.wrapping_add(1);
                write_data_frame(&mut writer, &DataFrame::Ping { nonce }, DATA_FRAME_LIMIT, codec).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tokio::{io::DuplexStream, time::timeout};
    use uuid::Uuid;

    const CODEC: DataFrameCodec = DataFrameCodec::BinaryV1;
    const DEADLINE: Duration = Duration::from_secs(5);
    const HEARTBEAT: Duration = Duration::from_secs(60);

    fn data(offset: u64) -> DataFrame {
        DataFrame::Data {
            flow_id: Uuid::nil(),
            offset,
            bytes: Bytes::from(offset.to_be_bytes().to_vec()),
        }
    }

    fn assert_data(frame: DataFrame, expected: u64) -> Result<()> {
        let DataFrame::Data {
            flow_id,
            offset,
            bytes,
        } = frame
        else {
            bail!("expected data frame");
        };
        assert_eq!(flow_id, Uuid::nil());
        assert_eq!(offset, expected);
        assert_eq!(bytes.as_ref(), expected.to_be_bytes());
        Ok(())
    }

    async fn business_frame<R: AsyncRead + Unpin>(
        reader: &mut DataFrameReader<R>,
    ) -> Result<DataFrame> {
        loop {
            let frame = reader.read().await?;
            if !matches!(frame, DataFrame::Ping { .. } | DataFrame::Pong { .. }) {
                return Ok(frame);
            }
        }
    }

    fn pump(
        stream: DuplexStream,
        mut outgoing: mpsc::Receiver<DataFrame>,
        events: mpsc::Sender<DataFrame>,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            run_data_carrier(
                stream,
                &mut outgoing,
                &events,
                |frame| frame,
                CODEC,
                HEARTBEAT,
                HEARTBEAT,
            )
            .await
        })
    }

    #[tokio::test]
    async fn full_inbound_events_do_not_block_outbound_frames() -> Result<()> {
        timeout(DEADLINE, async {
            let (stream, peer) = tokio::io::duplex(4096);
            let (out_tx, mut out_rx) = mpsc::channel(2);
            let (event_tx, mut event_rx) = mpsc::channel(1);
            let (observed_tx, mut observed_rx) = mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                run_data_carrier(
                    stream,
                    &mut out_rx,
                    &event_tx,
                    |frame| {
                        let _ = observed_tx.send(());
                        frame
                    },
                    CODEC,
                    HEARTBEAT,
                    HEARTBEAT,
                )
                .await
            });
            let (read, mut write) = tokio::io::split(peer);
            let mut reader = DataFrameReader::new(read, DATA_FRAME_LIMIT, CODEC);
            for offset in 0..2 {
                write_data_frame(&mut write, &data(offset), DATA_FRAME_LIMIT, CODEC).await?;
                observed_rx
                    .recv()
                    .await
                    .ok_or_else(|| anyhow!("frame fully read and wrapped"))?;
            }
            // Reading the second frame requires the first event to have been delivered. The
            // second wrap is therefore an explicit barrier proving a full event queue plus
            // a pending frame; no event is consumed until every outbound frame arrives.
            let send = async {
                for offset in 10..20 {
                    out_tx.send(data(offset)).await?;
                }
                Ok::<(), anyhow::Error>(())
            };
            let receive = async {
                for offset in 10..20 {
                    assert_data(business_frame(&mut reader).await?, offset)?;
                }
                Ok::<(), anyhow::Error>(())
            };
            tokio::try_join!(send, receive)?;
            for offset in 0..2 {
                assert_data(
                    event_rx
                        .recv()
                        .await
                        .ok_or_else(|| anyhow!("inbound event"))?,
                    offset,
                )?;
            }
            drop(out_tx);
            task.await??;
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn original_queue_capacities_preserve_bidirectional_frames() -> Result<()> {
        timeout(DEADLINE, async {
            let (stream, peer) = tokio::io::duplex(4096);
            let (out_tx, out_rx) = mpsc::channel(128);
            let (event_tx, mut event_rx) = mpsc::channel(32);
            let task = pump(stream, out_rx, event_tx);
            let (read, mut write) = tokio::io::split(peer);
            let mut reader = DataFrameReader::new(read, DATA_FRAME_LIMIT, CODEC);
            let outbound = async {
                for offset in 0..300 {
                    out_tx.send(data(offset)).await?;
                }
                Ok::<(), anyhow::Error>(())
            };
            let inbound = async {
                for offset in 0..300 {
                    write_data_frame(&mut write, &data(offset), DATA_FRAME_LIMIT, CODEC).await?;
                }
                Ok::<(), anyhow::Error>(())
            };
            let receive_outbound = async {
                for offset in 0..300 {
                    assert_data(business_frame(&mut reader).await?, offset)?;
                }
                Ok::<(), anyhow::Error>(())
            };
            let receive_inbound = async {
                for offset in 0..300 {
                    assert_data(
                        event_rx
                            .recv()
                            .await
                            .ok_or_else(|| anyhow!("inbound event"))?,
                        offset,
                    )?;
                }
                Ok::<(), anyhow::Error>(())
            };
            tokio::try_join!(outbound, inbound, receive_outbound, receive_inbound)?;
            drop(out_tx);
            task.await??;
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    #[tokio::test]
    async fn closed_outgoing_drains_final_fin_ack() -> Result<()> {
        timeout(DEADLINE, async {
            let (stream, peer) = tokio::io::duplex(4096);
            let (out_tx, out_rx) = mpsc::channel(2);
            let (event_tx, _event_rx) = mpsc::channel(1);
            out_tx.send(data(0)).await?;
            out_tx.send(DataFrame::FinAck { flow_id: Uuid::nil(), final_offset: 8 }).await?;

            drop(out_tx);
            let task = pump(stream, out_rx, event_tx);
            let mut reader = DataFrameReader::new(peer, DATA_FRAME_LIMIT, CODEC);
            assert_data(business_frame(&mut reader).await?, 0)?;
            assert!(matches!(business_frame(&mut reader).await?, DataFrame::FinAck { flow_id, final_offset: 8 } if flow_id == Uuid::nil()));
            task.await??;
            Ok::<(), anyhow::Error>(())
        }).await?
    }

    #[tokio::test]
    async fn closed_event_receiver_rejects_business_frame() -> Result<()> {
        timeout(DEADLINE, async {
            let (stream, mut peer) = tokio::io::duplex(4096);
            let (_out_tx, out_rx) = mpsc::channel(2);
            let (event_tx, event_rx) = mpsc::channel(1);
            drop(event_rx);
            let task = pump(stream, out_rx, event_tx);
            write_data_frame(&mut peer, &data(0), DATA_FRAME_LIMIT, CODEC).await?;
            assert!(task.await?.is_err());
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }
}
