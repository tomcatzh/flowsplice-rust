//! Independent wire fixtures and boundary checks, including decoder-only builds.
use anyhow::{Context, Result};
use flowsplice_pty_protocol::{
    ClientMessage, MAX_FRAME_BYTES, MAX_WIRE_FRAME_BYTES, Operation, PROTOCOL_VERSION, Reply,
    ServerMessage, read_message, read_server_message, write_message,
};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}
fn output(data: Vec<u8>) -> ServerMessage {
    ServerMessage::Output {
        attachment_id: id(1),
        data,
    }
}
fn history(lines: Vec<String>) -> ServerMessage {
    ServerMessage::Response {
        request_id: id(2),
        result: Reply::History {
            attachment_id: id(1),
            capture_id: id(3),
            total_lines: 700,
            start: 444,
            columns: 80,
            lines,
        },
    }
}
// These helpers construct small, statically bounded test vectors; conversion
// failure is a fixture bug, rather than a recoverable production error.
#[allow(clippy::unwrap_used)]
fn frame(codec: u8, body: &[u8]) -> Vec<u8> {
    let mut wire = u32::try_from(body.len() + 1)
        .unwrap()
        .to_be_bytes()
        .to_vec();
    wire.push(codec);
    wire.extend_from_slice(body);
    wire
}
// Independent Snappy literal encoder, solely for malformed/policy test vectors.
// No product encoder is linked in the default-feature test run.
#[allow(clippy::unwrap_used)]
fn literal(body: &[u8]) -> Vec<u8> {
    assert!(!body.is_empty());
    let mut result = Vec::new();
    let mut n = body.len();
    while n >= 128 {
        result.push(u8::try_from(n & 127).unwrap() | 128);
        n >>= 7;
    }
    result.push(u8::try_from(n).unwrap());
    let length = body.len() - 1;
    if length < 60 {
        result.push(u8::try_from(length << 2).unwrap());
    } else {
        let bytes = length.to_le_bytes();
        let count = bytes.iter().rposition(|b| *b != 0).unwrap_or(0) + 1;
        result.push(u8::try_from((59 + count) << 2).unwrap());
        result.extend_from_slice(&bytes[..count]);
    }
    result.extend_from_slice(body);
    result
}

#[tokio::test]
async fn independent_fixed_output_fixture() -> Result<()> {
    // 90-byte JSON, length varint 0x5a, literal tag 0xf0, literal length minus one 0x59.
    const BLOCK: &[u8] = b"\x5a\xf0\x59{\"type\":\"output\",\"attachment_id\":\"00000000-0000-0000-0000-000000000001\",\"data\":[0,255,65]}";
    let wire = frame(1, BLOCK);
    assert_eq!(
        read_server_message(&mut wire.as_slice()).await?,
        Some(output(vec![0, 255, 65]))
    );
    Ok(())
}

#[tokio::test]
async fn chunked_mixed_frames_preserve_binary_unicode_and_metadata() -> Result<()> {
    let messages = [
        ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            can_write: true,
        },
        output(
            (0..=255)
                .chain("中文😀\u{0}".as_bytes().iter().copied())
                .collect(),
        ),
        history(vec![
            "\u{1b}[31m中文😀\u{1b}[0m".into(),
            "exact second row".into(),
        ]),
        ServerMessage::SessionEnded { session_id: id(8) },
    ];
    let mut wire = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        let json = serde_json::to_vec(message)?;
        wire.extend(if i == 1 || i == 2 {
            frame(1, &literal(&json))
        } else {
            frame(0, &json)
        });
    }
    let (mut tx, mut rx) = tokio::io::duplex(7);
    let producer = tokio::spawn(async move {
        for byte in wire {
            tx.write_all(&[byte]).await?;
            tokio::task::yield_now().await;
        }
        tx.shutdown().await
    });
    for expected in messages {
        assert_eq!(read_server_message(&mut rx).await?, Some(expected));
    }
    assert_eq!(read_server_message(&mut rx).await?, None);
    producer.await??;
    Ok(())
}

#[tokio::test]
async fn truncation_unknown_codec_and_snappy_corruption_are_errors() -> Result<()> {
    let valid = frame(1, &literal(&serde_json::to_vec(&output(vec![7; 100]))?));
    assert_eq!(read_server_message(&mut [].as_slice()).await?, None);
    for n in 1..valid.len() {
        assert!(
            read_server_message(&mut &valid[..n]).await.is_err(),
            "accepted truncated prefix {n}"
        );
    }
    let mut trailing = literal(&serde_json::to_vec(&output(vec![1]))?);
    trailing.push(0);
    for body in [vec![128], vec![1, 1, 0], vec![5, 16, b'x'], trailing] {
        assert!(
            read_server_message(&mut frame(1, &body).as_slice())
                .await
                .is_err()
        );
    }
    for codec in [2, 127, 255] {
        assert!(
            read_server_message(&mut frame(codec, b"{}").as_slice())
                .await
                .is_err()
        );
    }
    for length in [0, 1, u32::try_from(MAX_WIRE_FRAME_BYTES + 1)?, u32::MAX] {
        let header = length.to_be_bytes();
        let error = read_server_message(&mut header.as_slice())
            .await
            .err()
            .context("expected framing rejection")?;
        assert!(error.to_string().contains("invalid PTY frame length"));
    }
    // Declares u32::MAX decoded bytes in only five encoded bytes: rejected by size
    // before allocating a decoded buffer or requiring any literal/copy payload.
    let huge = frame(1, &[255, 255, 255, 255, 15]);
    assert!(
        read_server_message(&mut huge.as_slice())
            .await
            .err()
            .context("expected framing rejection")?
            .to_string()
            .contains("exceeds limit")
    );
    Ok(())
}

#[tokio::test]
async fn compressed_controls_and_upstream_are_rejected() -> Result<()> {
    let controls = [
        ServerMessage::Hello {
            version: PROTOCOL_VERSION,
            can_write: true,
        },
        ServerMessage::Response {
            request_id: id(1),
            result: Reply::Ok,
        },
        ServerMessage::Ownership {
            session_id: id(1),
            epoch: 1,
            writer: None,
        },
        ServerMessage::Detached {
            attachment_id: id(1),
            reason: "closed".into(),
        },
        ServerMessage::SessionEnded { session_id: id(1) },
    ];
    for message in controls {
        let wire = frame(1, &literal(&serde_json::to_vec(&message)?));
        assert!(
            read_server_message(&mut wire.as_slice())
                .await
                .err()
                .context("expected framing rejection")?
                .to_string()
                .contains("only permitted")
        );
    }
    let input = ClientMessage::Request {
        request_id: id(1),
        operation: Operation::Input {
            attachment_id: id(2),
            writer_epoch: 1,
            data: vec![65; 4096],
        },
    };
    let mut raw = Vec::new();
    write_message(&mut raw, &input).await?;
    assert_eq!(raw[4], 0);
    assert_eq!(
        read_message::<ClientMessage>(&mut raw.as_slice()).await?,
        Some(input.clone())
    );
    let compressed = frame(1, &literal(&serde_json::to_vec(&input)?));
    assert!(
        read_message::<ClientMessage>(&mut compressed.as_slice())
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn raw_maximum_is_accepted_and_overflow_writes_nothing() -> Result<()> {
    let message = "x".repeat(MAX_FRAME_BYTES - 2);
    let mut wire = Vec::new();
    write_message(&mut wire, &message).await?;
    assert_eq!(wire.len(), MAX_FRAME_BYTES + 5);
    assert_eq!(
        read_message::<String>(&mut wire.as_slice()).await?,
        Some(message)
    );
    let mut untouched = vec![42];
    assert!(
        write_message(&mut untouched, &"x".repeat(MAX_FRAME_BYTES - 1))
            .await
            .is_err()
    );
    assert_eq!(untouched, vec![42]);
    Ok(())
}

#[cfg(feature = "snappy-encode")]
mod encoder {
    use super::*;
    use flowsplice_pty_protocol::{MIN_SNAPPY_BYTES, write_server_message};
    async fn encoded(message: &ServerMessage) -> Result<Vec<u8>> {
        let mut wire = Vec::new();
        write_server_message(&mut wire, message).await?;
        Ok(wire)
    }
    #[allow(clippy::unwrap_used)]
    fn sized_output(size: usize) -> ServerMessage {
        let base = serde_json::to_vec(&output(vec![])).unwrap().len();
        // N zeroes cost 2N-1 bytes; a single 10 adds one byte for either parity.
        let n = (size - base).div_ceil(2);
        let mut bytes = vec![0; n];
        if base + 2 * n - 1 < size {
            bytes[0] = 10;
        }
        let result = output(bytes);
        assert_eq!(serde_json::to_vec(&result).unwrap().len(), size);
        result
    }
    #[tokio::test]
    async fn threshold_and_eligible_messages() -> Result<()> {
        for (size, codec) in [(MIN_SNAPPY_BYTES - 1, 0), (MIN_SNAPPY_BYTES, 1)] {
            let message = sized_output(size);
            let wire = encoded(&message).await?;
            assert_eq!(wire[4], codec);
            assert_eq!(
                read_server_message(&mut wire.as_slice()).await?,
                Some(message)
            );
        }
        let message = history(vec!["repeated history 中文".repeat(20); 16]);
        let wire = encoded(&message).await?;
        assert_eq!(wire[4], 1);
        assert!(wire.len() - 5 < serde_json::to_vec(&message)?.len());
        assert_eq!(
            read_server_message(&mut wire.as_slice()).await?,
            Some(message)
        );
        let small = history(vec!["short".into()]);
        assert_eq!(encoded(&small).await?[4], 0);
        for message in [
            ServerMessage::Detached {
                attachment_id: id(1),
                reason: "a".repeat(4096),
            },
            ServerMessage::Response {
                request_id: id(2),
                result: Reply::Error {
                    code: "error".into(),
                    message: "b".repeat(4096),
                },
            },
            ServerMessage::Response {
                request_id: id(2),
                result: Reply::Sessions { sessions: vec![] },
            },
            ServerMessage::Response {
                request_id: id(2),
                result: Reply::SessionDetails { sessions: vec![] },
            },
            ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                can_write: true,
            },
            ServerMessage::Ownership {
                session_id: id(1),
                epoch: 1,
                writer: None,
            },
            ServerMessage::SessionEnded { session_id: id(1) },
        ] {
            assert_eq!(encoded(&message).await?[4], 0);
        }
        Ok(())
    }
    #[tokio::test]
    async fn incompressible_history_uses_strictly_smaller_fallback() -> Result<()> {
        let mut state = 0x8c67_942e_a1d3_5b09_u64;
        let text: String = (0..65536)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // Uniform printable ASCII excluding JSON escapes quote/backslash.
                let byte = 33 + (state % 90).to_le_bytes()[0];
                char::from(match byte {
                    b'"' => b'|',
                    b'\\' => b'~',
                    other => other,
                })
            })
            .collect();
        let message = ServerMessage::Response {
            request_id: Uuid::parse_str("137ba064-523e-49fc-ad28-e97c0482f13b")?,
            result: Reply::History {
                attachment_id: Uuid::parse_str("84d1eb2f-0956-4c37-b2fa-617ae805943c")?,
                capture_id: Uuid::parse_str("a5039e8c-d612-47fb-8e64-23970ba5c1df")?,
                total_lines: 256,
                start: 0,
                columns: 80,
                lines: vec![text],
            },
        };
        let json = serde_json::to_vec(&message)?;
        assert!(
            flowsplice_pty_codec::compress(&json)?.len() >= json.len(),
            "fixture must stay incompressible: {} -> {}",
            json.len(),
            flowsplice_pty_codec::compress(&json)?.len()
        );
        let wire = encoded(&message).await?;
        assert_eq!(wire[4], 0);
        assert_eq!(&wire[5..], json);
        Ok(())
    }
    #[tokio::test]
    async fn compressed_decoded_boundary_and_prewrite_rejection() -> Result<()> {
        let message = sized_output(MAX_FRAME_BYTES);
        let wire = encoded(&message).await?;
        assert_eq!(wire[4], 1);
        assert_eq!(
            read_server_message(&mut wire.as_slice()).await?,
            Some(message)
        );
        let oversized = sized_output(MAX_FRAME_BYTES + 1);
        let mut untouched = vec![42];
        assert!(
            write_server_message(&mut untouched, &oversized)
                .await
                .is_err()
        );
        assert_eq!(untouched, vec![42]);
        let compressed = flowsplice_pty_codec::compress(&serde_json::to_vec(&oversized)?)?;
        assert!(
            read_server_message(&mut frame(1, &compressed).as_slice())
                .await
                .err()
                .context("expected framing rejection")?
                .to_string()
                .contains("exceeds limit")
        );
        Ok(())
    }
}
