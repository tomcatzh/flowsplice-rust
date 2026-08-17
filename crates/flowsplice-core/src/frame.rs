use std::{io, time::Duration};

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

/// Stateful bounded JSON frame reader that preserves partial input when a read future is cancelled.
///
/// Tokio's `read_exact` is not cancellation safe. This reader deliberately advances its internal
/// prefix and payload cursors only after cancellation-safe `read` calls complete, so `read` may be
/// used inside `tokio::select!` and resumed after another branch wins.
pub struct JsonFrameReader<R> {
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
            reader,
            limit,
            prefix: [0_u8; 4],
            prefix_read: 0,
            payload: Vec::new(),
            payload_read: 0,
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

        let payload = std::mem::take(&mut self.payload);
        self.prefix = [0_u8; 4];
        self.prefix_read = 0;
        self.payload_read = 0;
        serde_json::from_slice(&payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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

/// Writes one bounded, length-prefixed JSON frame.
///
/// # Errors
///
/// Returns an I/O error when serialization fails, the result exceeds `limit`, or writing fails.
pub async fn write_json<W, T>(writer: &mut W, value: &T, limit: usize) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if bytes.is_empty() || bytes.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded frame length {} is outside 1..={limit}",
                bytes.len()
            ),
        ));
    }
    let len = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "frame length overflow"))?;
    writer.write_u32(len).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde::{Deserialize, Serialize};
    use std::time::Duration;

    use tokio::{io::duplex, time::timeout};

    use super::{JsonFrameReader, write_json};

    #[derive(Debug, Deserialize, PartialEq, Serialize)]
    struct Message {
        value: String,
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
}
