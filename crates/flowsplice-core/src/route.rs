use std::io;

use aws_lc_rs::hmac;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

const MAGIC: &[u8; 8] = b"FSLCRTE1";
const SIGNED_LEN: usize = 8 + 1 + 16;
const MAC_LEN: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum RouteSide {
    Travel = 1,
    Relay = 2,
    Home = 3,
}

impl TryFrom<u8> for RouteSide {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Travel),
            2 => Ok(Self::Relay),
            3 => Ok(Self::Home),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid route side",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RoutePreface {
    pub side: RouteSide,
    pub id: Uuid,
}

fn signed_bytes(side: RouteSide, id: Uuid) -> [u8; SIGNED_LEN] {
    let mut bytes = [0_u8; SIGNED_LEN];
    bytes[..MAGIC.len()].copy_from_slice(MAGIC);
    bytes[MAGIC.len()] = side as u8;
    bytes[MAGIC.len() + 1..].copy_from_slice(id.as_bytes());
    bytes
}

/// Writes an authenticated route preface.
///
/// # Errors
///
/// Returns an error when the secret length is invalid or the writer fails.
pub async fn write_preface<W>(
    writer: &mut W,
    side: RouteSide,
    id: Uuid,
    secret: &[u8],
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    if secret.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "route secret must be 32 bytes",
        ));
    }
    let signed = signed_bytes(side, id);
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    let tag = hmac::sign(&key, &signed);
    writer.write_all(&signed).await?;
    writer.write_all(tag.as_ref()).await?;
    writer.flush().await
}

/// Reads the fixed-size route preface without authenticating it.
///
/// # Errors
///
/// Returns an error for truncated input, invalid magic, side, or UUID bytes.
pub async fn read_preface<R>(reader: &mut R) -> io::Result<(RoutePreface, [u8; MAC_LEN])>
where
    R: AsyncRead + Unpin,
{
    let mut signed = [0_u8; SIGNED_LEN];
    let mut mac = [0_u8; MAC_LEN];
    reader.read_exact(&mut signed).await?;
    reader.read_exact(&mut mac).await?;
    if &signed[..MAGIC.len()] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid route preface magic",
        ));
    }
    let side = RouteSide::try_from(signed[MAGIC.len()])?;
    let id = Uuid::from_slice(&signed[MAGIC.len() + 1..])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok((RoutePreface { side, id }, mac))
}

#[must_use]
pub fn verify_preface(preface: RoutePreface, mac: &[u8], secret: &[u8]) -> bool {
    if secret.len() != 32 || mac.len() != MAC_LEN {
        return false;
    }
    let signed = signed_bytes(preface.side, preface.id);
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret);
    hmac::verify(&key, &signed, mac).is_ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use tokio::io::duplex;
    use uuid::Uuid;

    use super::{RouteSide, read_preface, verify_preface, write_preface};

    #[tokio::test]
    async fn signed_preface_round_trip_and_tamper_rejection() {
        let id = Uuid::new_v4();
        let secret = [7_u8; 32];
        let (mut left, mut right) = duplex(128);
        let writer =
            tokio::spawn(
                async move { write_preface(&mut left, RouteSide::Travel, id, &secret).await },
            );
        let (preface, mut mac) = read_preface(&mut right).await.unwrap();
        writer.await.unwrap().unwrap();
        assert_eq!(preface.id, id);
        assert!(verify_preface(preface, &mac, &secret));
        mac[0] ^= 1;
        assert!(!verify_preface(preface, &mac, &secret));
    }
}
