use std::{io, time::Duration};

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use uuid::Uuid;

use crate::{
    MAX_DATA_PAYLOAD,
    protocol::{DATA_PROTOCOL_BINARY_V1, DataFrame},
};

const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Stateful bounded JSON frame reader that preserves partial input when a read future is cancelled.
///
/// Tokio's `read_exact` is not cancellation safe. This reader deliberately advances its internal
/// prefix and payload cursors only after cancellation-safe `read` calls complete, so `read` may be
/// used inside `tokio::select!` and resumed after another branch wins.
pub struct JsonFrameReader<R> {
    inner: LengthPrefixedReader<R>,
}

struct LengthPrefixedReader<R> {
    reader: R,
    limit: usize,
    prefix: [u8; 4],
    prefix_read: usize,
    payload: Vec<u8>,
    payload_read: usize,
}

impl<R> JsonFrameReader<R>
where
    R: AsyncRead + Unpin,
{
    #[must_use]
    pub fn new(reader: R, limit: usize) -> Self {
        Self {
            inner: LengthPrefixedReader::new(reader, limit),
        }
    }

    /// Reads one frame while retaining partial progress across cancellation.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for truncated input, an invalid length, or invalid JSON.
    pub async fn read<T>(&mut self) -> io::Result<T>
    where
        T: DeserializeOwned,
    {
        let result = serde_json::from_slice(self.inner.read_payload().await?)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        self.inner.finish_frame();
        result
    }

    /// Reads one frame and fails if the complete frame does not arrive before `deadline`.
    ///
    /// # Errors
    ///
    /// Returns `TimedOut` on deadline expiry or the underlying frame error.
    pub async fn read_with_timeout<T>(&mut self, deadline: Duration) -> io::Result<T>
    where
        T: DeserializeOwned,
    {
        timeout(deadline, self.read())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "JSON frame read timed out"))?
    }
}

impl<R> LengthPrefixedReader<R>
where
    R: AsyncRead + Unpin,
{
    fn new(reader: R, limit: usize) -> Self {
        Self {
            reader,
            limit,
            prefix: [0_u8; 4],
            prefix_read: 0,
            payload: Vec::new(),
            payload_read: 0,
        }
    }

    async fn read_payload(&mut self) -> io::Result<&[u8]> {
        while self.prefix_read < self.prefix.len() {
            let count = self
                .reader
                .read(&mut self.prefix[self.prefix_read..])
                .await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed during frame length",
                ));
            }
            self.prefix_read += count;
        }

        if self.payload.is_empty() {
            let len = u32::from_be_bytes(self.prefix) as usize;
            if len == 0 || len > self.limit {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("frame length {len} is outside 1..={}", self.limit),
                ));
            }
            self.payload.resize(len, 0);
        }

        while self.payload_read < self.payload.len() {
            let count = self
                .reader
                .read(&mut self.payload[self.payload_read..])
                .await?;
            if count == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed during frame payload",
                ));
            }
            self.payload_read += count;
        }
        Ok(&self.payload)
    }

    fn finish_frame(&mut self) {
        self.prefix = [0_u8; 4];
        self.prefix_read = 0;
        self.payload.clear();
        self.payload_read = 0;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DataFrameCodec {
    Json,
    BinaryV1,
}

impl DataFrameCodec {
    #[must_use]
    pub const fn negotiate(peer_version: u16) -> Self {
        if peer_version >= DATA_PROTOCOL_BINARY_V1 {
            Self::BinaryV1
        } else {
            Self::Json
        }
    }

    #[must_use]
    pub const fn version(self) -> u16 {
        match self {
            Self::Json => 0,
            Self::BinaryV1 => DATA_PROTOCOL_BINARY_V1,
        }
    }
}

pub struct DataFrameReader<R> {
    inner: LengthPrefixedReader<R>,
    codec: DataFrameCodec,
}

impl<R> DataFrameReader<R>
where
    R: AsyncRead + Unpin,
{
    #[must_use]
    pub fn new(reader: R, limit: usize, codec: DataFrameCodec) -> Self {
        Self {
            inner: LengthPrefixedReader::new(reader, limit),
            codec,
        }
    }

    /// Reads and decodes one negotiated business-data frame.
    ///
    /// # Errors
    ///
    /// Returns an I/O error for truncated, oversized, malformed, or unknown frames.
    pub async fn read(&mut self) -> io::Result<DataFrame> {
        let result = decode_data_frame(self.inner.read_payload().await?, self.codec);
        self.inner.finish_frame();
        result
    }
}

fn decode_data_frame(payload: &[u8], codec: DataFrameCodec) -> io::Result<DataFrame> {
    if codec == DataFrameCodec::Json {
        return serde_json::from_slice(payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
    }
    let Some((&marker, body)) = payload.split_first() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "empty data frame",
        ));
    };
    match marker {
        0 => serde_json::from_slice(body)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        1 | 2 => {
            if body.len() < 24 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "binary data frame header is truncated",
                ));
            }
            let flow_id = Uuid::from_bytes(
                body[..16]
                    .try_into()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid flow id"))?,
            );
            let position = u64::from_be_bytes(
                body[16..24]
                    .try_into()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid position"))?,
            );
            let bytes = &body[24..];
            if bytes.len() > MAX_DATA_PAYLOAD {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "binary data payload exceeds the configured limit",
                ));
            }
            let bytes = bytes::Bytes::copy_from_slice(bytes);
            if marker == 1 {
                Ok(DataFrame::Data {
                    flow_id,
                    offset: position,
                    bytes,
                })
            } else {
                Ok(DataFrame::Datagram {
                    flow_id,
                    sequence: position,
                    bytes,
                })
            }
        }
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown binary data frame marker",
        )),
    }
}

/// Writes one bounded, length-prefixed JSON frame.
///
/// # Errors
///
/// Returns `TimedOut` if the complete write exceeds the default 10-second deadline, or an I/O
/// error when serialization fails, the result exceeds `limit`, or writing fails.
///
/// A write error or timeout can leave a partial frame on `writer`; callers must discard the
/// transport after an error rather than write another frame.
pub async fn write_json<W, T>(writer: &mut W, value: &T, limit: usize) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_json_with_timeout(writer, value, limit, DEFAULT_WRITE_TIMEOUT).await
}

async fn write_json_inner<W, T>(writer: &mut W, value: &T, limit: usize) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let payload = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    write_payload(writer, payload, limit).await
}

async fn write_payload<W>(writer: &mut W, payload: Vec<u8>, limit: usize) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if payload.is_empty() || payload.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded frame length {} is outside 1..={limit}",
                payload.len()
            ),
        ));
    }
    let len = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame length overflow"))?;
    let mut frame = Vec::with_capacity(payload.len() + 4);
    frame.extend_from_slice(&len.to_be_bytes());
    frame.extend_from_slice(&payload);
    writer.write_all(&frame).await?;
    writer.flush().await
}

/// Writes one business-data frame using the negotiated codec.
///
/// # Errors
///
/// Returns `TimedOut` if the complete write exceeds the default 10-second deadline, or an I/O
/// error when encoding exceeds a bound or the destination write fails.
///
/// A write error or timeout can leave a partial frame on `writer`; callers must discard the
/// transport after an error rather than write another frame.
pub async fn write_data_frame<W>(
    writer: &mut W,
    value: &DataFrame,
    limit: usize,
    codec: DataFrameCodec,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    write_data_frame_with_timeout(writer, value, limit, codec, DEFAULT_WRITE_TIMEOUT).await
}

async fn write_data_frame_inner<W>(
    writer: &mut W,
    value: &DataFrame,
    limit: usize,
    codec: DataFrameCodec,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = match (codec, value) {
        (
            DataFrameCodec::BinaryV1,
            DataFrame::Data {
                flow_id,
                offset,
                bytes,
            },
        ) => encode_binary_payload(1, flow_id, *offset, bytes)?,
        (
            DataFrameCodec::BinaryV1,
            DataFrame::Datagram {
                flow_id,
                sequence,
                bytes,
            },
        ) => encode_binary_payload(2, flow_id, *sequence, bytes)?,
        (DataFrameCodec::BinaryV1, value) => {
            let json = serde_json::to_vec(value)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
            let mut payload = Vec::with_capacity(json.len() + 1);
            payload.push(0);
            payload.extend_from_slice(&json);
            payload
        }
        (DataFrameCodec::Json, value) => serde_json::to_vec(value)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
    };
    write_payload(writer, payload, limit).await
}

fn encode_binary_payload(
    marker: u8,
    flow_id: &Uuid,
    position: u64,
    bytes: &[u8],
) -> io::Result<Vec<u8>> {
    if bytes.len() > MAX_DATA_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "binary data payload exceeds the configured limit",
        ));
    }
    let mut payload = Vec::with_capacity(bytes.len() + 25);
    payload.push(marker);
    payload.extend_from_slice(flow_id.as_bytes());
    payload.extend_from_slice(&position.to_be_bytes());
    payload.extend_from_slice(bytes);
    Ok(payload)
}

/// Writes one bounded JSON frame and fails when the complete write exceeds `deadline`.
///
/// # Errors
///
/// Returns `TimedOut` on deadline expiry or the underlying serialization/write error.
/// A write error or timeout can leave a partial frame on `writer`; callers must discard the
/// transport after an error rather than write another frame.
pub async fn write_json_with_timeout<W, T>(
    writer: &mut W,
    value: &T,
    limit: usize,
    deadline: Duration,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    timeout(deadline, write_json_inner(writer, value, limit))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "JSON frame write timed out"))?
}

/// Writes one business-data frame and fails when the complete write exceeds `deadline`.
///
/// # Errors
///
/// Returns `TimedOut` on deadline expiry or the underlying encoding/write error.
/// A write error or timeout can leave a partial frame on `writer`; callers must discard the
/// transport after an error rather than write another frame.
pub async fn write_data_frame_with_timeout<W>(
    writer: &mut W,
    value: &DataFrame,
    limit: usize,
    codec: DataFrameCodec,
    deadline: Duration,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    timeout(
        deadline,
        write_data_frame_inner(writer, value, limit, codec),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "data frame write timed out"))?
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use crate::protocol::DataFrame;
    use bytes::Bytes;
    use serde::{Deserialize, Serialize};
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };

    use tokio::{
        io::{AsyncWrite, duplex},
        time::timeout,
    };

    use super::{
        DataFrameCodec, DataFrameReader, JsonFrameReader, encode_binary_payload, write_data_frame,
        write_data_frame_with_timeout, write_json,
    };
    use uuid::Uuid;

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct Message {
        value: String,
    }

    #[derive(Default)]
    struct PartialThenStallWriter {
        bytes: Vec<u8>,
    }

    impl AsyncWrite for PartialThenStallWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            if buf.is_empty() {
                return Poll::Ready(Ok(0));
            }
            if this.bytes.is_empty() {
                this.bytes.push(buf[0]);
                Poll::Ready(Ok(1))
            } else {
                Poll::Pending
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Default)]
    struct FlushStallWriter {
        bytes: Vec<u8>,
        flush_called: bool,
    }

    impl AsyncWrite for FlushStallWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.bytes.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.get_mut().flush_called = true;
            Poll::Pending
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn round_trip() {
        let (mut left, mut right) = duplex(1024);
        let send = Message {
            value: "hello".to_owned(),
        };
        let writer = tokio::spawn(async move { write_json(&mut left, &send, 1024).await });
        let received: Message = JsonFrameReader::new(&mut right, 1024).read().await.unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(received.value, "hello");
    }

    #[tokio::test]
    async fn rejects_oversize_length_before_allocation() {
        use tokio::io::AsyncWriteExt;

        let (mut left, mut right) = duplex(16);
        left.write_u32(2048).await.unwrap();
        let error = JsonFrameReader::new(&mut right, 1024)
            .read::<Message>()
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn stateful_reader_preserves_partial_frame_across_cancellation() {
        use tokio::io::AsyncWriteExt;

        let (mut left, right) = duplex(1024);
        let payload = serde_json::to_vec(&Message {
            value: "resumed".to_owned(),
        })
        .unwrap();
        let prefix = u32::try_from(payload.len()).unwrap().to_be_bytes();
        left.write_all(&prefix[..2]).await.unwrap();

        let mut reader = JsonFrameReader::new(right, 1024);
        let timed_out = timeout(Duration::from_millis(20), reader.read::<Message>()).await;
        assert!(timed_out.is_err());

        left.write_all(&prefix[2..]).await.unwrap();
        left.write_all(&payload).await.unwrap();
        let received = reader.read::<Message>().await.unwrap();
        assert_eq!(received.value, "resumed");
    }

    #[tokio::test]
    async fn read_deadline_rejects_slow_loris_prefix() {
        use tokio::io::AsyncWriteExt;

        let (mut left, right) = duplex(16);
        left.write_all(&[0, 0]).await.unwrap();
        let mut reader = JsonFrameReader::new(right, 1024);
        let error = reader
            .read_with_timeout::<Message>(Duration::from_millis(20))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn data_frame_write_deadline_rejects_backpressure_for_json_and_binary_v1() {
        let frame = DataFrame::Data {
            flow_id: Uuid::new_v4(),
            offset: 42,
            bytes: Bytes::from_static(b"backpressure"),
        };

        for codec in [DataFrameCodec::Json, DataFrameCodec::BinaryV1] {
            let mut writer = PartialThenStallWriter::default();
            let error = write_data_frame_with_timeout(
                &mut writer,
                &frame,
                1024,
                codec,
                Duration::from_millis(20),
            )
            .await
            .unwrap_err();

            assert_eq!(error.kind(), io::ErrorKind::TimedOut);
            assert_eq!(writer.bytes.len(), 1, "{codec:?} should partially write");
        }
    }

    #[tokio::test]
    async fn data_frame_write_deadline_includes_flush() {
        let frame = DataFrame::Data {
            flow_id: Uuid::new_v4(),
            offset: 7,
            bytes: Bytes::from_static(b"flush"),
        };
        let mut writer = FlushStallWriter::default();

        let error = write_data_frame_with_timeout(
            &mut writer,
            &frame,
            1024,
            DataFrameCodec::BinaryV1,
            Duration::from_millis(20),
        )
        .await
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(writer.flush_called);
        assert!(!writer.bytes.is_empty());
    }

    #[tokio::test]
    async fn binary_data_round_trip_preserves_every_byte_value() {
        let flow_id = Uuid::new_v4();
        let bytes = Bytes::from((0_u8..=u8::MAX).cycle().take(65_536).collect::<Vec<_>>());
        let frame = DataFrame::Data {
            flow_id,
            offset: 42,
            bytes: bytes.clone(),
        };
        let (mut left, mut right) = duplex(128 * 1024);
        let writer = tokio::spawn(async move {
            write_data_frame(&mut left, &frame, 1024 * 1024, DataFrameCodec::BinaryV1).await
        });
        let received = DataFrameReader::new(&mut right, 1024 * 1024, DataFrameCodec::BinaryV1)
            .read()
            .await
            .unwrap();
        writer.await.unwrap().unwrap();
        match received {
            DataFrame::Data {
                flow_id: received_id,
                offset,
                bytes: received_bytes,
            } => {
                assert_eq!(received_id, flow_id);
                assert_eq!(offset, 42);
                assert_eq!(received_bytes, bytes);
            }
            _ => panic!("wrong data frame variant"),
        }
    }

    #[tokio::test]
    async fn binary_datagram_and_control_frames_round_trip() {
        let flow_id = Uuid::new_v4();
        let frames = [
            DataFrame::Datagram {
                flow_id,
                sequence: 7,
                bytes: Bytes::from_static(b"datagram"),
            },
            DataFrame::Ping { nonce: 9 },
        ];
        let (mut left, mut right) = duplex(4096);
        let writer = tokio::spawn(async move {
            for frame in frames {
                write_data_frame(&mut left, &frame, 4096, DataFrameCodec::BinaryV1).await?;
            }
            Ok::<_, std::io::Error>(())
        });
        let mut reader = DataFrameReader::new(&mut right, 4096, DataFrameCodec::BinaryV1);
        match reader.read().await.unwrap() {
            DataFrame::Datagram {
                sequence, bytes, ..
            } => {
                assert_eq!(sequence, 7);
                assert_eq!(bytes, Bytes::from_static(b"datagram"));
            }
            _ => panic!("wrong datagram frame variant"),
        }
        match reader.read().await.unwrap() {
            DataFrame::Ping { nonce } => assert_eq!(nonce, 9),
            _ => panic!("wrong control frame variant"),
        }
        writer.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn json_codec_remains_compatible_with_existing_frames() {
        let frame = DataFrame::Data {
            flow_id: Uuid::new_v4(),
            offset: 0,
            bytes: Bytes::from_static(b"legacy"),
        };
        let (mut left, mut right) = duplex(4096);
        let writer = tokio::spawn(async move {
            write_data_frame(&mut left, &frame, 4096, DataFrameCodec::Json).await
        });
        match JsonFrameReader::new(&mut right, 4096)
            .read::<DataFrame>()
            .await
            .unwrap()
        {
            DataFrame::Data { bytes, .. } => assert_eq!(bytes, Bytes::from_static(b"legacy")),
            _ => panic!("wrong JSON data frame variant"),
        }
        writer.await.unwrap().unwrap();
    }

    #[test]
    fn binary_encoding_avoids_json_number_array_amplification() {
        let flow_id = Uuid::new_v4();
        let bytes = vec![255_u8; 65_536];
        let binary = encode_binary_payload(1, &flow_id, 0, &bytes).unwrap();
        let json = serde_json::to_vec(&DataFrame::Data {
            flow_id,
            offset: 0,
            bytes: Bytes::from(bytes),
        })
        .unwrap();
        assert_eq!(binary.len(), 65_561);
        assert!(json.len() > binary.len() * 2);
    }

    #[tokio::test]
    async fn binary_reader_rejects_truncated_header() {
        use tokio::io::AsyncWriteExt;

        let (mut left, mut right) = duplex(64);
        left.write_u32(4).await.unwrap();
        left.write_all(&[1, 2, 3, 4]).await.unwrap();
        let error = DataFrameReader::new(&mut right, 64, DataFrameCodec::BinaryV1)
            .read()
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
