//! PTY-only framing. The shared encrypted socket transports opaque bytes.

use anyhow::{Result, bail};
use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{MAX_FRAME_BYTES, MAX_WIRE_FRAME_BYTES, Reply, ServerMessage};

const RAW_JSON: u8 = 0;
const SNAPPY_JSON: u8 = 1;
/// Smaller serialized output/history messages bypass compression entirely.
pub const MIN_SNAPPY_BYTES: usize = 512;

fn compression_candidate(message: &ServerMessage) -> bool {
    matches!(
        message,
        ServerMessage::Output { .. }
            | ServerMessage::Response {
                result: Reply::History { .. },
                ..
            }
    )
}

async fn read_body(
    reader: &mut (impl AsyncRead + Unpin),
    allow_snappy: bool,
) -> Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0_u8; 4];
    if reader.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).await?;
    let length = usize::try_from(u32::from_be_bytes(header))?;
    if !(2..=MAX_WIRE_FRAME_BYTES).contains(&length) {
        bail!("invalid PTY frame length");
    }
    let codec = reader.read_u8().await?;
    if codec != RAW_JSON && (codec != SNAPPY_JSON || !allow_snappy) {
        bail!("unsupported PTY frame encoding");
    }
    let mut body = vec![0; length - 1];
    reader.read_exact(&mut body).await?;
    if codec == SNAPPY_JSON {
        // The codec checks Snappy's declared output size before allocation.
        body = flowsplice_pty_codec::decompress(&body, MAX_FRAME_BYTES)?;
    }
    Ok(Some((codec, body)))
}

/// Read one uncompressed PTY frame, rejecting compression on the upstream path.
/// Only EOF before the first header byte is a clean end. Callers retain schema
/// validation and authorization responsibilities.
///
/// # Errors
/// Returns invalid/truncated framing, unsupported encoding, I/O or JSON errors.
pub async fn read_message<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<T>> {
    let Some((_, body)) = read_body(reader, false).await? else {
        return Ok(None);
    };
    Ok(Some(serde_json::from_slice(&body)?))
}

/// Read a Home message, transparently decoding bounded Snappy output/history.
/// This is the same decoder on desktop and mobile; no encoder is needed here.
/// Callers retain schema validation and authorization responsibilities.
///
/// # Errors
/// Returns invalid/truncated framing, unknown encoding, malformed or oversized
/// compressed data, compressed control messages, I/O or JSON errors.
pub async fn read_server_message(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<ServerMessage>> {
    let Some((codec, body)) = read_body(reader, true).await? else {
        return Ok(None);
    };
    let message = serde_json::from_slice(&body)?;
    if codec == SNAPPY_JSON && !compression_candidate(&message) {
        bail!("compression is only permitted for PTY output and history");
    }
    Ok(Some(message))
}

fn serialize(message: &impl Serialize) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(message)?;
    if body.is_empty() || body.len() > MAX_FRAME_BYTES {
        bail!("invalid PTY frame length");
    }
    Ok(body)
}

async fn write_body(writer: &mut (impl AsyncWrite + Unpin), codec: u8, body: &[u8]) -> Result<()> {
    writer
        .write_all(&u32::try_from(body.len() + 1)?.to_be_bytes())
        .await?;
    writer.write_all(&[codec]).await?;
    writer.write_all(body).await?;
    writer.flush().await?;
    Ok(())
}

/// Serialize and flush one bounded, raw JSON frame. Client requests, terminal
/// input and all control messages use this path without running an encoder.
///
/// # Errors
/// Returns serialization, invalid length or I/O errors.
pub async fn write_message<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &T,
) -> Result<()> {
    write_body(writer, RAW_JSON, &serialize(message)?).await
}

/// Write a Home message, applying Snappy Level 1 only to eligible output/history
/// of at least 512 serialized bytes and only retaining a smaller encoded body.
/// Each frame is independent; reconnects have no compression state to recover.
///
/// # Errors
/// Returns serialization, invalid length, codec or I/O errors, before emitting a
/// header when validation or compression fails.
#[cfg(feature = "snappy-encode")]
pub async fn write_server_message(
    writer: &mut (impl AsyncWrite + Unpin),
    message: &ServerMessage,
) -> Result<()> {
    let body = serialize(message)?;
    if compression_candidate(message) && body.len() >= MIN_SNAPPY_BYTES {
        let compressed = flowsplice_pty_codec::compress(&body)?;
        if compressed.len() < body.len() {
            return write_body(writer, SNAPPY_JSON, &compressed).await;
        }
    }
    write_body(writer, RAW_JSON, &body).await
}
