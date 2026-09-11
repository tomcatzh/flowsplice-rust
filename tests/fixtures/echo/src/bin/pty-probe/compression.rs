//! Observe actual application frames inside a disposable encrypted business connection.
use super::{Probe, attachment, print_command};
use anyhow::{Context, Result, ensure};
use flowsplice_enrollment::load_json;
use flowsplice_pty_protocol::{
    ClientMessage, MAX_FRAME_BYTES, Mode, Operation, PROTOCOL_VERSION, Reply, ServerMessage,
    read_server_message, write_message,
};
use flowsplice_travel_core::TravelCore;
use std::{path::Path, time::Duration};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

struct Frame {
    message: ServerMessage,
    codec: u8,
    wire_bytes: usize,
    decoded_bytes: usize,
}
async fn receive(stream: &mut (impl AsyncRead + Unpin)) -> Result<Frame> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let length = usize::try_from(stream.read_u32().await?)?;
        ensure!(
            (1..=MAX_FRAME_BYTES + 1).contains(&length),
            "unbounded PTY frame"
        );
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await?;
        let codec = body[0];
        let mut captured = u32::try_from(length)?.to_be_bytes().to_vec();
        captured.extend_from_slice(&body);
        let message = read_server_message(&mut captured.as_slice())
            .await?
            .context("missing captured frame")?;
        let decoded_bytes = serde_json::to_vec(&message)?.len();
        ensure!(
            decoded_bytes <= MAX_FRAME_BYTES,
            "decoded frame exceeds limit"
        );
        if codec == 1 {
            ensure!(
                length - 1 < decoded_bytes,
                "compressed frame did not reduce bytes"
            );
            ensure!(
                matches!(
                    &message,
                    ServerMessage::Output { .. }
                        | ServerMessage::Response {
                            result: Reply::History { .. },
                            ..
                        }
                ),
                "control message compressed"
            );
        } else {
            ensure!(codec == 0, "unknown codec");
        }
        Ok(Frame {
            message,
            codec,
            wire_bytes: length - 1,
            decoded_bytes,
        })
    })
    .await
    .context("wire frame timed out")?
}
async fn send(stream: &mut (impl AsyncWrite + Unpin), message: &ClientMessage) -> Result<()> {
    let mut captured = Vec::new();
    write_message(&mut captured, message).await?;
    ensure!(
        captured.get(4) == Some(&0),
        "client control/input was compressed"
    );
    let decoded: ClientMessage = serde_json::from_slice(&captured[5..])?;
    ensure!(decoded == *message, "raw client frame changed content");
    stream.write_all(&captured).await?;
    stream.flush().await?;
    Ok(())
}
async fn request(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    operation: Operation,
) -> Result<Frame> {
    let request_id = Uuid::new_v4();
    send(
        stream,
        &ClientMessage::Request {
            request_id,
            operation,
        },
    )
    .await?;
    for _ in 0..2048 {
        let frame = receive(stream).await?;
        if matches!(&frame.message, ServerMessage::Response { request_id: id, .. } if *id == request_id)
        {
            return Ok(frame);
        }
    }
    anyhow::bail!("response exceeded bounded event count")
}

pub(super) async fn exercise(
    args: &[String],
    writer: &mut Probe,
    session: Uuid,
    writer_attachment: Uuid,
    epoch: u64,
) -> Result<()> {
    let password = std::fs::read_to_string(&args[2])?;
    let root = std::fs::read_to_string(&args[3])?;
    let descriptor = load_json(Path::new(&args[4]))?;
    let (runtime, approved) = TravelCore::start_business(
        Path::new(&args[1]),
        password.trim_end(),
        root.trim(),
        &descriptor,
    )
    .await?;
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        let mut stream = runtime.connect_tcp(&approved.binding).await?;
        let result = exercise_stream(&mut stream, writer, session, writer_attachment, epoch).await;
        let closed = stream.shutdown().await;
        result?;
        closed?;
        Ok::<_, anyhow::Error>(())
    })
    .await
    .context("compression wire exercise timed out");
    runtime.shutdown().await;
    result??;
    println!("{{\"checkpoint\":\"encrypted-pty-snappy-wire-complete\"}}");
    Ok(())
}

// Keep the ordered wire acceptance sequence together, like the parent probe.
#[allow(clippy::too_many_lines)]
async fn exercise_stream(
    stream: &mut (impl AsyncRead + AsyncWrite + Unpin),
    writer: &mut Probe,
    session: Uuid,
    writer_attachment: Uuid,
    epoch: u64,
) -> Result<()> {
    send(
        stream,
        &ClientMessage::Hello {
            version: PROTOCOL_VERSION,
            label: "compression-observer".into(),
        },
    )
    .await?;
    let hello = receive(stream).await?;
    ensure!(
        hello.codec == 0
            && matches!(
                hello.message,
                ServerMessage::Hello {
                    version: PROTOCOL_VERSION,
                    ..
                }
            ),
        "Hello must be raw current version"
    );
    let list = request(stream, Operation::List).await?;
    ensure!(
        list.codec == 0
            && matches!(&list.message, ServerMessage::Response { result: Reply::Sessions { sessions }, .. } if sessions.iter().any(|s| s.id == session)),
        "raw session list missing"
    );
    let joined = request(
        stream,
        Operation::Join {
            session_id: session,
            mode: Mode::ReadOnly,
            columns: 80,
            rows: 24,
        },
    )
    .await?;
    ensure!(joined.codec == 0, "Join response compressed");
    let ServerMessage::Response { result, .. } = joined.message else {
        anyhow::bail!("Join reply missing")
    };
    let (_, observer, mode, _) = attachment(result)?;
    ensure!(mode == Mode::ReadOnly, "observer changed writer");
    let denied = request(
        stream,
        Operation::Input {
            attachment_id: observer,
            writer_epoch: epoch,
            data: b"forbidden\r".to_vec(),
        },
    )
    .await?;
    ensure!(
        denied.codec == 0
            && matches!(
                denied.message,
                ServerMessage::Response {
                    result: Reply::Error { .. },
                    ..
                }
            ),
        "observer input not denied with raw reply"
    );
    let done = format!("SNAPPY-DONE-{}", Uuid::new_v4());
    let command = format!(
        "i=0; while [ $i -lt 600 ]; do printf 'SNAPPYROW%04d-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\n' \"$i\"; i=$((i+1)); done; {}",
        print_command(&done)
    );
    writer.input(writer_attachment, epoch, &command).await?;
    let mut output = Vec::new();
    let mut compressed_output = 0;
    let mut saved = 0;
    for _ in 0..2048 {
        let frame = receive(stream).await?;
        if let ServerMessage::Output { data, .. } = frame.message {
            if frame.codec == 1 && data.windows(9).any(|w| w == b"SNAPPYROW") {
                compressed_output += 1;
                saved += frame.decoded_bytes - frame.wire_bytes;
            }
            output.extend(data);
            ensure!(
                output.len() <= 1024 * 1024,
                "output observation exceeded bound"
            );
            if String::from_utf8_lossy(&output).contains(&done) {
                break;
            }
        }
    }
    ensure!(
        compressed_output > 0 && saved > 0 && String::from_utf8_lossy(&output).contains(&done),
        "actual generated compressed output missing"
    );
    writer.text(&done).await?;
    let capture_id = Uuid::new_v4();
    let mut before = None;
    let mut rows = Vec::new();
    let mut compressed_history = 0;
    for _ in 0..16 {
        let frame = request(
            stream,
            Operation::History {
                attachment_id: observer,
                capture_id,
                before,
            },
        )
        .await?;
        let ServerMessage::Response {
            result: Reply::History { start, lines, .. },
            ..
        } = frame.message
        else {
            anyhow::bail!("history reply missing")
        };
        if lines.len() == 256 {
            ensure!(
                frame.codec == 1 && frame.wire_bytes < frame.decoded_bytes,
                "full history page not compressed"
            );
            compressed_history += 1;
        }
        rows.splice(0..0, lines);
        if start == 0 {
            break;
        }
        before = Some(start);
    }
    let numbers: Vec<u32> = rows
        .iter()
        .filter_map(|line| line.split("SNAPPYROW").nth(1)?.get(..4)?.parse().ok())
        .collect();
    ensure!(
        compressed_history > 0 && numbers == (0..600).collect::<Vec<_>>(),
        "compressed history lost or duplicated exact rows"
    );
    let detached = request(
        stream,
        Operation::Detach {
            attachment_id: observer,
        },
    )
    .await?;
    ensure!(
        detached.codec == 0
            && matches!(
                detached.message,
                ServerMessage::Response {
                    result: Reply::Ok,
                    ..
                }
            ),
        "raw detach failed"
    );
    Ok(())
}
