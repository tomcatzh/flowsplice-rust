use std::io;

use serde::{Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Reads one bounded, length-prefixed JSON frame.
///
/// # Errors
///
/// Returns an I/O error for truncated input, an invalid length, or invalid JSON.
pub async fn read_json<R, T>(reader: &mut R, limit: usize) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let len = reader.read_u32().await? as usize;
    if len == 0 || len > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} is outside 1..={limit}"),
        ));
    }
    let mut bytes = vec![0_u8; len];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
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
    use tokio::io::duplex;

    use super::{read_json, write_json};

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
        let received: Message = read_json(&mut right, 1024).await.unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(received.value, "hello");
    }

    #[tokio::test]
    async fn rejects_oversize_length_before_allocation() {
        use tokio::io::AsyncWriteExt;

        let (mut left, mut right) = duplex(16);
        left.write_u32(2048).await.unwrap();
        let error = read_json::<_, Message>(&mut right, 1024).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
